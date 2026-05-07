use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use shardtree::store::memory::MemoryShardStore;
use zcash_address::ZcashAddress;
use zcash_transparent::address::TransparentAddress;
use zebra_chain::serialization::ZcashDeserialize;

use crate::config::{LocalGenesisFundedKey, OrchardTxblastConfig};
use crate::txblast::orchard::{
    LaneRegistry, OrchardChainCursor, OrchardNullifierIndex, OrchardTree, OrchardTxblastTracer,
    PendingTxKind, PlannedOutput, RuntimePhase, TreasuryInventory,
    build_and_send_orchard_to_transparent_tx, build_and_send_shielding_tx, derive_orchard_keys,
    latest_checkpoint_anchor, latest_witness, orchard_to_transparent_fee, scan_block_range,
    shielding_fee,
};
use crate::txblast::rpc::{AddressUtxo, ZebraRpcClient};
use crate::txblast::transparent::{FundedKey, load_funded_key};
use crate::txblast::{OrchardBlastRuntimeConfig, TxblastNetworkParams, TxblastTraceConfig};

const COINBASE_MATURITY: u32 = 100;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const FUNDING_MARKER_FILE: &str = "runtime_keys_funded.txid";
const FUNDING_METADATA_FILE: &str = "runtime_keys_funded.json";
const FUNDING_METADATA_SCHEMA: &str = "kresko.runtime_funding.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeFundingMetadata {
    schema: String,
    minimum_recipient_zats: u64,
    shielding_txid: Option<String>,
    shielding_confirmation_height: Option<u32>,
    funding_txid: Option<String>,
    funding_confirmation_height: Option<u32>,
    verified_at_height: u32,
    recipients: Vec<RuntimeFundingRecipient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeFundingRecipient {
    name: String,
    address: String,
    spendable_non_coinbase_utxo_count: usize,
    spendable_non_coinbase_balance_zats: u64,
    immature_coinbase_utxo_count: usize,
    immature_coinbase_balance_zats: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeFundingLocalStatus {
    node: String,
    funded_key_name: String,
    funded_address: String,
    minimum_recipient_zats: u64,
    best_height: Option<u32>,
    best_block_hash: Option<String>,
    observed_funding_txid: Option<String>,
    expected_funding_txid: Option<String>,
    funding_tx_visible: bool,
    funding_tx_confirmed: bool,
    funding_tx_confirmations: Option<i64>,
    funding_tx_blockhash: Option<String>,
    spendable_non_coinbase_utxo_count: usize,
    spendable_non_coinbase_balance_zats: u64,
    immature_coinbase_utxo_count: usize,
    immature_coinbase_balance_zats: u64,
    ready: bool,
    stall_reason: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RecipientState {
    spendable_non_coinbase_utxo_count: usize,
    spendable_non_coinbase_balance_zats: u64,
    immature_coinbase_utxo_count: usize,
    immature_coinbase_balance_zats: u64,
}

pub async fn run_local(
    rpc_endpoint: &str,
    local_genesis_dir: &str,
    minimum_recipient_zats: u64,
    confirm_timeout_secs: u64,
    json: bool,
    verify_only: bool,
    expected_funding_txid: Option<&str>,
) -> Result<()> {
    let status = if verify_only {
        verify_runtime_funding_local(
            rpc_endpoint,
            local_genesis_dir,
            minimum_recipient_zats,
            expected_funding_txid,
        )
        .await?
    } else {
        fund_runtime_keys_local(
            rpc_endpoint,
            local_genesis_dir,
            minimum_recipient_zats,
            confirm_timeout_secs,
        )
        .await?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_runtime_funding_local_status(&status);
    }

    Ok(())
}

pub async fn ensure_local_runtime_funding(
    rpc_endpoint: &str,
    local_genesis_dir: &str,
    minimum_recipient_zats: u64,
    confirm_timeout_secs: u64,
    expected_funding_txid: Option<&str>,
) -> Result<()> {
    if let Some(expected_funding_txid) = expected_funding_txid {
        let status = verify_runtime_funding_local(
            rpc_endpoint,
            local_genesis_dir,
            minimum_recipient_zats,
            Some(expected_funding_txid),
        )
        .await?;
        if status.ready {
            return Ok(());
        }

        let reason = status
            .stall_reason
            .as_deref()
            .unwrap_or("runtime funding verification failed");
        println!(
            "[fund-runtime-keys] expected runtime funding tx {} is not ready locally ({reason}); refreshing funding state",
            expected_funding_txid
        );
    }

    let status = fund_runtime_keys_local(
        rpc_endpoint,
        local_genesis_dir,
        minimum_recipient_zats,
        confirm_timeout_secs,
    )
    .await?;
    if !status.ready {
        anyhow::bail!(
            "runtime funding did not become ready locally: {}",
            status
                .stall_reason
                .unwrap_or_else(|| "unknown stall".to_owned())
        );
    }

    Ok(())
}

async fn fund_runtime_keys_local(
    rpc_endpoint: &str,
    local_genesis_dir: &str,
    minimum_recipient_zats: u64,
    confirm_timeout_secs: u64,
) -> Result<RuntimeFundingLocalStatus> {
    if minimum_recipient_zats == 0 {
        anyhow::bail!("--minimum-recipient-zats must be greater than 0");
    }

    let local_genesis_dir = PathBuf::from(local_genesis_dir);
    let treasury_key_path = local_genesis_dir.join("treasury_key.json");
    let funded_keys_path = local_genesis_dir.join("funded_keys.json");
    let marker_path = local_genesis_dir.join(FUNDING_MARKER_FILE);
    let metadata_path = local_genesis_dir.join(FUNDING_METADATA_FILE);

    let (treasury_key, _) = load_funded_key(Some(
        treasury_key_path
            .to_str()
            .context("treasury key path is not valid UTF-8")?,
    ))?;
    let runtime_keys: Vec<LocalGenesisFundedKey> = serde_json::from_slice(
        &std::fs::read(&funded_keys_path)
            .with_context(|| format!("failed to read {}", funded_keys_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", funded_keys_path.display()))?;

    if runtime_keys.is_empty() {
        anyhow::bail!(
            "no runtime funded keys found in {}",
            funded_keys_path.display()
        );
    }

    let client = ZebraRpcClient::new(rpc_endpoint);
    let mut coinbase_cache = HashMap::new();
    let current_height = client.get_block_count().await?;
    let existing_metadata = read_existing_metadata(&metadata_path)?;
    let existing_marker_txid = read_existing_marker(&marker_path)?;
    let existing_balances = collect_runtime_recipient_state(
        &client,
        &runtime_keys,
        current_height,
        &mut coinbase_cache,
    )
    .await?;

    if recipients_meet_minimum(&existing_balances, minimum_recipient_zats) {
        let metadata = build_runtime_funding_metadata(
            minimum_recipient_zats,
            existing_metadata.as_ref(),
            existing_marker_txid.as_deref(),
            current_height,
            &existing_balances,
        );
        write_runtime_funding_metadata(&metadata_path, &metadata)?;
        if let Some(funding_txid) = metadata.funding_txid.as_deref() {
            write_runtime_funding_marker(&marker_path, funding_txid)?;
        }
        println!(
            "[fund-runtime-keys] runtime funded keys already have confirmed non-coinbase transparent balances; validated {} recipients at height {}",
            existing_balances.len(),
            current_height,
        );
        return verify_runtime_funding_local(
            rpc_endpoint,
            local_genesis_dir
                .to_str()
                .context("local genesis path is not valid UTF-8")?,
            minimum_recipient_zats,
            metadata.funding_txid.as_deref(),
        )
        .await;
    }

    if let Some(existing) = existing_marker_txid.as_deref() {
        println!(
            "[fund-runtime-keys] legacy marker {} present but recipient balances are below the required minimum; revalidating by funding again",
            existing,
        );
    }

    let treasury_utxo =
        select_spendable_treasury_utxo(&client, &treasury_key, current_height, &mut coinbase_cache)
            .await?;
    let treasury_orchard_keys = derive_orchard_keys(&treasury_key.secret_key.secret_bytes())?;

    let shield_value = treasury_utxo.satoshis.saturating_sub(shielding_fee(1));
    if shield_value < minimum_recipient_zats.saturating_mul(runtime_keys.len() as u64) {
        anyhow::bail!(
            "treasury UTXO value {} is too small to fund {} runtime keys after Orchard shielding",
            treasury_utxo.satoshis,
            runtime_keys.len(),
        );
    }

    let shield_anchor = fetch_orchard_anchor(&client).await?;
    let shield_target_height = current_height.saturating_add(10);
    let shield_submitted = build_and_send_shielding_tx(
        TxblastNetworkParams::LocalGenesis,
        &client,
        &treasury_key,
        &treasury_orchard_keys,
        &treasury_utxo.txid,
        treasury_utxo.output_index,
        &treasury_utxo.script,
        treasury_utxo.satoshis,
        &[PlannedOutput {
            role: crate::txblast::orchard::NoteRole::Reservoir,
            value: shield_value,
        }],
        shield_anchor,
        current_height,
        shield_target_height,
        PendingTxKind::WarmupShielding,
    )
    .await?;
    let shield_txid = shield_submitted.txid;
    let shield_pending = shield_submitted.pending;
    println!(
        "[fund-runtime-keys] submitted treasury shielding transaction {} ({} zats)",
        shield_txid, shield_value
    );

    let shield_height = wait_for_tx_confirmation(
        &client,
        &shield_txid,
        current_height,
        Duration::from_secs(confirm_timeout_secs),
    )
    .await?;

    let orchard_runtime = OrchardBlastRuntimeConfig::from_parts(
        OrchardTxblastConfig::default(),
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    let tracer = OrchardTxblastTracer::from_config(&TxblastTraceConfig::default(), "treasury");
    let mut tree: OrchardTree = OrchardTree::new(MemoryShardStore::empty(), 100);
    let mut next_position = 0u64;
    let mut nullifier_index = OrchardNullifierIndex::default();
    let mut cursor = OrchardChainCursor::default();
    let mut pending_txs = HashMap::from([(shield_txid.clone(), shield_pending)]);
    let mut registry = LaneRegistry::default();
    let mut treasury = TreasuryInventory::default();

    scan_block_range(
        &client,
        &treasury_orchard_keys,
        &mut tree,
        &mut next_position,
        &mut nullifier_index,
        1,
        shield_height,
        &mut pending_txs,
        &mut registry,
        &mut treasury,
        &mut cursor,
        &tracer,
        &orchard_runtime,
        RuntimePhase::BootstrapScan,
        0.0,
    )
    .await?;

    let funding_note = registry
        .take_reservoir()
        .context("failed to recover treasury Orchard note after shielding confirmation")?;
    let checkpoint =
        cursor
            .latest_checkpoint()
            .cloned()
            .unwrap_or_else(|| crate::txblast::orchard::BlockRef {
                height: shield_height,
                hash: String::new(),
            });
    let funding_anchor = latest_checkpoint_anchor(&tree, &checkpoint)?;
    let funding_witness = latest_witness(&tree, &funding_note, &checkpoint)?;

    let recipient_count = runtime_keys.len();
    let distributable = funding_note
        .value()
        .checked_sub(orchard_to_transparent_fee(recipient_count))
        .context("treasury Orchard note is too small to fund runtime keys after spend fee")?;
    let base_recipient_zats = distributable / recipient_count as u64;
    if base_recipient_zats < minimum_recipient_zats {
        anyhow::bail!(
            "treasury Orchard note cannot fund {} runtime keys: {} zats available after fee, {} zats each, minimum required is {}",
            recipient_count,
            distributable,
            base_recipient_zats,
            minimum_recipient_zats,
        );
    }

    let remainder = distributable % recipient_count as u64;
    let mut recipients = Vec::with_capacity(recipient_count);
    for (idx, key) in runtime_keys.iter().enumerate() {
        let mut value = base_recipient_zats;
        if idx == 0 {
            value = value.saturating_add(remainder);
        }
        recipients.push((transparent_address_for_runtime_key(key)?, value));
    }

    let funding_target_height = client.get_block_count().await?.saturating_add(10);
    let funding_txid = build_and_send_orchard_to_transparent_tx(
        TxblastNetworkParams::LocalGenesis,
        &client,
        &treasury_orchard_keys,
        &funding_note,
        funding_witness,
        funding_anchor,
        funding_target_height,
        &recipients,
    )
    .await?;
    println!(
        "[fund-runtime-keys] submitted transparent funding transaction {} to {} runtime keys (base={} zats, remainder={} zats)",
        funding_txid, recipient_count, base_recipient_zats, remainder,
    );

    let funding_height = wait_for_tx_confirmation(
        &client,
        &funding_txid,
        shield_height,
        Duration::from_secs(confirm_timeout_secs),
    )
    .await?;
    let verified_height = client.get_block_count().await?;
    let recipient_state = collect_runtime_recipient_state(
        &client,
        &runtime_keys,
        verified_height,
        &mut coinbase_cache,
    )
    .await?;
    if !recipients_meet_minimum(&recipient_state, minimum_recipient_zats) {
        anyhow::bail!(
            "transparent funding transaction {} confirmed at height {}, but one or more runtime funded keys still have insufficient confirmed transparent balance",
            funding_txid,
            funding_height,
        );
    }

    write_runtime_funding_marker(&marker_path, &funding_txid)?;
    let metadata = RuntimeFundingMetadata {
        schema: FUNDING_METADATA_SCHEMA.to_owned(),
        minimum_recipient_zats,
        shielding_txid: Some(shield_txid.clone()),
        shielding_confirmation_height: Some(shield_height),
        funding_txid: Some(funding_txid.clone()),
        funding_confirmation_height: Some(funding_height),
        verified_at_height: verified_height,
        recipients: recipient_state,
    };
    write_runtime_funding_metadata(&metadata_path, &metadata)?;

    println!(
        "[fund-runtime-keys] transparent runtime funding confirmed at height {} and verified across {} runtime keys",
        funding_height,
        metadata.recipients.len(),
    );
    verify_runtime_funding_local(
        rpc_endpoint,
        local_genesis_dir
            .to_str()
            .context("local genesis path is not valid UTF-8")?,
        minimum_recipient_zats,
        Some(&funding_txid),
    )
    .await
}

fn read_existing_marker(path: &Path) -> Result<Option<String>> {
    let existing = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };

    let existing = existing.trim();
    if existing.is_empty() {
        Ok(None)
    } else {
        Ok(Some(existing.to_owned()))
    }
}

fn read_existing_metadata(path: &Path) -> Result<Option<RuntimeFundingMetadata>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };

    let metadata = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(metadata))
}

fn write_runtime_funding_marker(path: &Path, funding_txid: &str) -> Result<()> {
    std::fs::write(path, format!("{funding_txid}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn write_runtime_funding_metadata(path: &Path, metadata: &RuntimeFundingMetadata) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(metadata)?;
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn build_runtime_funding_metadata(
    minimum_recipient_zats: u64,
    existing_metadata: Option<&RuntimeFundingMetadata>,
    existing_marker_txid: Option<&str>,
    verified_at_height: u32,
    recipients: &[RuntimeFundingRecipient],
) -> RuntimeFundingMetadata {
    RuntimeFundingMetadata {
        schema: FUNDING_METADATA_SCHEMA.to_owned(),
        minimum_recipient_zats,
        shielding_txid: existing_metadata.and_then(|metadata| metadata.shielding_txid.clone()),
        shielding_confirmation_height: existing_metadata
            .and_then(|metadata| metadata.shielding_confirmation_height),
        funding_txid: existing_metadata
            .and_then(|metadata| metadata.funding_txid.clone())
            .or_else(|| existing_marker_txid.map(ToOwned::to_owned)),
        funding_confirmation_height: existing_metadata
            .and_then(|metadata| metadata.funding_confirmation_height),
        verified_at_height,
        recipients: recipients.to_vec(),
    }
}

async fn collect_runtime_recipient_state(
    client: &ZebraRpcClient,
    runtime_keys: &[LocalGenesisFundedKey],
    current_height: u32,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<Vec<RuntimeFundingRecipient>> {
    let mut recipients = Vec::with_capacity(runtime_keys.len());

    for key in runtime_keys {
        let utxos = client.get_address_utxos(&key.address).await?;
        let state =
            classify_recipient_state(client, &utxos, current_height, coinbase_cache, None).await?;

        recipients.push(RuntimeFundingRecipient {
            name: key.name.clone(),
            address: key.address.clone(),
            spendable_non_coinbase_utxo_count: state.spendable_non_coinbase_utxo_count,
            spendable_non_coinbase_balance_zats: state.spendable_non_coinbase_balance_zats,
            immature_coinbase_utxo_count: state.immature_coinbase_utxo_count,
            immature_coinbase_balance_zats: state.immature_coinbase_balance_zats,
        });
    }

    Ok(recipients)
}

fn recipients_meet_minimum(
    recipients: &[RuntimeFundingRecipient],
    minimum_recipient_zats: u64,
) -> bool {
    recipients.iter().all(|recipient| {
        recipient.spendable_non_coinbase_utxo_count > 0
            && recipient.spendable_non_coinbase_balance_zats >= minimum_recipient_zats
    })
}

async fn classify_recipient_state(
    client: &ZebraRpcClient,
    utxos: &[AddressUtxo],
    current_height: u32,
    coinbase_cache: &mut HashMap<String, bool>,
    expected_funding_txid: Option<&str>,
) -> Result<RecipientState> {
    let mut state = RecipientState::default();

    for utxo in utxos {
        let is_coinbase = is_coinbase_transaction(client, &utxo.txid, coinbase_cache).await?;
        if is_coinbase {
            let maturity_height = utxo.height.saturating_add(COINBASE_MATURITY);
            if current_height < maturity_height {
                state.immature_coinbase_utxo_count += 1;
                state.immature_coinbase_balance_zats = state
                    .immature_coinbase_balance_zats
                    .saturating_add(utxo.satoshis);
                continue;
            }
        } else if expected_funding_txid.is_some_and(|expected| utxo.txid != expected) {
            continue;
        }

        if !is_coinbase {
            state.spendable_non_coinbase_utxo_count += 1;
            state.spendable_non_coinbase_balance_zats = state
                .spendable_non_coinbase_balance_zats
                .saturating_add(utxo.satoshis);
        }
    }

    Ok(state)
}

async fn verify_runtime_funding_local(
    rpc_endpoint: &str,
    local_genesis_dir: &str,
    minimum_recipient_zats: u64,
    expected_funding_txid: Option<&str>,
) -> Result<RuntimeFundingLocalStatus> {
    let client = ZebraRpcClient::new(rpc_endpoint);
    let info = client.get_blockchain_info().await?;
    let best_height = info["blocks"].as_u64().map(|value| value as u32);
    let best_block_hash = info["bestblockhash"].as_str().map(ToOwned::to_owned);
    let current_height =
        best_height.context("missing current block height from getblockchaininfo")?;
    let local_genesis_dir = PathBuf::from(local_genesis_dir);
    let metadata_path = local_genesis_dir.join(FUNDING_METADATA_FILE);
    let marker_path = local_genesis_dir.join(FUNDING_MARKER_FILE);
    let existing_metadata = read_existing_metadata(&metadata_path)?;
    let observed_funding_txid = existing_metadata
        .as_ref()
        .and_then(|metadata| metadata.funding_txid.clone())
        .or(read_existing_marker(&marker_path)?);

    let (funded_key, _) = load_funded_key(None)?;
    let utxos = client
        .get_address_utxos(&funded_key.address.to_string())
        .await?;
    let mut coinbase_cache = HashMap::new();
    let recipient_state = classify_recipient_state(
        &client,
        &utxos,
        current_height,
        &mut coinbase_cache,
        expected_funding_txid,
    )
    .await?;

    let expected_funding_txid = expected_funding_txid
        .map(ToOwned::to_owned)
        .or_else(|| observed_funding_txid.clone());
    let expected_funding_utxo_observed =
        expected_funding_txid.is_some() && recipient_state.spendable_non_coinbase_utxo_count > 0;
    let funding_tx = if let Some(txid) = expected_funding_txid.as_deref() {
        client.try_get_raw_transaction_verbose(txid).await?
    } else {
        None
    };
    let funding_tx_visible = funding_tx.is_some() || expected_funding_utxo_observed;
    let funding_tx_confirmations = funding_tx.as_ref().and_then(|tx| tx.confirmations);
    let funding_tx_confirmed = funding_tx_confirmations.is_some_and(|value| value > 0)
        && funding_tx
            .as_ref()
            .and_then(|tx| tx.blockhash.as_ref())
            .is_some()
        || expected_funding_utxo_observed;
    let funding_tx_blockhash = funding_tx.and_then(|tx| tx.blockhash);

    let ready = recipient_state.spendable_non_coinbase_utxo_count > 0
        && recipient_state.spendable_non_coinbase_balance_zats >= minimum_recipient_zats
        && expected_funding_txid
            .as_deref()
            .map(|_| funding_tx_confirmed)
            .unwrap_or(true);
    let stall_reason = runtime_funding_stall_reason(
        ready,
        expected_funding_txid.as_deref(),
        funding_tx_visible,
        funding_tx_confirmed,
        &recipient_state,
        minimum_recipient_zats,
    );

    Ok(RuntimeFundingLocalStatus {
        node: node_name(),
        funded_key_name: funded_key.name,
        funded_address: funded_key.address.to_string(),
        minimum_recipient_zats,
        best_height: Some(current_height),
        best_block_hash,
        observed_funding_txid,
        expected_funding_txid,
        funding_tx_visible,
        funding_tx_confirmed,
        funding_tx_confirmations,
        funding_tx_blockhash,
        spendable_non_coinbase_utxo_count: recipient_state.spendable_non_coinbase_utxo_count,
        spendable_non_coinbase_balance_zats: recipient_state.spendable_non_coinbase_balance_zats,
        immature_coinbase_utxo_count: recipient_state.immature_coinbase_utxo_count,
        immature_coinbase_balance_zats: recipient_state.immature_coinbase_balance_zats,
        ready,
        stall_reason,
        error: None,
    })
}

fn runtime_funding_stall_reason(
    ready: bool,
    expected_funding_txid: Option<&str>,
    funding_tx_visible: bool,
    funding_tx_confirmed: bool,
    recipient_state: &RecipientState,
    minimum_recipient_zats: u64,
) -> Option<String> {
    if ready {
        return None;
    }

    if expected_funding_txid.is_some() {
        if !funding_tx_visible {
            return Some("awaiting_runtime_funding_visibility".to_owned());
        }
        if !funding_tx_confirmed {
            return Some("runtime_funding_seen_but_unconfirmed".to_owned());
        }
        return Some("runtime_funding_seen_but_below_minimum".to_owned());
    }

    if recipient_state.immature_coinbase_utxo_count > 0 {
        Some("waiting_for_coinbase_maturity".to_owned())
    } else if recipient_state.spendable_non_coinbase_balance_zats < minimum_recipient_zats {
        Some("runtime_funding_seen_but_below_minimum".to_owned())
    } else {
        Some("awaiting_runtime_funding_visibility".to_owned())
    }
}

fn print_runtime_funding_local_status(status: &RuntimeFundingLocalStatus) {
    println!(
        "node={} funded_key={} address={} ready={} height={} balance_non_coinbase={} utxos_non_coinbase={} funding_tx_visible={} funding_tx_confirmed={} stall_reason={}",
        status.node,
        status.funded_key_name,
        status.funded_address,
        status.ready,
        status
            .best_height
            .map(|value| value.to_string())
            .unwrap_or_else(|| "N/A".to_owned()),
        status.spendable_non_coinbase_balance_zats,
        status.spendable_non_coinbase_utxo_count,
        status.funding_tx_visible,
        status.funding_tx_confirmed,
        status.stall_reason.as_deref().unwrap_or("-"),
    );
    if let Some(txid) = status.expected_funding_txid.as_deref() {
        println!("expected_funding_txid={txid}");
    }
    if let Some(txid) = status.observed_funding_txid.as_deref() {
        println!("observed_funding_txid={txid}");
    }
    if let Some(hash) = status.best_block_hash.as_deref() {
        println!("best_block_hash={hash}");
    }
}

fn node_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn select_spendable_treasury_utxo(
    client: &ZebraRpcClient,
    treasury_key: &FundedKey,
    current_height: u32,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<AddressUtxo> {
    let utxos = client
        .get_address_utxos(&treasury_key.address.to_string())
        .await?;
    let mut best = None;

    for utxo in utxos {
        let is_coinbase = is_coinbase_transaction(client, &utxo.txid, coinbase_cache).await?;
        if is_coinbase && current_height < utxo.height.saturating_add(COINBASE_MATURITY) {
            continue;
        }

        if best
            .as_ref()
            .map(|candidate: &AddressUtxo| candidate.satoshis < utxo.satoshis)
            .unwrap_or(true)
        {
            best = Some(utxo);
        }
    }

    best.context(
        "no spendable treasury UTXO found; make sure the cached bootstrap chain was loaded and miners are running",
    )
}

fn transparent_address_for_runtime_key(key: &LocalGenesisFundedKey) -> Result<TransparentAddress> {
    let zaddr = ZcashAddress::try_from_encoded(&key.address)
        .with_context(|| format!("invalid runtime funding address {}", key.address))?;
    zaddr.convert::<TransparentAddress>().map_err(|e| {
        anyhow::anyhow!(
            "runtime funding address {} is not transparent: {:?}",
            key.address,
            e
        )
    })
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

async fn wait_for_tx_confirmation(
    client: &ZebraRpcClient,
    expected_txid: &str,
    start_height: u32,
    timeout: Duration,
) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    let mut last_checked_height = start_height;

    loop {
        let current_height = client.get_block_count().await?;
        if current_height > last_checked_height {
            for height in (last_checked_height + 1)..=current_height {
                let block_bytes = client.getblock_raw(height).await?;
                let block = zebra_chain::block::Block::zcash_deserialize(&block_bytes[..])
                    .with_context(|| {
                        format!(
                            "failed to deserialize block while confirming tx at height {height}"
                        )
                    })?;
                if block
                    .transactions
                    .iter()
                    .any(|tx| tx.hash().to_string() == expected_txid)
                {
                    return Ok(height);
                }
            }
            last_checked_height = current_height;
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for transaction {} to confirm",
                expected_txid
            );
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn is_coinbase_transaction(
    client: &ZebraRpcClient,
    txid: &str,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<bool> {
    if let Some(is_coinbase) = coinbase_cache.get(txid) {
        return Ok(*is_coinbase);
    }

    let tx = client.get_raw_transaction_verbose(txid).await?;
    let is_coinbase = tx.vin.first().is_some_and(|vin| vin.coinbase.is_some());
    coinbase_cache.insert(txid.to_owned(), is_coinbase);
    Ok(is_coinbase)
}

