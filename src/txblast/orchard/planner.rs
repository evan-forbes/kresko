use std::collections::HashMap;

use crate::txblast::OrchardBlastRuntimeConfig;

use super::{
    LaneRegistry, PendingTx, PendingTxCounts, PendingTxKind, ScheduledWork, TreasuryInventory,
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

pub(crate) fn plan_next_work(
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    pending_txs: &HashMap<String, PendingTx>,
    cfg: &OrchardBlastRuntimeConfig,
) -> Option<ScheduledWork> {
    let pending = pending_counts(pending_txs);

    if registry.ready_lane_count() < cfg.lane_low_watermark
        && registry.reservoir_count() > 0
        && pending.expansion < cfg.fanout_max_in_flight
    {
        return registry
            .take_reservoir()
            .map(ScheduledWork::ReservoirExpand);
    }

    if treasury.backlog_count() > 0 && pending.treasury_reseed == 0 {
        return treasury
            .take_ready_utxo()
            .map(ScheduledWork::TreasuryReseed);
    }

    registry.take_ready_lane().map(ScheduledWork::LaneAdvance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OrchardTxblastConfig;
    use crate::txblast::orchard::{PendingTx, TreasuryUtxo};

    fn test_cfg() -> OrchardBlastRuntimeConfig {
        OrchardBlastRuntimeConfig::from_parts(
            OrchardTxblastConfig {
                lanes_per_miner: 8,
                lane_value_zats: 100_000,
                fanout_source_value_zats: 500_000,
                fanout_outputs: 4,
            },
            Some(16),
            Some(8),
            Some(4),
            Some(2),
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
    fn planner_prefers_treasury_when_no_reservoirs_exist() {
        let cfg = test_cfg();
        let mut registry = LaneRegistry::default();
        let mut treasury_inventory = TreasuryInventory::default();
        treasury_inventory.refresh_discovered(vec![treasury("a:0", 1_000_000)]);

        match plan_next_work(
            &mut registry,
            &mut treasury_inventory,
            &HashMap::new(),
            &cfg,
        ) {
            Some(ScheduledWork::TreasuryReseed(utxo)) => assert_eq!(utxo.outpoint_id, "a:0"),
            other => panic!("expected treasury reseed, got {other:?}"),
        }
    }

    #[test]
    fn pending_counts_tracks_reseed_and_expansion() {
        let pending = HashMap::from([
            (
                "a".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    num_actions: 1,
                    kind: PendingTxKind::ReservoirExpand,
                    spent_transparent_outpoint: None,
                },
            ),
            (
                "b".to_owned(),
                PendingTx {
                    recovered_notes: vec![],
                    num_actions: 0,
                    kind: PendingTxKind::TreasuryReseed,
                    spent_transparent_outpoint: Some("b:0".to_owned()),
                },
            ),
        ]);

        let counts = pending_counts(&pending);
        assert_eq!(counts.total, 2);
        assert_eq!(counts.expansion, 1);
        assert_eq!(counts.treasury_reseed, 1);
    }
}
