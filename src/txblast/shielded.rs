use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::orchard::{
    LaneRegistry, OrchardKeys, OrchardTree, OrchardTxblastTracer, PendingTx, PendingTxKind,
    RuntimePhase, ScheduledWork, TreasuryInventory, build_and_send_lane_advance_tx,
    build_and_send_reservoir_expand_tx, build_and_send_shielding_tx,
    build_and_send_treasury_reseed_tx, derive_orchard_keys, latest_checkpoint_anchor,
    latest_witness, min_lane_value, min_reservoir_value, min_treasury_reseed_value, pending_counts,
    plan_next_work, plan_shielding_outputs, refresh_treasury_inventory, scan_block_range,
    wait_for_block_advance,
};
use super::rpc::ZebraRpcClient;
use super::transparent::FundedKey;
use super::{OrchardBlastRuntimeConfig, TxblastTraceConfig};

const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LOOP_IDLE_SLEEP: Duration = Duration::from_millis(100);

fn is_unknown_orchard_anchor(error: &str) -> bool {
    error.contains("unknown Orchard anchor")
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

async fn refresh_and_trace_treasury(
    client: &ZebraRpcClient,
    key: &FundedKey,
    current_height: u32,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    treasury: &mut TreasuryInventory,
    coinbase_cache: &mut HashMap<String, bool>,
    tracer: &OrchardTxblastTracer,
    phase: RuntimePhase,
    registry: &LaneRegistry,
    pending_txs: &HashMap<String, PendingTx>,
    submit_credit: f64,
) -> Result<Option<u32>> {
    let refresh = refresh_treasury_inventory(
        client,
        &key.address.to_string(),
        current_height,
        min_treasury_reseed_value(orchard_cfg),
        treasury,
        coinbase_cache,
    )
    .await?;

    tracer.trace_registry(
        Some(current_height),
        phase,
        "treasury_refresh",
        registry,
        treasury,
        pending_counts(pending_txs),
        submit_credit,
        orchard_cfg,
    );

    Ok(refresh.earliest_maturity_height)
}

async fn run_bootstrap(
    client: &ZebraRpcClient,
    key: &FundedKey,
    keys: &OrchardKeys,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    tracer: &OrchardTxblastTracer,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    last_scanned_height: &mut u32,
    latest_checkpoint: &mut Option<u32>,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    pending_txs: &mut HashMap<String, PendingTx>,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<()> {
    let mut phase = RuntimePhase::BootstrapScan;
    transition_phase(
        tracer,
        phase,
        *last_scanned_height,
        "phase_enter",
        registry,
        treasury,
        pending_txs,
    );

    loop {
        let earliest_maturity = refresh_and_trace_treasury(
            client,
            key,
            *last_scanned_height,
            orchard_cfg,
            treasury,
            coinbase_cache,
            tracer,
            phase,
            registry,
            pending_txs,
            0.0,
        )
        .await?;

        if registry.ready_lane_count() > 0
            || registry.reservoir_count() > 0
            || treasury.backlog_count() > 0
        {
            break;
        }

        let Some(earliest_maturity) = earliest_maturity else {
            anyhow::bail!(
                "no transparent treasury UTXOs or Orchard notes found for {}. make sure local genesis seed blocks were loaded",
                key.address,
            );
        };

        println!(
            "[shielded] waiting for coinbase maturity (need height {earliest_maturity}, currently {})",
            *last_scanned_height
        );
        while *last_scanned_height < earliest_maturity {
            let current = client.get_block_count().await?;
            if current > *last_scanned_height {
                scan_block_range(
                    client,
                    tree,
                    next_position,
                    *last_scanned_height + 1,
                    current,
                    pending_txs,
                    registry,
                    treasury,
                    latest_checkpoint,
                    tracer,
                    orchard_cfg,
                    phase,
                    0.0,
                )
                .await?;
                *last_scanned_height = current;
                continue;
            }

            tokio::time::sleep(BLOCK_POLL_INTERVAL).await;
        }
    }

    if registry.ready_lane_count() >= orchard_cfg.target_ready_lanes
        || treasury.backlog_count() == 0
    {
        return Ok(());
    }

    phase = RuntimePhase::BootstrapShield;
    transition_phase(
        tracer,
        phase,
        *last_scanned_height,
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

    while registry.ready_lane_count() < orchard_cfg.target_ready_lanes
        && treasury.backlog_count() > 0
    {
        let utxo = treasury
            .take_ready_utxo()
            .expect("bootstrap loop should only run with treasury backlog");
        let remaining_lane_target = orchard_cfg
            .target_ready_lanes
            .saturating_sub(registry.ready_lane_count());
        let planned_outputs =
            plan_shielding_outputs(utxo.satoshis, remaining_lane_target, orchard_cfg)?;

        let anchor = fetch_orchard_anchor(client).await?;
        let target_height = client.get_block_count().await? + 10;
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
            height: Some(*last_scanned_height),
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
                Some(*last_scanned_height),
                "note_submitted",
                recovered,
                Some(&txid),
                Some("bootstrap_shield_output"),
            );
        }
        pending_txs.insert(txid.clone(), pending);

        let new_height = wait_for_block_advance(client, *last_scanned_height).await?;
        scan_block_range(
            client,
            tree,
            next_position,
            *last_scanned_height + 1,
            new_height,
            pending_txs,
            registry,
            treasury,
            latest_checkpoint,
            tracer,
            orchard_cfg,
            phase,
            0.0,
        )
        .await?;
        *last_scanned_height = new_height;

        while pending_txs.contains_key(&txid) {
            let h = wait_for_block_advance(client, *last_scanned_height).await?;
            scan_block_range(
                client,
                tree,
                next_position,
                *last_scanned_height + 1,
                h,
                pending_txs,
                registry,
                treasury,
                latest_checkpoint,
                tracer,
                orchard_cfg,
                phase,
                0.0,
            )
            .await?;
            *last_scanned_height = h;
        }

        refresh_and_trace_treasury(
            client,
            key,
            *last_scanned_height,
            orchard_cfg,
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
    let mut registry = LaneRegistry::default();
    let mut treasury = TreasuryInventory::default();
    let mut pending_txs: HashMap<String, PendingTx> = HashMap::new();
    let mut next_position: u64 = 0;
    let mut latest_checkpoint: Option<u32> = None;
    let mut coinbase_cache = HashMap::new();

    let current_height = client.get_block_count().await?;
    let anchor = fetch_orchard_anchor(client).await?;
    transition_phase(
        &tracer,
        RuntimePhase::BootstrapScan,
        current_height,
        "runtime_start",
        &registry,
        &treasury,
        &pending_txs,
    );
    if anchor != orchard::Anchor::empty_tree() && current_height > 0 {
        println!("[shielded] scanning blocks 1..{current_height} for existing Orchard commitments");
        scan_block_range(
            client,
            &mut tree,
            &mut next_position,
            1,
            current_height,
            &mut pending_txs,
            &mut registry,
            &mut treasury,
            &mut latest_checkpoint,
            &tracer,
            orchard_cfg,
            RuntimePhase::BootstrapScan,
            0.0,
        )
        .await?;
        println!(
            "[shielded] scanned {} blocks, tree has {} commitments",
            current_height, next_position,
        );
    }

    let mut last_scanned_height = current_height;
    run_bootstrap(
        client,
        key,
        &keys,
        orchard_cfg,
        &tracer,
        &mut tree,
        &mut next_position,
        &mut last_scanned_height,
        &mut latest_checkpoint,
        &mut registry,
        &mut treasury,
        &mut pending_txs,
        &mut coinbase_cache,
    )
    .await?;

    if registry.ready_lane_count() == 0 && registry.reservoir_count() == 0 {
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
        last_scanned_height,
        "phase_enter",
        &registry,
        &treasury,
        &pending_txs,
    );
    tracer.trace_registry(
        Some(last_scanned_height),
        RuntimePhase::SteadyState,
        "steady_state_start",
        &registry,
        &treasury,
        pending_counts(&pending_txs),
        0.0,
        orchard_cfg,
    );

    let mut tx_count: u64 = 0;
    let mut err_count: u64 = 0;
    let mut submit_credit = 0.0f64;
    let mut last_refill = Instant::now();
    let mut last_progress = Instant::now();

    loop {
        let current = client
            .get_block_count()
            .await
            .unwrap_or(last_scanned_height);
        if current > last_scanned_height {
            if let Err(e) = scan_block_range(
                client,
                &mut tree,
                &mut next_position,
                last_scanned_height + 1,
                current,
                &mut pending_txs,
                &mut registry,
                &mut treasury,
                &mut latest_checkpoint,
                &tracer,
                orchard_cfg,
                RuntimePhase::SteadyState,
                submit_credit,
            )
            .await
            {
                eprintln!(
                    "[shielded][warn] block scan at {}..{} failed: {e}",
                    last_scanned_height + 1,
                    current,
                );
            }
            last_scanned_height = current;

            if let Err(e) = refresh_and_trace_treasury(
                client,
                key,
                current,
                orchard_cfg,
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
            }
        }

        let elapsed = last_refill.elapsed().as_secs_f64();
        last_refill = Instant::now();
        submit_credit =
            (submit_credit + elapsed * rate as f64).min(orchard_cfg.max_in_flight as f64);

        let mut submitted_any = false;
        while submit_credit >= 1.0 && pending_txs.len() < orchard_cfg.max_in_flight {
            let Some(checkpoint_height) = latest_checkpoint else {
                break;
            };
            let anchor = latest_checkpoint_anchor(&tree, checkpoint_height)?;
            let target_height = current + 10;
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

                    let merkle_path = match latest_witness(&tree, &tracked, checkpoint_height) {
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
                            registry.requeue(tracked.clone());
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_requeued",
                                &tracked,
                                None,
                                Some("witness_error"),
                            );
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
                                eprintln!(
                                    "[shielded][warn] dropping reservoir note {} after anchor mismatch",
                                    tracked.note_id
                                );
                                registry.drain_note();
                                tracer.trace_tracked_note(
                                    Some(current),
                                    "note_drained",
                                    &tracked,
                                    None,
                                    Some("unknown_orchard_anchor"),
                                );
                                continue;
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

                    let merkle_path = match latest_witness(&tree, &tracked, checkpoint_height) {
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
                                error: Some(error),
                            });
                            registry.requeue(tracked.clone());
                            tracer.trace_tracked_note(
                                Some(current),
                                "note_requeued",
                                &tracked,
                                None,
                                Some("witness_error"),
                            );
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
                                error: Some(error),
                            });
                            if is_unknown_orchard_anchor(&error) {
                                eprintln!(
                                    "[shielded][warn] dropping lane {:?} note {} after anchor mismatch",
                                    tracked.lane_id,
                                    tracked.note_id
                                );
                                registry.drain_note();
                                tracer.trace_tracked_note(
                                    Some(current),
                                    "note_drained",
                                    &tracked,
                                    None,
                                    Some("unknown_orchard_anchor"),
                                );
                                continue;
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
            );
            last_progress = Instant::now();
        }

        if !submitted_any {
            tokio::time::sleep(LOOP_IDLE_SLEEP).await;
        }
    }
}
