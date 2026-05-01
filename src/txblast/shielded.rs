use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

use super::orchard::{
    BuiltTx, LaneRegistry, OrchardChainCursor, OrchardKeys, OrchardNullifierIndex, OrchardTree,
    OrchardTxblastTracer, PendingRpcStatus, PendingTx, PendingTxKind, RuntimePhase, ScheduledWork,
    TrackedNote, TreasuryInventory, build_and_send_shielding_tx, build_lane_advance_tx,
    derive_orchard_keys, detect_reorg_reason, latest_checkpoint_anchor, latest_witness,
    min_bootstrap_shield_value, min_lane_value, pending_counts, pending_trace_summary,
    plan_next_work, plan_shielding_outputs, poll_best_tip, refresh_treasury_inventory,
    scan_block_range, seed_orchard_tree_from_treestate, wait_for_tip_change,
};
use super::rpc::ZebraRpcClient;
use super::transparent::FundedKey;
use super::{OrchardBlastRuntimeConfig, TxblastTraceConfig};

const LOOP_IDLE_SLEEP: Duration = Duration::from_millis(100);
const BUILD_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const TARGET_HEIGHT_OFFSET_BLOCKS: u32 = 100;

fn is_unknown_orchard_anchor(error: &str) -> bool {
    error.contains("unknown Orchard anchor")
}

fn is_witness_rebuild_error(error: &str) -> bool {
    error.contains("witness_at_checkpoint_id") || error.contains("no witness for position")
}

fn duration_ms_u64(duration_ms: u128) -> u64 {
    duration_ms.min(u128::from(u64::MAX)) as u64
}

fn classify_error(error: &str) -> Option<&'static str> {
    if error.contains("unknown Orchard anchor") {
        Some("unknown_orchard_anchor")
    } else if error.contains("same effects as one already in the mempool") {
        Some("same_effects_already_in_mempool")
    } else if error.contains("duplicate nullifier") {
        Some("duplicate_nullifier")
    } else if error.contains("already spent in mempool") {
        Some("already_spent_in_mempool")
    } else if error.contains("verification cancelled") {
        Some("verification_cancelled")
    } else if error.contains("witness_at_checkpoint_id") {
        Some("witness_at_checkpoint_missing")
    } else if error.contains("no witness for position") {
        Some("witness_position_missing")
    } else if error.contains("transaction not found")
        || error.contains("no such mempool or blockchain transaction")
    {
        Some("tx_not_found")
    } else if error.contains("timeout") {
        Some("rpc_timeout")
    } else {
        None
    }
}

struct BuildHeartbeat {
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl BuildHeartbeat {
    fn start(
        tracer: &OrchardTxblastTracer,
        height: u32,
        phase: RuntimePhase,
        tx_kind: PendingTxKind,
        lane_id: Option<u64>,
        note_id: Option<&str>,
        note_role: Option<super::orchard::NoteRole>,
        note_value: Option<u64>,
        pending: super::orchard::PendingTxCounts,
        registry: super::orchard::state::RegistrySnapshot,
        treasury: super::orchard::state::TreasurySnapshot,
        reason: &'static str,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let tracer = tracer.clone();
        let note_id = note_id.map(ToOwned::to_owned);
        let started_at = Instant::now();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(BUILD_HEARTBEAT_INTERVAL).await;
                if task_stop.load(Ordering::Relaxed) {
                    break;
                }
                tracer.trace_event(super::orchard::tracing::EventContext {
                    height: Some(height),
                    phase,
                    event: "build_heartbeat",
                    tx_kind: Some(tx_kind),
                    txid: None,
                    lane_id,
                    note_id: note_id.as_deref(),
                    note_role,
                    note_value,
                    pending,
                    registry,
                    treasury,
                    reason: Some(reason),
                    error: None,
                    error_class: None,
                    build_duration_ms: Some(duration_ms_u64(started_at.elapsed().as_millis())),
                    rpc_submit_duration_ms: None,
                    confirm_delay_ms: None,
                    confirm_delay_blocks: None,
                });
            }
        });

        Self { stop, task }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

async fn probe_submitted_tx_visibility(
    client: &ZebraRpcClient,
    tracer: &OrchardTxblastTracer,
    registry: &LaneRegistry,
    treasury: &TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    height: u32,
    phase: RuntimePhase,
    txid: &str,
) {
    let pending_counts_snapshot = pending_counts(pending_txs);
    let registry_snapshot = registry.snapshot();
    let treasury_snapshot = treasury.snapshot();

    let tx = match client.try_get_raw_transaction_verbose(txid).await {
        Ok(tx) => tx,
        Err(error) => {
            if let Some(pending) = pending_txs.get(txid) {
                let error_text = error.to_string();
                tracer.trace_event(super::orchard::tracing::EventContext {
                    height: Some(height),
                    phase,
                    event: "tx_post_submit_rpc_lookup_failed",
                    tx_kind: Some(pending.kind),
                    txid: Some(txid),
                    lane_id: pending.spent_lane_id,
                    note_id: pending.spent_note_id.as_deref(),
                    note_role: pending.spent_note_role,
                    note_value: pending.spent_note_value,
                    pending: pending_counts_snapshot,
                    registry: registry_snapshot,
                    treasury: treasury_snapshot,
                    reason: Some("post_submit_rpc_probe"),
                    error: Some(error_text.clone()),
                    error_class: classify_error(&error_text),
                    build_duration_ms: None,
                    rpc_submit_duration_ms: None,
                    confirm_delay_ms: Some(0),
                    confirm_delay_blocks: Some(0),
                });
            }
            return;
        }
    };

    let Some(pending) = pending_txs.get_mut(txid) else {
        return;
    };

    let event = match tx {
        None => "tx_post_submit_not_visible",
        Some(tx) if tx.blockhash.is_some() || tx.confirmations.unwrap_or(0) > 0 => {
            pending.last_rpc_status = PendingRpcStatus::ConfirmedByRpc;
            "tx_post_submit_confirmed_rpc"
        }
        Some(_) => {
            pending.last_rpc_status = PendingRpcStatus::InMempool;
            "tx_post_submit_mempool_seen"
        }
    };

    tracer.trace_event(super::orchard::tracing::EventContext {
        height: Some(height),
        phase,
        event,
        tx_kind: Some(pending.kind),
        txid: Some(txid),
        lane_id: pending.spent_lane_id,
        note_id: pending.spent_note_id.as_deref(),
        note_role: pending.spent_note_role,
        note_value: pending.spent_note_value,
        pending: pending_counts_snapshot,
        registry: registry_snapshot,
        treasury: treasury_snapshot,
        reason: Some("post_submit_rpc_probe"),
        error: None,
        error_class: None,
        build_duration_ms: None,
        rpc_submit_duration_ms: None,
        confirm_delay_ms: Some(duration_ms_u64(pending.submitted_at.elapsed().as_millis())),
        confirm_delay_blocks: Some(height.saturating_sub(pending.submitted_height)),
    });
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

fn transition_phase(
    tracer: &OrchardTxblastTracer,
    phase: RuntimePhase,
    height: u32,
    event: &'static str,
    registry: &LaneRegistry,
    treasury: &TreasuryInventory,
    pending_txs: &HashMap<String, PendingTx>,
) {
    tracer.trace_event(super::orchard::tracing::EventContext {
        height: Some(height),
        phase,
        event,
        tx_kind: None,
        txid: None,
        lane_id: None,
        note_id: None,
        note_role: None,
        note_value: None,
        pending: pending_counts(pending_txs),
        registry: registry.snapshot(),
        treasury: treasury.snapshot(),
        reason: None,
        error: None,
        error_class: None,
        build_duration_ms: None,
        rpc_submit_duration_ms: None,
        confirm_delay_ms: None,
        confirm_delay_blocks: None,
    });
}

async fn seed_orchard_tree_for_scan_start(
    client: &ZebraRpcClient,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    scan_start_height: u32,
) -> Result<()> {
    if scan_start_height <= 1 {
        return Ok(());
    }

    let frontier_height = scan_start_height - 1;
    println!(
        "[shielded] seeding Orchard tree from treestate at height {}",
        frontier_height
    );
    seed_orchard_tree_from_treestate(tree, client, frontier_height).await?;
    *next_position = tree
        .frontier()
        .map_err(|e| anyhow::anyhow!("seeded Orchard frontier read failed: {e:?}"))?
        .tree_size();
    Ok(())
}

async fn rescan_orchard_state_from_chain(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    tracer: &OrchardTxblastTracer,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    nullifier_index: &mut OrchardNullifierIndex,
    cursor: &mut OrchardChainCursor,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    submit_credit: f64,
    scan_start_height: u32,
    reason: &'static str,
    error: Option<String>,
) -> Result<()> {
    let rebuild_height = cursor.last_scanned_height();
    transition_phase(
        tracer,
        RuntimePhase::Recovering,
        rebuild_height,
        "phase_enter",
        registry,
        treasury,
        pending_txs,
    );
    tracer.trace_event(super::orchard::tracing::EventContext {
        height: Some(rebuild_height),
        phase: RuntimePhase::Recovering,
        event: "chain_rebuild_started",
        tx_kind: None,
        txid: None,
        lane_id: None,
        note_id: None,
        note_role: None,
        note_value: None,
        pending: pending_counts(pending_txs),
        registry: registry.snapshot(),
        treasury: treasury.snapshot(),
        reason: Some(reason),
        error_class: error.as_deref().and_then(classify_error),
        error,
        build_duration_ms: None,
        rpc_submit_duration_ms: None,
        confirm_delay_ms: None,
        confirm_delay_blocks: None,
    });
    tracer.trace_registry(
        Some(rebuild_height),
        RuntimePhase::Recovering,
        "rebuild_start",
        registry,
        treasury,
        pending_counts(pending_txs),
        pending_trace_summary(pending_txs, rebuild_height),
        submit_credit,
        orchard_cfg,
        Some(reason),
    );

    *tree = OrchardTree::new(shardtree::store::memory::MemoryShardStore::empty(), 100);
    *next_position = 0;
    nullifier_index.clear();
    registry.reset_for_rebuild();
    cursor.reset_for_rebuild();

    seed_orchard_tree_for_scan_start(client, tree, next_position, scan_start_height).await?;

    let best_tip = poll_best_tip(client).await?;
    cursor.record_best_tip(best_tip.clone());
    if best_tip.height >= scan_start_height {
        scan_block_range(
            client,
            keys,
            tree,
            next_position,
            nullifier_index,
            scan_start_height,
            best_tip.height,
            pending_txs,
            registry,
            treasury,
            cursor,
            tracer,
            orchard_cfg,
            RuntimePhase::Recovering,
            submit_credit,
        )
        .await?;
    } else {
        cursor.record_last_scanned(best_tip);
    }

    tracer.trace_registry(
        Some(cursor.last_scanned_height()),
        RuntimePhase::Recovering,
        "rebuild_complete",
        registry,
        treasury,
        pending_counts(pending_txs),
        pending_trace_summary(pending_txs, cursor.last_scanned_height()),
        submit_credit,
        orchard_cfg,
        Some(reason),
    );

    Ok(())
}

async fn refresh_and_trace_treasury(
    client: &ZebraRpcClient,
    key: &FundedKey,
    current_height: u32,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    expected_runtime_funding_txid: Option<&str>,
    treasury: &mut TreasuryInventory,
    coinbase_cache: &mut HashMap<String, bool>,
    tracer: &OrchardTxblastTracer,
    phase: RuntimePhase,
    registry: &LaneRegistry,
    pending_txs: &HashMap<String, PendingTx>,
    submit_credit: f64,
) -> Result<super::orchard::treasury::TreasuryRefresh> {
    let refresh = refresh_treasury_inventory(
        client,
        &key.address.to_string(),
        current_height,
        min_bootstrap_shield_value(orchard_cfg),
        expected_runtime_funding_txid,
        treasury,
        coinbase_cache,
    )
    .await?;
    let reason = bootstrap_wait_reason(
        registry,
        treasury,
        &refresh,
        min_bootstrap_shield_value(orchard_cfg),
        expected_runtime_funding_txid,
    );

    tracer.trace_registry(
        Some(current_height),
        phase,
        "treasury_refresh",
        registry,
        treasury,
        pending_counts(pending_txs),
        pending_trace_summary(pending_txs, current_height),
        submit_credit,
        orchard_cfg,
        reason,
    );

    Ok(refresh)
}

async fn reconcile_pending_txs(
    client: &ZebraRpcClient,
    tracer: &OrchardTxblastTracer,
    registry: &LaneRegistry,
    treasury: &TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    height: u32,
    phase: RuntimePhase,
) -> Result<Option<&'static str>> {
    let txids = pending_txs.keys().cloned().collect::<Vec<_>>();
    let mut evicted_any = false;

    for txid in txids {
        let tx = match client.try_get_raw_transaction_verbose(&txid).await {
            Ok(tx) => tx,
            Err(error) => {
                if let Some(pending) = pending_txs.get(&txid) {
                    tracer.trace_event(super::orchard::tracing::EventContext {
                        height: Some(height),
                        phase,
                        event: "pending_tx_rpc_lookup_failed",
                        tx_kind: Some(pending.kind),
                        txid: Some(&txid),
                        lane_id: pending.spent_lane_id,
                        note_id: pending.spent_note_id.as_deref(),
                        note_role: pending.spent_note_role,
                        note_value: pending.spent_note_value,
                        pending: pending_counts(pending_txs),
                        registry: registry.snapshot(),
                        treasury: treasury.snapshot(),
                        reason: Some("pending_tx_rpc_lookup"),
                        error_class: classify_error(&error.to_string()),
                        error: Some(error.to_string()),
                        build_duration_ms: None,
                        rpc_submit_duration_ms: None,
                        confirm_delay_ms: None,
                        confirm_delay_blocks: Some(height.saturating_sub(pending.submitted_height)),
                    });
                }
                return Err(error);
            }
        };

        let Some(tx) = tx else {
            let Some(pending) = pending_txs.remove(&txid) else {
                continue;
            };
            evicted_any = true;
            tracer.trace_event(super::orchard::tracing::EventContext {
                height: Some(height),
                phase,
                event: "pending_tx_evicted",
                tx_kind: Some(pending.kind),
                txid: Some(&txid),
                lane_id: pending.spent_lane_id,
                note_id: pending.spent_note_id.as_deref(),
                note_role: pending.spent_note_role,
                note_value: pending.spent_note_value,
                pending: pending_counts(pending_txs),
                registry: registry.snapshot(),
                treasury: treasury.snapshot(),
                reason: Some("rebuilding_after_pending_eviction"),
                error: None,
                error_class: None,
                build_duration_ms: None,
                rpc_submit_duration_ms: None,
                confirm_delay_ms: Some(duration_ms_u64(pending.submitted_at.elapsed().as_millis())),
                confirm_delay_blocks: Some(height.saturating_sub(pending.submitted_height)),
            });
            continue;
        };

        let rpc_status = if tx.blockhash.is_some() || tx.confirmations.unwrap_or(0) > 0 {
            PendingRpcStatus::ConfirmedByRpc
        } else {
            PendingRpcStatus::InMempool
        };

        let Some((
            kind,
            lane_id,
            note_id,
            note_role,
            note_value,
            pending_age_ms,
            pending_age_blocks,
        )) = pending_txs.get_mut(&txid).and_then(|pending| {
            if pending.last_rpc_status == rpc_status {
                return None;
            }

            pending.last_rpc_status = rpc_status;
            Some((
                pending.kind,
                pending.spent_lane_id,
                pending.spent_note_id.clone(),
                pending.spent_note_role,
                pending.spent_note_value,
                duration_ms_u64(pending.submitted_at.elapsed().as_millis()),
                height.saturating_sub(pending.submitted_height),
            ))
        })
        else {
            continue;
        };
        tracer.trace_event(super::orchard::tracing::EventContext {
            height: Some(height),
            phase,
            event: match rpc_status {
                PendingRpcStatus::Unknown => unreachable!("unknown is not an observed RPC state"),
                PendingRpcStatus::InMempool => "pending_tx_mempool_seen",
                PendingRpcStatus::ConfirmedByRpc => "pending_tx_confirmed_rpc",
            },
            tx_kind: Some(kind),
            txid: Some(&txid),
            lane_id: lane_id,
            note_id: note_id.as_deref(),
            note_role: note_role,
            note_value: note_value,
            pending: pending_counts(pending_txs),
            registry: registry.snapshot(),
            treasury: treasury.snapshot(),
            reason: Some("pending_tx_rpc_state_changed"),
            error: None,
            error_class: None,
            build_duration_ms: None,
            rpc_submit_duration_ms: None,
            confirm_delay_ms: Some(pending_age_ms),
            confirm_delay_blocks: Some(pending_age_blocks),
        });
    }

    Ok(evicted_any.then_some("rebuilding_after_pending_eviction"))
}

async fn sync_orchard_chain_state(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    tracer: &OrchardTxblastTracer,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    nullifier_index: &mut OrchardNullifierIndex,
    cursor: &mut OrchardChainCursor,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    submit_credit: f64,
    phase: RuntimePhase,
    scan_start_height: u32,
) -> Result<()> {
    let best_tip = poll_best_tip(client).await?;
    cursor.record_best_tip(best_tip.clone());

    if let Some(reason) = detect_reorg_reason(client, cursor).await? {
        return rescan_orchard_state_from_chain(
            client,
            keys,
            orchard_cfg,
            tracer,
            tree,
            next_position,
            nullifier_index,
            cursor,
            registry,
            treasury,
            pending_txs,
            submit_credit,
            scan_start_height,
            reason,
            None,
        )
        .await;
    }

    if let Some(reason) = reconcile_pending_txs(
        client,
        tracer,
        registry,
        treasury,
        pending_txs,
        cursor.last_scanned_height(),
        phase,
    )
    .await?
    {
        return rescan_orchard_state_from_chain(
            client,
            keys,
            orchard_cfg,
            tracer,
            tree,
            next_position,
            nullifier_index,
            cursor,
            registry,
            treasury,
            pending_txs,
            submit_credit,
            scan_start_height,
            reason,
            None,
        )
        .await;
    }

    let start_height = cursor
        .last_scanned_height()
        .saturating_add(1)
        .max(scan_start_height);
    if best_tip.height >= start_height {
        scan_block_range(
            client,
            keys,
            tree,
            next_position,
            nullifier_index,
            start_height,
            best_tip.height,
            pending_txs,
            registry,
            treasury,
            cursor,
            tracer,
            orchard_cfg,
            phase,
            submit_credit,
        )
        .await?;
    } else if cursor.last_scanned().is_none() {
        cursor.record_last_scanned(best_tip);
    }

    Ok(())
}

fn bootstrap_wait_reason<'a>(
    registry: &LaneRegistry,
    treasury: &TreasuryInventory,
    refresh: &super::orchard::treasury::TreasuryRefresh,
    minimum_runtime_funding_zats: u64,
    expected_runtime_funding_txid: Option<&'a str>,
) -> Option<&'static str> {
    if registry.spendable_note_count() > 0 || treasury.backlog_count() > 0 {
        return None;
    }

    if expected_runtime_funding_txid.is_some() {
        if !refresh.funding_tx_visible {
            return Some("awaiting_runtime_funding_visibility");
        }
        if !refresh.funding_tx_confirmed {
            return Some("runtime_funding_seen_but_unconfirmed");
        }
        if refresh.spendable_funding_utxo_count == 0
            || refresh.spendable_funding_balance_zats < minimum_runtime_funding_zats
        {
            return Some("runtime_funding_seen_but_below_minimum");
        }
    }

    if refresh.earliest_maturity_height.is_some() {
        Some("waiting_for_coinbase_maturity")
    } else {
        Some("awaiting_transparent_runtime_funds")
    }
}

async fn run_bootstrap(
    client: &ZebraRpcClient,
    key: &FundedKey,
    keys: &OrchardKeys,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    expected_runtime_funding_txid: Option<&str>,
    tracer: &OrchardTxblastTracer,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    nullifier_index: &mut OrchardNullifierIndex,
    cursor: &mut OrchardChainCursor,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    coinbase_cache: &mut HashMap<String, bool>,
    scan_start_height: u32,
) -> Result<()> {
    let mut phase = RuntimePhase::BootstrapScan;
    transition_phase(
        tracer,
        phase,
        cursor.last_scanned_height(),
        "phase_enter",
        registry,
        treasury,
        pending_txs,
    );

    loop {
        let refresh = refresh_and_trace_treasury(
            client,
            key,
            cursor.last_scanned_height(),
            orchard_cfg,
            expected_runtime_funding_txid,
            treasury,
            coinbase_cache,
            tracer,
            phase,
            registry,
            pending_txs,
            0.0,
        )
        .await?;

        if registry.spendable_note_count() > 0 || treasury.backlog_count() > 0 {
            break;
        }

        let wait_reason = bootstrap_wait_reason(
            registry,
            treasury,
            &refresh,
            min_bootstrap_shield_value(orchard_cfg),
            expected_runtime_funding_txid,
        )
        .unwrap_or("awaiting_transparent_runtime_funds");

        if wait_reason == "waiting_for_coinbase_maturity" {
            let earliest_maturity = refresh
                .earliest_maturity_height
                .expect("coinbase maturity reason should include a maturity height");
            println!(
                "[shielded] waiting for coinbase maturity (need height {earliest_maturity}, currently {})",
                cursor.last_scanned_height()
            );
        } else {
            println!("[shielded] waiting for bootstrap funding state: {wait_reason}");
        }

        let tip_before_wait = cursor
            .best_tip()
            .cloned()
            .unwrap_or(poll_best_tip(client).await?);
        cursor.record_best_tip(tip_before_wait.clone());
        let _ = wait_for_tip_change(client, &tip_before_wait).await?;
        sync_orchard_chain_state(
            client,
            keys,
            orchard_cfg,
            tracer,
            tree,
            next_position,
            nullifier_index,
            cursor,
            registry,
            treasury,
            pending_txs,
            0.0,
            phase,
            scan_start_height,
        )
        .await?;
    }

    if registry.spendable_note_count() >= orchard_cfg.target_ready_lanes
        || treasury.backlog_count() == 0
    {
        return Ok(());
    }

    phase = RuntimePhase::BootstrapShield;
    transition_phase(
        tracer,
        phase,
        cursor.last_scanned_height(),
        "phase_enter",
        registry,
        treasury,
        pending_txs,
    );
    println!(
        "[shielded] bootstrap shielding treasury backlog into Orchard lanes (treasury_utxos={}, target_ready_lanes={})",
        treasury.backlog_count(),
        orchard_cfg.target_ready_lanes,
    );

    while registry.spendable_note_count() < orchard_cfg.target_ready_lanes
        && treasury.backlog_count() > 0
    {
        let utxo = treasury
            .take_ready_utxo()
            .expect("bootstrap loop should only run with treasury backlog");
        let remaining_lane_target = orchard_cfg
            .target_ready_lanes
            .saturating_sub(registry.spendable_note_count());
        let planned_outputs =
            plan_shielding_outputs(utxo.satoshis, remaining_lane_target, orchard_cfg)?;

        let anchor = fetch_orchard_anchor(client).await?;
        let target_height = client
            .get_block_count()
            .await?
            .saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
        tracer.trace_registry(
            Some(cursor.last_scanned_height()),
            phase,
            "build_start",
            registry,
            treasury,
            pending_counts(pending_txs),
            pending_trace_summary(pending_txs, cursor.last_scanned_height()),
            0.0,
            orchard_cfg,
            Some("proving_bootstrap_shield"),
        );
        let heartbeat = BuildHeartbeat::start(
            tracer,
            cursor.last_scanned_height(),
            phase,
            PendingTxKind::WarmupShielding,
            None,
            Some(&utxo.outpoint_id),
            None,
            Some(utxo.satoshis),
            pending_counts(pending_txs),
            registry.snapshot(),
            treasury.snapshot(),
            "proving_bootstrap_shield",
        );
        let submitted = build_and_send_shielding_tx(
            orchard_cfg.network_params,
            client,
            key,
            keys,
            &utxo.txid,
            utxo.output_index,
            &utxo.script,
            utxo.satoshis,
            &planned_outputs,
            anchor,
            cursor.last_scanned_height(),
            target_height,
            PendingTxKind::WarmupShielding,
        )
        .await?;
        heartbeat.finish();
        let txid = submitted.txid;
        let pending = submitted.pending;
        tracer.trace_event(super::orchard::tracing::EventContext {
            height: Some(cursor.last_scanned_height()),
            phase,
            event: "tx_submitted",
            tx_kind: Some(PendingTxKind::WarmupShielding),
            txid: Some(&txid),
            lane_id: None,
            note_id: None,
            note_role: None,
            note_value: Some(utxo.satoshis),
            pending: {
                let mut counts = pending_counts(pending_txs);
                counts.total += 1;
                counts
            },
            registry: registry.snapshot(),
            treasury: treasury.snapshot(),
            reason: Some("bootstrap_shield"),
            error: None,
            error_class: None,
            build_duration_ms: Some(submitted.build_duration_ms),
            rpc_submit_duration_ms: Some(submitted.rpc_submit_duration_ms),
            confirm_delay_ms: None,
            confirm_delay_blocks: None,
        });
        for recovered in &pending.recovered_notes {
            tracer.trace_recovered_note(
                Some(cursor.last_scanned_height()),
                "note_submitted",
                recovered,
                Some(&txid),
                Some("bootstrap_shield_output"),
            );
        }
        pending_txs.insert(txid.clone(), pending);
        probe_submitted_tx_visibility(
            client,
            tracer,
            registry,
            treasury,
            pending_txs,
            cursor.last_scanned_height(),
            phase,
            &txid,
        )
        .await;

        let tip_before_wait = cursor
            .best_tip()
            .cloned()
            .unwrap_or(poll_best_tip(client).await?);
        cursor.record_best_tip(tip_before_wait.clone());
        let _ = wait_for_tip_change(client, &tip_before_wait).await?;
        sync_orchard_chain_state(
            client,
            keys,
            orchard_cfg,
            tracer,
            tree,
            next_position,
            nullifier_index,
            cursor,
            registry,
            treasury,
            pending_txs,
            0.0,
            phase,
            scan_start_height,
        )
        .await?;

        while pending_txs.contains_key(&txid) {
            let tip_before_wait = cursor
                .best_tip()
                .cloned()
                .unwrap_or(poll_best_tip(client).await?);
            cursor.record_best_tip(tip_before_wait.clone());
            let _ = wait_for_tip_change(client, &tip_before_wait).await?;
            sync_orchard_chain_state(
                client,
                keys,
                orchard_cfg,
                tracer,
                tree,
                next_position,
                nullifier_index,
                cursor,
                registry,
                treasury,
                pending_txs,
                0.0,
                phase,
                scan_start_height,
            )
            .await?;
        }

        refresh_and_trace_treasury(
            client,
            key,
            cursor.last_scanned_height(),
            orchard_cfg,
            expected_runtime_funding_txid,
            treasury,
            coinbase_cache,
            tracer,
            phase,
            registry,
            pending_txs,
            0.0,
        )
        .await?;
    }

    Ok(())
}

pub async fn run(
    client: &ZebraRpcClient,
    key: &FundedKey,
    rate: u64,
    _amount: f64,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    trace_config: &TxblastTraceConfig,
    expected_runtime_funding_txid: Option<&str>,
    wallet_birthday_height: Option<u32>,
) -> Result<()> {
    if rate == 0 {
        anyhow::bail!("--rate must be greater than 0");
    }
    if min_lane_value(orchard_cfg) <= super::orchard::ORCHARD_SPEND_FEE {
        anyhow::bail!(
            "orchard lane value {} zats is too small to sustain lane spends with {} zats fee",
            orchard_cfg.lane_premine.lane_value_zats,
            super::orchard::ORCHARD_SPEND_FEE,
        );
    }

    let secret_bytes: [u8; 32] = key.secret_key.secret_bytes();
    let keys = derive_orchard_keys(&secret_bytes)?;
    let tracer = OrchardTxblastTracer::from_config(trace_config, &key.name);
    println!(
        "[shielded] Orchard address derived from funded key '{}'",
        key.name,
    );

    let mut tree: OrchardTree =
        OrchardTree::new(shardtree::store::memory::MemoryShardStore::empty(), 100);
    let mut nullifier_index = OrchardNullifierIndex::default();
    let mut cursor = OrchardChainCursor::default();
    let mut registry = LaneRegistry::default();
    let mut treasury = TreasuryInventory::default();
    let mut pending_txs: HashMap<String, PendingTx> = HashMap::new();
    let mut next_position: u64 = 0;
    let mut coinbase_cache = HashMap::new();
    let scan_start_height = wallet_birthday_height.unwrap_or(0).max(1);

    let best_tip = poll_best_tip(client).await?;
    cursor.record_best_tip(best_tip.clone());
    transition_phase(
        &tracer,
        RuntimePhase::BootstrapScan,
        best_tip.height,
        "runtime_start",
        &registry,
        &treasury,
        &pending_txs,
    );
    tracer.trace_registry(
        Some(best_tip.height),
        RuntimePhase::BootstrapScan,
        "runtime_startup",
        &registry,
        &treasury,
        pending_counts(&pending_txs),
        pending_trace_summary(&pending_txs, best_tip.height),
        0.0,
        orchard_cfg,
        Some("starting_up"),
    );
    seed_orchard_tree_for_scan_start(client, &mut tree, &mut next_position, scan_start_height)
        .await?;
    if best_tip.height >= scan_start_height {
        println!(
            "[shielded] scanning blocks {}..{} for existing Orchard commitments",
            scan_start_height, best_tip.height
        );
        scan_block_range(
            client,
            &keys,
            &mut tree,
            &mut next_position,
            &mut nullifier_index,
            scan_start_height,
            best_tip.height,
            &mut pending_txs,
            &mut registry,
            &mut treasury,
            &mut cursor,
            &tracer,
            orchard_cfg,
            RuntimePhase::BootstrapScan,
            0.0,
        )
        .await?;
        println!(
            "[shielded] scanned {} blocks, tree has {} commitments",
            best_tip
                .height
                .saturating_sub(scan_start_height)
                .saturating_add(1),
            next_position,
        );
    } else {
        cursor.record_last_scanned(best_tip);
    }

    run_bootstrap(
        client,
        key,
        &keys,
        orchard_cfg,
        expected_runtime_funding_txid,
        &tracer,
        &mut tree,
        &mut next_position,
        &mut nullifier_index,
        &mut cursor,
        &mut registry,
        &mut treasury,
        &mut pending_txs,
        &mut coinbase_cache,
        scan_start_height,
    )
    .await?;

    if registry.spendable_note_count() == 0 {
        anyhow::bail!("no spendable Orchard notes after bootstrap. shielding may have failed.");
    }

    println!(
        "[shielded] steady state ready_lanes={}, reservoirs={}, treasury_backlog={}, target_ready_lanes={}, max_in_flight={}",
        registry.ready_lane_count(),
        registry.reservoir_count(),
        treasury.backlog_count(),
        orchard_cfg.target_ready_lanes,
        orchard_cfg.max_in_flight,
    );
    transition_phase(
        &tracer,
        RuntimePhase::SteadyState,
        cursor.last_scanned_height(),
        "phase_enter",
        &registry,
        &treasury,
        &pending_txs,
    );
    tracer.trace_registry(
        Some(cursor.last_scanned_height()),
        RuntimePhase::SteadyState,
        "steady_state_start",
        &registry,
        &treasury,
        pending_counts(&pending_txs),
        pending_trace_summary(&pending_txs, cursor.last_scanned_height()),
        0.0,
        orchard_cfg,
        None,
    );

    let mut tx_count: u64 = 0;
    let mut err_count: u64 = 0;
    let mut submit_credit = 0.0f64;
    let mut last_refill = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_treasury_refresh_height = None;

    loop {
        if let Err(error) = sync_orchard_chain_state(
            client,
            &keys,
            orchard_cfg,
            &tracer,
            &mut tree,
            &mut next_position,
            &mut nullifier_index,
            &mut cursor,
            &mut registry,
            &mut treasury,
            &mut pending_txs,
            submit_credit,
            RuntimePhase::SteadyState,
            scan_start_height,
        )
        .await
        {
            tracer.trace_event(super::orchard::tracing::EventContext {
                height: Some(cursor.last_scanned_height()),
                phase: RuntimePhase::SteadyState,
                event: "chain_sync_failed",
                tx_kind: None,
                txid: None,
                lane_id: None,
                note_id: None,
                note_role: None,
                note_value: None,
                pending: pending_counts(&pending_txs),
                registry: registry.snapshot(),
                treasury: treasury.snapshot(),
                reason: Some("sync_orchard_chain_state"),
                error: Some(error.to_string()),
                error_class: classify_error(&error.to_string()),
                build_duration_ms: None,
                rpc_submit_duration_ms: None,
                confirm_delay_ms: None,
                confirm_delay_blocks: None,
            });
            eprintln!(
                "[shielded][warn] Orchard chain sync failed at height {}: {error}",
                cursor.last_scanned_height()
            );
            tokio::time::sleep(LOOP_IDLE_SLEEP).await;
            continue;
        }

        let current = cursor.last_scanned_height();
        if last_treasury_refresh_height != Some(current) {
            if let Err(e) = refresh_and_trace_treasury(
                client,
                key,
                current,
                orchard_cfg,
                expected_runtime_funding_txid,
                &mut treasury,
                &mut coinbase_cache,
                &tracer,
                RuntimePhase::SteadyState,
                &registry,
                &pending_txs,
                submit_credit,
            )
            .await
            {
                tracer.trace_event(super::orchard::tracing::EventContext {
                    height: Some(current),
                    phase: RuntimePhase::SteadyState,
                    event: "treasury_refresh_failed",
                    tx_kind: None,
                    txid: None,
                    lane_id: None,
                    note_id: None,
                    note_role: None,
                    note_value: None,
                    pending: pending_counts(&pending_txs),
                    registry: registry.snapshot(),
                    treasury: treasury.snapshot(),
                    reason: Some("refresh_treasury_inventory"),
                    error: Some(e.to_string()),
                    error_class: classify_error(&e.to_string()),
                    build_duration_ms: None,
                    rpc_submit_duration_ms: None,
                    confirm_delay_ms: None,
                    confirm_delay_blocks: None,
                });
                eprintln!("[shielded][warn] treasury refresh failed at height {current}: {e}");
            } else {
                last_treasury_refresh_height = Some(current);
            }
        }

        let elapsed = last_refill.elapsed().as_secs_f64();
        last_refill = Instant::now();
        submit_credit =
            (submit_credit + elapsed * rate as f64).min(orchard_cfg.max_in_flight as f64);

        let mut submitted_any = false;

        // --- Parallel batch proving for lane advances ---
        //
        // Phase 1: Validate checkpoint, compute anchor, batch up to
        //          proving_workers work items with their merkle witnesses.
        // Phase 2: Prove all items in parallel via spawn_blocking.
        // Phase 3: Submit built txs serially, handle results.

        if submit_credit >= 1.0 && pending_txs.len() < orchard_cfg.max_in_flight {
            let Some(checkpoint) = cursor.latest_checkpoint().cloned() else {
                // No checkpoint — nothing to do this iteration.
                if !submitted_any {
                    tokio::time::sleep(LOOP_IDLE_SLEEP).await;
                }
                continue;
            };

            match client.get_block_hash(checkpoint.height).await {
                Ok(hash) if hash == checkpoint.hash => {}
                Ok(_) | Err(_) => {
                    rescan_orchard_state_from_chain(
                        client,
                        &keys,
                        orchard_cfg,
                        &tracer,
                        &mut tree,
                        &mut next_position,
                        &mut nullifier_index,
                        &mut cursor,
                        &mut registry,
                        &mut treasury,
                        &mut pending_txs,
                        submit_credit,
                        scan_start_height,
                        "rebuilding_after_checkpoint_mismatch",
                        None,
                    )
                    .await?;
                    continue;
                }
            }

            let anchor = latest_checkpoint_anchor(&tree, &checkpoint)?;
            let target_height = current.saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);

            // Determine how many items we can batch.
            let batch_capacity = std::cmp::min(
                orchard_cfg.proving_workers,
                std::cmp::min(
                    orchard_cfg.max_in_flight.saturating_sub(pending_txs.len()),
                    submit_credit as usize,
                ),
            );

            // Phase 1: Plan work items and compute witnesses.
            let mut lane_batch: Vec<(TrackedNote, orchard::tree::MerklePath)> = Vec::new();
            let mut batch_aborted = false;

            while lane_batch.len() < batch_capacity {
                let Some(work) =
                    plan_next_work(&mut registry, &mut treasury, &pending_txs, orchard_cfg)
                else {
                    break;
                };

                match work {
                    ScheduledWork::LaneAdvance(tracked) => {
                        if tracked.value() <= super::orchard::ORCHARD_SPEND_FEE {
                            registry.drain_note();
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_drained",
                                &tracked,
                                None,
                                Some("lane_below_spend_fee"),
                            );
                            continue;
                        }

                        tracer.trace_tracked_note(
                            Some(current),
                            "note_selected",
                            &tracked,
                            None,
                            Some("lane_advance"),
                        );

                        let merkle_path = match latest_witness(&tree, &tracked, &checkpoint) {
                            Ok(path) => path,
                            Err(e) => {
                                let error = e.to_string();
                                eprintln!("[shielded][warn] lane witness error: {error}");
                                tracer.trace_event(super::orchard::tracing::EventContext {
                                    height: Some(current),
                                    phase: RuntimePhase::SteadyState,
                                    event: "witness_error",
                                    tx_kind: Some(PendingTxKind::LaneAdvance),
                                    txid: None,
                                    lane_id: tracked.lane_id,
                                    note_id: Some(&tracked.note_id),
                                    note_role: Some(tracked.role),
                                    note_value: Some(tracked.value()),
                                    pending: pending_counts(&pending_txs),
                                    registry: registry.snapshot(),
                                    treasury: treasury.snapshot(),
                                    reason: Some("lane_advance"),
                                    error: Some(error.clone()),
                                    error_class: classify_error(&error),
                                    build_duration_ms: None,
                                    rpc_submit_duration_ms: None,
                                    confirm_delay_ms: None,
                                    confirm_delay_blocks: None,
                                });
                                if is_witness_rebuild_error(&error) {
                                    // Requeue already-batched items before rescan.
                                    for (batched, _) in lane_batch.drain(..) {
                                        registry.requeue(batched);
                                    }
                                    rescan_orchard_state_from_chain(
                                        client,
                                        &keys,
                                        orchard_cfg,
                                        &tracer,
                                        &mut tree,
                                        &mut next_position,
                                        &mut nullifier_index,
                                        &mut cursor,
                                        &mut registry,
                                        &mut treasury,
                                        &mut pending_txs,
                                        submit_credit,
                                        scan_start_height,
                                        "rebuilding_after_witness_error",
                                        Some(error),
                                    )
                                    .await?;
                                    batch_aborted = true;
                                } else {
                                    registry.requeue(tracked.clone());
                                    tracer.trace_tracked_note(
                                        Some(current),
                                        "note_requeued",
                                        &tracked,
                                        None,
                                        Some("witness_error"),
                                    );
                                }
                                break;
                            }
                        };

                        lane_batch.push((tracked, merkle_path));
                    }
                    // ReservoirExpand / TreasuryReseed are currently unreachable
                    // (plan_next_work only returns LaneAdvance). If they become
                    // reachable, they'll be handled serially in a future iteration
                    // after the batch is processed.
                    ScheduledWork::ReservoirExpand(tracked) => {
                        registry.requeue(tracked);
                        break;
                    }
                    ScheduledWork::TreasuryReseed(utxo) => {
                        treasury.requeue(utxo);
                        break;
                    }
                }
            }

            if !batch_aborted && !lane_batch.is_empty() {
                let batch_len = lane_batch.len();
                tracer.trace_registry(
                    Some(current),
                    RuntimePhase::SteadyState,
                    "build_start",
                    &registry,
                    &treasury,
                    pending_counts(&pending_txs),
                    pending_trace_summary(&pending_txs, current),
                    submit_credit,
                    orchard_cfg,
                    Some("proving_lane_advance_batch"),
                );

                // Phase 2: Prove in parallel via spawn_blocking.
                let mut tracked_notes: Vec<TrackedNote> = Vec::with_capacity(batch_len);
                let mut build_handles: Vec<tokio::task::JoinHandle<anyhow::Result<BuiltTx>>> =
                    Vec::with_capacity(batch_len);
                let network_params = orchard_cfg.network_params;

                for (tracked, merkle_path) in lane_batch {
                    let keys_clone = keys.clone();
                    let tracked_clone = tracked.clone();
                    build_handles.push(tokio::task::spawn_blocking(move || {
                        build_lane_advance_tx(
                            network_params,
                            &keys_clone,
                            &tracked_clone,
                            merkle_path,
                            anchor,
                            target_height,
                        )
                    }));
                    tracked_notes.push(tracked);
                }

                // Collect all build results (awaiting each handle in order).
                let mut built_results: Vec<(TrackedNote, Result<BuiltTx, String>)> =
                    Vec::with_capacity(batch_len);
                for (tracked, handle) in tracked_notes.into_iter().zip(build_handles) {
                    match handle.await {
                        Ok(Ok(built)) => built_results.push((tracked, Ok(built))),
                        Ok(Err(e)) => built_results.push((tracked, Err(e.to_string()))),
                        Err(join_err) => {
                            eprintln!("[shielded][warn] proving task panicked: {join_err}");
                            built_results.push((tracked, Err(join_err.to_string())));
                        }
                    }
                }

                // Phase 3: Submit built txs and handle results.
                let mut needs_rescan = false;
                let mut rescan_error: Option<String> = None;

                for (tracked, result) in built_results {
                    match result {
                        Ok(built) => {
                            if needs_rescan {
                                // Anchor is stale; skip submission, requeue.
                                registry.requeue(tracked);
                                continue;
                            }

                            let submit_start = Instant::now();
                            match client.send_raw_transaction(&built.tx_hex).await {
                                Ok(txid) => {
                                    let rpc_submit_duration_ms =
                                        duration_ms_u64(submit_start.elapsed().as_millis());
                                    let submitted_at = Instant::now();
                                    let recovered_notes = built
                                        .recovered_notes
                                        .into_iter()
                                        .map(|note| {
                                            note.with_origin(&txid, Some(tracked.note_id.clone()))
                                        })
                                        .collect();

                                    let pending = PendingTx {
                                        recovered_notes,
                                        kind: PendingTxKind::LaneAdvance,
                                        spent_note_id: Some(tracked.note_id.clone()),
                                        spent_lane_id: tracked.lane_id,
                                        spent_note_role: Some(tracked.role),
                                        spent_note_value: Some(tracked.value()),
                                        spent_transparent_outpoint: None,
                                        submitted_at,
                                        submitted_height: current,
                                        last_rpc_status: PendingRpcStatus::Unknown,
                                    };

                                    tracer.trace_event(super::orchard::tracing::EventContext {
                                        height: Some(current),
                                        phase: RuntimePhase::SteadyState,
                                        event: "tx_submitted",
                                        tx_kind: Some(PendingTxKind::LaneAdvance),
                                        txid: Some(&txid),
                                        lane_id: tracked.lane_id,
                                        note_id: Some(&tracked.note_id),
                                        note_role: Some(tracked.role),
                                        note_value: Some(tracked.value()),
                                        pending: {
                                            let mut counts = pending_counts(&pending_txs);
                                            counts.total += 1;
                                            counts
                                        },
                                        registry: registry.snapshot(),
                                        treasury: treasury.snapshot(),
                                        reason: None,
                                        error: None,
                                        error_class: None,
                                        build_duration_ms: Some(built.build_duration_ms),
                                        rpc_submit_duration_ms: Some(rpc_submit_duration_ms),
                                        confirm_delay_ms: None,
                                        confirm_delay_blocks: None,
                                    });
                                    tracer.trace_tracked_note(
                                        Some(current),
                                        "note_submitted",
                                        &tracked,
                                        Some(&txid),
                                        Some("lane_advance"),
                                    );
                                    for recovered in &pending.recovered_notes {
                                        tracer.trace_recovered_note(
                                            Some(current),
                                            "note_submitted",
                                            recovered,
                                            Some(&txid),
                                            Some("lane_advance_output"),
                                        );
                                    }
                                    pending_txs.insert(txid.clone(), pending);
                                    probe_submitted_tx_visibility(
                                        client,
                                        &tracer,
                                        &registry,
                                        &treasury,
                                        &mut pending_txs,
                                        current,
                                        RuntimePhase::SteadyState,
                                        &txid,
                                    )
                                    .await;
                                    tx_count += 1;
                                    submit_credit -= 1.0;
                                    submitted_any = true;
                                }
                                Err(e) => {
                                    err_count += 1;
                                    let error = e.to_string();
                                    eprintln!(
                                        "[shielded][warn] lane advance submit failed for lane {:?}: {error}",
                                        tracked.lane_id
                                    );
                                    tracer.trace_event(super::orchard::tracing::EventContext {
                                        height: Some(current),
                                        phase: RuntimePhase::SteadyState,
                                        event: "tx_submit_failed",
                                        tx_kind: Some(PendingTxKind::LaneAdvance),
                                        txid: None,
                                        lane_id: tracked.lane_id,
                                        note_id: Some(&tracked.note_id),
                                        note_role: Some(tracked.role),
                                        note_value: Some(tracked.value()),
                                        pending: pending_counts(&pending_txs),
                                        registry: registry.snapshot(),
                                        treasury: treasury.snapshot(),
                                        reason: Some("lane_advance"),
                                        error: Some(error.clone()),
                                        error_class: classify_error(&error),
                                        build_duration_ms: Some(built.build_duration_ms),
                                        rpc_submit_duration_ms: None,
                                        confirm_delay_ms: None,
                                        confirm_delay_blocks: None,
                                    });
                                    if is_unknown_orchard_anchor(&error) {
                                        needs_rescan = true;
                                        rescan_error = Some(error);
                                    }
                                    registry.requeue(tracked.clone());
                                    tracer.trace_tracked_note(
                                        Some(current),
                                        "note_requeued",
                                        &tracked,
                                        None,
                                        Some("submit_failed"),
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            if needs_rescan {
                                registry.requeue(tracked);
                                continue;
                            }
                            err_count += 1;
                            eprintln!(
                                "[shielded][warn] lane advance build failed for lane {:?}: {error}",
                                tracked.lane_id
                            );
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "tx_build_failed",
                                tx_kind: Some(PendingTxKind::LaneAdvance),
                                txid: None,
                                lane_id: tracked.lane_id,
                                note_id: Some(&tracked.note_id),
                                note_role: Some(tracked.role),
                                note_value: Some(tracked.value()),
                                pending: pending_counts(&pending_txs),
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: Some("lane_advance"),
                                error: Some(error.clone()),
                                error_class: classify_error(&error),
                                build_duration_ms: None,
                                rpc_submit_duration_ms: None,
                                confirm_delay_ms: None,
                                confirm_delay_blocks: None,
                            });
                            if is_unknown_orchard_anchor(&error) {
                                needs_rescan = true;
                                rescan_error = Some(error);
                            }
                            registry.requeue(tracked.clone());
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_requeued",
                                &tracked,
                                None,
                                Some("build_failed"),
                            );
                        }
                    }
                }

                if needs_rescan {
                    rescan_orchard_state_from_chain(
                        client,
                        &keys,
                        orchard_cfg,
                        &tracer,
                        &mut tree,
                        &mut next_position,
                        &mut nullifier_index,
                        &mut cursor,
                        &mut registry,
                        &mut treasury,
                        &mut pending_txs,
                        submit_credit,
                        scan_start_height,
                        "rebuilding_after_anchor_rejection",
                        rescan_error,
                    )
                    .await?;
                }
            }
        }

        if last_progress.elapsed() >= orchard_cfg.progress_interval {
            println!(
                "[shielded][progress] submitted={}, errors={}, ready_lanes={}, reservoirs={}, treasury_backlog={}, pending={}, pending_fanout={}, pending_reseed={}, drained={}",
                tx_count,
                err_count,
                registry.ready_lane_count(),
                registry.reservoir_count(),
                treasury.backlog_count(),
                pending_txs.len(),
                pending_counts(&pending_txs).expansion,
                pending_counts(&pending_txs).treasury_reseed,
                registry.drained_notes(),
            );
            tracer.trace_registry(
                Some(current),
                RuntimePhase::SteadyState,
                "progress",
                &registry,
                &treasury,
                pending_counts(&pending_txs),
                pending_trace_summary(&pending_txs, current),
                submit_credit,
                orchard_cfg,
                None,
            );
            last_progress = Instant::now();
        }

        if !submitted_any {
            tokio::time::sleep(LOOP_IDLE_SLEEP).await;
        }
    }
}
