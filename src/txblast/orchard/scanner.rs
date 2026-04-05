use std::collections::HashMap;

use anyhow::{Context, Result};
use ff::PrimeField;
use incrementalmerkletree::{Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use shardtree::ShardTree;
use shardtree::store::memory::MemoryShardStore;
use zebra_chain::serialization::ZcashDeserialize;

use crate::txblast::OrchardBlastRuntimeConfig;
use crate::txblast::rpc::ZebraRpcClient;

use super::{
    LaneRegistry, OrchardTxblastTracer, PendingTx, RuntimePhase, TrackedNote, TreasuryInventory,
    pending_counts,
};

pub(crate) type OrchardTree = ShardTree<MemoryShardStore<MerkleHashOrchard, u32>, 32, 16>;

pub(crate) async fn scan_block_range(
    client: &ZebraRpcClient,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    start_height: u32,
    end_height: u32,
    pending_txs: &mut HashMap<String, PendingTx>,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    latest_checkpoint: &mut Option<u32>,
    tracer: &OrchardTxblastTracer,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    phase: RuntimePhase,
    submit_credit: f64,
) -> Result<()> {
    if start_height > end_height {
        return Ok(());
    }

    for h in start_height..=end_height {
        let had_actions = scan_block(
            client,
            tree,
            next_position,
            h,
            pending_txs,
            registry,
            treasury,
            tracer,
            phase,
        )
        .await?;
        if had_actions {
            *latest_checkpoint = Some(h);
        }

        tracer.trace_registry(
            Some(h),
            phase,
            "block_scan",
            registry,
            treasury,
            pending_counts(pending_txs),
            submit_credit,
            orchard_cfg,
        );
    }

    Ok(())
}

pub(crate) async fn wait_for_block_advance(
    client: &ZebraRpcClient,
    after_height: u32,
) -> Result<u32> {
    loop {
        let current = client.get_block_count().await?;
        if current > after_height {
            return Ok(current);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub(crate) fn latest_checkpoint_anchor(
    tree: &OrchardTree,
    checkpoint_height: u32,
) -> Result<orchard::Anchor> {
    let root = tree
        .root_at_checkpoint_id(&checkpoint_height)
        .map_err(|e| anyhow::anyhow!("root_at_checkpoint_id: {e:?}"))?
        .ok_or_else(|| anyhow::anyhow!("no root at checkpoint {checkpoint_height}"))?;
    Ok(orchard::Anchor::from(root))
}

pub(crate) fn latest_witness(
    tree: &OrchardTree,
    tracked: &TrackedNote,
    checkpoint_height: u32,
) -> Result<orchard::tree::MerklePath> {
    let witness = tree
        .witness_at_checkpoint_id(tracked.position, &checkpoint_height)
        .map_err(|e| anyhow::anyhow!("witness_at_checkpoint_id: {e:?}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no witness for position {} at checkpoint {}",
                u64::from(tracked.position),
                checkpoint_height
            )
        })?;
    Ok(witness.into())
}

async fn scan_block(
    client: &ZebraRpcClient,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    height: u32,
    pending_txs: &mut HashMap<String, PendingTx>,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    tracer: &OrchardTxblastTracer,
    phase: RuntimePhase,
) -> Result<bool> {
    let block_bytes = client.getblock_raw(height).await?;
    let block = zebra_chain::block::Block::zcash_deserialize(&block_bytes[..])
        .with_context(|| format!("failed to deserialize block at height {height}"))?;

    struct CommitmentEntry {
        hash: MerkleHashOrchard,
        is_our_note: bool,
    }

    let mut entries = Vec::new();

    for tx in &block.transactions {
        let tx_hash = tx.hash().to_string();
        let Some(shielded_data) = tx.orchard_shielded_data() else {
            continue;
        };

        let is_our_tx = pending_txs.contains_key(&tx_hash);

        for (action_idx, action) in shielded_data.actions().enumerate() {
            let cmx_bytes = action.cm_x.to_repr();
            let hash = MerkleHashOrchard::from_bytes(&cmx_bytes)
                .expect("note commitment should be a valid MerkleHashOrchard");

            let is_our_note = is_our_tx
                && pending_txs
                    .get(&tx_hash)
                    .map(|pending| {
                        pending
                            .recovered_notes
                            .iter()
                            .any(|recovered| recovered.action_idx == action_idx)
                    })
                    .unwrap_or(false);

            entries.push(CommitmentEntry { hash, is_our_note });
        }

        if let Some(pending) = is_our_tx.then(|| pending_txs.remove(&tx_hash)).flatten() {
            if let Some(outpoint_id) = pending.spent_transparent_outpoint.as_deref() {
                treasury.confirm_spent(outpoint_id);
            }

            tracer.trace_event(super::tracing::EventContext {
                height: Some(height),
                phase,
                event: "tx_confirmed",
                tx_kind: Some(pending.kind),
                txid: Some(&tx_hash),
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
            let base_pos = *next_position + entries.len() as u64 - pending.num_actions as u64;
            for recovered in pending.recovered_notes {
                let position = Position::from(base_pos + recovered.action_idx as u64);
                let tracked = registry.activate_recovered_note(recovered, position, height);
                tracer.trace_tracked_note(Some(height), "note_activated", &tracked, None, None);
            }
        }
    }

    if entries.is_empty() {
        return Ok(false);
    }

    let last_idx = entries.len() - 1;
    for (i, entry) in entries.iter().enumerate() {
        let retention = match (entry.is_our_note, i == last_idx) {
            (true, true) => Retention::Checkpoint {
                id: height,
                marking: Marking::Marked,
            },
            (false, true) => Retention::Checkpoint {
                id: height,
                marking: Marking::None,
            },
            (true, false) => Retention::Marked,
            (false, false) => Retention::Ephemeral,
        };

        tree.append(entry.hash, retention)
            .map_err(|e| anyhow::anyhow!("tree append at height {height}: {e:?}"))?;
        *next_position += 1;
    }

    Ok(true)
}
