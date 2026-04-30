use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use futures::future::join_all;
use ripemd::{Digest as RipemdDigest, Ripemd160};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zcash_address::ZcashAddress;
use zcash_transparent::address::TransparentAddress;
use zebra_chain::parameters::NetworkKind as ZebraNetworkKind;
use zebra_chain::transparent;

use crate::config::{
    Config, Instance, LocalGenesisFundedKey, NetworkKind, OrchardTxblastConfig, resolve_value,
    select_instances, shellexpand,
};
use crate::ssh;
use crate::tmux;
use crate::txblast::orchard::{
    ORCHARD_SPEND_FEE, build_and_send_transparent_fanout_tx, shielding_fee, transparent_fanout_fee,
};
use crate::txblast::rpc::ZebraRpcClient;
use crate::txblast::transparent::FundedKey;
use crate::txblast::{OrchardBlastRuntimeConfig, TxblastNetworkParams};

const STATE_VERSION: u32 = 1;
const DEFAULT_TARGET_BLOCK_BYTES: u64 = 1_000_000;
const DEFAULT_BLOCK_SPACING_SECS: u64 = 75;
const DEFAULT_DURATION_SECS: u64 = 900;
const DEFAULT_MEASURED_TX_BYTES: u64 = 3_000;
const DEFAULT_SAFETY_MARGIN: f64 = 0.20;
const PREPARE_CONFIRMATIONS: u32 = 10;
const REMOTE_INSTALL_TIMEOUT: Duration = Duration::from_secs(45);
const REMOTE_START_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_FUNDED_KEY_PATH: &str = "/root/.config/funded_key.json";

#[derive(Clone, Debug)]
pub struct WalletInitArgs {
    pub directory: Option<String>,
    pub network: Option<NetworkKind>,
    pub birthday_height: Option<u32>,
    pub lanes_per_node: usize,
    pub lane_value_zats: u64,
    pub fanout_width: usize,
    pub require_mainnet_confirmation: bool,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct DepositAddressArgs {
    pub directory: Option<String>,
    pub json: bool,
}

#[derive(Clone, Debug)]
pub struct DepositImportArgs {
    pub directory: Option<String>,
    pub txid: String,
    pub vout: Option<u32>,
    pub amount_zats: Option<u64>,
    pub address: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DepositStatusArgs {
    pub directory: Option<String>,
    pub rpc_endpoint: Option<String>,
    pub confirmations: u32,
    pub json: bool,
}

#[derive(Clone, Debug)]
pub struct PlanArgs {
    pub directory: Option<String>,
    pub target_block_bytes: u64,
    pub block_spacing_secs: u64,
    pub duration_secs: u64,
    pub nodes: String,
    pub measured_tx_bytes: u64,
    pub max_mempool_bytes: Option<u64>,
    pub safety_margin: f64,
    pub allow_underfunded_plan: bool,
    pub json: bool,
}

#[derive(Clone, Debug)]
pub struct GuardedLifecycleArgs {
    pub directory: Option<String>,
    pub plan: Option<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct PublicRunArgs {
    pub directory: Option<String>,
    pub plan: Option<String>,
    pub target_block_bytes: Option<u64>,
    pub max_global_bytes_per_sec: Option<u64>,
    pub max_node_bytes_per_sec: Option<u64>,
    pub max_pending_txs: Option<usize>,
    pub max_pending_bytes: Option<u64>,
    pub max_mempool_bytes: Option<u64>,
    pub feedback_window_blocks: Option<u64>,
    pub trace_dir: Option<String>,
    pub mainnet_i_understand_fees: bool,
}

#[derive(Clone, Debug)]
pub struct WithdrawArgs {
    pub directory: Option<String>,
    pub to: String,
    pub amount: String,
    pub dry_run: bool,
    pub mainnet_i_understand_finality: bool,
}

#[derive(Clone, Debug)]
pub struct RecoverInventoryArgs {
    pub directory: Option<String>,
    pub from_height: Option<u32>,
    pub json: bool,
}

#[derive(Clone, Debug)]
pub struct RecoverSweepArgs {
    pub directory: Option<String>,
    pub to: String,
    pub from_height: Option<u32>,
    pub dry_run: bool,
    pub mainnet_i_understand_recovery: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct TxblastWallet {
    version: u32,
    network: NetworkKind,
    birthday_height: u32,
    created_at_unix: u64,
    control: PublicKeyRecord,
    hot_keys: Vec<PublicKeyRecord>,
    defaults: WalletDefaults,
    deposits: Vec<ImportedDeposit>,
    plans: Vec<TxblastPlan>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TxblastRecovery {
    version: u32,
    network: NetworkKind,
    birthday_height: u32,
    created_at_unix: u64,
    control: SecretKeyRecord,
    hot_keys: Vec<SecretKeyRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PublicKeyRecord {
    key_id: String,
    role: KeyRole,
    node_name: Option<String>,
    address: String,
    public_key_hex: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SecretKeyRecord {
    key_id: String,
    role: KeyRole,
    node_name: Option<String>,
    address: String,
    public_key_hex: String,
    secret_key_hex: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeyRole {
    Control,
    Hot,
}

#[derive(Debug, Serialize, Deserialize)]
struct WalletDefaults {
    lanes_per_node: usize,
    lane_value_zats: u64,
    fanout_width: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ImportedDeposit {
    txid: String,
    vout: Option<u32>,
    amount_zats: Option<u64>,
    address: Option<String>,
    imported_at_unix: u64,
    state: DepositState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DepositState {
    Imported,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TxblastPlan {
    id: String,
    created_at_unix: u64,
    network: NetworkKind,
    target_block_bytes: u64,
    block_spacing_secs: u64,
    duration_secs: u64,
    measured_tx_bytes: u64,
    selected_nodes: Vec<String>,
    global_bytes_per_sec: f64,
    global_txs_per_sec: f64,
    per_node_bytes_per_sec: f64,
    per_node_txs_per_sec: f64,
    lanes_per_node: usize,
    lane_value_zats: u64,
    expected_run_txs: u64,
    run_fee_zats: u64,
    prepare_fee_zats: u64,
    withdraw_fee_zats: u64,
    required_zats_before_margin: u64,
    required_zats_with_margin: u64,
    imported_deposit_zats: u64,
    underfunded: bool,
    max_mempool_bytes: Option<u64>,
    safety_margin: f64,
}

#[derive(Debug, Serialize)]
struct DepositStatus {
    network: NetworkKind,
    birthday_height: u32,
    deposit_address: String,
    imported_deposit_count: usize,
    imported_deposit_zats: u64,
    rpc_endpoint: Option<String>,
    rpc_error: Option<String>,
    chain_height: Option<u32>,
    confirmed_utxo_count: Option<usize>,
    confirmed_utxo_zats: Option<u64>,
    confirmations_required: u32,
    latest_plan: Option<TxblastPlan>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicPrepareRecord {
    version: u32,
    plan_id: String,
    network: NetworkKind,
    fanout_txid: String,
    prepared_at_unix: u64,
    hot_keys: Vec<PreparedHotKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreparedHotKey {
    node_name: String,
    address: String,
    value_zats: u64,
}

pub async fn wallet_init(base_directory: &str, args: WalletInitArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = load_config_if_present(&dir)?;
    let network = args
        .network
        .or_else(|| config.as_ref().map(|config| config.network_kind))
        .unwrap_or(NetworkKind::PublicTestnet);
    ensure_public_network(network, "txblast wallet init")?;
    let _network_params = TxblastNetworkParams::from_network_kind(network);
    if network == NetworkKind::Mainnet && !args.require_mainnet_confirmation {
        anyhow::bail!(
            "refusing to create a mainnet txblast wallet without --require-mainnet-confirmation"
        );
    }
    if args.lanes_per_node == 0 {
        anyhow::bail!("--lanes-per-node must be greater than 0");
    }
    if args.lane_value_zats == 0 {
        anyhow::bail!("--lane-value-zats must be greater than 0");
    }
    if args.fanout_width == 0 {
        anyhow::bail!("--fanout-width must be greater than 0");
    }

    let state_dir = state_dir(&dir);
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create {}", state_dir.display()))?;
    let wallet_path = wallet_path(&dir);
    let recovery_path = recovery_path(&dir);
    if !args.force && (wallet_path.exists() || recovery_path.exists()) {
        anyhow::bail!(
            "txblast wallet already exists at {}; pass --force to replace it",
            state_dir.display()
        );
    }

    let created_at_unix = now_unix();
    let birthday_height = args.birthday_height.unwrap_or(0);
    let control = generate_key_record(network, "control", KeyRole::Control, None)?;
    let node_names = config
        .as_ref()
        .map(|config| {
            config
                .miners
                .iter()
                .map(|instance| instance.name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let hot_keys = node_names
        .iter()
        .map(|node_name| {
            generate_key_record(
                network,
                &format!("hot-{node_name}"),
                KeyRole::Hot,
                Some(node_name),
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let wallet = TxblastWallet {
        version: STATE_VERSION,
        network,
        birthday_height,
        created_at_unix,
        control: control.public(),
        hot_keys: hot_keys.iter().map(SecretKeyRecord::public).collect(),
        defaults: WalletDefaults {
            lanes_per_node: args.lanes_per_node,
            lane_value_zats: args.lane_value_zats,
            fanout_width: args.fanout_width,
        },
        deposits: vec![],
        plans: vec![],
    };
    let recovery = TxblastRecovery {
        version: STATE_VERSION,
        network,
        birthday_height,
        created_at_unix,
        control,
        hot_keys,
    };

    write_json(&wallet_path, &wallet)?;
    write_json_private(&recovery_path, &recovery)?;

    println!(
        "Created public-network txblast wallet in {}",
        state_dir.display()
    );
    println!("  network: {}", network);
    println!("  deposit address: {}", wallet.control.address);
    println!("  hot keys: {}", wallet.hot_keys.len());
    println!("  recovery bundle: {}", recovery_path.display());
    Ok(())
}

pub fn deposit_address(base_directory: &str, args: DepositAddressArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let wallet = load_wallet(&dir)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "network": wallet.network,
                "transparent_address": wallet.control.address,
            }))?
        );
    } else {
        println!("{}", wallet.control.address);
    }
    Ok(())
}

pub fn deposit_import(base_directory: &str, args: DepositImportArgs) -> Result<()> {
    validate_txid(&args.txid)?;
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let mut wallet = load_wallet(&dir)?;
    if let Some(address) = args.address.as_deref() {
        validate_transparent_address_for_network(address, wallet.network)?;
    }
    if wallet
        .deposits
        .iter()
        .any(|deposit| deposit.txid == args.txid && deposit.vout == args.vout)
    {
        anyhow::bail!("deposit {}:{:?} is already imported", args.txid, args.vout);
    }

    wallet.deposits.push(ImportedDeposit {
        txid: args.txid,
        vout: args.vout,
        amount_zats: args.amount_zats,
        address: args.address,
        imported_at_unix: now_unix(),
        state: DepositState::Imported,
    });
    write_json(&wallet_path(&dir), &wallet)?;
    println!(
        "Imported deposit candidate. It remains untrusted until verified by RPC and confirmations."
    );
    Ok(())
}

pub async fn deposit_status(base_directory: &str, args: DepositStatusArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let wallet = load_wallet(&dir)?;
    let imported_deposit_zats = imported_deposit_zats(&wallet);
    let explicit_rpc_endpoint = args.rpc_endpoint.is_some();
    let rpc_endpoint = args
        .rpc_endpoint
        .or_else(|| default_rpc_endpoint(&dir).ok());
    let mut rpc_error = None;
    let mut chain_height = None;
    let mut confirmed_utxo_count = None;
    let mut confirmed_utxo_zats = None;

    if let Some(endpoint) = rpc_endpoint.as_deref() {
        let client = ZebraRpcClient::new(endpoint);
        match client.get_block_count().await {
            Ok(current) => {
                let utxos = match client.get_address_utxos(&wallet.control.address).await {
                    Ok(utxos) => utxos,
                    Err(error) if !explicit_rpc_endpoint => {
                        rpc_error = Some(error.to_string());
                        vec![]
                    }
                    Err(error) => return Err(error),
                };
                let confirmed = utxos
                    .into_iter()
                    .filter(|utxo| {
                        utxo.height > 0
                            && current.saturating_sub(utxo.height).saturating_add(1)
                                >= args.confirmations
                    })
                    .collect::<Vec<_>>();
                chain_height = Some(current);
                confirmed_utxo_count = Some(confirmed.len());
                confirmed_utxo_zats = Some(confirmed.iter().map(|utxo| utxo.satoshis).sum());
            }
            Err(error) if !explicit_rpc_endpoint => {
                rpc_error = Some(error.to_string());
            }
            Err(error) => return Err(error),
        }
    }

    let latest_plan = wallet.plans.last().cloned();
    let status = DepositStatus {
        network: wallet.network,
        birthday_height: wallet.birthday_height,
        deposit_address: wallet.control.address,
        imported_deposit_count: wallet.deposits.len(),
        imported_deposit_zats,
        rpc_endpoint,
        rpc_error,
        chain_height,
        confirmed_utxo_count,
        confirmed_utxo_zats,
        confirmations_required: args.confirmations,
        latest_plan,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("network: {}", status.network);
        println!("deposit address: {}", status.deposit_address);
        println!(
            "imported deposits: {} ({} zats declared)",
            status.imported_deposit_count, status.imported_deposit_zats
        );
        if let Some(endpoint) = status.rpc_endpoint.as_deref() {
            println!("rpc endpoint: {endpoint}");
            if let Some(error) = status.rpc_error.as_deref() {
                println!("rpc status: unavailable ({error})");
            }
            println!(
                "chain height: {}",
                status
                    .chain_height
                    .map(|height| height.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "confirmed UTXOs: {} ({} zats)",
                status.confirmed_utxo_count.unwrap_or(0),
                status.confirmed_utxo_zats.unwrap_or(0)
            );
        } else {
            println!("rpc endpoint: not configured; status is imported-deposit only");
        }
        if let Some(plan) = status.latest_plan.as_ref() {
            println!(
                "latest plan: {} required={} zats underfunded={}",
                plan.id, plan.required_zats_with_margin, plan.underfunded
            );
        }
    }

    Ok(())
}

pub fn plan(base_directory: &str, args: PlanArgs) -> Result<()> {
    if args.target_block_bytes == 0 {
        anyhow::bail!("--target-block-bytes must be greater than 0");
    }
    if args.block_spacing_secs == 0 {
        anyhow::bail!("--block-spacing-secs must be greater than 0");
    }
    if args.duration_secs == 0 {
        anyhow::bail!("--duration-secs must be greater than 0");
    }
    if args.measured_tx_bytes == 0 {
        anyhow::bail!("--measured-tx-bytes must be greater than 0");
    }
    if args.safety_margin < 0.0 {
        anyhow::bail!("--safety-margin must not be negative");
    }

    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast plan")?;
    let mut wallet = load_wallet(&dir)?;
    if wallet.network != config.network_kind {
        anyhow::bail!(
            "wallet network {} does not match experiment network {}",
            wallet.network,
            config.network_kind
        );
    }
    let targets = select_instances(&config.miners, &args.nodes);
    if targets.is_empty() {
        anyhow::bail!("no matching nodes for --nodes {}", args.nodes);
    }

    let node_count = targets.len() as u64;
    let global_bytes_per_sec = args.target_block_bytes as f64 / args.block_spacing_secs as f64;
    let global_txs_per_sec = global_bytes_per_sec / args.measured_tx_bytes as f64;
    let per_node_bytes_per_sec = global_bytes_per_sec / node_count as f64;
    let per_node_txs_per_sec = global_txs_per_sec / node_count as f64;
    let expected_run_txs = (global_txs_per_sec * args.duration_secs as f64).ceil() as u64;
    let run_fee_zats = expected_run_txs.saturating_mul(ORCHARD_SPEND_FEE);
    let prepare_fee_zats = node_count.saturating_mul(shielding_fee(wallet.defaults.lanes_per_node));
    let withdraw_fee_zats = node_count.saturating_mul(ORCHARD_SPEND_FEE);
    let lane_principal_zats = node_count
        .saturating_mul(wallet.defaults.lanes_per_node as u64)
        .saturating_mul(wallet.defaults.lane_value_zats);
    let required_zats_before_margin = lane_principal_zats
        .saturating_add(run_fee_zats)
        .saturating_add(prepare_fee_zats)
        .saturating_add(withdraw_fee_zats);
    let required_zats_with_margin = apply_margin(required_zats_before_margin, args.safety_margin)?;
    let imported_deposit_zats = imported_deposit_zats(&wallet);
    let underfunded = imported_deposit_zats < required_zats_with_margin;
    if underfunded && !args.allow_underfunded_plan {
        anyhow::bail!(
            "imported deposits declare {} zats, but the plan requires {} zats with margin; pass --allow-underfunded-plan to record it anyway",
            imported_deposit_zats,
            required_zats_with_margin
        );
    }

    let created_at_unix = now_unix();
    let plan = TxblastPlan {
        id: format!("plan-{created_at_unix}"),
        created_at_unix,
        network: wallet.network,
        target_block_bytes: args.target_block_bytes,
        block_spacing_secs: args.block_spacing_secs,
        duration_secs: args.duration_secs,
        measured_tx_bytes: args.measured_tx_bytes,
        selected_nodes: targets.iter().map(|target| target.name.clone()).collect(),
        global_bytes_per_sec,
        global_txs_per_sec,
        per_node_bytes_per_sec,
        per_node_txs_per_sec,
        lanes_per_node: wallet.defaults.lanes_per_node,
        lane_value_zats: wallet.defaults.lane_value_zats,
        expected_run_txs,
        run_fee_zats,
        prepare_fee_zats,
        withdraw_fee_zats,
        required_zats_before_margin,
        required_zats_with_margin,
        imported_deposit_zats,
        underfunded,
        max_mempool_bytes: args.max_mempool_bytes,
        safety_margin: args.safety_margin,
    };
    wallet.plans.push(plan.clone());
    write_json(&wallet_path(&dir), &wallet)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("created txblast plan {}", plan.id);
        println!(
            "  target: {} bytes/block over {}s blocks ({:.2} bytes/s)",
            plan.target_block_bytes, plan.block_spacing_secs, plan.global_bytes_per_sec
        );
        println!(
            "  fleet: {} nodes, {:.4} tx/s global, {:.4} tx/s per node",
            plan.selected_nodes.len(),
            plan.global_txs_per_sec,
            plan.per_node_txs_per_sec
        );
        println!(
            "  required: {} zats before margin, {} zats with margin",
            plan.required_zats_before_margin, plan.required_zats_with_margin
        );
        if plan.underfunded {
            println!("  funding: underfunded from imported deposits");
        }
    }

    Ok(())
}

pub async fn prepare(base_directory: &str, args: GuardedLifecycleArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast prepare")?;
    let wallet = load_wallet(&dir)?;
    let recovery = load_recovery(&dir)?;
    ensure_wallet_matches_config(&wallet, &recovery, &config)?;
    let plan = select_plan(&wallet, args.plan.as_deref())?.clone();
    let targets = active_instances_for_plan(&config, &plan)?;
    let hot_keys = hot_keys_for_targets(&recovery, &targets)?;
    let minimum_per_node = shielding_fee(plan.lanes_per_node)
        .saturating_add(plan.lanes_per_node as u64 * plan.lane_value_zats);

    if args.dry_run {
        println!(
            "dry run: prepare would fan out at least {} zats into {} hot keys for plan {} on {}",
            plan.required_zats_with_margin,
            targets.len(),
            plan.id,
            wallet.network
        );
        return Ok(());
    }

    let operator = targets
        .first()
        .context("prepare requires at least one active planned node")?;
    let rpc_endpoint = format!("http://{}:{}", operator.public_ip, config.rpc_port());
    println!(
        "using {} RPC endpoint for public prepare: {}",
        operator.name, rpc_endpoint
    );
    let client = ZebraRpcClient::new(&rpc_endpoint);
    let current_height = client.get_block_count().await?;
    let confirmed_utxos = confirmed_control_utxos(
        &client,
        &wallet.control.address,
        current_height,
        PREPARE_CONFIRMATIONS,
    )
    .await?;
    let selected = select_fanout_inputs(
        confirmed_utxos,
        plan.required_zats_with_margin,
        targets.len(),
    )?;
    let input_total: u64 = selected.iter().map(|utxo| utxo.satoshis).sum();
    let (hot_total, change_value, fee) = plan_fanout_amounts(
        input_total,
        selected.len(),
        plan.required_zats_with_margin,
        targets.len(),
    )?;
    let per_node_values = split_amount(hot_total, targets.len());
    for (target, value) in targets.iter().zip(per_node_values.iter()) {
        if *value < minimum_per_node {
            anyhow::bail!(
                "planned hot-key value for {} is {} zats, below the minimum {} zats needed to shield {} lanes",
                target.name,
                value,
                minimum_per_node,
                plan.lanes_per_node
            );
        }
    }

    let mut recipients = Vec::with_capacity(targets.len() + usize::from(change_value > 0));
    let mut prepared_hot_keys = Vec::with_capacity(targets.len());
    for ((target, hot_key), value) in targets.iter().zip(hot_keys.iter()).zip(per_node_values) {
        recipients.push((transparent_address_for_encoded(&hot_key.address)?, value));
        prepared_hot_keys.push(PreparedHotKey {
            node_name: target.name.clone(),
            address: hot_key.address.clone(),
            value_zats: value,
        });
    }
    if change_value > 0 {
        recipients.push((
            transparent_address_for_encoded(&wallet.control.address)?,
            change_value,
        ));
    }

    let control_key = funded_key_from_secret(&recovery.control)?;
    let fanout_txid = build_and_send_transparent_fanout_tx(
        TxblastNetworkParams::from_network_kind(wallet.network),
        &client,
        &control_key,
        &selected,
        current_height.saturating_add(1),
        &recipients,
    )
    .await?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);
    install_hot_keys(&targets, &hot_keys, &key).await?;

    let record = PublicPrepareRecord {
        version: STATE_VERSION,
        plan_id: plan.id.clone(),
        network: wallet.network,
        fanout_txid: fanout_txid.clone(),
        prepared_at_unix: now_unix(),
        hot_keys: prepared_hot_keys,
    };
    write_json(&prepare_path(&dir, &plan.id), &record)?;
    write_json(&latest_prepare_path(&dir), &record)?;

    println!("prepared public txblast plan {}", plan.id);
    println!("  fanout txid: {fanout_txid}");
    println!("  hot keys: {}", targets.len());
    println!("  hot-key total: {hot_total} zats");
    println!("  change: {change_value} zats");
    println!("  transparent fanout fee: {fee} zats");
    println!("  remote funded key path: {REMOTE_FUNDED_KEY_PATH}");
    Ok(())
}

pub async fn run_public(base_directory: &str, args: PublicRunArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast run")?;
    let wallet = load_wallet(&dir)?;
    let plan = select_plan(&wallet, args.plan.as_deref())?;
    let prepare_record = load_latest_prepare(&dir)?;
    if prepare_record.plan_id != plan.id {
        anyhow::bail!(
            "latest prepare record is for {}, but run selected {}; rerun `kresko txblast prepare --plan {}`",
            prepare_record.plan_id,
            plan.id,
            plan.id
        );
    }
    if prepare_record.network != wallet.network {
        anyhow::bail!(
            "prepare record network {} does not match wallet network {}",
            prepare_record.network,
            wallet.network
        );
    }
    if config.network_kind == NetworkKind::Mainnet && !args.mainnet_i_understand_fees {
        anyhow::bail!("refusing mainnet public txblast run without --mainnet-i-understand-fees");
    }
    let targets = active_instances_for_plan(&config, plan)?;
    for target in &targets {
        if !prepare_record
            .hot_keys
            .iter()
            .any(|hot_key| hot_key.node_name == target.name)
        {
            anyhow::bail!(
                "prepare record for plan {} is missing hot-key funding for {}; rerun `kresko txblast prepare --plan {}`",
                plan.id,
                target.name,
                plan.id
            );
        }
    }
    let target_block_bytes = args.target_block_bytes.unwrap_or(plan.target_block_bytes);
    let global_bps = args
        .max_global_bytes_per_sec
        .map(|value| value as f64)
        .unwrap_or(target_block_bytes as f64 / plan.block_spacing_secs as f64);
    let per_node_bps = args
        .max_node_bytes_per_sec
        .map(|value| value as f64)
        .unwrap_or(global_bps / targets.len() as f64);
    let rate = (per_node_bps / plan.measured_tx_bytes as f64)
        .ceil()
        .max(1.0) as u64;
    let pending_tx_cap = args.max_pending_txs.or_else(|| {
        args.max_pending_bytes
            .map(|bytes| std::cmp::max(1, (bytes / plan.measured_tx_bytes).max(1) as usize))
    });
    let orchard_premine = OrchardTxblastConfig {
        lanes_per_miner: plan.lanes_per_node,
        lane_value_zats: plan.lane_value_zats,
        fanout_source_value_zats: config.orchard_txblast.fanout_source_value_zats,
        fanout_outputs: wallet.defaults.fanout_width,
    };
    let orchard_runtime = OrchardBlastRuntimeConfig::from_parts_with_network(
        orchard_premine,
        TxblastNetworkParams::from_network_kind(wallet.network),
        pending_tx_cap,
        Some(plan.lanes_per_node),
        None,
        None,
        None,
        None,
    )?;
    println!("public txblast run plan {}", plan.id);
    println!("  selected nodes: {}", plan.selected_nodes.len());
    println!("  global byte budget: {:.2} bytes/s", global_bps);
    println!(
        "  per-node budget: {:.2} bytes/s (~{} tx/s)",
        per_node_bps, rate
    );
    println!(
        "  per-node cap: {}",
        args.max_node_bytes_per_sec
            .map(|value| format!("{value} bytes/s"))
            .unwrap_or_else(|| "plan split".to_string())
    );
    println!(
        "  pending guards: txs={:?} bytes={:?} mempool={:?}",
        args.max_pending_txs, args.max_pending_bytes, args.max_mempool_bytes
    );
    println!(
        "  feedback window: {:?} blocks, trace dir: {}",
        args.feedback_window_blocks,
        args.trace_dir.as_deref().unwrap_or("(default)")
    );
    if args.max_mempool_bytes.is_some() || args.feedback_window_blocks.is_some() {
        println!(
            "  note: mempool feedback controls are recorded but not enforced by txblast-local"
        );
    }

    let rpc_port = config.rpc_port();
    let trace_dir = args
        .trace_dir
        .as_deref()
        .unwrap_or("/root/.cache/kresko/txblast-traces");
    let script = format!(
        r#"#!/bin/bash
kresko txblast-local \
    --rpc-endpoint http://localhost:{rpc_port} \
    --network {network} \
    --rate {rate} \
    --amount 0.001 \
    --orchard-lanes-per-miner {lanes_per_miner} \
    --orchard-lane-value-zats {lane_value_zats} \
    --orchard-fanout-source-value-zats {fanout_source_value_zats} \
    --orchard-fanout-outputs {fanout_outputs} \
    --orchard-max-in-flight {max_in_flight} \
    --orchard-target-ready-lanes {target_ready_lanes} \
    --orchard-lane-low-watermark {lane_low_watermark} \
    --orchard-fanout-max-in-flight {fanout_max_in_flight} \
    --orchard-proving-workers {proving_workers} \
    --orchard-progress-interval-secs {progress_interval_secs} \
    --skip-funding \
    --trace-dir {trace_dir} \
    --funded-key-path {funded_key_path}
"#,
        network = wallet.network,
        lanes_per_miner = orchard_runtime.lane_premine.lanes_per_miner,
        lane_value_zats = orchard_runtime.lane_premine.lane_value_zats,
        fanout_source_value_zats = orchard_runtime.lane_premine.fanout_source_value_zats,
        fanout_outputs = orchard_runtime.lane_premine.fanout_outputs,
        max_in_flight = orchard_runtime.max_in_flight,
        target_ready_lanes = orchard_runtime.target_ready_lanes,
        lane_low_watermark = orchard_runtime.lane_low_watermark,
        fanout_max_in_flight = orchard_runtime.fanout_max_in_flight,
        proving_workers = orchard_runtime.proving_workers,
        progress_interval_secs = orchard_runtime.progress_interval.as_secs(),
        trace_dir = shell_single_quote(trace_dir),
        funded_key_path = shell_single_quote(REMOTE_FUNDED_KEY_PATH),
    );

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);
    let owned_targets: Vec<_> = targets.into_iter().cloned().collect();
    let results = tmux::run_script_in_tmux(
        &owned_targets,
        &key,
        &script,
        "txblast",
        REMOTE_START_TIMEOUT,
    )
    .await;

    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: public txblast started"),
            Err(e) => eprintln!("  {name}: failed: {e}"),
        }
    }
    Ok(())
}

pub fn stop(base_directory: &str, args: GuardedLifecycleArgs) -> Result<()> {
    guarded_lifecycle(
        base_directory,
        args.directory.as_deref(),
        args.plan.as_deref(),
        args.dry_run,
        "stop",
        "public txblast stop is waiting for the public runner control channel; use `kresko kill-session --session txblast` for existing local-genesis sessions",
    )
}

pub fn status(base_directory: &str, args: GuardedLifecycleArgs) -> Result<()> {
    guarded_lifecycle(
        base_directory,
        args.directory.as_deref(),
        args.plan.as_deref(),
        args.dry_run,
        "status",
        "wallet/deposit status is available via `kresko txblast deposit status`; unified public workload status needs scanner-backed lane inventory",
    )
}

pub fn withdraw(base_directory: &str, args: WithdrawArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let wallet = load_wallet(&dir)?;
    validate_transparent_address_for_network(&args.to, wallet.network)?;
    if wallet.network == NetworkKind::Mainnet && !args.mainnet_i_understand_finality {
        anyhow::bail!("refusing mainnet withdrawal without --mainnet-i-understand-finality");
    }
    if args.dry_run {
        println!(
            "dry run: would withdraw {} to {} on {}",
            args.amount, args.to, wallet.network
        );
        return Ok(());
    }
    anyhow::bail!(
        "public txblast withdraw is not enabled until scanner-backed fan-in and sweep transaction building is implemented"
    );
}

pub fn recover_inventory(base_directory: &str, args: RecoverInventoryArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let wallet = load_wallet(&dir)?;
    let recovery = load_recovery(&dir)?;
    let from_height = args.from_height.unwrap_or(wallet.birthday_height);
    let report = serde_json::json!({
        "network": wallet.network,
        "from_height": from_height,
        "control_key": recovery.control.address,
        "hot_keys": recovery.hot_keys.len(),
        "scanner": "not_implemented",
        "message": "recovery bundle is present; chain scanner and sweep builder are the next implementation phase",
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("network: {}", wallet.network);
        println!("from height: {from_height}");
        println!("control key: {}", recovery.control.address);
        println!("hot keys: {}", recovery.hot_keys.len());
        println!("scanner: not implemented yet");
    }
    Ok(())
}

pub fn recover_sweep(base_directory: &str, args: RecoverSweepArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let wallet = load_wallet(&dir)?;
    validate_transparent_address_for_network(&args.to, wallet.network)?;
    if wallet.network == NetworkKind::Mainnet && !args.mainnet_i_understand_recovery {
        anyhow::bail!("refusing mainnet recovery sweep without --mainnet-i-understand-recovery");
    }
    if args.dry_run {
        println!(
            "dry run: would scan from height {} and sweep recoverable funds to {}",
            args.from_height.unwrap_or(wallet.birthday_height),
            args.to
        );
        return Ok(());
    }
    anyhow::bail!(
        "public txblast recovery sweep is not enabled until the scanner-backed sweep builder is implemented"
    );
}

fn guarded_lifecycle(
    base_directory: &str,
    directory: Option<&str>,
    plan_id: Option<&str>,
    dry_run: bool,
    command_name: &str,
    message: &str,
) -> Result<()> {
    let dir = resolve_directory(base_directory, directory);
    let wallet = load_wallet(&dir)?;
    let plan = select_plan(&wallet, plan_id)?;
    if dry_run {
        println!(
            "dry run: {command_name} would use plan {} on {}",
            plan.id, wallet.network
        );
        return Ok(());
    }
    anyhow::bail!("{message}");
}

fn select_plan<'a>(wallet: &'a TxblastWallet, plan_id: Option<&str>) -> Result<&'a TxblastPlan> {
    match plan_id {
        Some(id) => wallet
            .plans
            .iter()
            .find(|plan| plan.id == id)
            .with_context(|| format!("unknown txblast plan id {id}")),
        None => wallet
            .plans
            .last()
            .context("no txblast plans exist; run `kresko txblast plan` first"),
    }
}

fn ensure_wallet_matches_config(
    wallet: &TxblastWallet,
    recovery: &TxblastRecovery,
    config: &Config,
) -> Result<()> {
    if wallet.network != config.network_kind {
        anyhow::bail!(
            "wallet network {} does not match experiment network {}",
            wallet.network,
            config.network_kind
        );
    }
    if recovery.network != wallet.network {
        anyhow::bail!(
            "recovery network {} does not match wallet network {}",
            recovery.network,
            wallet.network
        );
    }
    Ok(())
}

fn active_instances_for_plan<'a>(
    config: &'a Config,
    plan: &TxblastPlan,
) -> Result<Vec<&'a Instance>> {
    let mut targets = Vec::with_capacity(plan.selected_nodes.len());
    for node_name in &plan.selected_nodes {
        let instance = config
            .miners
            .iter()
            .find(|instance| instance.name == *node_name && instance.public_ip != "TBD")
            .with_context(|| format!("planned node {node_name} is not active in config.json"))?;
        targets.push(instance);
    }
    if targets.is_empty() {
        anyhow::bail!("plan {} has no selected nodes", plan.id);
    }
    Ok(targets)
}

fn hot_keys_for_targets<'a>(
    recovery: &'a TxblastRecovery,
    targets: &[&Instance],
) -> Result<Vec<&'a SecretKeyRecord>> {
    targets
        .iter()
        .map(|target| {
            recovery
                .hot_keys
                .iter()
                .find(|key| key.node_name.as_deref() == Some(target.name.as_str()))
                .with_context(|| {
                    format!(
                        "no hot key in recovery bundle for {}; recreate the wallet after adding nodes",
                        target.name
                    )
                })
        })
        .collect()
}

async fn confirmed_control_utxos(
    client: &ZebraRpcClient,
    address: &str,
    current_height: u32,
    confirmations: u32,
) -> Result<Vec<crate::txblast::rpc::AddressUtxo>> {
    let mut utxos = client.get_address_utxos(address).await?;
    utxos.retain(|utxo| {
        utxo.height > 0
            && current_height.saturating_sub(utxo.height).saturating_add(1) >= confirmations
    });
    utxos.sort_by(|a, b| b.satoshis.cmp(&a.satoshis));
    if utxos.is_empty() {
        anyhow::bail!(
            "no control-wallet UTXOs with at least {} confirmations at {}",
            confirmations,
            address
        );
    }
    Ok(utxos)
}

fn select_fanout_inputs(
    utxos: Vec<crate::txblast::rpc::AddressUtxo>,
    required_hot_zats: u64,
    node_count: usize,
) -> Result<Vec<crate::txblast::rpc::AddressUtxo>> {
    let mut selected = Vec::new();
    let mut total = 0u64;

    for utxo in utxos {
        selected.push(utxo);
        total = selected.iter().map(|input| input.satoshis).sum();
        let fee = transparent_fanout_fee(selected.len(), node_count);
        if total >= required_hot_zats.saturating_add(fee) {
            return Ok(selected);
        }
    }

    anyhow::bail!(
        "confirmed control-wallet balance {} zats is insufficient; need at least {} zats plus transparent fanout fees",
        total,
        required_hot_zats
    )
}

fn plan_fanout_amounts(
    input_total: u64,
    input_count: usize,
    required_hot_zats: u64,
    node_count: usize,
) -> Result<(u64, u64, u64)> {
    let fee_without_change = transparent_fanout_fee(input_count, node_count);
    if input_total < required_hot_zats.saturating_add(fee_without_change) {
        anyhow::bail!(
            "selected inputs {} zats are insufficient for {} zats plus fee {}",
            input_total,
            required_hot_zats,
            fee_without_change
        );
    }

    let fee_with_change = transparent_fanout_fee(input_count, node_count + 1);
    if input_total > required_hot_zats.saturating_add(fee_with_change) {
        let change = input_total - required_hot_zats - fee_with_change;
        Ok((required_hot_zats, change, fee_with_change))
    } else {
        let hot_total = input_total - fee_without_change;
        Ok((hot_total, 0, fee_without_change))
    }
}

fn split_amount(total: u64, parts: usize) -> Vec<u64> {
    let base = total / parts as u64;
    let remainder = total % parts as u64;
    (0..parts)
        .map(|idx| base + u64::from(idx < remainder as usize))
        .collect()
}

fn resolve_directory(base_directory: &str, override_directory: Option<&str>) -> PathBuf {
    PathBuf::from(override_directory.unwrap_or(base_directory))
}

fn state_dir(dir: &Path) -> PathBuf {
    dir.join(".kresko").join("txblast")
}

fn wallet_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("wallet.json")
}

fn recovery_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("recovery.json")
}

fn prepare_path(dir: &Path, plan_id: &str) -> PathBuf {
    state_dir(dir).join(format!("prepared-{plan_id}.json"))
}

fn latest_prepare_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("prepared.latest.json")
}

fn load_latest_prepare(dir: &Path) -> Result<PublicPrepareRecord> {
    let path = latest_prepare_path(dir);
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_config_if_present(dir: &Path) -> Result<Option<Config>> {
    let path = dir.join("config.json");
    if path.exists() {
        Ok(Some(Config::load(dir)?))
    } else {
        Ok(None)
    }
}

fn load_wallet(dir: &Path) -> Result<TxblastWallet> {
    let path = wallet_path(dir);
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_recovery(dir: &Path) -> Result<TxblastRecovery> {
    let path = recovery_path(dir);
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_json(path, value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

fn ensure_public_network(network: NetworkKind, command_name: &str) -> Result<()> {
    if network.is_public_network() {
        Ok(())
    } else {
        anyhow::bail!("{command_name} requires public-testnet or mainnet")
    }
}

fn default_rpc_endpoint(dir: &Path) -> Result<String> {
    let config = Config::load(dir)?;
    Ok(format!("http://localhost:{}", config.rpc_port()))
}

fn imported_deposit_zats(wallet: &TxblastWallet) -> u64 {
    wallet
        .deposits
        .iter()
        .filter_map(|deposit| deposit.amount_zats)
        .sum()
}

fn apply_margin(value: u64, margin: f64) -> Result<u64> {
    if !margin.is_finite() {
        anyhow::bail!("safety margin must be finite");
    }
    Ok(((value as f64) * (1.0 + margin)).ceil() as u64)
}

fn validate_txid(txid: &str) -> Result<()> {
    let decoded = hex::decode(txid).with_context(|| format!("invalid txid hex: {txid}"))?;
    if decoded.len() != 32 {
        anyhow::bail!("txid must decode to 32 bytes");
    }
    Ok(())
}

fn validate_transparent_address_for_network(address: &str, network: NetworkKind) -> Result<()> {
    let parsed: transparent::Address = address
        .parse()
        .with_context(|| format!("invalid transparent address: {address}"))?;
    let expected = zebra_network_kind(network);
    if parsed.network_kind() != expected {
        anyhow::bail!(
            "address {} belongs to {:?}, expected {:?} for {}",
            address,
            parsed.network_kind(),
            expected,
            network
        );
    }
    Ok(())
}

fn transparent_address_for_encoded(address: &str) -> Result<TransparentAddress> {
    let zaddr = ZcashAddress::try_from_encoded(address)
        .with_context(|| format!("invalid transparent address {address}"))?;
    zaddr
        .convert::<TransparentAddress>()
        .map_err(|e| anyhow::anyhow!("address {address} is not a transparent address: {:?}", e))
}

fn funded_key_from_secret(record: &SecretKeyRecord) -> Result<FundedKey> {
    let key_bytes = hex::decode(&record.secret_key_hex)
        .with_context(|| format!("{} secret_key_hex is not valid hex", record.key_id))?;
    if key_bytes.len() != 32 {
        anyhow::bail!(
            "{} secret_key_hex must decode to 32 bytes, got {}",
            record.key_id,
            key_bytes.len()
        );
    }
    let secret_key = SecretKey::from_slice(&key_bytes)
        .with_context(|| format!("{} secret key is invalid", record.key_id))?;
    let secp = Secp256k1::new();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    if hex::encode(public_key.serialize()) != record.public_key_hex {
        anyhow::bail!(
            "{} public_key_hex does not match secret_key_hex",
            record.key_id
        );
    }
    let address: transparent::Address = record
        .address
        .parse()
        .with_context(|| format!("invalid transparent address {}", record.address))?;
    Ok(FundedKey {
        name: record.key_id.clone(),
        address,
        secret_key,
        public_key,
    })
}

async fn install_hot_keys(
    targets: &[&Instance],
    hot_keys: &[&SecretKeyRecord],
    ssh_key: &str,
) -> Result<()> {
    let futs = targets
        .iter()
        .zip(hot_keys.iter())
        .map(|(target, hot_key)| {
            let ip = target.public_ip.clone();
            let name = target.name.clone();
            let ssh_key = ssh_key.to_owned();
            let payload = LocalGenesisFundedKey {
                name: hot_key.key_id.clone(),
                secret_key_hex: hot_key.secret_key_hex.clone(),
                public_key_hex: hot_key.public_key_hex.clone(),
                address: hot_key.address.clone(),
            };
            async move {
                let json = serde_json::to_string_pretty(&payload)?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(json);
                let command = format!(
                    "mkdir -p /root/.config && echo '{}' | base64 -d > {} && chmod 0600 {}",
                    encoded, REMOTE_FUNDED_KEY_PATH, REMOTE_FUNDED_KEY_PATH
                );
                ssh::ssh_exec_timeout(&ip, &ssh_key, &command, REMOTE_INSTALL_TIMEOUT)
                    .await
                    .with_context(|| format!("failed to install hot key on {name}"))?;
                Ok::<_, anyhow::Error>(name)
            }
        });

    for result in join_all(futs).await {
        let name = result?;
        println!("  {name}: installed hot funded key");
    }
    Ok(())
}

fn generate_key_record(
    network: NetworkKind,
    key_id: &str,
    role: KeyRole,
    node_name: Option<&String>,
) -> Result<SecretKeyRecord> {
    let secp = Secp256k1::new();
    let secret_key = loop {
        let bytes = rand::random::<[u8; 32]>();
        if let Ok(secret_key) = SecretKey::from_slice(&bytes) {
            break secret_key;
        }
    };
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let address = transparent::Address::from_pub_key_hash(
        zebra_network_kind(network),
        hash160(&public_key.serialize()),
    );

    Ok(SecretKeyRecord {
        key_id: key_id.to_string(),
        role,
        node_name: node_name.cloned(),
        address: address.to_string(),
        public_key_hex: hex::encode(public_key.serialize()),
        secret_key_hex: hex::encode(secret_key.secret_bytes()),
    })
}

fn hash160(payload: &[u8]) -> [u8; 20] {
    let sha_hash = Sha256::digest(payload);
    let ripe_hash = Ripemd160::digest(sha_hash);
    let mut out = [0u8; 20];
    out.copy_from_slice(&ripe_hash);
    out
}

fn zebra_network_kind(network: NetworkKind) -> ZebraNetworkKind {
    match network {
        NetworkKind::Mainnet => ZebraNetworkKind::Mainnet,
        NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => ZebraNetworkKind::Testnet,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

impl SecretKeyRecord {
    fn public(&self) -> PublicKeyRecord {
        PublicKeyRecord {
            key_id: self.key_id.clone(),
            role: self.role,
            node_name: self.node_name.clone(),
            address: self.address.clone(),
            public_key_hex: self.public_key_hex.clone(),
        }
    }
}

impl Default for WalletInitArgs {
    fn default() -> Self {
        Self {
            directory: None,
            network: None,
            birthday_height: None,
            lanes_per_node: 100,
            lane_value_zats: 30_000,
            fanout_width: 1,
            require_mainnet_confirmation: false,
            force: false,
        }
    }
}

impl Default for PlanArgs {
    fn default() -> Self {
        Self {
            directory: None,
            target_block_bytes: DEFAULT_TARGET_BLOCK_BYTES,
            block_spacing_secs: DEFAULT_BLOCK_SPACING_SECS,
            duration_secs: DEFAULT_DURATION_SECS,
            nodes: "all".to_string(),
            measured_tx_bytes: DEFAULT_MEASURED_TX_BYTES,
            max_mempool_bytes: None,
            safety_margin: DEFAULT_SAFETY_MARGIN,
            allow_underfunded_plan: false,
            json: false,
        }
    }
}
