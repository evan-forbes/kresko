use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::orchard::{
    LaneRegistry, OrchardChainCursor, OrchardKeys, OrchardNullifierIndex, OrchardTree,
    OrchardTxblastTracer, PendingTx, PendingTxKind, RuntimePhase, ScheduledWork, TreasuryInventory,
    build_and_send_lane_advance_tx, build_and_send_reservoir_expand_tx,
    build_and_send_shielding_tx, build_and_send_treasury_reseed_tx, derive_orchard_keys,
    detect_reorg_reason, latest_checkpoint_anchor, latest_witness, min_bootstrap_shield_value,
    min_lane_value, min_reservoir_value, pending_counts, plan_next_work, plan_shielding_outputs,
    poll_best_tip, refresh_treasury_inventory, scan_block_range, wait_for_tip_change,
};
use super::rpc::ZebraRpcClient;
use super::transparent::FundedKey;
use super::{OrchardBlastRuntimeConfig, TxblastTraceConfig};

const LOOP_IDLE_SLEEP: Duration = Duration::from_millis(100);
const TARGET_HEIGHT_OFFSET_BLOCKS: u32 = 100;

fn is_unknown_orchard_anchor(error: &str) -> bool {
    error.contains("unknown Orchard anchor")
}

fn is_witness_rebuild_error(error: &str) -> bool {
    error.contains("witness_at_checkpoint_id") || error.contains("no witness for position")
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
    });
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
        error,
    });
    tracer.trace_registry(
        Some(rebuild_height),
        RuntimePhase::Recovering,
        "rebuild_start",
        registry,
        treasury,
        pending_counts(pending_txs),
        submit_credit,
        orchard_cfg,
        Some(reason),
    );

    *tree = OrchardTree::new(shardtree::store::memory::MemoryShardStore::empty(), 100);
    *next_position = 0;
    nullifier_index.clear();
    registry.reset_for_rebuild();
    cursor.reset_for_rebuild();

    let best_tip = poll_best_tip(client).await?;
    cursor.record_best_tip(best_tip.clone());
    if best_tip.height > 0 {
        scan_block_range(
            client,
            keys,
            tree,
            next_position,
            nullifier_index,
            1,
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
) -> Result<Option<&'static str>> {
    let txids = pending_txs.keys().cloned().collect::<Vec<_>>();
    let mut evicted_any = false;

    for txid in txids {
        if client
            .try_get_raw_transaction_verbose(&txid)
            .await?
            .is_some()
        {
            continue;
        }

        let Some(pending) = pending_txs.remove(&txid) else {
            continue;
        };
        evicted_any = true;
        tracer.trace_event(super::orchard::tracing::EventContext {
            height: Some(height),
            phase: RuntimePhase::Recovering,
            event: "pending_tx_evicted",
            tx_kind: Some(pending.kind),
            txid: Some(&txid),
            lane_id: None,
            note_id: pending.spent_note_id.as_deref(),
            note_role: None,
            note_value: None,
            pending: pending_counts(pending_txs),
            registry: registry.snapshot(),
            treasury: treasury.snapshot(),
            reason: Some("rebuilding_after_pending_eviction"),
            error: None,
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
            reason,
            None,
        )
        .await;
    }

    let start_height = cursor.last_scanned_height().saturating_add(1);
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
        let (txid, pending) = build_and_send_shielding_tx(
            client,
            key,
            keys,
            &utxo.txid,
            utxo.output_index,
            &utxo.script,
            utxo.satoshis,
            &planned_outputs,
            anchor,
            target_height,
            PendingTxKind::WarmupShielding,
        )
        .await?;
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
        0.0,
        orchard_cfg,
        Some("starting_up"),
    );
    if best_tip.height > 0 {
        println!(
            "[shielded] scanning blocks 1..{} for existing Orchard commitments",
            best_tip.height
        );
        scan_block_range(
            client,
            &keys,
            &mut tree,
            &mut next_position,
            &mut nullifier_index,
            1,
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
            best_tip.height, next_position,
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
        )
        .await
        {
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
        while submit_credit >= 1.0 && pending_txs.len() < orchard_cfg.max_in_flight {
            let Some(checkpoint) = cursor.latest_checkpoint().cloned() else {
                break;
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
                        "rebuilding_after_checkpoint_mismatch",
                        None,
                    )
                    .await?;
                    break;
                }
            }

            let anchor = latest_checkpoint_anchor(&tree, &checkpoint)?;
            let target_height = current.saturating_add(TARGET_HEIGHT_OFFSET_BLOCKS);
            let Some(work) =
                plan_next_work(&mut registry, &mut treasury, &pending_txs, orchard_cfg)
            else {
                break;
            };

            match work {
                ScheduledWork::ReservoirExpand(tracked) => {
                    if tracked.value() < min_reservoir_value(orchard_cfg) {
                        if tracked.value() > super::orchard::ORCHARD_SPEND_FEE {
                            let promoted = registry.promote_reservoir_to_lane(tracked);
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_promoted_to_lane",
                                &promoted,
                                None,
                                Some("reservoir_below_expansion_floor"),
                            );
                        } else {
                            registry.drain_note();
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_drained",
                                &tracked,
                                None,
                                Some("reservoir_below_spend_fee"),
                            );
                        }
                        continue;
                    }

                    tracer.trace_tracked_note(
                        Some(current),
                        "note_selected",
                        &tracked,
                        None,
                        Some("reservoir_expand"),
                    );
                    tracer.trace_registry(
                        Some(current),
                        RuntimePhase::SteadyState,
                        "build_start",
                        &registry,
                        &treasury,
                        pending_counts(&pending_txs),
                        submit_credit,
                        orchard_cfg,
                        Some("proving_reservoir_expand"),
                    );

                    let merkle_path = match latest_witness(&tree, &tracked, &checkpoint) {
                        Ok(path) => path,
                        Err(e) => {
                            let error = e.to_string();
                            eprintln!("[shielded][warn] reservoir witness error: {error}");
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "witness_error",
                                tx_kind: Some(PendingTxKind::ReservoirExpand),
                                txid: None,
                                lane_id: tracked.lane_id,
                                note_id: Some(&tracked.note_id),
                                note_role: Some(tracked.role),
                                note_value: Some(tracked.value()),
                                pending: pending_counts(&pending_txs),
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: Some("reservoir_expand"),
                                error: Some(error.clone()),
                            });
                            if is_witness_rebuild_error(&error) {
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
                                    "rebuilding_after_witness_error",
                                    Some(error),
                                )
                                .await?;
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

                    match build_and_send_reservoir_expand_tx(
                        client,
                        &keys,
                        &tracked,
                        merkle_path,
                        anchor,
                        target_height,
                        orchard_cfg,
                    )
                    .await
                    {
                        Ok((txid, pending)) => {
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "tx_submitted",
                                tx_kind: Some(PendingTxKind::ReservoirExpand),
                                txid: Some(&txid),
                                lane_id: tracked.lane_id,
                                note_id: Some(&tracked.note_id),
                                note_role: Some(tracked.role),
                                note_value: Some(tracked.value()),
                                pending: {
                                    let mut counts = pending_counts(&pending_txs);
                                    counts.total += 1;
                                    counts.expansion += 1;
                                    counts
                                },
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: None,
                                error: None,
                            });
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_submitted",
                                &tracked,
                                Some(&txid),
                                Some("reservoir_expand"),
                            );
                            for recovered in &pending.recovered_notes {
                                tracer.trace_recovered_note(
                                    Some(current),
                                    "note_submitted",
                                    recovered,
                                    Some(&txid),
                                    Some("reservoir_expand_output"),
                                );
                            }
                            pending_txs.insert(txid.clone(), pending);
                            tx_count += 1;
                            submit_credit -= 1.0;
                            submitted_any = true;
                        }
                        Err(e) => {
                            err_count += 1;
                            let error = e.to_string();
                            eprintln!("[shielded][warn] reservoir expansion failed: {error}");
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "tx_submit_failed",
                                tx_kind: Some(PendingTxKind::ReservoirExpand),
                                txid: None,
                                lane_id: tracked.lane_id,
                                note_id: Some(&tracked.note_id),
                                note_role: Some(tracked.role),
                                note_value: Some(tracked.value()),
                                pending: pending_counts(&pending_txs),
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: Some("reservoir_expand"),
                                error: Some(error.clone()),
                            });
                            if is_unknown_orchard_anchor(&error) {
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
                                    "rebuilding_after_anchor_rejection",
                                    Some(error),
                                )
                                .await?;
                                break;
                            }
                            registry.requeue(tracked.clone());
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_requeued",
                                &tracked,
                                None,
                                Some("submit_failed"),
                            );
                            break;
                        }
                    }
                }
                ScheduledWork::TreasuryReseed(utxo) => {
                    tracer.trace_event(super::orchard::tracing::EventContext {
                        height: Some(current),
                        phase: RuntimePhase::SteadyState,
                        event: "treasury_selected",
                        tx_kind: Some(PendingTxKind::TreasuryReseed),
                        txid: None,
                        lane_id: None,
                        note_id: Some(&utxo.outpoint_id),
                        note_role: None,
                        note_value: Some(utxo.satoshis),
                        pending: pending_counts(&pending_txs),
                        registry: registry.snapshot(),
                        treasury: treasury.snapshot(),
                        reason: Some("treasury_reseed"),
                        error: None,
                    });
                    tracer.trace_registry(
                        Some(current),
                        RuntimePhase::SteadyState,
                        "build_start",
                        &registry,
                        &treasury,
                        pending_counts(&pending_txs),
                        submit_credit,
                        orchard_cfg,
                        Some("proving_treasury_reseed"),
                    );

                    match build_and_send_treasury_reseed_tx(
                        client,
                        key,
                        &keys,
                        &utxo,
                        anchor,
                        target_height,
                        orchard_cfg,
                    )
                    .await
                    {
                        Ok((txid, pending)) => {
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "tx_submitted",
                                tx_kind: Some(PendingTxKind::TreasuryReseed),
                                txid: Some(&txid),
                                lane_id: None,
                                note_id: Some(&utxo.outpoint_id),
                                note_role: None,
                                note_value: Some(utxo.satoshis),
                                pending: {
                                    let mut counts = pending_counts(&pending_txs);
                                    counts.total += 1;
                                    counts.treasury_reseed += 1;
                                    counts
                                },
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: None,
                                error: None,
                            });
                            for recovered in &pending.recovered_notes {
                                tracer.trace_recovered_note(
                                    Some(current),
                                    "note_submitted",
                                    recovered,
                                    Some(&txid),
                                    Some("treasury_reseed_output"),
                                );
                            }
                            pending_txs.insert(txid.clone(), pending);
                            tx_count += 1;
                            submit_credit -= 1.0;
                            submitted_any = true;
                        }
                        Err(e) => {
                            err_count += 1;
                            let error = e.to_string();
                            eprintln!("[shielded][warn] treasury reseed failed: {error}");
                            tracer.trace_event(super::orchard::tracing::EventContext {
                                height: Some(current),
                                phase: RuntimePhase::SteadyState,
                                event: "tx_submit_failed",
                                tx_kind: Some(PendingTxKind::TreasuryReseed),
                                txid: None,
                                lane_id: None,
                                note_id: Some(&utxo.outpoint_id),
                                note_role: None,
                                note_value: Some(utxo.satoshis),
                                pending: pending_counts(&pending_txs),
                                registry: registry.snapshot(),
                                treasury: treasury.snapshot(),
                                reason: Some("treasury_reseed"),
                                error: Some(error.clone()),
                            });
                            if is_unknown_orchard_anchor(&error) {
                                treasury.requeue(utxo);
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
                                    "rebuilding_after_anchor_rejection",
                                    Some(error),
                                )
                                .await?;
                                break;
                            }
                            treasury.requeue(utxo);
                            break;
                        }
                    }
                }
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
                    tracer.trace_registry(
                        Some(current),
                        RuntimePhase::SteadyState,
                        "build_start",
                        &registry,
                        &treasury,
                        pending_counts(&pending_txs),
                        submit_credit,
                        orchard_cfg,
                        Some("proving_lane_advance"),
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
                            });
                            if is_witness_rebuild_error(&error) {
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
                                    "rebuilding_after_witness_error",
                                    Some(error),
                                )
                                .await?;
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

                    match build_and_send_lane_advance_tx(
                        client,
                        &keys,
                        &tracked,
                        merkle_path,
                        anchor,
                        target_height,
                    )
                    .await
                    {
                        Ok((txid, pending)) => {
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
                            tx_count += 1;
                            submit_credit -= 1.0;
                            submitted_any = true;
                        }
                        Err(e) => {
                            err_count += 1;
                            let error = e.to_string();
                            eprintln!(
                                "[shielded][warn] lane advance failed for lane {:?}: {error}",
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
                            });
                            if is_unknown_orchard_anchor(&error) {
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
                                    "rebuilding_after_anchor_rejection",
                                    Some(error),
                                )
                                .await?;
                                break;
                            }
                            registry.requeue(tracked.clone());
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_requeued",
                                &tracked,
                                None,
                                Some("submit_failed"),
                            );
                            break;
                        }
                    }
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
