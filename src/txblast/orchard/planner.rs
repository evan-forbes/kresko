use std::collections::HashMap;

use crate::txblast::OrchardBlastRuntimeConfig;

use super::{
    LaneRegistry, PendingRpcStatus, PendingTraceSummary, PendingTx, PendingTxCounts, PendingTxKind,
    ScheduledWork, TreasuryInventory,
};

pub(crate) fn pending_counts(pending_txs: &HashMap<String, PendingTx>) -> PendingTxCounts {
    let mut counts = PendingTxCounts {
        total: pending_txs.len(),
        ..PendingTxCounts::default()
    };

    for pending in pending_txs.values() {
        match pending.kind {
            PendingTxKind::ReservoirExpand => counts.expansion += 1,
            PendingTxKind::TreasuryReseed => counts.treasury_reseed += 1,
            PendingTxKind::WarmupShielding | PendingTxKind::LaneAdvance => {}
        }
    }

    counts
}

pub(crate) fn pending_trace_summary(
    pending_txs: &HashMap<String, PendingTx>,
    current_height: u32,
) -> PendingTraceSummary {
    let oldest_pending_ms = pending_txs
        .values()
        .map(|pending| pending.submitted_at.elapsed().as_millis())
        .max()
        .map(duration_ms_u64);
    let oldest_pending_blocks = pending_txs
        .values()
        .map(|pending| current_height.saturating_sub(pending.submitted_height))
        .max();
    let rpc_pending_unknown = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::Unknown)
        .count();
    let oldest_unknown_pending_ms = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::Unknown)
        .map(|pending| pending.submitted_at.elapsed().as_millis())
        .max()
        .map(duration_ms_u64);
    let oldest_unknown_pending_blocks = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::Unknown)
        .map(|pending| current_height.saturating_sub(pending.submitted_height))
        .max();
    let rpc_pending_mempool = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::InMempool)
        .count();
    let rpc_pending_confirmed = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::ConfirmedByRpc)
        .count();
    let oldest_mempool_pending_ms = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::InMempool)
        .map(|pending| pending.submitted_at.elapsed().as_millis())
        .max()
        .map(duration_ms_u64);
    let oldest_mempool_pending_blocks = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::InMempool)
        .map(|pending| current_height.saturating_sub(pending.submitted_height))
        .max();
    let oldest_confirmed_rpc_pending_ms = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::ConfirmedByRpc)
        .map(|pending| pending.submitted_at.elapsed().as_millis())
        .max()
        .map(duration_ms_u64);
    let oldest_confirmed_rpc_pending_blocks = pending_txs
        .values()
        .filter(|pending| pending.last_rpc_status == PendingRpcStatus::ConfirmedByRpc)
        .map(|pending| current_height.saturating_sub(pending.submitted_height))
        .max();

    PendingTraceSummary {
        oldest_pending_ms,
        oldest_pending_blocks,
        rpc_pending_unknown,
        oldest_unknown_pending_ms,
        oldest_unknown_pending_blocks,
        rpc_pending_mempool,
        rpc_pending_confirmed,
        oldest_mempool_pending_ms,
        oldest_mempool_pending_blocks,
        oldest_confirmed_rpc_pending_ms,
        oldest_confirmed_rpc_pending_blocks,
    }
}

pub(crate) fn plan_next_work(
    registry: &mut LaneRegistry,
    _treasury: &mut TreasuryInventory,
    _pending_txs: &HashMap<String, PendingTx>,
    _cfg: &OrchardBlastRuntimeConfig,
) -> Option<ScheduledWork> {
    registry.take_ready_lane().map(ScheduledWork::LaneAdvance)
}

fn duration_ms_u64(duration_ms: u128) -> u64 {
    duration_ms.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OrchardTxblastConfig;
    use crate::txblast::orchard::{NoteRole, PendingTx, TreasuryUtxo};
    use std::time::{Duration, Instant};

    fn test_cfg() -> OrchardBlastRuntimeConfig {
        OrchardBlastRuntimeConfig::from_parts(
            OrchardTxblastConfig {
                lanes_per_miner: 8,
                lane_value_zats: 30_000,
                fanout_source_value_zats: 500_000,
                fanout_outputs: 4,
            },
            Some(16),
            Some(8),
            Some(4),
            Some(2),
            None,
            Some(5),
        )
        .expect("runtime config should be valid")
    }

    fn treasury(id: &str, satoshis: u64) -> TreasuryUtxo {
        TreasuryUtxo {
            outpoint_id: id.to_owned(),
            txid: "txid".to_owned(),
            output_index: 0,
            script: "51".to_owned(),
            satoshis,
            height: 1,
        }
    }

    #[test]
    fn planner_ignores_treasury_reseed_in_lane_only_mode() {
        let cfg = test_cfg();
        let mut registry = LaneRegistry::default();
        let mut treasury_inventory = TreasuryInventory::default();
        treasury_inventory.refresh_discovered(vec![treasury("a:0", 1_000_000)]);

        assert!(
            plan_next_work(
                &mut registry,
                &mut treasury_inventory,
                &HashMap::new(),
                &cfg,
            )
            .is_none()
        );
    }

    #[test]
    fn pending_counts_tracks_reseed_and_expansion() {
        let pending = HashMap::from([
            (
                "a".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    kind: PendingTxKind::ReservoirExpand,
                    spent_note_id: None,
                    spent_lane_id: None,
                    spent_note_role: None,
                    spent_note_value: None,
                    spent_transparent_outpoint: None,
                    submitted_at: Instant::now(),
                    submitted_height: 10,
                    last_rpc_status: PendingRpcStatus::Unknown,
                },
            ),
            (
                "b".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    kind: PendingTxKind::TreasuryReseed,
                    spent_note_id: None,
                    spent_lane_id: None,
                    spent_note_role: None,
                    spent_note_value: None,
                    spent_transparent_outpoint: Some("b:0".to_owned()),
                    submitted_at: Instant::now(),
                    submitted_height: 12,
                    last_rpc_status: PendingRpcStatus::Unknown,
                },
            ),
        ]);

        let counts = pending_counts(&pending);
        assert_eq!(counts.total, 2);
        assert_eq!(counts.expansion, 1);
        assert_eq!(counts.treasury_reseed, 1);
    }

    #[test]
    fn pending_trace_summary_tracks_oldest_pending_age_and_blocks() {
        let now = Instant::now();
        let pending = HashMap::from([
            (
                "a".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    kind: PendingTxKind::LaneAdvance,
                    spent_note_id: Some("a".to_owned()),
                    spent_lane_id: Some(7),
                    spent_note_role: Some(NoteRole::Lane),
                    spent_note_value: Some(30_000),
                    spent_transparent_outpoint: None,
                    submitted_at: now - Duration::from_millis(1500),
                    submitted_height: 10,
                    last_rpc_status: PendingRpcStatus::InMempool,
                },
            ),
            (
                "b".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    kind: PendingTxKind::LaneAdvance,
                    spent_note_id: Some("b".to_owned()),
                    spent_lane_id: Some(8),
                    spent_note_role: Some(NoteRole::Lane),
                    spent_note_value: Some(25_000),
                    spent_transparent_outpoint: None,
                    submitted_at: now - Duration::from_millis(500),
                    submitted_height: 14,
                    last_rpc_status: PendingRpcStatus::ConfirmedByRpc,
                },
            ),
        ]);

        let summary = pending_trace_summary(&pending, 18);
        assert_eq!(summary.oldest_pending_blocks, Some(8));
        assert!(summary.oldest_pending_ms.is_some_and(|value| value >= 1500));
        assert_eq!(summary.rpc_pending_unknown, 0);
        assert_eq!(summary.oldest_unknown_pending_blocks, None);
        assert_eq!(summary.rpc_pending_mempool, 1);
        assert_eq!(summary.rpc_pending_confirmed, 1);
        assert_eq!(summary.oldest_mempool_pending_blocks, Some(8));
        assert_eq!(summary.oldest_confirmed_rpc_pending_blocks, Some(4));
    }
}
