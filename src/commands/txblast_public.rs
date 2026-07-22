use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use futures::future::join_all;
use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::{Marking, Retention};
use orchard::tree::MerkleHashOrchard;
use ripemd::{Digest as RipemdDigest, Ripemd160};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use shardtree::store::memory::MemoryShardStore;
use zcash_address::ZcashAddress;
use zcash_primitives::merkle_tree::{read_frontier_v0, read_frontier_v1};
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
    BlockRef, LaneRegistry, MIN_NOTE_VALUE, NoteRole, ORCHARD_SPEND_FEE, OrchardChainCursor,
    OrchardNullifierIndex, OrchardTree, OrchardTxblastTracer, PlannedOutput, TrackedNote,
    TreasuryInventory, build_and_send_orchard_fanout_tx, build_and_send_orchard_to_transparent_tx,
    build_and_send_orchard_to_transparent_with_change_tx, build_and_send_shielding_tx,
    derive_orchard_keys, latest_checkpoint_anchor, latest_witness, orchard_fanout_fee,
    orchard_to_transparent_fee, orchard_to_transparent_with_change_fee, scan_block_range,
    shielding_fee,
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
const PREPARE_CONFIRMATIONS: u32 = 3;
const TARGET_HEIGHT_OFFSET_BLOCKS: u32 = 100;
const REMOTE_INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const REMOTE_INSTALL_ATTEMPTS: usize = 3;
const REMOTE_INSTALL_RETRY_BACKOFF: Duration = Duration::from_secs(10);
const REMOTE_START_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_FUNDED_KEY_PATH: &str = "/root/.config/funded_key.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WithdrawalAmount {
    All,
    Zats(u64),
}

#[derive(Clone, Debug)]
pub struct WalletInitArgs {
    pub directory: Option<String>,
    pub network: Option<NetworkKind>,
    pub birthday_height: Option<u32>,
    pub rpc_endpoint: Option<String>,
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
    pub rpc_endpoint: Option<String>,
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
    auto_imported_deposit_count: usize,
    auto_updated_deposit_count: usize,
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
    shield_txids: Vec<String>,
    fanout_txids: Vec<String>,
    prepared_at_unix: u64,
    hot_keys: Vec<PreparedHotKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreparedHotKey {
    node_name: String,
    address: String,
    value_zats: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicTxblastState {
    version: u32,
    network: NetworkKind,
    updated_at_unix: u64,
    confirmed_deposits: Vec<DurableDeposit>,
    control_inventory: DurableInventory,
    hot_inventory: Vec<DurableHotInventory>,
    #[serde(default)]
    shield_txids: Vec<DurableTx>,
    #[serde(default)]
    reservoir_split_txids: Vec<DurableTx>,
    #[serde(default)]
    fanout_txids: Vec<DurableTx>,
    #[serde(default)]
    sweep_txids: Vec<DurableTx>,
    #[serde(default)]
    pending_transactions: Vec<DurableTx>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableDeposit {
    outpoint_id: String,
    txid: String,
    vout: u32,
    value_zats: u64,
    height: u32,
    state: DurableDepositState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableDepositState {
    Confirmed,
    ShieldingSubmitted,
    Shielded,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
struct DurableInventory {
    last_scanned_height: u32,
    last_scanned_hash: Option<String>,
    note_count: usize,
    value_zats: u64,
    notes: Vec<DurableNote>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableHotInventory {
    node_name: String,
    key_id: String,
    last_scanned_height: u32,
    #[serde(default)]
    last_scanned_hash: Option<String>,
    note_count: usize,
    value_zats: u64,
    notes: Vec<DurableNote>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableNote {
    note_id: String,
    origin_txid: String,
    action_index: usize,
    role: NoteRole,
    value_zats: u64,
    confirmation_height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DurableTx {
    txid: String,
    kind: DurableTxKind,
    submitted_at_unix: u64,
    plan_id: Option<String>,
    status: DurableTxStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableTxKind {
    ShieldDeposit,
    ControlReservoirSplit,
    ShieldedFanout,
    WithdrawSweep,
    RecoverySweep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableTxStatus {
    Submitted,
    Confirmed,
}

struct ScannedInventory {
    last_height: u32,
    last_hash: Option<String>,
    checkpoint: Option<BlockRef>,
    registry: LaneRegistry,
    tree: OrchardTree,
}

struct CachedScannedInventory {
    durable: DurableInventory,
    scan: Option<ScannedInventory>,
    cache_hit: bool,
}

struct ShieldedFanoutBatch {
    source_note: TrackedNote,
    recipients: Vec<(orchard::Address, PlannedOutput)>,
}

struct ControlReservoirSplitBatch {
    source_note: TrackedNote,
    outputs: Vec<PlannedOutput>,
}

struct WithdrawalCandidate {
    inventory_index: usize,
    note: TrackedNote,
}

struct HotKeyInstallOutcome {
    name: String,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct PlannedWithdrawal {
    candidate_index: usize,
    output_zats: u64,
    with_change: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedWithdrawalValue {
    candidate_index: usize,
    output_zats: u64,
    with_change: bool,
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
    let birthday_height =
        resolve_wallet_birthday_height(&dir, args.birthday_height, args.rpc_endpoint.as_deref())
            .await?;
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
    println!("  birthday height: {}", wallet.birthday_height);
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
    let mut wallet = load_wallet(&dir)?;
    let explicit_rpc_endpoint = args.rpc_endpoint.is_some();
    let rpc_endpoint = args
        .rpc_endpoint
        .or_else(|| default_rpc_endpoint(&dir).ok());
    let mut rpc_error = None;
    let mut chain_height = None;
    let mut confirmed_utxo_count = None;
    let mut confirmed_utxo_zats = None;
    let mut sync_result = DepositSyncResult::default();

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
                sync_result = sync_imported_deposits_from_utxos(&mut wallet, &confirmed);
                if sync_result.changed() {
                    write_json(&wallet_path(&dir), &wallet)?;
                }
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

    let imported_deposit_zats = imported_deposit_zats(&wallet);
    let latest_plan = wallet.plans.last().cloned();
    let status = DepositStatus {
        network: wallet.network,
        birthday_height: wallet.birthday_height,
        deposit_address: wallet.control.address,
        imported_deposit_count: wallet.deposits.len(),
        imported_deposit_zats,
        auto_imported_deposit_count: sync_result.inserted,
        auto_updated_deposit_count: sync_result.updated,
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
        if status.auto_imported_deposit_count > 0 || status.auto_updated_deposit_count > 0 {
            println!(
                "auto-synced deposits: {} imported, {} updated",
                status.auto_imported_deposit_count, status.auto_updated_deposit_count
            );
        }
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

pub async fn plan(base_directory: &str, args: PlanArgs) -> Result<()> {
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

    let explicit_rpc_endpoint = args.rpc_endpoint.is_some();
    let rpc_endpoint = args
        .rpc_endpoint
        .or_else(|| default_rpc_endpoint(&dir).ok());
    if let Some(endpoint) = rpc_endpoint.as_deref() {
        let client = ZebraRpcClient::new(endpoint);
        match fetch_confirmed_control_utxos(&client, &wallet.control.address, PREPARE_CONFIRMATIONS)
            .await
        {
            Ok(confirmed) => {
                let sync_result = sync_imported_deposits_from_utxos(&mut wallet, &confirmed.utxos);
                if sync_result.changed() {
                    write_json(&wallet_path(&dir), &wallet)?;
                    eprintln!(
                        "auto-synced deposits from {endpoint}: {} imported, {} updated",
                        sync_result.inserted, sync_result.updated
                    );
                }
            }
            Err(error) if !explicit_rpc_endpoint => {
                eprintln!("warning: failed to auto-sync deposits from {endpoint}: {error}");
            }
            Err(error) => return Err(error),
        }
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
    let orchard_runtime = public_orchard_runtime(&config, &wallet, &plan)?;

    if args.dry_run {
        println!(
            "dry run: prepare would shield deposits and fan out at least {} zats into {} shielded hot inventories for plan {} on {}",
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
    let current_hash = if current_height > 0 {
        Some(client.get_block_hash(current_height).await?)
    } else {
        None
    };
    println!(
        "prepare plan {} on {}: height={}, wallet_birthday={}, nodes={}, lanes_per_node={}, lane_value={} zats, fanout_width={}",
        plan.id,
        wallet.network,
        current_height,
        wallet.birthday_height,
        targets.len(),
        plan.lanes_per_node,
        plan.lane_value_zats,
        wallet.defaults.fanout_width,
    );
    println!(
        "transparent deposits require {PREPARE_CONFIRMATIONS} confirmations; submitted shield/fanout txs require at least 1 confirmation before prepare advances"
    );
    let mut state = load_public_state(&dir, wallet.network)?;
    let pending_before = state.pending_transactions.len();
    if pending_before > 0 {
        println!("checking {pending_before} pending public txblast transaction(s)...");
    }
    refresh_pending_transactions(&client, &mut state).await?;
    if pending_before > 0 {
        let pending_after = state.pending_transactions.len();
        println!(
            "pending tx refresh: {} confirmed, {} still pending",
            pending_before.saturating_sub(pending_after),
            pending_after
        );
        for tx in &state.pending_transactions {
            println!("  pending {:?}: {}", tx.kind, tx.txid);
        }
    }

    println!(
        "checking confirmed control-address UTXOs for {}...",
        wallet.control.address
    );
    let visible_confirmed_utxos = confirmed_control_utxos(
        &client,
        &wallet.control.address,
        current_height,
        PREPARE_CONFIRMATIONS,
    )
    .await?;
    let visible_confirmed_value = visible_confirmed_utxos
        .iter()
        .map(|utxo| utxo.satoshis)
        .sum::<u64>();
    println!(
        "confirmed control UTXOs: {} totaling {} zats",
        visible_confirmed_utxos.len(),
        visible_confirmed_value
    );
    remember_confirmed_deposits(&mut state, &visible_confirmed_utxos);
    if !has_pending_kind(&state, DurableTxKind::ShieldDeposit) {
        mark_missing_deposits_shielded(&mut state, &visible_confirmed_utxos);
    }

    let unshielded_utxos = unshielded_deposit_utxos(&state, visible_confirmed_utxos.clone());
    println!(
        "unshielded confirmed deposits ready to shield: {}",
        unshielded_utxos.len()
    );
    if !unshielded_utxos.is_empty() {
        let control_key = funded_key_from_secret(&recovery.control)?;
        let control_orchard_keys = derive_orchard_keys(&secret_bytes(&recovery.control)?)?;
        let fanout_source_value = fanout_source_note_value(
            wallet.defaults.fanout_width,
            plan.lane_value_zats,
            config.orchard_txblast.fanout_source_value_zats,
        );
        let mut remaining_source_outputs = plan
            .lanes_per_node
            .saturating_mul(targets.len())
            .div_ceil(wallet.defaults.fanout_width);
        println!("fetching Orchard anchor for deposit shielding...");
        let anchor = fetch_orchard_anchor(&client).await?;
        let target_height = current_height.saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
        let mut submitted = Vec::new();
        for (idx, utxo) in unshielded_utxos.iter().enumerate() {
            let (outputs, source_outputs) = plan_shielding_reservoir_outputs(
                utxo.satoshis,
                remaining_source_outputs,
                fanout_source_value,
            )
            .with_context(|| {
                format!(
                    "deposit {}:{} value {} is too small to shield",
                    utxo.txid, utxo.output_index, utxo.satoshis
                )
            })?;
            remaining_source_outputs = remaining_source_outputs.saturating_sub(source_outputs);
            let confirmations = current_height.saturating_sub(utxo.height).saturating_add(1);
            println!(
                "building shielding tx {}/{} for {}:{} (value={} zats, outputs={}, height={}, confirmations={})",
                idx + 1,
                unshielded_utxos.len(),
                utxo.txid,
                utxo.output_index,
                utxo.satoshis,
                outputs.len(),
                utxo.height,
                confirmations
            );
            let tx = build_and_send_shielding_tx(
                TxblastNetworkParams::from_network_kind(wallet.network),
                &client,
                &control_key,
                &control_orchard_keys,
                &utxo.txid,
                utxo.output_index,
                &utxo.script,
                utxo.satoshis,
                &outputs,
                anchor,
                current_height,
                target_height,
                crate::txblast::orchard::PendingTxKind::WarmupShielding,
            )
            .await?;
            record_submitted_tx(
                &mut state,
                tx.txid.clone(),
                DurableTxKind::ShieldDeposit,
                Some(plan.id.clone()),
            );
            submitted.push(tx.txid);
        }
        mark_deposits_shielding_submitted(&mut state, &unshielded_utxos);
        write_public_state(&dir, &mut state)?;
        println!(
            "submitted {} deposit shielding transaction(s)",
            submitted.len()
        );
        for txid in submitted {
            println!("  shield deposit txid: {txid}");
        }
        println!(
            "rerun `kresko txblast prepare --plan {}` after confirmation to fan out shielded funds",
            plan.id
        );
        return Ok(());
    }

    println!(
        "scanning Orchard inventories from height {} to {} (cached inventories at this tip are reused)...",
        wallet.birthday_height.max(1),
        current_height
    );
    let control_cached = cached_control_inventory(&state, current_height, current_hash.as_deref());
    let birthday_height = wallet.birthday_height;
    let hot_cached = hot_inventory_cache(
        &state,
        &targets,
        &hot_keys,
        current_height,
        current_hash.as_deref(),
    );
    let control_future = scan_or_cached_inventory(
        rpc_endpoint.clone(),
        recovery.control.clone(),
        birthday_height,
        orchard_runtime.clone(),
        current_height,
        current_hash.clone(),
        control_cached,
    );
    let hot_futures =
        targets
            .iter()
            .zip(hot_keys.iter())
            .zip(hot_cached)
            .map(|((target, hot_key), cached)| {
                let rpc_endpoint = rpc_endpoint.clone();
                let record = (*hot_key).clone();
                let orchard_runtime = orchard_runtime.clone();
                let current_hash = current_hash.clone();
                let node_name = target.name.clone();
                async move {
                    let scan = scan_or_cached_inventory(
                        rpc_endpoint,
                        record,
                        birthday_height,
                        orchard_runtime,
                        current_height,
                        current_hash,
                        cached,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((node_name, scan))
                }
            });
    let (control_scan_result, hot_scan_results) =
        futures::join!(control_future, async { join_all(hot_futures).await });
    let mut control_scan_result = control_scan_result?;
    let hot_scan_results = hot_scan_results.into_iter().collect::<Result<Vec<_>>>()?;
    let control_snapshot = control_scan_result.durable.snapshot();
    println!(
        "control Orchard scan complete: notes={}, value={} zats, reservoirs={} ({} zats), lanes={} ({} zats), scanned_height={}{}",
        control_scan_result.durable.note_count,
        control_scan_result.durable.value_zats,
        control_snapshot.reservoirs,
        control_snapshot.reservoir_total_value,
        control_snapshot.ready_lanes,
        control_snapshot.lane_total_value,
        control_scan_result.durable.last_scanned_height,
        if control_scan_result.cache_hit {
            " (cached)"
        } else {
            ""
        }
    );
    state.control_inventory = control_scan_result.durable.clone();

    let mut prepared_hot_keys = Vec::with_capacity(targets.len());
    let mut hot_inventory = Vec::with_capacity(targets.len());
    let mut hot_lane_requirements = Vec::with_capacity(targets.len());
    let mut all_hot_ready = true;
    for (target, hot_key, (_node_name, scan_result)) in targets
        .iter()
        .zip(hot_keys.iter())
        .zip(hot_scan_results.into_iter())
        .map(|((target, hot_key), result)| (target, hot_key, result))
    {
        let durable = scan_result.durable;
        let snapshot = durable.snapshot();
        let needed_lanes =
            hot_lane_top_up_count(snapshot.ready_lanes, snapshot.lane_total_value, &plan);
        println!(
            "  {}: notes={}, value={} zats, ready_lanes={}/{}, lane_total_value={} zats, top_up_lanes_needed={}{}",
            target.name,
            durable.note_count,
            durable.value_zats,
            snapshot.ready_lanes,
            plan.lanes_per_node,
            snapshot.lane_total_value,
            needed_lanes,
            if scan_result.cache_hit {
                " (cached)"
            } else {
                ""
            }
        );
        if needed_lanes > 0 {
            all_hot_ready = false;
        }
        hot_lane_requirements.push(needed_lanes);
        prepared_hot_keys.push(PreparedHotKey {
            node_name: target.name.clone(),
            address: hot_key.address.clone(),
            value_zats: durable.value_zats,
        });
        hot_inventory.push(DurableHotInventory {
            node_name: target.name.clone(),
            key_id: hot_key.key_id.clone(),
            last_scanned_height: durable.last_scanned_height,
            last_scanned_hash: durable.last_scanned_hash,
            note_count: durable.note_count,
            value_zats: durable.value_zats,
            notes: durable.notes,
        });
    }
    state.hot_inventory = hot_inventory;
    write_public_state(&dir, &mut state)?;

    if all_hot_ready {
        println!("all hot inventories are ready; installing hot keys on remote nodes...");
        let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
        let key = shellexpand(&key);
        install_hot_keys(&targets, &hot_keys, &key).await?;

        let fanout_txids = state
            .fanout_txids
            .iter()
            .filter(|tx| tx.plan_id.as_deref() == Some(plan.id.as_str()))
            .map(|tx| tx.txid.clone())
            .collect::<Vec<_>>();
        let record = PublicPrepareRecord {
            version: STATE_VERSION,
            plan_id: plan.id.clone(),
            network: wallet.network,
            shield_txids: state
                .shield_txids
                .iter()
                .filter(|tx| tx.plan_id.as_deref() == Some(plan.id.as_str()))
                .map(|tx| tx.txid.clone())
                .collect(),
            fanout_txids,
            prepared_at_unix: now_unix(),
            hot_keys: prepared_hot_keys,
        };
        write_json(&prepare_path(&dir, &plan.id), &record)?;
        write_json(&latest_prepare_path(&dir), &record)?;
        write_public_state(&dir, &mut state)?;

        println!("prepared public txblast plan {}", plan.id);
        println!("  shielded hot keys: {}", targets.len());
        println!(
            "  total hot inventory: {} zats",
            record
                .hot_keys
                .iter()
                .map(|key| key.value_zats)
                .sum::<u64>()
        );
        println!("  remote funded key path: {REMOTE_FUNDED_KEY_PATH}");
        return Ok(());
    }

    if !state.pending_transactions.is_empty() {
        write_public_state(&dir, &mut state)?;
        println!(
            "waiting for {} pending public txblast transaction(s); rerun prepare after confirmation",
            state.pending_transactions.len()
        );
        for tx in &state.pending_transactions {
            println!("  pending {:?}: {}", tx.kind, tx.txid);
        }
        return Ok(());
    }

    let control_scan = match control_scan_result.scan.take() {
        Some(scan) => scan,
        None => {
            println!(
                "cached control inventory needs witnesses for spending; rebuilding control scan from height {} to {}...",
                wallet.birthday_height.max(1),
                current_height
            );
            scan_orchard_inventory_to_tip(
                &client,
                &recovery.control,
                wallet.birthday_height,
                &orchard_runtime,
                current_height,
                current_hash.clone(),
            )
            .await?
        }
    };
    let control_notes = control_scan.registry.spendable_notes();
    let required_hot_lanes = hot_lane_requirements.iter().sum::<usize>();
    let control_orchard_keys = derive_orchard_keys(&secret_bytes(&recovery.control)?)?;
    let reservoir_split_batches = plan_control_reservoir_split_batches(
        &control_notes,
        required_hot_lanes,
        wallet.defaults.fanout_width,
        plan.lane_value_zats,
        config.orchard_txblast.fanout_source_value_zats,
    )?;
    if !reservoir_split_batches.is_empty() {
        println!(
            "planning control reservoir split: batches={}, required_hot_lanes={}, fanout_width={}",
            reservoir_split_batches.len(),
            required_hot_lanes,
            wallet.defaults.fanout_width
        );
        let checkpoint = control_scan
            .checkpoint
            .as_ref()
            .context("control Orchard inventory has no checkpoint for reservoir split witnesses")?;
        println!("fetching witness anchor for control reservoir split...");
        let anchor = latest_checkpoint_anchor(&control_scan.tree, checkpoint)?;
        let target_height = current_height.saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
        let control_address = control_orchard_keys.address();
        let mut submitted = Vec::new();
        let split_batch_count = reservoir_split_batches.len();
        for (idx, batch) in reservoir_split_batches.into_iter().enumerate() {
            println!(
                "building control reservoir split tx {}/{} with {} reservoir output(s)",
                idx + 1,
                split_batch_count,
                batch.outputs.len()
            );
            let witness = latest_witness(&control_scan.tree, &batch.source_note, checkpoint)?;
            let recipients = batch
                .outputs
                .into_iter()
                .map(|output| (control_address, output))
                .collect::<Vec<_>>();
            let txid = build_and_send_orchard_fanout_tx(
                TxblastNetworkParams::from_network_kind(wallet.network),
                &client,
                &control_orchard_keys,
                &batch.source_note,
                witness,
                anchor,
                target_height,
                &recipients,
            )
            .await?;
            record_submitted_tx(
                &mut state,
                txid.clone(),
                DurableTxKind::ControlReservoirSplit,
                Some(plan.id.clone()),
            );
            submitted.push(txid);
        }
        write_public_state(&dir, &mut state)?;
        println!(
            "submitted {} control reservoir split transaction(s)",
            submitted.len()
        );
        for txid in submitted {
            println!("  control reservoir split txid: {txid}");
        }
        println!(
            "rerun `kresko txblast prepare --plan {}` after reservoir split confirmation to fan out shielded funds",
            plan.id
        );
        return Ok(());
    }

    println!(
        "planning shielded fanout: control_notes={}, control_value={} zats, required_hot_lanes={}",
        control_notes.len(),
        control_notes.iter().map(TrackedNote::value).sum::<u64>(),
        required_hot_lanes
    );
    let fanout_batches = plan_shielded_fanout_batches(
        &control_notes,
        &hot_keys,
        &hot_lane_requirements,
        &plan,
        wallet.defaults.fanout_width,
    )?;
    println!(
        "planned {} shielded fanout transaction(s)",
        fanout_batches.len()
    );
    let checkpoint = control_scan
        .checkpoint
        .as_ref()
        .context("control Orchard inventory has no checkpoint for fanout witnesses")?;
    println!("fetching witness anchor for shielded fanout...");
    let anchor = latest_checkpoint_anchor(&control_scan.tree, checkpoint)?;
    let target_height = current_height.saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
    let mut submitted = Vec::new();
    let fanout_batch_count = fanout_batches.len();
    for (idx, batch) in fanout_batches.into_iter().enumerate() {
        println!(
            "building fanout tx {}/{} with {} recipient lane output(s)",
            idx + 1,
            fanout_batch_count,
            batch.recipients.len()
        );
        let witness = latest_witness(&control_scan.tree, &batch.source_note, checkpoint)?;
        let txid = build_and_send_orchard_fanout_tx(
            TxblastNetworkParams::from_network_kind(wallet.network),
            &client,
            &control_orchard_keys,
            &batch.source_note,
            witness,
            anchor,
            target_height,
            &batch.recipients,
        )
        .await?;
        record_submitted_tx(
            &mut state,
            txid.clone(),
            DurableTxKind::ShieldedFanout,
            Some(plan.id.clone()),
        );
        submitted.push(txid);
    }
    write_public_state(&dir, &mut state)?;
    println!(
        "submitted {} shielded fanout transaction(s)",
        submitted.len()
    );
    for txid in submitted {
        println!("  shielded fanout txid: {txid}");
    }
    println!(
        "rerun `kresko txblast prepare --plan {}` after fanout confirmation to finalize remote hot-key installation",
        plan.id
    );
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
    --funded-key-path {funded_key_path} \
    --wallet-birthday-height {wallet_birthday_height}
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
        wallet_birthday_height = wallet.birthday_height,
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

pub async fn withdraw(base_directory: &str, args: WithdrawArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast withdraw")?;
    let wallet = load_wallet(&dir)?;
    let recovery = load_recovery(&dir)?;
    ensure_wallet_matches_config(&wallet, &recovery, &config)?;
    validate_transparent_address_for_network(&args.to, wallet.network)?;
    if wallet.network == NetworkKind::Mainnet && !args.mainnet_i_understand_finality {
        anyhow::bail!("refusing mainnet withdrawal without --mainnet-i-understand-finality");
    }
    let plan = wallet.plans.last().context("no txblast plan exists")?;
    let orchard_runtime = public_orchard_runtime(&config, &wallet, plan)?;
    let rpc_endpoint = public_rpc_endpoint(&config)?;
    let client = ZebraRpcClient::new(&rpc_endpoint);
    let mut state = load_public_state(&dir, wallet.network)?;
    refresh_pending_transactions(&client, &mut state).await?;

    let mut inventories = Vec::new();
    inventories.push((
        &recovery.control,
        scan_orchard_inventory(
            &client,
            &recovery.control,
            wallet.birthday_height,
            &orchard_runtime,
        )
        .await?,
    ));
    for hot_key in &recovery.hot_keys {
        inventories.push((
            hot_key,
            scan_orchard_inventory(&client, hot_key, wallet.birthday_height, &orchard_runtime)
                .await?,
        ));
    }
    let amount = parse_withdraw_amount(&args.amount)?;
    let withdrawal_candidates = withdrawal_candidates(&inventories);
    let withdrawal_plan = plan_withdrawal_sweeps(&withdrawal_candidates, amount)?;
    let planned_output_zats: u64 = withdrawal_plan.iter().map(|entry| entry.output_zats).sum();
    if args.dry_run {
        println!(
            "dry run: would withdraw {} zats to {} on {} from {} zats shielded inventory",
            planned_output_zats,
            args.to,
            wallet.network,
            inventories
                .iter()
                .map(|(_, scan)| scan.registry.spendable_value())
                .sum::<u64>()
        );
        return Ok(());
    }
    if !state.pending_transactions.is_empty() {
        anyhow::bail!(
            "{} public txblast transaction(s) are pending; wait for confirmation before withdrawing",
            state.pending_transactions.len()
        );
    }

    let recipient = transparent_address_for_encoded(&args.to)?;
    let mut submitted = Vec::new();
    for planned in withdrawal_plan {
        let candidate = withdrawal_candidates
            .get(planned.candidate_index)
            .context("withdrawal planner returned an invalid candidate index")?;
        let (record, scan) = inventories
            .get(candidate.inventory_index)
            .context("withdrawal planner returned an invalid inventory index")?;
        let Some(checkpoint) = scan.checkpoint.as_ref() else {
            anyhow::bail!("planned withdrawal note has no Orchard checkpoint");
        };
        let anchor = latest_checkpoint_anchor(&scan.tree, checkpoint)?;
        let keys = derive_orchard_keys(&secret_bytes(record)?)?;
        let target_height = client
            .get_block_count()
            .await?
            .saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
        let witness = latest_witness(&scan.tree, &candidate.note, checkpoint)?;
        let txid = if planned.with_change {
            build_and_send_orchard_to_transparent_with_change_tx(
                TxblastNetworkParams::from_network_kind(wallet.network),
                &client,
                &keys,
                &candidate.note,
                witness,
                anchor,
                target_height,
                &recipient,
                planned.output_zats,
                Some(candidate.note.role),
            )
            .await?
        } else {
            build_and_send_orchard_to_transparent_tx(
                TxblastNetworkParams::from_network_kind(wallet.network),
                &client,
                &keys,
                &candidate.note,
                witness,
                anchor,
                target_height,
                &[(recipient.clone(), planned.output_zats)],
            )
            .await?
        };
        record_submitted_tx(&mut state, txid.clone(), DurableTxKind::WithdrawSweep, None);
        submitted.push(txid);
    }
    write_public_state(&dir, &mut state)?;
    println!(
        "submitted {} withdrawal sweep transaction(s)",
        submitted.len()
    );
    for txid in submitted {
        println!("  withdrawal txid: {txid}");
    }
    Ok(())
}

pub async fn recover_inventory(base_directory: &str, args: RecoverInventoryArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast recover inventory")?;
    let wallet = load_wallet(&dir)?;
    let recovery = load_recovery(&dir)?;
    ensure_wallet_matches_config(&wallet, &recovery, &config)?;
    let from_height = args.from_height.unwrap_or(wallet.birthday_height);
    let orchard_runtime = public_orchard_runtime_for_recovery(&config, &wallet)?;
    let rpc_endpoint = public_rpc_endpoint(&config)?;
    let client = ZebraRpcClient::new(&rpc_endpoint);

    let control_scan =
        scan_orchard_inventory(&client, &recovery.control, from_height, &orchard_runtime).await?;
    let mut hot = Vec::new();
    for hot_key in &recovery.hot_keys {
        let scan = scan_orchard_inventory(&client, hot_key, from_height, &orchard_runtime).await?;
        hot.push(serde_json::json!({
            "key_id": hot_key.key_id,
            "node_name": hot_key.node_name,
            "note_count": scan.registry.spendable_note_count(),
            "value_zats": scan.registry.spendable_value(),
            "last_scanned_height": scan.last_height,
        }));
    }
    let hot_value: u64 = hot
        .iter()
        .filter_map(|entry| entry.get("value_zats").and_then(|v| v.as_u64()))
        .sum();
    let hot_note_count: u64 = hot
        .iter()
        .filter_map(|entry| entry.get("note_count").and_then(|v| v.as_u64()))
        .sum();
    let report = serde_json::json!({
        "network": wallet.network,
        "from_height": from_height,
        "control_key": recovery.control.address,
        "rpc_endpoint": rpc_endpoint,
        "control": {
            "note_count": control_scan.registry.spendable_note_count(),
            "value_zats": control_scan.registry.spendable_value(),
            "last_scanned_height": control_scan.last_height,
        },
        "hot": hot,
        "total_note_count": control_scan.registry.spendable_note_count() as u64 + hot_note_count,
        "total_value_zats": control_scan.registry.spendable_value() + hot_value,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("network: {}", wallet.network);
        println!("from height: {from_height}");
        println!("control key: {}", recovery.control.address);
        println!("hot keys: {}", recovery.hot_keys.len());
        println!(
            "recoverable inventory: {} notes, {} zats",
            report["total_note_count"], report["total_value_zats"]
        );
    }
    Ok(())
}

pub async fn recover_sweep(base_directory: &str, args: RecoverSweepArgs) -> Result<()> {
    let dir = resolve_directory(base_directory, args.directory.as_deref());
    let config = Config::load(&dir)?;
    ensure_public_network(config.network_kind, "txblast recover sweep")?;
    let wallet = load_wallet(&dir)?;
    let recovery = load_recovery(&dir)?;
    ensure_wallet_matches_config(&wallet, &recovery, &config)?;
    validate_transparent_address_for_network(&args.to, wallet.network)?;
    if wallet.network == NetworkKind::Mainnet && !args.mainnet_i_understand_recovery {
        anyhow::bail!("refusing mainnet recovery sweep without --mainnet-i-understand-recovery");
    }
    let from_height = args.from_height.unwrap_or(wallet.birthday_height);
    let orchard_runtime = public_orchard_runtime_for_recovery(&config, &wallet)?;
    let rpc_endpoint = public_rpc_endpoint(&config)?;
    let client = ZebraRpcClient::new(&rpc_endpoint);
    let mut state = load_public_state(&dir, wallet.network)?;
    refresh_pending_transactions(&client, &mut state).await?;
    if !state.pending_transactions.is_empty() && !args.dry_run {
        anyhow::bail!(
            "{} public txblast transaction(s) are pending; wait for confirmation before recovery sweep",
            state.pending_transactions.len()
        );
    }

    let mut inventories = Vec::new();
    inventories.push((
        &recovery.control,
        scan_orchard_inventory(&client, &recovery.control, from_height, &orchard_runtime).await?,
    ));
    for hot_key in &recovery.hot_keys {
        inventories.push((
            hot_key,
            scan_orchard_inventory(&client, hot_key, from_height, &orchard_runtime).await?,
        ));
    }
    let total_spendable: u64 = inventories
        .iter()
        .map(|(_, scan)| scan.registry.spendable_value())
        .sum();
    if args.dry_run {
        println!(
            "dry run: would scan from height {} and sweep up to {} recoverable zats to {}",
            from_height, total_spendable, args.to
        );
        return Ok(());
    }
    if total_spendable == 0 {
        anyhow::bail!("no recoverable shielded txblast inventory found");
    }
    let recipient = transparent_address_for_encoded(&args.to)?;
    let mut submitted = Vec::new();
    for (record, scan) in inventories {
        let Some(checkpoint) = scan.checkpoint.as_ref() else {
            continue;
        };
        let anchor = latest_checkpoint_anchor(&scan.tree, checkpoint)?;
        let keys = derive_orchard_keys(&secret_bytes(record)?)?;
        for note in scan.registry.spendable_notes() {
            let output = note.value().saturating_sub(orchard_to_transparent_fee(1));
            if output == 0 {
                continue;
            }
            let witness = latest_witness(&scan.tree, &note, checkpoint)?;
            let target_height = client
                .get_block_count()
                .await?
                .saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
            let txid = build_and_send_orchard_to_transparent_tx(
                TxblastNetworkParams::from_network_kind(wallet.network),
                &client,
                &keys,
                &note,
                witness,
                anchor,
                target_height,
                &[(recipient.clone(), output)],
            )
            .await?;
            record_submitted_tx(&mut state, txid.clone(), DurableTxKind::RecoverySweep, None);
            submitted.push(txid);
        }
    }
    write_public_state(&dir, &mut state)?;
    println!(
        "submitted {} recovery sweep transaction(s)",
        submitted.len()
    );
    for txid in submitted {
        println!("  recovery sweep txid: {txid}");
    }
    Ok(())
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

async fn resolve_wallet_birthday_height(
    dir: &Path,
    explicit_height: Option<u32>,
    explicit_rpc_endpoint: Option<&str>,
) -> Result<u32> {
    if let Some(height) = explicit_height {
        return Ok(height);
    }

    let endpoint = explicit_rpc_endpoint
        .map(ToOwned::to_owned)
        .or_else(|| default_rpc_endpoint(dir).ok());
    let Some(endpoint) = endpoint else {
        eprintln!(
            "warning: no RPC endpoint available for wallet birthday height; defaulting to height 0"
        );
        return Ok(0);
    };

    match ZebraRpcClient::new(&endpoint).get_block_count().await {
        Ok(height) => Ok(height),
        Err(error) if explicit_rpc_endpoint.is_none() => {
            eprintln!(
                "warning: failed to query wallet birthday height from {endpoint}: {error}; defaulting to height 0"
            );
            Ok(0)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to query wallet birthday height from {endpoint}")),
    }
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
    Ok(utxos)
}

struct ConfirmedControlUtxos {
    utxos: Vec<crate::txblast::rpc::AddressUtxo>,
}

async fn fetch_confirmed_control_utxos(
    client: &ZebraRpcClient,
    address: &str,
    confirmations: u32,
) -> Result<ConfirmedControlUtxos> {
    let current_height = client.get_block_count().await?;
    let utxos = confirmed_control_utxos(client, address, current_height, confirmations).await?;
    Ok(ConfirmedControlUtxos { utxos })
}

#[derive(Default)]
struct DepositSyncResult {
    inserted: usize,
    updated: usize,
}

impl DepositSyncResult {
    fn changed(&self) -> bool {
        self.inserted > 0 || self.updated > 0
    }
}

fn sync_imported_deposits_from_utxos(
    wallet: &mut TxblastWallet,
    utxos: &[crate::txblast::rpc::AddressUtxo],
) -> DepositSyncResult {
    let mut result = DepositSyncResult::default();
    let address = wallet.control.address.clone();
    let imported_at_unix = now_unix();

    for utxo in utxos {
        let existing = wallet
            .deposits
            .iter()
            .position(|deposit| {
                deposit.txid == utxo.txid && deposit.vout == Some(utxo.output_index)
            })
            .or_else(|| {
                wallet
                    .deposits
                    .iter()
                    .position(|deposit| deposit.txid == utxo.txid && deposit.vout.is_none())
            });

        if let Some(index) = existing {
            let deposit = &mut wallet.deposits[index];
            let mut updated = false;
            if deposit.vout != Some(utxo.output_index) {
                deposit.vout = Some(utxo.output_index);
                updated = true;
            }
            if deposit.amount_zats != Some(utxo.satoshis) {
                deposit.amount_zats = Some(utxo.satoshis);
                updated = true;
            }
            if deposit.address.as_deref() != Some(address.as_str()) {
                deposit.address = Some(address.clone());
                updated = true;
            }
            if updated {
                result.updated += 1;
            }
        } else {
            wallet.deposits.push(ImportedDeposit {
                txid: utxo.txid.clone(),
                vout: Some(utxo.output_index),
                amount_zats: Some(utxo.satoshis),
                address: Some(address.clone()),
                imported_at_unix,
                state: DepositState::Imported,
            });
            result.inserted += 1;
        }
    }

    result
}

fn remember_confirmed_deposits(
    state: &mut PublicTxblastState,
    utxos: &[crate::txblast::rpc::AddressUtxo],
) {
    let mut known = state
        .confirmed_deposits
        .iter()
        .map(|deposit| deposit.outpoint_id.clone())
        .collect::<HashSet<_>>();
    for utxo in utxos {
        let outpoint_id = format!("{}:{}", utxo.txid, utxo.output_index);
        if known.insert(outpoint_id.clone()) {
            state.confirmed_deposits.push(DurableDeposit {
                outpoint_id,
                txid: utxo.txid.clone(),
                vout: utxo.output_index,
                value_zats: utxo.satoshis,
                height: utxo.height,
                state: DurableDepositState::Confirmed,
            });
        }
    }
}

fn unshielded_deposit_utxos(
    state: &PublicTxblastState,
    utxos: Vec<crate::txblast::rpc::AddressUtxo>,
) -> Vec<crate::txblast::rpc::AddressUtxo> {
    let unavailable = state
        .confirmed_deposits
        .iter()
        .filter(|deposit| deposit.state != DurableDepositState::Confirmed)
        .map(|deposit| deposit.outpoint_id.clone())
        .collect::<HashSet<_>>();
    utxos
        .into_iter()
        .filter(|utxo| !unavailable.contains(&format!("{}:{}", utxo.txid, utxo.output_index)))
        .collect()
}

fn record_submitted_tx(
    state: &mut PublicTxblastState,
    txid: String,
    kind: DurableTxKind,
    plan_id: Option<String>,
) {
    if state.pending_transactions.iter().any(|tx| tx.txid == txid)
        || state.shield_txids.iter().any(|tx| tx.txid == txid)
        || state.reservoir_split_txids.iter().any(|tx| tx.txid == txid)
        || state.fanout_txids.iter().any(|tx| tx.txid == txid)
        || state.sweep_txids.iter().any(|tx| tx.txid == txid)
    {
        return;
    }

    state.pending_transactions.push(DurableTx {
        txid,
        kind,
        submitted_at_unix: now_unix(),
        plan_id,
        status: DurableTxStatus::Submitted,
    });
}

fn has_pending_kind(state: &PublicTxblastState, kind: DurableTxKind) -> bool {
    state.pending_transactions.iter().any(|tx| tx.kind == kind)
}

async fn refresh_pending_transactions(
    client: &ZebraRpcClient,
    state: &mut PublicTxblastState,
) -> Result<()> {
    let mut remaining = Vec::new();
    for mut tx in state.pending_transactions.drain(..) {
        let confirmed = client
            .try_get_raw_transaction_verbose(&tx.txid)
            .await?
            .and_then(|verbose| verbose.confirmations)
            .unwrap_or(0)
            > 0;
        if confirmed {
            tx.status = DurableTxStatus::Confirmed;
            match tx.kind {
                DurableTxKind::ControlReservoirSplit => state.reservoir_split_txids.push(tx),
                DurableTxKind::ShieldedFanout => state.fanout_txids.push(tx),
                DurableTxKind::WithdrawSweep | DurableTxKind::RecoverySweep => {
                    state.sweep_txids.push(tx)
                }
                DurableTxKind::ShieldDeposit => state.shield_txids.push(tx),
            }
        } else {
            remaining.push(tx);
        }
    }
    state.pending_transactions = remaining;
    Ok(())
}

fn mark_deposits_shielding_submitted(
    state: &mut PublicTxblastState,
    inputs: &[crate::txblast::rpc::AddressUtxo],
) {
    let input_ids = inputs
        .iter()
        .map(|utxo| format!("{}:{}", utxo.txid, utxo.output_index))
        .collect::<HashSet<_>>();
    for deposit in &mut state.confirmed_deposits {
        if input_ids.contains(&deposit.outpoint_id) {
            deposit.state = DurableDepositState::ShieldingSubmitted;
        }
    }
}

fn mark_missing_deposits_shielded(
    state: &mut PublicTxblastState,
    visible_utxos: &[crate::txblast::rpc::AddressUtxo],
) {
    let visible_ids = visible_utxos
        .iter()
        .map(|utxo| format!("{}:{}", utxo.txid, utxo.output_index))
        .collect::<HashSet<_>>();
    for deposit in &mut state.confirmed_deposits {
        if deposit.state == DurableDepositState::ShieldingSubmitted
            && !visible_ids.contains(&deposit.outpoint_id)
        {
            deposit.state = DurableDepositState::Shielded;
        }
    }
}

impl DurableInventory {
    fn snapshot(&self) -> crate::txblast::orchard::state::RegistrySnapshot {
        let mut snapshot = crate::txblast::orchard::state::RegistrySnapshot::default();
        for note in &self.notes {
            match note.role {
                NoteRole::Lane => {
                    snapshot.ready_lanes += 1;
                    snapshot.lane_total_value =
                        snapshot.lane_total_value.saturating_add(note.value_zats);
                }
                NoteRole::Reservoir => {
                    snapshot.reservoirs += 1;
                    snapshot.reservoir_total_value = snapshot
                        .reservoir_total_value
                        .saturating_add(note.value_zats);
                }
            }
        }
        snapshot
    }
}

fn cached_control_inventory(
    state: &PublicTxblastState,
    current_height: u32,
    current_hash: Option<&str>,
) -> Option<DurableInventory> {
    inventory_at_tip(&state.control_inventory, current_height, current_hash)
        .then(|| state.control_inventory.clone())
}

fn hot_inventory_cache(
    state: &PublicTxblastState,
    targets: &[&Instance],
    hot_keys: &[&SecretKeyRecord],
    current_height: u32,
    current_hash: Option<&str>,
) -> Vec<Option<DurableInventory>> {
    targets
        .iter()
        .zip(hot_keys.iter())
        .map(|(target, hot_key)| {
            state
                .hot_inventory
                .iter()
                .find(|inventory| {
                    inventory.node_name == target.name
                        && inventory.key_id == hot_key.key_id
                        && inventory.last_scanned_height == current_height
                        && inventory.last_scanned_hash.as_deref() == current_hash
                })
                .map(|inventory| DurableInventory {
                    last_scanned_height: inventory.last_scanned_height,
                    last_scanned_hash: inventory.last_scanned_hash.clone(),
                    note_count: inventory.note_count,
                    value_zats: inventory.value_zats,
                    notes: inventory.notes.clone(),
                })
        })
        .collect()
}

fn inventory_at_tip(
    inventory: &DurableInventory,
    current_height: u32,
    current_hash: Option<&str>,
) -> bool {
    inventory.last_scanned_height == current_height
        && inventory.last_scanned_hash.as_deref() == current_hash
}

async fn scan_or_cached_inventory(
    rpc_endpoint: String,
    record: SecretKeyRecord,
    from_height: u32,
    orchard_cfg: OrchardBlastRuntimeConfig,
    current_height: u32,
    current_hash: Option<String>,
    cached: Option<DurableInventory>,
) -> Result<CachedScannedInventory> {
    if let Some(durable) = cached {
        return Ok(CachedScannedInventory {
            durable,
            scan: None,
            cache_hit: true,
        });
    }

    let client = ZebraRpcClient::new(&rpc_endpoint);
    let scan = scan_orchard_inventory_to_tip(
        &client,
        &record,
        from_height,
        &orchard_cfg,
        current_height,
        current_hash,
    )
    .await?;
    let durable = durable_inventory(&scan);
    Ok(CachedScannedInventory {
        durable,
        scan: Some(scan),
        cache_hit: false,
    })
}

async fn scan_orchard_inventory(
    client: &ZebraRpcClient,
    record: &SecretKeyRecord,
    from_height: u32,
    orchard_cfg: &OrchardBlastRuntimeConfig,
) -> Result<ScannedInventory> {
    let best_height = client.get_block_count().await?;
    let best_hash = if best_height > 0 {
        Some(client.get_block_hash(best_height).await?)
    } else {
        None
    };
    scan_orchard_inventory_to_tip(
        client,
        record,
        from_height,
        orchard_cfg,
        best_height,
        best_hash,
    )
    .await
}

async fn scan_orchard_inventory_to_tip(
    client: &ZebraRpcClient,
    record: &SecretKeyRecord,
    from_height: u32,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    best_height: u32,
    best_hash: Option<String>,
) -> Result<ScannedInventory> {
    let secret_bytes = secret_bytes(record)?;
    let keys = derive_orchard_keys(&secret_bytes)?;
    let mut tree: OrchardTree = OrchardTree::new(MemoryShardStore::empty(), 100);
    let mut next_position = 0u64;
    let mut nullifier_index = OrchardNullifierIndex::default();
    let mut cursor = OrchardChainCursor::default();
    let mut registry = LaneRegistry::default();
    let mut treasury = TreasuryInventory::default();
    let mut pending_txs = HashMap::new();
    let tracer = OrchardTxblastTracer::from_config(
        &crate::txblast::TxblastTraceConfig {
            enabled: false,
            directory: None,
        },
        &record.key_id,
    );

    let start_height = from_height.max(1);
    if start_height > 1 {
        let frontier_height = start_height - 1;
        seed_orchard_tree_from_treestate(&mut tree, client, frontier_height).await?;
        next_position = tree
            .frontier()
            .map_err(|e| anyhow::anyhow!("seeded Orchard frontier read failed: {e:?}"))?
            .tree_size();
    }
    if best_height >= start_height {
        scan_block_range(
            client,
            &keys,
            &mut tree,
            &mut next_position,
            &mut nullifier_index,
            start_height,
            best_height,
            &mut pending_txs,
            &mut registry,
            &mut treasury,
            &mut cursor,
            &tracer,
            orchard_cfg,
            crate::txblast::orchard::RuntimePhase::Recovering,
            0.0,
        )
        .await?;
    }

    Ok(ScannedInventory {
        last_height: best_height,
        last_hash: best_hash,
        checkpoint: cursor.latest_checkpoint().cloned(),
        registry,
        tree,
    })
}

async fn seed_orchard_tree_from_treestate(
    tree: &mut OrchardTree,
    client: &ZebraRpcClient,
    height: u32,
) -> Result<()> {
    let treestate = client.z_get_treestate(height).await?;
    let final_state_hex = treestate
        .pointer("/orchard/commitments/finalState")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if final_state_hex.is_empty() {
        return Ok(());
    }
    let final_root_hex = treestate
        .pointer("/orchard/commitments/finalRoot")
        .and_then(|v| v.as_str());

    let final_state = hex::decode(final_state_hex)
        .with_context(|| format!("orchard finalState at height {height} is not valid hex"))?;
    let frontier = parse_orchard_treestate_frontier(&final_state, final_root_hex, height)?;
    tree.insert_frontier(
        frontier,
        Retention::Checkpoint {
            id: height,
            marking: Marking::None,
        },
    )
    .map_err(|e| anyhow::anyhow!("failed to seed Orchard tree at height {height}: {e:?}"))?;
    Ok(())
}

fn parse_orchard_treestate_frontier(
    final_state: &[u8],
    final_root_hex: Option<&str>,
    height: u32,
) -> Result<Frontier<MerkleHashOrchard, 32>> {
    let expected_root = final_root_hex
        .filter(|root| !root.is_empty())
        .map(|root| {
            hex::decode(root)
                .with_context(|| format!("orchard finalRoot at height {height} is not valid hex"))?
                .try_into()
                .map_err(|_| {
                    anyhow::anyhow!("orchard finalRoot at height {height} is not 32 bytes")
                })
        })
        .transpose()?;

    let legacy = read_frontier_v0(final_state);
    if let Ok(frontier) = legacy.as_ref() {
        if frontier_matches_root(frontier, expected_root.as_ref()) {
            return Ok(frontier.clone());
        }
    }

    let frontier_v1 = read_frontier_v1(final_state);
    if let Ok(frontier) = frontier_v1.as_ref() {
        if frontier_matches_root(frontier, expected_root.as_ref()) {
            return Ok(frontier.clone());
        }
    }

    match (legacy, frontier_v1) {
        (Ok(_), Ok(_)) => anyhow::bail!(
            "failed to parse Orchard finalState at height {height}: decoded roots did not match finalRoot"
        ),
        (Err(legacy_err), Err(frontier_err)) => anyhow::bail!(
            "failed to parse Orchard finalState at height {height}: legacy={legacy_err}; frontier_v1={frontier_err}"
        ),
        (Err(legacy_err), Ok(_)) => anyhow::bail!(
            "failed to parse Orchard finalState at height {height}: frontier_v1 root did not match finalRoot; legacy={legacy_err}"
        ),
        (Ok(_), Err(frontier_err)) => anyhow::bail!(
            "failed to parse Orchard finalState at height {height}: legacy root did not match finalRoot; frontier_v1={frontier_err}"
        ),
    }
}

fn frontier_matches_root(
    frontier: &Frontier<MerkleHashOrchard, 32>,
    expected_root: Option<&[u8; 32]>,
) -> bool {
    expected_root
        .map(|root| frontier.root().to_bytes() == *root)
        .unwrap_or(true)
}

fn durable_inventory(scan: &ScannedInventory) -> DurableInventory {
    DurableInventory {
        last_scanned_height: scan.last_height,
        last_scanned_hash: scan.last_hash.clone(),
        note_count: scan.registry.spendable_note_count(),
        value_zats: scan.registry.spendable_value(),
        notes: scan
            .registry
            .spendable_notes()
            .into_iter()
            .map(durable_note)
            .collect(),
    }
}

fn durable_note(note: TrackedNote) -> DurableNote {
    let value_zats = note.value();
    DurableNote {
        note_id: note.note_id,
        origin_txid: note.origin_txid,
        action_index: note.origin_action_idx,
        role: note.role,
        value_zats,
        confirmation_height: note.last_confirmation_height,
    }
}

fn secret_bytes(record: &SecretKeyRecord) -> Result<[u8; 32]> {
    hex::decode(&record.secret_key_hex)
        .with_context(|| format!("{} secret_key_hex is not valid hex", record.key_id))?
        .try_into()
        .map_err(|bytes: Vec<u8>| {
            anyhow::anyhow!(
                "{} secret_key_hex must decode to 32 bytes, got {}",
                record.key_id,
                bytes.len()
            )
        })
}

fn public_orchard_runtime(
    config: &Config,
    wallet: &TxblastWallet,
    plan: &TxblastPlan,
) -> Result<OrchardBlastRuntimeConfig> {
    let orchard_premine = OrchardTxblastConfig {
        lanes_per_miner: plan.lanes_per_node,
        lane_value_zats: plan.lane_value_zats,
        fanout_source_value_zats: config.orchard_txblast.fanout_source_value_zats,
        fanout_outputs: wallet.defaults.fanout_width,
    };
    OrchardBlastRuntimeConfig::from_parts_with_network(
        orchard_premine,
        TxblastNetworkParams::from_network_kind(wallet.network),
        None,
        Some(plan.lanes_per_node),
        None,
        None,
        None,
        None,
    )
}

fn public_orchard_runtime_for_recovery(
    config: &Config,
    wallet: &TxblastWallet,
) -> Result<OrchardBlastRuntimeConfig> {
    if let Some(plan) = wallet.plans.last() {
        return public_orchard_runtime(config, wallet, plan);
    }
    let orchard_premine = OrchardTxblastConfig {
        lanes_per_miner: wallet.defaults.lanes_per_node,
        lane_value_zats: wallet.defaults.lane_value_zats,
        fanout_source_value_zats: config.orchard_txblast.fanout_source_value_zats,
        fanout_outputs: wallet.defaults.fanout_width,
    };
    OrchardBlastRuntimeConfig::from_parts_with_network(
        orchard_premine,
        TxblastNetworkParams::from_network_kind(wallet.network),
        None,
        Some(wallet.defaults.lanes_per_node),
        None,
        None,
        None,
        None,
    )
}

fn public_rpc_endpoint(config: &Config) -> Result<String> {
    let instance = config
        .miners
        .iter()
        .find(|instance| instance.public_ip != "TBD")
        .context("no active public node in config.json for txblast RPC")?;
    Ok(format!(
        "http://{}:{}",
        instance.public_ip,
        config.rpc_port()
    ))
}

fn parse_withdraw_amount(value: &str) -> Result<WithdrawalAmount> {
    if value == "all" {
        Ok(WithdrawalAmount::All)
    } else {
        let zats = value
            .parse::<u64>()
            .with_context(|| format!("withdraw amount must be zats or \"all\", got {value}"))?;
        if zats == 0 {
            anyhow::bail!("withdraw amount must be greater than 0");
        }
        Ok(WithdrawalAmount::Zats(zats))
    }
}

fn withdrawal_candidates(
    inventories: &[(&SecretKeyRecord, ScannedInventory)],
) -> Vec<WithdrawalCandidate> {
    inventories
        .iter()
        .enumerate()
        .flat_map(|(inventory_index, (_, scan))| {
            scan.registry
                .spendable_notes()
                .into_iter()
                .map(move |note| WithdrawalCandidate {
                    inventory_index,
                    note,
                })
        })
        .collect()
}

fn plan_withdrawal_sweeps(
    candidates: &[WithdrawalCandidate],
    amount: WithdrawalAmount,
) -> Result<Vec<PlannedWithdrawal>> {
    let values = candidates
        .iter()
        .enumerate()
        .map(|(candidate_index, candidate)| (candidate_index, candidate.note.value()))
        .collect::<Vec<_>>();
    plan_withdrawal_values(&values, amount).map(|planned| {
        planned
            .into_iter()
            .map(|entry| PlannedWithdrawal {
                candidate_index: entry.candidate_index,
                output_zats: entry.output_zats,
                with_change: entry.with_change,
            })
            .collect()
    })
}

fn plan_withdrawal_values(
    note_values: &[(usize, u64)],
    amount: WithdrawalAmount,
) -> Result<Vec<PlannedWithdrawalValue>> {
    let full_sweep_fee = orchard_to_transparent_fee(1);
    let change_fee = orchard_to_transparent_with_change_fee();
    let mut notes = note_values.to_vec();
    notes.sort_by_key(|(_, value)| std::cmp::Reverse(*value));

    match amount {
        WithdrawalAmount::All => {
            let planned = notes
                .into_iter()
                .filter_map(|(candidate_index, value)| {
                    let output_zats = value.checked_sub(full_sweep_fee)?;
                    (output_zats > 0).then_some(PlannedWithdrawalValue {
                        candidate_index,
                        output_zats,
                        with_change: false,
                    })
                })
                .collect::<Vec<_>>();
            if planned.is_empty() {
                anyhow::bail!("no shielded txblast inventory found to withdraw");
            }
            Ok(planned)
        }
        WithdrawalAmount::Zats(target) => {
            let mut remaining = target;
            let mut planned = Vec::new();
            for (candidate_index, value) in notes {
                if remaining == 0 {
                    break;
                }
                if value >= remaining.saturating_add(change_fee) {
                    planned.push(PlannedWithdrawalValue {
                        candidate_index,
                        output_zats: remaining,
                        with_change: true,
                    });
                    remaining = 0;
                    break;
                }
                if let Some(output_zats) = value.checked_sub(full_sweep_fee)
                    && output_zats > 0
                {
                    planned.push(PlannedWithdrawalValue {
                        candidate_index,
                        output_zats,
                        with_change: false,
                    });
                    remaining = remaining.saturating_sub(output_zats);
                }
            }
            if remaining > 0 {
                anyhow::bail!(
                    "insufficient spendable shielded inventory for withdrawal; {} zats remain unfunded after note/fee selection",
                    remaining
                );
            }
            Ok(planned)
        }
    }
}

async fn fetch_orchard_anchor(client: &ZebraRpcClient) -> Result<orchard::Anchor> {
    let height = client.get_block_count().await?;
    if height == 0 {
        return Ok(orchard::Anchor::empty_tree());
    }

    let treestate = client.z_get_treestate(height).await?;
    let root_hex = treestate
        .pointer("/orchard/commitments/finalRoot")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if root_hex.is_empty()
        || root_hex == "0000000000000000000000000000000000000000000000000000000000000000"
    {
        return Ok(orchard::Anchor::empty_tree());
    }

    let root_bytes: [u8; 32] = hex::decode(root_hex)
        .context("orchard finalRoot is not valid hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("orchard finalRoot is not 32 bytes"))?;
    let ct = orchard::Anchor::from_bytes(root_bytes);
    if bool::from(ct.is_none()) {
        anyhow::bail!("orchard finalRoot is not a valid anchor");
    }

    Ok(ct.unwrap())
}

fn plan_shielded_fanout_batches(
    control_notes: &[TrackedNote],
    hot_keys: &[&SecretKeyRecord],
    lane_requirements: &[usize],
    plan: &TxblastPlan,
    fanout_width: usize,
) -> Result<Vec<ShieldedFanoutBatch>> {
    if fanout_width == 0 {
        anyhow::bail!("fanout width must be greater than 0");
    }
    if hot_keys.len() != lane_requirements.len() {
        anyhow::bail!(
            "fanout planner received {} hot keys but {} lane requirements",
            hot_keys.len(),
            lane_requirements.len()
        );
    }
    let required_lanes = lane_requirements.iter().sum::<usize>();
    let mut slots = Vec::with_capacity(required_lanes);
    for (hot_key, required) in hot_keys.iter().zip(lane_requirements.iter()) {
        let address = derive_orchard_keys(&secret_bytes(hot_key)?)?.address();
        for _ in 0..*required {
            slots.push(address);
        }
    }
    if slots.is_empty() {
        anyhow::bail!("plan {} has no hot lane slots", plan.id);
    }

    let mut offset = 0usize;
    let mut batches = Vec::new();
    let mut notes = control_notes.to_vec();

    while offset < slots.len() {
        let remaining = slots.len() - offset;
        let desired_outputs = std::cmp::min(remaining, fanout_width);
        let mut selected = None;

        for output_count in (1..=desired_outputs).rev() {
            let fee = orchard_fanout_fee(output_count);
            let min_total = plan.lane_value_zats.saturating_mul(output_count as u64);
            selected = notes
                .iter()
                .enumerate()
                .filter(|(_, note)| {
                    note.value()
                        .checked_sub(fee)
                        .is_some_and(|distributable| distributable >= min_total)
                })
                .min_by_key(|(_, note)| note.value())
                .map(|(idx, _)| (idx, output_count));
            if selected.is_some() {
                break;
            }
        }

        let Some((note_idx, output_count)) = selected else {
            break;
        };

        let source_note = notes.swap_remove(note_idx);
        let distributable = source_note
            .value()
            .checked_sub(orchard_fanout_fee(output_count))
            .expect("selected fanout source covers fee");
        let values = split_amount(distributable, output_count);
        let recipients = values
            .into_iter()
            .enumerate()
            .map(|(idx, value)| {
                (
                    slots[offset + idx],
                    PlannedOutput {
                        role: NoteRole::Lane,
                        value,
                    },
                )
            })
            .collect::<Vec<_>>();
        batches.push(ShieldedFanoutBatch {
            source_note,
            recipients,
        });
        offset += output_count;
    }

    if offset < slots.len() {
        let control_value: u64 = control_notes.iter().map(TrackedNote::value).sum();
        anyhow::bail!(
            "shielded control inventory cannot fund {} lane outputs for plan {}; planned {} of {}, control inventory={} zats",
            slots.len(),
            plan.id,
            offset,
            slots.len(),
            control_value
        );
    }

    Ok(batches)
}

fn plan_control_reservoir_split_batches(
    control_notes: &[TrackedNote],
    required_hot_lanes: usize,
    fanout_width: usize,
    lane_value_zats: u64,
    target_reservoir_value_zats: u64,
) -> Result<Vec<ControlReservoirSplitBatch>> {
    if required_hot_lanes == 0 {
        return Ok(vec![]);
    }
    if fanout_width == 0 {
        anyhow::bail!("fanout width must be greater than 0");
    }

    let source_value =
        fanout_source_note_value(fanout_width, lane_value_zats, target_reservoir_value_zats);
    let required_sources = required_hot_lanes.div_ceil(fanout_width);
    let oversized_threshold = source_value.saturating_mul(2);
    let usable_existing = control_notes
        .iter()
        .filter(|note| note.value() >= source_value && note.value() <= oversized_threshold)
        .count();
    let mut sources_needed = required_sources.saturating_sub(usable_existing);
    if sources_needed == 0 {
        return Ok(vec![]);
    }

    let mut candidates = control_notes
        .iter()
        .filter(|note| note.value() > oversized_threshold)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|note| std::cmp::Reverse(note.value()));

    let mut batches = Vec::new();
    for source_note in candidates {
        if sources_needed == 0 {
            break;
        }
        if let Some((outputs, source_outputs)) =
            plan_reservoir_outputs_from_value(source_note.value(), sources_needed, source_value)
        {
            if source_outputs == 0 {
                continue;
            }
            sources_needed = sources_needed.saturating_sub(source_outputs);
            batches.push(ControlReservoirSplitBatch {
                source_note,
                outputs,
            });
        }
    }

    if sources_needed > 0 {
        let control_value: u64 = control_notes.iter().map(TrackedNote::value).sum();
        anyhow::bail!(
            "shielded control inventory cannot split enough control reservoirs for {} hot lane outputs; missing {} fanout source reservoir(s), control inventory={} zats",
            required_hot_lanes,
            sources_needed,
            control_value
        );
    }

    Ok(batches)
}

fn fanout_source_note_value(
    fanout_width: usize,
    lane_value_zats: u64,
    target_reservoir_value_zats: u64,
) -> u64 {
    let minimum_fanout_source = lane_value_zats
        .saturating_mul(fanout_width as u64)
        .saturating_add(orchard_fanout_fee(fanout_width));
    std::cmp::max(
        std::cmp::max(target_reservoir_value_zats, minimum_fanout_source),
        MIN_NOTE_VALUE,
    )
}

fn plan_reservoir_outputs_from_value(
    input_value: u64,
    max_source_outputs: usize,
    source_value: u64,
) -> Option<(Vec<PlannedOutput>, usize)> {
    if max_source_outputs == 0 {
        return None;
    }

    for source_count in (1..=max_source_outputs).rev() {
        let source_total = source_value.checked_mul(source_count as u64)?;
        let fee_without_change = orchard_fanout_fee(source_count);
        let Some(leftover_without_change) = input_value
            .checked_sub(source_total)
            .and_then(|remaining| remaining.checked_sub(fee_without_change))
        else {
            continue;
        };

        let mut outputs = vec![
            PlannedOutput {
                role: NoteRole::Reservoir,
                value: source_value,
            };
            source_count
        ];

        let fee_with_change = orchard_fanout_fee(source_count + 1);
        let change = input_value
            .checked_sub(source_total)
            .and_then(|remaining| remaining.checked_sub(fee_with_change));
        if let Some(change) = change {
            if change >= MIN_NOTE_VALUE {
                outputs.push(PlannedOutput {
                    role: NoteRole::Reservoir,
                    value: change,
                });
                return Some((outputs, source_count));
            }
        }

        if leftover_without_change > 0 {
            let last = outputs.last_mut().expect("source outputs are non-empty");
            last.value = last.value.saturating_add(leftover_without_change);
        }
        return Some((outputs, source_count));
    }

    None
}

fn plan_shielding_reservoir_outputs(
    input_value: u64,
    max_source_outputs: usize,
    source_value: u64,
) -> Result<(Vec<PlannedOutput>, usize)> {
    if max_source_outputs > 0 {
        for source_count in (1..=max_source_outputs).rev() {
            let source_total = source_value
                .checked_mul(source_count as u64)
                .context("shielding source output total overflowed")?;
            let fee_without_change = shielding_fee(source_count);
            let Some(leftover_without_change) = input_value
                .checked_sub(source_total)
                .and_then(|remaining| remaining.checked_sub(fee_without_change))
            else {
                continue;
            };

            let mut outputs = vec![
                PlannedOutput {
                    role: NoteRole::Reservoir,
                    value: source_value,
                };
                source_count
            ];
            let fee_with_change = shielding_fee(source_count + 1);
            let change = input_value
                .checked_sub(source_total)
                .and_then(|remaining| remaining.checked_sub(fee_with_change));
            if let Some(change) = change {
                if change >= MIN_NOTE_VALUE {
                    outputs.push(PlannedOutput {
                        role: NoteRole::Reservoir,
                        value: change,
                    });
                    return Ok((outputs, source_count));
                }
            }

            if leftover_without_change > 0 {
                let last = outputs.last_mut().expect("source outputs are non-empty");
                last.value = last.value.saturating_add(leftover_without_change);
            }
            return Ok((outputs, source_count));
        }
    }

    let output_value = input_value
        .checked_sub(shielding_fee(1))
        .context("deposit value is too small to shield")?;
    Ok((
        vec![PlannedOutput {
            role: NoteRole::Reservoir,
            value: output_value,
        }],
        usize::from(output_value >= source_value),
    ))
}

fn hot_lane_top_up_count(ready_lanes: usize, lane_total_value: u64, plan: &TxblastPlan) -> usize {
    let count_deficit = plan.lanes_per_node.saturating_sub(ready_lanes);
    let value_deficit = (plan.lanes_per_node as u64)
        .saturating_mul(plan.lane_value_zats)
        .saturating_sub(lane_total_value);
    let value_deficit_lanes = ceil_div(value_deficit, plan.lane_value_zats) as usize;
    std::cmp::max(count_deficit, value_deficit_lanes)
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        value.saturating_add(divisor.saturating_sub(1)) / divisor
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

fn public_state_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("state.json")
}

fn load_latest_prepare(dir: &Path) -> Result<PublicPrepareRecord> {
    let path = latest_prepare_path(dir);
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_public_state(dir: &Path, network: NetworkKind) -> Result<PublicTxblastState> {
    let path = public_state_path(dir);
    if !path.exists() {
        return Ok(PublicTxblastState {
            version: STATE_VERSION,
            network,
            updated_at_unix: now_unix(),
            confirmed_deposits: vec![],
            control_inventory: DurableInventory::default(),
            hot_inventory: vec![],
            shield_txids: vec![],
            reservoir_split_txids: vec![],
            fanout_txids: vec![],
            sweep_txids: vec![],
            pending_transactions: vec![],
        });
    }
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let state: PublicTxblastState = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if state.network != network {
        anyhow::bail!(
            "public txblast state network {} does not match wallet network {}",
            state.network,
            network
        );
    }
    Ok(state)
}

fn write_public_state(dir: &Path, state: &mut PublicTxblastState) -> Result<()> {
    state.updated_at_unix = now_unix();
    write_json(&public_state_path(dir), state)
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
    if let Some(instance) = config
        .miners
        .iter()
        .find(|instance| instance.public_ip != "TBD")
    {
        Ok(format!(
            "http://{}:{}",
            instance.public_ip,
            config.rpc_port()
        ))
    } else {
        Ok(format!("http://localhost:{}", config.rpc_port()))
    }
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
                let result = install_hot_key_with_retries(&name, &ip, &ssh_key, &command).await;
                Ok::<_, anyhow::Error>(match result {
                    Ok(()) => HotKeyInstallOutcome { name, error: None },
                    Err(error) => HotKeyInstallOutcome {
                        name,
                        error: Some(format!("{error:#}")),
                    },
                })
            }
        });

    let mut failures = Vec::new();
    for result in join_all(futs).await {
        let outcome = result?;
        if let Some(error) = outcome.error {
            eprintln!(
                "  {}: failed to install hot funded key: {error}",
                outcome.name
            );
            failures.push((outcome.name, error));
        } else {
            println!("  {}: installed hot funded key", outcome.name);
        }
    }

    if !failures.is_empty() {
        eprintln!(
            "warning: hot-key installation failed on {}/{} node(s); continuing. Rerun prepare to retry before starting txblast.",
            failures.len(),
            targets.len()
        );
    }
    Ok(())
}

async fn install_hot_key_with_retries(
    name: &str,
    ip: &str,
    ssh_key: &str,
    command: &str,
) -> Result<()> {
    for attempt in 1..=REMOTE_INSTALL_ATTEMPTS {
        match ssh::ssh_exec_long_connect_timeout(ip, ssh_key, command, REMOTE_INSTALL_TIMEOUT).await
        {
            Ok(_) => return Ok(()),
            Err(error) if attempt < REMOTE_INSTALL_ATTEMPTS => {
                eprintln!(
                    "  {name}: hot-key install attempt {attempt}/{REMOTE_INSTALL_ATTEMPTS} failed: {error}; retrying in {}s",
                    REMOTE_INSTALL_RETRY_BACKOFF.as_secs()
                );
                tokio::time::sleep(REMOTE_INSTALL_RETRY_BACKOFF).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to install hot key on {name}"));
            }
        }
    }
    unreachable!("install retry loop always returns");
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
        if derive_orchard_keys(&bytes).is_ok()
            && let Ok(secret_key) = SecretKey::from_slice(&bytes)
        {
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
            rpc_endpoint: None,
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
            rpc_endpoint: None,
            allow_underfunded_plan: false,
            json: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use incrementalmerkletree::Position;
    use orchard::keys::{FullViewingKey, Scope, SpendingKey};
    use orchard::note::{RandomSeed, Rho};
    use orchard::value::NoteValue;

    #[test]
    fn split_amount_distributes_remainder_to_early_parts() {
        assert_eq!(split_amount(10, 3), vec![4, 3, 3]);
    }

    #[test]
    fn parse_withdraw_amount_accepts_all_or_zats() {
        assert_eq!(
            parse_withdraw_amount("all").expect("all"),
            WithdrawalAmount::All
        );
        assert_eq!(
            parse_withdraw_amount("1000").expect("zats"),
            WithdrawalAmount::Zats(1000)
        );
        assert!(parse_withdraw_amount("0").is_err());
        assert!(parse_withdraw_amount("1.0").is_err());
    }

    #[test]
    fn plan_withdrawal_all_uses_net_after_fees() {
        let fee = orchard_to_transparent_fee(1);
        let planned = plan_withdrawal_values(
            &[(0, 50_000), (1, fee), (2, fee.saturating_sub(1))],
            WithdrawalAmount::All,
        )
        .expect("all withdrawal plan");

        assert_eq!(
            planned,
            vec![PlannedWithdrawalValue {
                candidate_index: 0,
                output_zats: 50_000 - fee,
                with_change: false,
            }]
        );
    }

    #[test]
    fn plan_withdrawal_explicit_fails_before_partial_selection_when_underfunded() {
        let fee = orchard_to_transparent_fee(1);
        let result = plan_withdrawal_values(&[(0, fee + 1)], WithdrawalAmount::Zats(fee + 2));

        assert!(result.is_err());
    }

    #[test]
    fn plan_withdrawal_explicit_uses_change_for_final_note() {
        let planned =
            plan_withdrawal_values(&[(0, 40_000), (1, 100_000)], WithdrawalAmount::Zats(60_000))
                .expect("explicit withdrawal plan");

        assert_eq!(
            planned,
            vec![PlannedWithdrawalValue {
                candidate_index: 1,
                output_zats: 60_000,
                with_change: true,
            }]
        );
    }

    #[test]
    fn hot_lane_top_up_tracks_count_and_value_deficits() {
        let plan = test_plan(4, 30_000);

        assert_eq!(hot_lane_top_up_count(4, 120_000, &plan), 0);
        assert_eq!(hot_lane_top_up_count(2, 120_000, &plan), 2);
        assert_eq!(hot_lane_top_up_count(4, 75_000, &plan), 2);
    }

    #[test]
    fn control_reservoir_split_handles_different_source_note_sizes() {
        let notes = vec![
            test_tracked_note(209_970_000, 0),
            test_tracked_note(80_000_000, 1),
        ];
        let batches = plan_control_reservoir_split_batches(&notes, 400, 10, 30_000, 500_000)
            .expect("split plan");

        let source_outputs = batches
            .iter()
            .flat_map(|batch| batch.outputs.iter())
            .filter(|output| output.value == 500_000)
            .count();
        let leftover_outputs = batches
            .iter()
            .flat_map(|batch| batch.outputs.iter())
            .filter(|output| output.value > 500_000)
            .count();

        assert_eq!(source_outputs, 40);
        assert_eq!(leftover_outputs, 1);
        assert_eq!(batches.len(), 1);
    }

    #[test]
    fn control_reservoir_split_preserves_existing_right_sized_notes() {
        let mut notes = (0..38)
            .map(|idx| test_tracked_note(500_000, idx))
            .collect::<Vec<_>>();
        notes.push(test_tracked_note(20_000_000, 38));

        let batches = plan_control_reservoir_split_batches(&notes, 400, 10, 30_000, 500_000)
            .expect("split plan");
        let source_outputs = batches
            .iter()
            .flat_map(|batch| batch.outputs.iter())
            .filter(|output| output.value == 500_000)
            .count();

        assert_eq!(source_outputs, 2);
    }

    #[test]
    fn orchard_treestate_parser_accepts_zebra_legacy_final_state() {
        let final_state = hex::decode(concat!(
            "0110a95f1929822baf13e8999e8f2e589c50a003916388febde6253daa8ea3732f",
            "01fd5a4ff083978d964338bf73fc2f0f41456ed7bec1b07f06b0f3317771da582a1f",
            "000000019127df82c42f8737629170adb607b6aa34f03b06a322ef251bda98fff5227212",
            "00015198a25a82a15e2b248c0c781705d931d39133831895deb9709ac33dee8e5f15",
            "0001b37549a9218887a1e3ce940b97ad3a3353139278b28e95ded97b0f57c191b827",
            "00000000017642b0cae10d1858517dfc8a3d9f63ec765284422b808e116145be0cbb42023d",
            "00000001e4bb5b2b7df9f4c3f508da258d91255399df66e84af8795377b7696bb87df423",
            "00012829e8aacdf1501baaeb5cb6e189d4e7182228e3d4b9acf54713595241e97f21",
            "017c8ece2b2ab2355d809b58809b21c7a5e95cfc693cd689387f7533ec8749261e",
            "01cc2dcaa338b312112db04b435a706d63244dd435238f0aa1e9e1598d3547081",
            "0012dcc4273c8a0ed2337ecf7879380a07e7d427c7f9d82e538002bd1442978402c",
            "01daf63debf5b40df902dae98dadc029f281474d190cddecef1b10653248a23415",
            "0001e2bca6a8d987d668defba89dc082196a922634ed88e065c669e526bb8815ee1b",
            "000000000000"
        ))
        .expect("hex");
        let frontier = parse_orchard_treestate_frontier(
            &final_state,
            Some("7acccc7b71e09a2d02a8d3816b84d4a8bbc100380efcaaafeee1ab3c6c0c6100"),
            3_326_759,
        )
        .expect("zebra legacy treestate");

        assert_eq!(frontier.tree_size(), 49_946_962);
    }

    #[test]
    fn shielded_fanout_prefers_smallest_sufficient_source_note() {
        let plan = test_plan(10, 30_000);
        let hot_key = test_secret_key_record();
        let hot_keys = vec![&hot_key];
        let notes = vec![
            test_tracked_note(20_000_000, 0),
            test_tracked_note(500_000, 1),
        ];

        let batches =
            plan_shielded_fanout_batches(&notes, &hot_keys, &[10], &plan, 10).expect("fanout");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].source_note.value(), 500_000);
    }

    #[test]
    fn record_submitted_tx_is_idempotent_across_reservoir_splits() {
        let mut state = empty_public_state();

        record_submitted_tx(
            &mut state,
            "split-tx".to_string(),
            DurableTxKind::ControlReservoirSplit,
            Some("plan-1".to_string()),
        );
        state.reservoir_split_txids.push(DurableTx {
            txid: "confirmed-split".to_string(),
            kind: DurableTxKind::ControlReservoirSplit,
            submitted_at_unix: 0,
            plan_id: Some("plan-1".to_string()),
            status: DurableTxStatus::Confirmed,
        });
        record_submitted_tx(
            &mut state,
            "confirmed-split".to_string(),
            DurableTxKind::ControlReservoirSplit,
            Some("plan-1".to_string()),
        );

        assert_eq!(state.pending_transactions.len(), 1);
    }

    #[test]
    fn pending_shield_deposit_blocks_missing_deposit_finalization() {
        let mut state = empty_public_state();
        state.confirmed_deposits.push(DurableDeposit {
            outpoint_id: "a:0".to_string(),
            txid: "a".to_string(),
            vout: 0,
            value_zats: 50_000,
            height: 1,
            state: DurableDepositState::ShieldingSubmitted,
        });
        state.pending_transactions.push(DurableTx {
            txid: "shield-tx".to_string(),
            kind: DurableTxKind::ShieldDeposit,
            submitted_at_unix: 0,
            plan_id: Some("plan-1".to_string()),
            status: DurableTxStatus::Submitted,
        });

        if !has_pending_kind(&state, DurableTxKind::ShieldDeposit) {
            mark_missing_deposits_shielded(&mut state, &[]);
        }

        assert_eq!(
            state.confirmed_deposits[0].state,
            DurableDepositState::ShieldingSubmitted
        );
        state.pending_transactions.clear();
        if !has_pending_kind(&state, DurableTxKind::ShieldDeposit) {
            mark_missing_deposits_shielded(&mut state, &[]);
        }

        assert_eq!(
            state.confirmed_deposits[0].state,
            DurableDepositState::Shielded
        );
    }

    #[test]
    fn record_submitted_tx_is_idempotent() {
        let mut state = empty_public_state();

        record_submitted_tx(
            &mut state,
            "abcd".to_string(),
            DurableTxKind::ShieldedFanout,
            Some("plan-1".to_string()),
        );
        record_submitted_tx(
            &mut state,
            "abcd".to_string(),
            DurableTxKind::ShieldedFanout,
            Some("plan-1".to_string()),
        );

        assert_eq!(state.pending_transactions.len(), 1);
    }

    fn empty_public_state() -> PublicTxblastState {
        PublicTxblastState {
            version: STATE_VERSION,
            network: NetworkKind::PublicTestnet,
            updated_at_unix: 0,
            confirmed_deposits: vec![],
            control_inventory: DurableInventory::default(),
            hot_inventory: vec![],
            shield_txids: vec![],
            reservoir_split_txids: vec![],
            fanout_txids: vec![],
            sweep_txids: vec![],
            pending_transactions: vec![],
        }
    }

    fn test_secret_key_record() -> SecretKeyRecord {
        let secret = [7u8; 32];
        SecretKeyRecord {
            key_id: "hot-0".to_string(),
            role: KeyRole::Hot,
            node_name: Some("node-0".to_string()),
            address: String::new(),
            public_key_hex: String::new(),
            secret_key_hex: hex::encode(secret),
        }
    }

    fn test_tracked_note(value_zats: u64, idx: usize) -> TrackedNote {
        let sk = SpendingKey::from_bytes([7u8; 32]).unwrap();
        let fvk = FullViewingKey::from(&sk);
        let address = fvk.address_at(0u32, Scope::External);
        let mut rho_bytes = [0u8; 32];
        rho_bytes[0] = (idx as u8).wrapping_add(1);
        let rho = Rho::from_bytes(&rho_bytes).unwrap();
        let rseed = (1u8..=u8::MAX)
            .find_map(|byte| {
                let mut rseed_bytes = [0u8; 32];
                rseed_bytes[0] = byte;
                Option::<RandomSeed>::from(RandomSeed::from_bytes(rseed_bytes, &rho))
            })
            .expect("valid random seed");
        let note = Option::<orchard::Note>::from(orchard::Note::from_parts(
            address,
            NoteValue::from_raw(value_zats),
            rho,
            rseed,
            // V2 is the ZIP 212 Orchard plaintext. V3 is the Ironwood
            // (ZIP 2005) format -- switching pools is a deliberate change,
            // not a default.
            orchard::note::NoteVersion::V2,
        ))
        .expect("valid test note");

        TrackedNote {
            note_id: format!("note-{idx}"),
            parent_note_id: None,
            origin_txid: format!("txid-{idx}"),
            origin_action_idx: idx,
            lane_id: None,
            note,
            position: Position::from(idx as u64),
            role: NoteRole::Reservoir,
            last_confirmation_height: 1,
        }
    }

    fn test_plan(lanes_per_node: usize, lane_value_zats: u64) -> TxblastPlan {
        TxblastPlan {
            id: "plan-1".to_string(),
            created_at_unix: 0,
            network: NetworkKind::PublicTestnet,
            target_block_bytes: DEFAULT_TARGET_BLOCK_BYTES,
            block_spacing_secs: DEFAULT_BLOCK_SPACING_SECS,
            duration_secs: DEFAULT_DURATION_SECS,
            measured_tx_bytes: DEFAULT_MEASURED_TX_BYTES,
            selected_nodes: vec!["node-0".to_string()],
            global_bytes_per_sec: 0.0,
            global_txs_per_sec: 0.0,
            per_node_bytes_per_sec: 0.0,
            per_node_txs_per_sec: 0.0,
            lanes_per_node,
            lane_value_zats,
            expected_run_txs: 0,
            run_fee_zats: 0,
            prepare_fee_zats: 0,
            withdraw_fee_zats: 0,
            required_zats_before_margin: 0,
            required_zats_with_margin: 0,
            imported_deposit_zats: 0,
            underfunded: false,
            max_mempool_bytes: None,
            safety_margin: DEFAULT_SAFETY_MARGIN,
        }
    }
}
