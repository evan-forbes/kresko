use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use ff::PrimeField;
use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::{Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use shardtree::ShardTree;
use shardtree::store::memory::MemoryShardStore;
use zcash_primitives::merkle_tree::{read_frontier_v0, read_frontier_v1};
use zcash_protocol::consensus;
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};

use crate::txblast::OrchardBlastRuntimeConfig;
use crate::txblast::rpc::ZebraRpcClient;

use super::{
    LaneRegistry, OrchardKeys, OrchardTxblastTracer, PendingTx, RuntimePhase, TrackedNote,
    TreasuryInventory, decode_txblast_note_role, pending_counts, pending_trace_summary,
};

pub(crate) type OrchardTree = ShardTree<MemoryShardStore<MerkleHashOrchard, u32>, 32, 16>;
pub(crate) type OrchardNullifierIndex = HashMap<[u8; 32], String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockRef {
    pub(crate) height: u32,
    pub(crate) hash: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OrchardChainCursor {
    best_tip: Option<BlockRef>,
    last_scanned: Option<BlockRef>,
    latest_checkpoint: Option<BlockRef>,
}

impl OrchardChainCursor {
    pub(crate) fn best_tip(&self) -> Option<&BlockRef> {
        self.best_tip.as_ref()
    }

    pub(crate) fn last_scanned(&self) -> Option<&BlockRef> {
        self.last_scanned.as_ref()
    }

    pub(crate) fn latest_checkpoint(&self) -> Option<&BlockRef> {
        self.latest_checkpoint.as_ref()
    }

    pub(crate) fn last_scanned_height(&self) -> u32 {
        self.last_scanned
            .as_ref()
            .map(|block| block.height)
            .unwrap_or(0)
    }

    pub(crate) fn record_best_tip(&mut self, tip: BlockRef) {
        self.best_tip = Some(tip);
    }

    pub(crate) fn record_last_scanned(&mut self, block: BlockRef) {
        self.last_scanned = Some(block);
    }

    pub(crate) fn record_checkpoint(&mut self, block: BlockRef) {
        self.latest_checkpoint = Some(block);
    }

    pub(crate) fn reset_for_rebuild(&mut self) {
        self.last_scanned = None;
        self.latest_checkpoint = None;
    }
}

pub(crate) async fn poll_best_tip(client: &ZebraRpcClient) -> Result<BlockRef> {
    let height = client.get_block_count().await?;
    let hash = client.get_best_block_hash().await?;
    Ok(BlockRef { height, hash })
}

pub(crate) async fn wait_for_tip_change(
    client: &ZebraRpcClient,
    previous: &BlockRef,
) -> Result<BlockRef> {
    loop {
        let current = poll_best_tip(client).await?;
        if current.height != previous.height || current.hash != previous.hash {
            return Ok(current);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub(crate) async fn detect_reorg_reason(
    client: &ZebraRpcClient,
    cursor: &OrchardChainCursor,
) -> Result<Option<&'static str>> {
    if let Some(last_scanned) = cursor.last_scanned() {
        match client.get_block_hash(last_scanned.height).await {
            Ok(hash) if hash == last_scanned.hash => {}
            Ok(_) | Err(_) => return Ok(Some("rebuilding_after_reorg_detection")),
        }
    }

    if let Some(checkpoint) = cursor.latest_checkpoint() {
        match client.get_block_hash(checkpoint.height).await {
            Ok(hash) if hash == checkpoint.hash => {}
            Ok(_) | Err(_) => return Ok(Some("rebuilding_after_checkpoint_mismatch")),
        }
    }

    Ok(None)
}

pub(crate) async fn scan_block_range(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    nullifier_index: &mut OrchardNullifierIndex,
    start_height: u32,
    end_height: u32,
    pending_txs: &mut HashMap<String, PendingTx>,
    registry: &mut LaneRegistry,
    treasury: &mut TreasuryInventory,
    cursor: &mut OrchardChainCursor,
    tracer: &OrchardTxblastTracer,
    orchard_cfg: &OrchardBlastRuntimeConfig,
    phase: RuntimePhase,
    submit_credit: f64,
) -> Result<()> {
    if start_height > end_height {
        return Ok(());
    }

    let reserved_note_ids = pending_spent_note_ids(pending_txs);

    for height in start_height..=end_height {
        let block_hash = client.get_block_hash(height).await?;
        let had_actions = scan_block(
            client,
            keys,
            tree,
            next_position,
            nullifier_index,
            &reserved_note_ids,
            height,
            pending_txs,
            registry,
            treasury,
            tracer,
            phase,
        )
        .await?;

        let block_ref = BlockRef {
            height,
            hash: block_hash,
        };
        cursor.record_last_scanned(block_ref.clone());
        if had_actions {
            cursor.record_checkpoint(block_ref);
        }

        tracer.trace_registry(
            Some(height),
            phase,
            "block_scan",
            registry,
            treasury,
            pending_counts(pending_txs),
            pending_trace_summary(pending_txs, height),
            submit_credit,
            orchard_cfg,
            None,
        );
    }

    Ok(())
}

pub(crate) async fn seed_orchard_tree_from_treestate(
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

pub(crate) fn latest_checkpoint_anchor(
    tree: &OrchardTree,
    checkpoint: &BlockRef,
) -> Result<orchard::Anchor> {
    let root = tree
        .root_at_checkpoint_id(&checkpoint.height)
        .map_err(|e| anyhow::anyhow!("root_at_checkpoint_id: {e:?}"))?
        .ok_or_else(|| anyhow::anyhow!("no root at checkpoint {}", checkpoint.height))?;
    Ok(orchard::Anchor::from(root))
}

pub(crate) fn latest_witness(
    tree: &OrchardTree,
    tracked: &TrackedNote,
    checkpoint: &BlockRef,
) -> Result<orchard::tree::MerklePath> {
    let witness = tree
        .witness_at_checkpoint_id(tracked.position, &checkpoint.height)
        .map_err(|e| anyhow::anyhow!("witness_at_checkpoint_id: {e:?}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no witness for position {} at checkpoint {}",
                u64::from(tracked.position),
                checkpoint.height
            )
        })?;
    Ok(witness.into())
}

fn pending_spent_note_ids(pending_txs: &HashMap<String, PendingTx>) -> HashSet<String> {
    pending_txs
        .values()
        .filter_map(|pending| pending.spent_note_id.clone())
        .collect()
}

async fn scan_block(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    nullifier_index: &mut OrchardNullifierIndex,
    reserved_note_ids: &HashSet<String>,
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
    let external_ivk = keys.external_ivk();

    struct CommitmentEntry {
        hash: MerkleHashOrchard,
        recovered: Option<super::RecoveredNote>,
        note_nullifier: Option<[u8; 32]>,
    }

    let mut entries = Vec::new();

    for tx in &block.transactions {
        let tx_hash = tx.hash().to_string();
        let Some(shielded_data) = tx.orchard_shielded_data() else {
            continue;
        };
        let nu = tx.network_upgrade().unwrap_or(NetworkUpgrade::Nu5);
        let branch_id = nu
            .branch_id()
            .ok_or_else(|| anyhow::anyhow!("missing branch id for orchard transaction {tx_hash}"))
            .and_then(|branch_id| {
                consensus::BranchId::try_from(branch_id).map_err(|_| {
                    anyhow::anyhow!("invalid branch id for orchard transaction {tx_hash}")
                })
            })?;
        let tx_bytes = tx
            .zcash_serialize_to_vec()
            .with_context(|| format!("failed to serialize orchard transaction {tx_hash}"))?;
        let lib_tx = zcash_primitives::transaction::Transaction::read(&tx_bytes[..], branch_id)
            .with_context(|| format!("failed to convert orchard transaction {tx_hash}"))?;
        let bundle = lib_tx.orchard_bundle().ok_or_else(|| {
            anyhow::anyhow!("missing orchard bundle after conversion for {tx_hash}")
        })?;

        for nullifier in shielded_data.nullifiers() {
            let nullifier: [u8; 32] = (*nullifier).into();
            if let Some(note_id) = nullifier_index.remove(&nullifier) {
                if let Some(spent) = registry.remove_note(&note_id) {
                    tracer.trace_tracked_note(
                        Some(height),
                        "note_spent_confirmed",
                        &spent,
                        Some(&tx_hash),
                        None,
                    );
                }
            }
        }

        let pending = pending_txs.remove(&tx_hash);
        let mut pending_notes_by_action = pending
            .as_ref()
            .map(|pending| {
                pending
                    .recovered_notes
                    .iter()
                    .cloned()
                    .map(|note| (note.action_idx, note))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let mut recovered_by_action = HashMap::new();
        for (action_idx, _, note, _, memo) in
            bundle.decrypt_outputs_with_keys(&[external_ivk.clone()])
        {
            let recovered =
                if let Some(mut pending_note) = pending_notes_by_action.remove(&action_idx) {
                    pending_note.note = note;
                    pending_note
                } else if let Some(role) = decode_txblast_note_role(&memo) {
                    super::RecoveredNote {
                        note_id: format!("{tx_hash}:{action_idx}"),
                        parent_note_id: None,
                        origin_txid: tx_hash.clone(),
                        action_idx,
                        note,
                        role,
                    }
                } else {
                    continue;
                };
            let note_nullifier = keys.note_nullifier_bytes(&recovered.note);
            recovered_by_action.insert(action_idx, (recovered, note_nullifier));
        }

        for (action_idx, cm_x) in shielded_data.note_commitments().enumerate() {
            let cmx_bytes = cm_x.to_repr();
            let hash = MerkleHashOrchard::from_bytes(&cmx_bytes)
                .expect("note commitment should be a valid MerkleHashOrchard");
            let (recovered, note_nullifier) = recovered_by_action
                .remove(&action_idx)
                .map(|(recovered, note_nullifier)| (Some(recovered), Some(note_nullifier)))
                .or_else(|| {
                    pending_notes_by_action
                        .remove(&action_idx)
                        .map(|recovered| {
                            let note_nullifier = keys.note_nullifier_bytes(&recovered.note);
                            (Some(recovered), Some(note_nullifier))
                        })
                })
                .unwrap_or((None, None));

            entries.push(CommitmentEntry {
                hash,
                recovered,
                note_nullifier,
            });
        }

        if let Some(pending) = pending {
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
                error_class: None,
                build_duration_ms: None,
                rpc_submit_duration_ms: None,
                confirm_delay_ms: Some(duration_ms_u64(pending.submitted_at.elapsed().as_millis())),
                confirm_delay_blocks: Some(height.saturating_sub(pending.submitted_height)),
            });
        }
    }

    if entries.is_empty() {
        return Ok(false);
    }

    let last_idx = entries.len() - 1;
    for (idx, entry) in entries.into_iter().enumerate() {
        let retention = match (entry.recovered.is_some(), idx == last_idx) {
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
        let position = Position::from(*next_position);
        *next_position += 1;

        let Some(recovered) = entry.recovered else {
            continue;
        };

        if reserved_note_ids.contains(&recovered.note_id) {
            tracer.trace_recovered_note(
                Some(height),
                "note_reserved_pending",
                &recovered,
                None,
                Some("pending_input_reserved"),
            );
            continue;
        }

        let tracked = registry.activate_recovered_note(recovered, position, height);
        if let Some(note_nullifier) = entry.note_nullifier {
            nullifier_index.insert(note_nullifier, tracked.note_id.clone());
        }
        tracer.trace_tracked_note(Some(height), "note_activated", &tracked, None, None);
    }

    Ok(true)
}

fn duration_ms_u64(duration_ms: u128) -> u64 {
    duration_ms.min(u128::from(u64::MAX)) as u64
}
