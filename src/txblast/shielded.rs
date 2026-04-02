use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ff::PrimeField;
use incrementalmerkletree::{Marking, Position, Retention};
use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use orchard::note_encryption::OrchardDomain;
use orchard::tree::MerkleHashOrchard;
use shardtree::store::memory::MemoryShardStore;
use shardtree::ShardTree;
use zcash_note_encryption::try_output_recovery_with_ovk;
use zcash_primitives::transaction::builder::{BuildConfig, Builder};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::{self, BlockHeight, NetworkType, NetworkUpgrade};
use zcash_protocol::memo::MemoBytes;
use zcash_transparent::bundle::{OutPoint, TxOut};
use zcash_transparent::builder::TransparentSigningSet;
use zebra_chain::serialization::{BytesInDisplayOrder, ZcashDeserialize, ZcashSerialize};

use super::rpc::ZebraRpcClient;
use super::transparent::FundedKey;

const BASE_FEE_ZATS: u64 = 20_000;
const MIN_NOTE_VALUE: u64 = 50_000;
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

type OrchardTree = ShardTree<MemoryShardStore<MerkleHashOrchard, u32>, 32, 16>;

// ---------------------------------------------------------------------------
// Minimal consensus parameters — all upgrades active at height 1.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct KreskoTestnet;

impl consensus::Parameters for KreskoTestnet {
    fn network_type(&self) -> NetworkType {
        NetworkType::Test
    }

    fn activation_height(&self, _nu: NetworkUpgrade) -> Option<BlockHeight> {
        Some(BlockHeight::from_u32(1))
    }
}

// ---------------------------------------------------------------------------
// Dummy Sapling provers — never called, builder type signature requires them.
// ---------------------------------------------------------------------------

struct NoSaplingSpendProver;

impl sapling_crypto::prover::SpendProver for NoSaplingSpendProver {
    type Proof = sapling_crypto::bundle::GrothProofBytes;

    fn prepare_circuit(
        _: sapling_crypto::ProofGenerationKey,
        _: sapling_crypto::Diversifier,
        _: sapling_crypto::Rseed,
        _: sapling_crypto::value::NoteValue,
        _: jubjub::Fr,
        _: sapling_crypto::value::ValueCommitTrapdoor,
        _: bls12_381::Scalar,
        _: sapling_crypto::MerklePath,
    ) -> Option<sapling_crypto::circuit::Spend> {
        unreachable!("no Sapling spends in shielded txblast")
    }

    fn create_proof<R: rand_core_06::RngCore>(
        &self,
        _: sapling_crypto::circuit::Spend,
        _: &mut R,
    ) -> Self::Proof {
        unreachable!("no Sapling spends in shielded txblast")
    }

    fn encode_proof(proof: Self::Proof) -> sapling_crypto::bundle::GrothProofBytes {
        proof
    }
}

struct NoSaplingOutputProver;

impl sapling_crypto::prover::OutputProver for NoSaplingOutputProver {
    type Proof = sapling_crypto::bundle::GrothProofBytes;

    fn prepare_circuit(
        _: &sapling_crypto::keys::EphemeralSecretKey,
        _: sapling_crypto::PaymentAddress,
        _: jubjub::Fr,
        _: sapling_crypto::value::NoteValue,
        _: sapling_crypto::value::ValueCommitTrapdoor,
    ) -> sapling_crypto::circuit::Output {
        unreachable!("no Sapling outputs in shielded txblast")
    }

    fn create_proof<R: rand_core_06::RngCore>(
        &self,
        _: sapling_crypto::circuit::Output,
        _: &mut R,
    ) -> Self::Proof {
        unreachable!("no Sapling outputs in shielded txblast")
    }

    fn encode_proof(proof: Self::Proof) -> sapling_crypto::bundle::GrothProofBytes {
        proof
    }
}

// ---------------------------------------------------------------------------
// Orchard key container (full key set for spending + receiving).
// ---------------------------------------------------------------------------

struct OrchardKeys {
    #[allow(dead_code)]
    sk: SpendingKey,
    fvk: FullViewingKey,
    address: orchard::Address,
    ovk: orchard::keys::OutgoingViewingKey,
}

fn derive_orchard_keys(secret: &[u8; 32]) -> Result<OrchardKeys> {
    let ct = SpendingKey::from_bytes(*secret);
    if bool::from(ct.is_none()) {
        anyhow::bail!("funded key secret bytes are not a valid Orchard SpendingKey");
    }
    let sk = ct.unwrap();
    let fvk = FullViewingKey::from(&sk);
    let address = fvk.address_at(0u32, Scope::External);
    let ovk = fvk.to_ovk(Scope::External);
    Ok(OrchardKeys { sk, fvk, address, ovk })
}

// ---------------------------------------------------------------------------
// Note tracking types.
// ---------------------------------------------------------------------------

struct TrackedNote {
    note: orchard::Note,
    position: Position,
}

struct PendingTx {
    /// (action_index, recovered_note) pairs from OVK decryption.
    recovered_notes: Vec<(usize, orchard::Note)>,
    /// Total Orchard actions in this transaction.
    num_actions: usize,
}

// ---------------------------------------------------------------------------
// Anchor management.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Type bridging: zebra-chain → zcash_transparent types via serialization.
// ---------------------------------------------------------------------------

fn bridge_outpoint(zc: &zebra_chain::transparent::OutPoint) -> OutPoint {
    OutPoint::new(zc.hash.0, zc.index)
}

fn bridge_txout(zc: &zebra_chain::transparent::Output) -> Result<TxOut> {
    let mut bytes = Vec::new();
    zc.zcash_serialize(&mut bytes)
        .context("failed to serialize transparent output")?;
    let mut cursor = std::io::Cursor::new(&bytes);
    TxOut::read(&mut cursor).map_err(|e| anyhow::anyhow!("bridge TxOut: {e}"))
}

#[allow(dead_code)]
fn funded_key_to_transparent_address(
    key: &FundedKey,
) -> Result<zcash_transparent::address::TransparentAddress> {
    let script_bytes = key.address.script().as_raw_bytes().to_vec();
    if script_bytes.len() == 25
        && script_bytes[0] == 0x76
        && script_bytes[1] == 0xa9
        && script_bytes[2] == 0x14
        && script_bytes[23] == 0x88
        && script_bytes[24] == 0xac
    {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&script_bytes[3..23]);
        Ok(zcash_transparent::address::TransparentAddress::PublicKeyHash(hash))
    } else {
        anyhow::bail!("funded key address is not a standard P2PKH address")
    }
}

// ---------------------------------------------------------------------------
// Note recovery from a built transaction via OVK decryption.
// ---------------------------------------------------------------------------

fn recover_notes_from_tx(
    tx: &zcash_primitives::transaction::Transaction,
    ovk: &orchard::keys::OutgoingViewingKey,
) -> Vec<(usize, orchard::Note)> {
    let Some(bundle) = tx.orchard_bundle() else {
        return vec![];
    };
    bundle
        .actions()
        .iter()
        .enumerate()
        .filter_map(|(i, action)| {
            let domain = OrchardDomain::for_action(action);
            try_output_recovery_with_ovk(
                &domain,
                ovk,
                action,
                action.cv_net(),
                &action.encrypted_note().out_ciphertext,
            )
            .map(|(note, _addr, _memo)| (i, note))
        })
        .collect()
}

fn count_orchard_actions(tx: &zcash_primitives::transaction::Transaction) -> usize {
    tx.orchard_bundle()
        .map(|b| b.actions().len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Build and submit a shielding transaction (transparent → Orchard, no change).
// Used for both coinbase and non-coinbase UTXOs in the warmup phase.
// ---------------------------------------------------------------------------

async fn build_and_send_shielding_tx(
    client: &ZebraRpcClient,
    funded_key: &FundedKey,
    keys: &OrchardKeys,
    utxo_outpoint: &zebra_chain::transparent::OutPoint,
    utxo_output: &zebra_chain::transparent::Output,
    anchor: orchard::Anchor,
    target_height: u32,
) -> Result<(String, PendingTx)> {
    let input_value = u64::from(utxo_output.value);
    if input_value <= BASE_FEE_ZATS {
        anyhow::bail!("input value {} is not enough to pay fee", input_value);
    }

    let output_value = input_value.saturating_sub(BASE_FEE_ZATS);

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    // Transparent input.
    let outpoint = bridge_outpoint(utxo_outpoint);
    let coin = bridge_txout(utxo_output)?;
    builder
        .add_transparent_input(funded_key.public_key, outpoint, coin)
        .map_err(|e| anyhow::anyhow!("add_transparent_input: {e}"))?;

    // Orchard output — full value minus fee, NO transparent change.
    builder
        .add_orchard_output::<zip317::FeeError>(
            Some(keys.ovk.clone()),
            keys.address,
            output_value,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;

    // Sign and prove.
    let mut signing_set = TransparentSigningSet::new();
    signing_set.add_key(funded_key.secret_key);

    let fee_rule = zip317::FeeRule::standard();
    let start = Instant::now();
    let result = builder
        .build(
            &signing_set,
            &[],
            &[],
            rand_core_06::OsRng,
            &NoSaplingSpendProver,
            &NoSaplingOutputProver,
            &fee_rule,
        )
        .map_err(|e| anyhow::anyhow!("transaction build failed: {e}"))?;
    let proving_ms = start.elapsed().as_millis();

    // Recover notes from the built transaction (before submitting).
    let tx = result.transaction();
    let recovered_notes = recover_notes_from_tx(tx, &keys.ovk);
    let num_actions = count_orchard_actions(tx);

    // Serialize and submit.
    let mut tx_bytes = Vec::new();
    tx.write(&mut tx_bytes)
        .map_err(|e| anyhow::anyhow!("failed to serialize transaction: {e}"))?;
    let tx_hex = hex::encode(&tx_bytes);
    let txid = client.send_raw_transaction(&tx_hex).await?;

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok((
        txid,
        PendingTx {
            recovered_notes,
            num_actions,
        },
    ))
}

// ---------------------------------------------------------------------------
// Build and submit an Orchard→Orchard spend transaction.
// ---------------------------------------------------------------------------

async fn build_and_send_orchard_spend_tx(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    note: &orchard::Note,
    merkle_path: orchard::tree::MerklePath,
    anchor: orchard::Anchor,
    target_height: u32,
) -> Result<(String, PendingTx)> {
    let note_value = note.value().inner();
    if note_value <= BASE_FEE_ZATS {
        anyhow::bail!("note value {} is not enough to pay fee", note_value);
    }

    let output_total = note_value.saturating_sub(BASE_FEE_ZATS);

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    // Orchard spend.
    builder
        .add_orchard_spend::<zip317::FeeError>(keys.fvk.clone(), *note, merkle_path)
        .map_err(|e| anyhow::anyhow!("add_orchard_spend: {e}"))?;

    // Split into 2 outputs if value is sufficient, otherwise single output.
    if output_total >= 2 * MIN_NOTE_VALUE {
        let half = output_total / 2;
        let other = output_total - half;
        builder
            .add_orchard_output::<zip317::FeeError>(
                Some(keys.ovk.clone()),
                keys.address,
                half,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow::anyhow!("add_orchard_output (split 1): {e}"))?;
        builder
            .add_orchard_output::<zip317::FeeError>(
                Some(keys.ovk.clone()),
                keys.address,
                other,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow::anyhow!("add_orchard_output (split 2): {e}"))?;
    } else {
        builder
            .add_orchard_output::<zip317::FeeError>(
                Some(keys.ovk.clone()),
                keys.address,
                output_total,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;
    }

    // Build — no transparent inputs, empty signing set.
    let signing_set = TransparentSigningSet::new();
    let fee_rule = zip317::FeeRule::standard();
    let start = Instant::now();
    let result = builder
        .build(
            &signing_set,
            &[],
            &[],
            rand_core_06::OsRng,
            &NoSaplingSpendProver,
            &NoSaplingOutputProver,
            &fee_rule,
        )
        .map_err(|e| anyhow::anyhow!("Orchard spend build failed: {e}"))?;
    let proving_ms = start.elapsed().as_millis();

    let tx = result.transaction();
    let recovered_notes = recover_notes_from_tx(tx, &keys.ovk);
    let num_actions = count_orchard_actions(tx);

    let mut tx_bytes = Vec::new();
    tx.write(&mut tx_bytes)?;
    let tx_hex = hex::encode(&tx_bytes);
    let txid = client.send_raw_transaction(&tx_hex).await?;

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok((
        txid,
        PendingTx {
            recovered_notes,
            num_actions,
        },
    ))
}

// ---------------------------------------------------------------------------
// Block scanning — fetch a block, extract Orchard commitments, update tree.
// ---------------------------------------------------------------------------

async fn scan_block(
    client: &ZebraRpcClient,
    tree: &mut OrchardTree,
    next_position: &mut u64,
    height: u32,
    pending_txs: &mut HashMap<String, PendingTx>,
    spendable_notes: &mut Vec<TrackedNote>,
) -> Result<bool> {
    let block_bytes = client.getblock_raw(height).await?;
    let block = zebra_chain::block::Block::zcash_deserialize(&block_bytes[..])
        .with_context(|| format!("failed to deserialize block at height {height}"))?;

    // Collect all Orchard commitments from the block, noting which are ours.
    struct CommitmentEntry {
        hash: MerkleHashOrchard,
        is_our_note: bool,
    }

    let mut entries = Vec::new();

    for tx in &block.transactions {
        let tx_hash = tx.hash().to_string();
        let orchard_data = tx.orchard_shielded_data();
        let Some(shielded_data) = orchard_data else {
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
                    .map(|p| p.recovered_notes.iter().any(|(idx, _)| *idx == action_idx))
                    .unwrap_or(false);

            entries.push(CommitmentEntry {
                hash,
                is_our_note,
            });
        }

        // If this is our tx, record base position and promote notes.
        if let Some(pending) = is_our_tx
            .then(|| pending_txs.remove(&tx_hash))
            .flatten()
        {
            let base_pos = *next_position
                + entries.len() as u64
                - pending.num_actions as u64;

            for (action_idx, note) in pending.recovered_notes {
                let position = Position::from(base_pos + action_idx as u64);
                spendable_notes.push(TrackedNote { note, position });
            }
        }
    }

    if entries.is_empty() {
        return Ok(false);
    }

    // Append all commitments to the tree. The last one gets a checkpoint.
    let last_idx = entries.len() - 1;
    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == last_idx;
        let retention = match (entry.is_our_note, is_last) {
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

    Ok(true) // block had Orchard actions
}

// ---------------------------------------------------------------------------
// Wait for the chain to advance past a given height.
// ---------------------------------------------------------------------------

async fn wait_for_block_advance(client: &ZebraRpcClient, after_height: u32) -> Result<u32> {
    loop {
        let current = client.get_block_count().await?;
        if current > after_height {
            return Ok(current);
        }
        tokio::time::sleep(BLOCK_POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Main run loop — Phase 1 (warmup) then Phase 2 (steady state).
// ---------------------------------------------------------------------------

pub async fn run(
    client: &ZebraRpcClient,
    key: &FundedKey,
    rate: u64,
    _amount: f64,
) -> Result<()> {
    if rate == 0 {
        anyhow::bail!("--rate must be greater than 0");
    }

    let secret_bytes: [u8; 32] = key.secret_key.secret_bytes();
    let keys = derive_orchard_keys(&secret_bytes)?;
    println!(
        "[shielded] Orchard address derived from funded key '{}'",
        key.name,
    );

    // State.
    let mut tree: OrchardTree = ShardTree::new(MemoryShardStore::empty(), 100);
    let mut spendable_notes: Vec<TrackedNote> = Vec::new();
    let mut pending_txs: HashMap<String, PendingTx> = HashMap::new();
    let mut next_position: u64 = 0;
    let mut latest_checkpoint: Option<u32> = None;

    // ------------------------------------------------------------------
    // Phase 0: Scan existing blocks to build the commitment tree.
    // ------------------------------------------------------------------

    let current_height = client.get_block_count().await?;
    let anchor = fetch_orchard_anchor(client).await?;

    if anchor != orchard::Anchor::empty_tree() && current_height > 0 {
        println!(
            "[shielded] scanning blocks 1..{current_height} for existing Orchard commitments"
        );
        for h in 1..=current_height {
            let had_actions = scan_block(
                client,
                &mut tree,
                &mut next_position,
                h,
                &mut pending_txs,
                &mut spendable_notes,
            )
            .await?;
            if had_actions {
                latest_checkpoint = Some(h);
            }
        }
        println!(
            "[shielded] scanned {} blocks, tree has {} commitments",
            current_height, next_position,
        );
    }

    let mut last_scanned_height = current_height;

    // ------------------------------------------------------------------
    // Phase 1: Shield transparent UTXOs (warmup).
    // Coinbase UTXOs require no transparent change output.
    // Non-coinbase UTXOs are also fully shielded (no change).
    // ------------------------------------------------------------------

    let rpc_utxos = client
        .get_address_utxos(&key.address.to_string())
        .await?;

    if rpc_utxos.is_empty() && spendable_notes.is_empty() {
        anyhow::bail!(
            "no transparent UTXOs or Orchard notes found for {}. make sure premine blocks were loaded",
            key.address,
        );
    }

    let shieldable: Vec<_> = rpc_utxos
        .into_iter()
        .filter(|u| u.satoshis > BASE_FEE_ZATS)
        .collect();

    if !shieldable.is_empty() {
        println!(
            "[shielded] Phase 1: shielding {} transparent UTXOs",
            shieldable.len()
        );
    }

    for utxo_data in &shieldable {
        let txid_bytes: [u8; 32] = hex::decode(&utxo_data.txid)
            .with_context(|| format!("invalid txid: {}", utxo_data.txid))?
            .try_into()
            .map_err(|_| anyhow::anyhow!("txid is not 32 bytes: {}", utxo_data.txid))?;
        let txid_hash =
            zebra_chain::transaction::Hash::from_bytes_in_display_order(&txid_bytes);

        let script_bytes = hex::decode(&utxo_data.script)
            .with_context(|| format!("invalid script hex: {}", utxo_data.script))?;
        let value =
            zebra_chain::amount::Amount::<zebra_chain::amount::NonNegative>::try_from(
                utxo_data.satoshis,
            )
            .context("invalid UTXO amount")?;
        let utxo_output =
            zebra_chain::transparent::Output::new(value, zebra_chain::transparent::Script::new(&script_bytes));
        let utxo_outpoint = zebra_chain::transparent::OutPoint {
            hash: txid_hash,
            index: utxo_data.output_index,
        };

        let anchor = fetch_orchard_anchor(client).await?;
        let target_height = client.get_block_count().await? + 10;

        let (txid, pending) = build_and_send_shielding_tx(
            client,
            key,
            &keys,
            &utxo_outpoint,
            &utxo_output,
            anchor,
            target_height,
        )
        .await?;

        let note_count = pending.recovered_notes.len();
        pending_txs.insert(txid.clone(), pending);
        println!(
            "[shielded] submitted shielding tx: {txid} ({note_count} notes recovered)"
        );

        // Wait for confirmation.
        let new_height = wait_for_block_advance(client, last_scanned_height).await?;

        // Scan new blocks to update tree and confirm notes.
        for h in (last_scanned_height + 1)..=new_height {
            let had_actions = scan_block(
                client,
                &mut tree,
                &mut next_position,
                h,
                &mut pending_txs,
                &mut spendable_notes,
            )
            .await?;
            if had_actions {
                latest_checkpoint = Some(h);
            }
        }
        last_scanned_height = new_height;

        if pending_txs.contains_key(&txid) {
            // Tx wasn't in the block yet — keep waiting.
            println!("[shielded] shielding tx not yet confirmed, waiting...");
            loop {
                let h = wait_for_block_advance(client, last_scanned_height).await?;
                for bh in (last_scanned_height + 1)..=h {
                    let had_actions = scan_block(
                        client,
                        &mut tree,
                        &mut next_position,
                        bh,
                        &mut pending_txs,
                        &mut spendable_notes,
                    )
                    .await?;
                    if had_actions {
                        latest_checkpoint = Some(bh);
                    }
                }
                last_scanned_height = h;
                if !pending_txs.contains_key(&txid) {
                    break;
                }
            }
        }

        println!(
            "[shielded] shielding confirmed. spendable notes: {}",
            spendable_notes.len()
        );
    }

    if spendable_notes.is_empty() {
        anyhow::bail!(
            "no spendable Orchard notes after warmup. shielding may have failed."
        );
    }

    println!(
        "[shielded] Phase 2: starting Orchard→Orchard blast with {} notes",
        spendable_notes.len()
    );

    // ------------------------------------------------------------------
    // Phase 2: Orchard→Orchard steady state.
    // ------------------------------------------------------------------

    let interval = Duration::from_secs_f64(1.0 / rate as f64);
    let mut ticker = tokio::time::interval(interval);
    let mut tx_count: u64 = 0;
    let mut err_count: u64 = 0;

    loop {
        ticker.tick().await;

        // Scan any new blocks to confirm pending notes and refresh tree.
        let current = client.get_block_count().await.unwrap_or(last_scanned_height);
        if current > last_scanned_height {
            for h in (last_scanned_height + 1)..=current {
                match scan_block(
                    client,
                    &mut tree,
                    &mut next_position,
                    h,
                    &mut pending_txs,
                    &mut spendable_notes,
                )
                .await
                {
                    Ok(true) => latest_checkpoint = Some(h),
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("[shielded][warn] block scan at {h} failed: {e}");
                        break;
                    }
                }
            }
            last_scanned_height = current;
        }

        // Need a spendable note.
        if spendable_notes.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        // Need a checkpoint for witness generation.
        let Some(checkpoint_height) = latest_checkpoint else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };

        // Pop a note (prefer larger values for better splitting).
        let tracked = spendable_notes.pop().unwrap();

        // Skip notes that are too small to be useful.
        if tracked.note.value().inner() <= BASE_FEE_ZATS {
            continue;
        }

        // Get witness (merkle path) from the tree.
        let witness = match tree.witness_at_checkpoint_id(tracked.position, &checkpoint_height) {
            Ok(Some(path)) => path,
            Ok(None) => {
                eprintln!(
                    "[shielded][warn] no witness for position {:?} at checkpoint {checkpoint_height}, requeueing",
                    u64::from(tracked.position),
                );
                spendable_notes.push(tracked);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(e) => {
                eprintln!("[shielded][warn] witness error: {e:?}, requeueing note");
                spendable_notes.push(tracked);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let merkle_path: orchard::tree::MerklePath = witness.into();

        // Get anchor from tree root at checkpoint.
        let root = tree
            .root_at_checkpoint_id(&checkpoint_height)
            .map_err(|e| anyhow::anyhow!("root_at_checkpoint_id: {e:?}"))?
            .ok_or_else(|| anyhow::anyhow!("no root at checkpoint {checkpoint_height}"))?;
        let anchor = orchard::Anchor::from(root);

        let target_height = current + 10;

        match build_and_send_orchard_spend_tx(
            client,
            &keys,
            &tracked.note,
            merkle_path,
            anchor,
            target_height,
        )
        .await
        {
            Ok((txid, pending)) => {
                tx_count += 1;
                pending_txs.insert(txid.clone(), pending);
                if tx_count.is_multiple_of(10) {
                    println!(
                        "[shielded][{tx_count}] Orchard spend tx: {txid} (notes={}, pending={}, errors={err_count})",
                        spendable_notes.len(),
                        pending_txs.len(),
                    );
                }
            }
            Err(e) => {
                err_count += 1;
                eprintln!("[shielded][{}] Orchard spend failed: {e}", tx_count + err_count);
                // Put note back for retry.
                spendable_notes.push(tracked);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
