use std::time::Instant;

use anyhow::{Context, Result};
use orchard::keys::{FullViewingKey, Scope, SpendAuthorizingKey, SpendingKey};
use orchard::note_encryption::OrchardDomain;
use zcash_note_encryption::try_output_recovery_with_ovk;
use zcash_primitives::transaction::builder::{BuildConfig, Builder};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::{self, BlockHeight, NetworkType, NetworkUpgrade};
use zcash_protocol::memo::MemoBytes;
use zcash_transparent::builder::TransparentSigningSet;
use zcash_transparent::bundle::{OutPoint, TxOut};
use zebra_chain::serialization::{BytesInDisplayOrder, ZcashSerialize};

use crate::txblast::OrchardBlastRuntimeConfig;
use crate::txblast::rpc::ZebraRpcClient;
use crate::txblast::transparent::FundedKey;

use super::{NoteRole, PendingTx, PendingTxKind, PlannedOutput, RecoveredNote, TrackedNote};

pub(crate) const MIN_NOTE_VALUE: u64 = 50_000;
pub(crate) const ORCHARD_SPEND_FEE: u64 = 10_000;

fn orchard_bundle_actions(spends: usize, outputs: usize) -> usize {
    std::cmp::max(2, std::cmp::max(spends, outputs))
}

fn zip317_fee(transparent_inputs: usize, orchard_actions: usize) -> u64 {
    const MARGINAL_FEE: u64 = 5_000;
    const GRACE_ACTIONS: u64 = 2;
    let total = transparent_inputs as u64 + orchard_actions as u64;
    MARGINAL_FEE * std::cmp::max(GRACE_ACTIONS, total)
}

#[derive(Clone, Debug)]
struct KreskoTestnet;

impl consensus::Parameters for KreskoTestnet {
    fn network_type(&self) -> NetworkType {
        NetworkType::Test
    }

    fn activation_height(&self, nu: NetworkUpgrade) -> Option<BlockHeight> {
        match nu {
            NetworkUpgrade::Nu6_1 => None,
            _ => Some(BlockHeight::from_u32(1)),
        }
    }
}

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

pub(crate) struct OrchardKeys {
    #[allow(dead_code)]
    sk: SpendingKey,
    sak: SpendAuthorizingKey,
    fvk: FullViewingKey,
    address: orchard::Address,
    ovk: orchard::keys::OutgoingViewingKey,
}

pub(crate) fn derive_orchard_keys(secret: &[u8; 32]) -> Result<OrchardKeys> {
    let ct = SpendingKey::from_bytes(*secret);
    if bool::from(ct.is_none()) {
        anyhow::bail!("funded key secret bytes are not a valid Orchard SpendingKey");
    }
    let sk = ct.unwrap();
    let sak = SpendAuthorizingKey::from(&sk);
    let fvk = FullViewingKey::from(&sk);
    let address = fvk.address_at(0u32, Scope::External);
    let ovk = fvk.to_ovk(Scope::External);
    Ok(OrchardKeys {
        sk,
        sak,
        fvk,
        address,
        ovk,
    })
}

pub(crate) fn min_lane_value(cfg: &OrchardBlastRuntimeConfig) -> u64 {
    std::cmp::max(cfg.lane_premine.lane_value_zats, MIN_NOTE_VALUE)
}

pub(crate) fn target_reservoir_value(cfg: &OrchardBlastRuntimeConfig) -> u64 {
    std::cmp::max(cfg.lane_premine.fanout_source_value_zats, MIN_NOTE_VALUE)
}

pub(crate) fn min_reservoir_value(cfg: &OrchardBlastRuntimeConfig) -> u64 {
    zip317_fee(
        0,
        orchard_bundle_actions(1, cfg.lane_premine.fanout_outputs + 1),
    ) + cfg.lane_premine.fanout_outputs as u64 * min_lane_value(cfg)
        + target_reservoir_value(cfg)
}

pub(crate) fn min_treasury_reseed_value(cfg: &OrchardBlastRuntimeConfig) -> u64 {
    zip317_fee(1, orchard_bundle_actions(0, 1)) + min_reservoir_value(cfg)
}

pub(crate) fn plan_shielding_outputs(
    input_value: u64,
    remaining_lane_target: usize,
    cfg: &OrchardBlastRuntimeConfig,
) -> Result<Vec<PlannedOutput>> {
    let lane_value = min_lane_value(cfg);
    let reservoir_min = min_reservoir_value(cfg);
    let mut lane_count = remaining_lane_target;

    loop {
        let lane_total = lane_count as u64 * lane_value;
        let fee_without_reservoir = zip317_fee(1, orchard_bundle_actions(0, lane_count));
        if input_value < lane_total.saturating_add(fee_without_reservoir) {
            if lane_count == 0 {
                break;
            }
            lane_count -= 1;
            continue;
        }

        let fee_with_reservoir = zip317_fee(1, orchard_bundle_actions(0, lane_count + 1));
        if input_value >= lane_total.saturating_add(fee_with_reservoir + reservoir_min) {
            let reservoir_value = input_value - lane_total - fee_with_reservoir;
            let mut outputs = vec![
                PlannedOutput {
                    role: NoteRole::Lane,
                    value: lane_value,
                };
                lane_count
            ];
            outputs.push(PlannedOutput {
                role: NoteRole::Reservoir,
                value: reservoir_value,
            });
            return Ok(outputs);
        }

        if lane_count > 0 {
            return Ok(vec![
                PlannedOutput {
                    role: NoteRole::Lane,
                    value: lane_value,
                };
                lane_count
            ]);
        }

        if input_value >= fee_with_reservoir + reservoir_min {
            return Ok(vec![PlannedOutput {
                role: NoteRole::Reservoir,
                value: input_value - fee_with_reservoir,
            }]);
        }

        break;
    }

    anyhow::bail!(
        "input value {} zats is too small to create any Orchard lane or reservoir notes",
        input_value
    );
}

pub(crate) fn plan_reservoir_expand_outputs(
    input_value: u64,
    cfg: &OrchardBlastRuntimeConfig,
) -> Result<Vec<PlannedOutput>> {
    let lane_count = cfg.lane_premine.fanout_outputs;
    let output_count = lane_count + 1;
    let fee = zip317_fee(0, orchard_bundle_actions(1, output_count));
    let lane_value = min_lane_value(cfg);
    let reservoir_floor = target_reservoir_value(cfg);

    let required = fee + lane_count as u64 * lane_value + reservoir_floor;
    if input_value < required {
        anyhow::bail!(
            "reservoir note value {} is too small to preserve a {} zats reservoir while creating {} lanes of {} zats",
            input_value,
            reservoir_floor,
            lane_count,
            lane_value,
        );
    }

    let reservoir_value = input_value - fee - lane_count as u64 * lane_value;
    let mut outputs = vec![
        PlannedOutput {
            role: NoteRole::Lane,
            value: lane_value,
        };
        lane_count
    ];
    outputs.push(PlannedOutput {
        role: NoteRole::Reservoir,
        value: reservoir_value,
    });

    Ok(outputs)
}

pub(crate) fn plan_treasury_reseed_outputs(
    input_value: u64,
    cfg: &OrchardBlastRuntimeConfig,
) -> Result<Vec<PlannedOutput>> {
    let reservoir_floor = min_reservoir_value(cfg);
    let max_outputs = std::cmp::max(1, cfg.lane_premine.fanout_outputs);
    let mut output_count = std::cmp::min(max_outputs, (input_value / reservoir_floor) as usize);
    output_count = output_count.max(1);

    while output_count > 0 {
        let fee = zip317_fee(1, orchard_bundle_actions(0, output_count));
        let Some(distributable) = input_value.checked_sub(fee) else {
            output_count -= 1;
            continue;
        };
        if distributable < reservoir_floor.saturating_mul(output_count as u64) {
            output_count -= 1;
            continue;
        }

        let base_value = distributable / output_count as u64;
        let remainder = distributable % output_count as u64;
        let outputs = (0..output_count)
            .map(|idx| PlannedOutput {
                role: NoteRole::Reservoir,
                value: base_value + u64::from(idx < remainder as usize),
            })
            .collect::<Vec<_>>();

        if outputs.iter().all(|output| output.value >= reservoir_floor) {
            return Ok(outputs);
        }

        output_count -= 1;
    }

    anyhow::bail!(
        "input value {} zats is too small to create an expansion-ready treasury reservoir",
        input_value
    );
}

pub(crate) async fn build_and_send_shielding_tx(
    client: &ZebraRpcClient,
    funded_key: &FundedKey,
    keys: &OrchardKeys,
    utxo_txid: &str,
    utxo_output_index: u32,
    utxo_script: &str,
    input_value: u64,
    outputs: &[PlannedOutput],
    anchor: orchard::Anchor,
    target_height: u32,
    kind: PendingTxKind,
) -> Result<(String, PendingTx)> {
    let expected_fee = zip317_fee(1, orchard_bundle_actions(0, outputs.len()));
    let output_total: u64 = outputs.iter().map(|output| output.value).sum();
    if input_value != output_total + expected_fee {
        anyhow::bail!(
            "planned shielding outputs {} plus fee {} do not match input value {}",
            output_total,
            expected_fee,
            input_value
        );
    }

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    let outpoint = transparent_outpoint(utxo_txid, utxo_output_index)?;
    let coin = transparent_txout(input_value, utxo_script)?;
    builder
        .add_transparent_input(funded_key.public_key, outpoint, coin)
        .map_err(|e| anyhow::anyhow!("add_transparent_input: {e}"))?;

    for output in outputs {
        builder
            .add_orchard_output::<zip317::FeeError>(
                Some(keys.ovk.clone()),
                keys.address,
                output.value,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;
    }

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

    let tx = result.transaction();
    let roles: Vec<NoteRole> = outputs.iter().map(|output| output.role).collect();
    let recovered_notes = recover_notes_from_tx(tx, &keys.ovk, &roles);
    let num_actions = count_orchard_actions(tx);

    let mut tx_bytes = Vec::new();
    tx.write(&mut tx_bytes)
        .map_err(|e| anyhow::anyhow!("failed to serialize transaction: {e}"))?;
    let txid = client.send_raw_transaction(&hex::encode(&tx_bytes)).await?;
    let parent_note_id = Some(format!("{utxo_txid}:{utxo_output_index}"));
    let recovered_notes = recovered_notes
        .into_iter()
        .map(|note| note.with_origin(&txid, parent_note_id.clone()))
        .collect();

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok((
        txid,
        PendingTx {
            recovered_notes,
            num_actions,
            kind,
            spent_transparent_outpoint: Some(format!("{utxo_txid}:{utxo_output_index}")),
        },
    ))
}

pub(crate) async fn build_and_send_lane_advance_tx(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    tracked: &TrackedNote,
    merkle_path: orchard::tree::MerklePath,
    anchor: orchard::Anchor,
    target_height: u32,
) -> Result<(String, PendingTx)> {
    let fee = zip317_fee(0, orchard_bundle_actions(1, 1));
    let note_value = tracked.value();
    if note_value <= fee {
        anyhow::bail!("note value {} is not enough to pay fee {}", note_value, fee);
    }

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    builder
        .add_orchard_spend::<zip317::FeeError>(keys.fvk.clone(), tracked.note, merkle_path)
        .map_err(|e| anyhow::anyhow!("add_orchard_spend: {e}"))?;
    builder
        .add_orchard_output::<zip317::FeeError>(
            Some(keys.ovk.clone()),
            keys.address,
            note_value - fee,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;

    let signing_set = TransparentSigningSet::new();
    let fee_rule = zip317::FeeRule::standard();
    let start = Instant::now();
    let result = builder
        .build(
            &signing_set,
            &[],
            &[keys.sak.clone()],
            rand_core_06::OsRng,
            &NoSaplingSpendProver,
            &NoSaplingOutputProver,
            &fee_rule,
        )
        .map_err(|e| anyhow::anyhow!("Orchard lane build failed: {e}"))?;
    let proving_ms = start.elapsed().as_millis();

    let tx = result.transaction();
    let recovered_notes = recover_notes_from_tx(tx, &keys.ovk, &[NoteRole::Lane]);
    let num_actions = count_orchard_actions(tx);

    let mut tx_bytes = Vec::new();
    tx.write(&mut tx_bytes)?;
    let txid = client.send_raw_transaction(&hex::encode(&tx_bytes)).await?;
    let recovered_notes = recovered_notes
        .into_iter()
        .map(|note| note.with_origin(&txid, Some(tracked.note_id.clone())))
        .collect();

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok((
        txid,
        PendingTx {
            recovered_notes,
            num_actions,
            kind: PendingTxKind::LaneAdvance,
            spent_transparent_outpoint: None,
        },
    ))
}

pub(crate) async fn build_and_send_reservoir_expand_tx(
    client: &ZebraRpcClient,
    keys: &OrchardKeys,
    tracked: &TrackedNote,
    merkle_path: orchard::tree::MerklePath,
    anchor: orchard::Anchor,
    target_height: u32,
    cfg: &OrchardBlastRuntimeConfig,
) -> Result<(String, PendingTx)> {
    let planned_outputs = plan_reservoir_expand_outputs(tracked.value(), cfg)?;

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    builder
        .add_orchard_spend::<zip317::FeeError>(keys.fvk.clone(), tracked.note, merkle_path)
        .map_err(|e| anyhow::anyhow!("add_orchard_spend: {e}"))?;

    for output in &planned_outputs {
        builder
            .add_orchard_output::<zip317::FeeError>(
                Some(keys.ovk.clone()),
                keys.address,
                output.value,
                MemoBytes::empty(),
            )
            .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;
    }

    let signing_set = TransparentSigningSet::new();
    let fee_rule = zip317::FeeRule::standard();
    let start = Instant::now();
    let result = builder
        .build(
            &signing_set,
            &[],
            &[keys.sak.clone()],
            rand_core_06::OsRng,
            &NoSaplingSpendProver,
            &NoSaplingOutputProver,
            &fee_rule,
        )
        .map_err(|e| anyhow::anyhow!("Orchard fanout build failed: {e}"))?;
    let proving_ms = start.elapsed().as_millis();

    let tx = result.transaction();
    let roles = planned_outputs
        .iter()
        .map(|output| output.role)
        .collect::<Vec<_>>();
    let recovered_notes = recover_notes_from_tx(tx, &keys.ovk, &roles);
    let num_actions = count_orchard_actions(tx);

    let mut tx_bytes = Vec::new();
    tx.write(&mut tx_bytes)?;
    let txid = client.send_raw_transaction(&hex::encode(&tx_bytes)).await?;
    let recovered_notes = recovered_notes
        .into_iter()
        .map(|note| note.with_origin(&txid, Some(tracked.note_id.clone())))
        .collect();

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok((
        txid,
        PendingTx {
            recovered_notes,
            num_actions,
            kind: PendingTxKind::ReservoirExpand,
            spent_transparent_outpoint: None,
        },
    ))
}

pub(crate) async fn build_and_send_treasury_reseed_tx(
    client: &ZebraRpcClient,
    funded_key: &FundedKey,
    keys: &OrchardKeys,
    utxo: &super::TreasuryUtxo,
    anchor: orchard::Anchor,
    target_height: u32,
    cfg: &OrchardBlastRuntimeConfig,
) -> Result<(String, PendingTx)> {
    let planned_outputs = plan_treasury_reseed_outputs(utxo.satoshis, cfg)?;
    build_and_send_shielding_tx(
        client,
        funded_key,
        keys,
        &utxo.txid,
        utxo.output_index,
        &utxo.script,
        utxo.satoshis,
        &planned_outputs,
        anchor,
        target_height,
        PendingTxKind::TreasuryReseed,
    )
    .await
}

fn recover_notes_from_tx(
    tx: &zcash_primitives::transaction::Transaction,
    ovk: &orchard::keys::OutgoingViewingKey,
    roles: &[NoteRole],
) -> Vec<RecoveredNote> {
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
            .map(|(note, _addr, _memo)| {
                RecoveredNote::pending(i, note, roles.get(i).copied().unwrap_or(NoteRole::Lane))
            })
        })
        .collect()
}

fn count_orchard_actions(tx: &zcash_primitives::transaction::Transaction) -> usize {
    tx.orchard_bundle().map(|b| b.actions().len()).unwrap_or(0)
}

fn transparent_outpoint(txid: &str, output_index: u32) -> Result<OutPoint> {
    let txid_bytes: [u8; 32] = hex::decode(txid)
        .with_context(|| format!("invalid txid: {txid}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("txid is not 32 bytes: {txid}"))?;
    Ok(OutPoint::new(
        zebra_chain::transaction::Hash::from_bytes_in_display_order(&txid_bytes).0,
        output_index,
    ))
}

fn transparent_txout(value: u64, script_hex: &str) -> Result<TxOut> {
    let script_bytes =
        hex::decode(script_hex).with_context(|| format!("invalid script hex: {script_hex}"))?;
    let value = zebra_chain::amount::Amount::<zebra_chain::amount::NonNegative>::try_from(value)
        .context("invalid UTXO amount")?;
    let output = zebra_chain::transparent::Output::new(
        value,
        zebra_chain::transparent::Script::new(&script_bytes),
    );
    let mut bytes = Vec::new();
    output
        .zcash_serialize(&mut bytes)
        .context("failed to serialize transparent output")?;
    let mut cursor = std::io::Cursor::new(&bytes);
    TxOut::read(&mut cursor).map_err(|e| anyhow::anyhow!("bridge TxOut: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OrchardTxblastConfig;

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

    #[test]
    fn shielding_plan_prefers_target_lanes_and_reservoir() {
        let cfg = test_cfg();
        let outputs =
            plan_shielding_outputs(1_000_000_000, cfg.target_ready_lanes, &cfg).expect("plan");
        let lane_count = outputs
            .iter()
            .filter(|output| output.role == NoteRole::Lane)
            .count();
        let reservoir_count = outputs
            .iter()
            .filter(|output| output.role == NoteRole::Reservoir)
            .count();

        assert_eq!(lane_count, cfg.target_ready_lanes);
        assert_eq!(reservoir_count, 1);
    }

    #[test]
    fn shielding_plan_can_make_reservoir_only() {
        let cfg = test_cfg();
        let outputs = plan_shielding_outputs(1_000_000_000, 0, &cfg).expect("plan");

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].role, NoteRole::Reservoir);
        assert!(outputs[0].value >= min_reservoir_value(&cfg));
    }

    #[test]
    fn reservoir_expand_plan_preserves_reservoir() {
        let cfg = test_cfg();
        let outputs =
            plan_reservoir_expand_outputs(min_reservoir_value(&cfg) + 50_000, &cfg).expect("plan");

        assert_eq!(outputs.len(), cfg.lane_premine.fanout_outputs + 1);
        assert_eq!(
            outputs
                .iter()
                .filter(|output| output.role == NoteRole::Lane)
                .count(),
            cfg.lane_premine.fanout_outputs
        );
        assert_eq!(
            outputs.last().map(|output| output.role),
            Some(NoteRole::Reservoir)
        );
        assert!(outputs.last().expect("reservoir output").value >= target_reservoir_value(&cfg));
    }

    #[test]
    fn treasury_reseed_plan_creates_multiple_reservoirs() {
        let cfg = test_cfg();
        let outputs = plan_treasury_reseed_outputs(1_000_000_000, &cfg).expect("plan");

        assert_eq!(outputs.len(), cfg.lane_premine.fanout_outputs);
        assert!(
            outputs
                .iter()
                .all(|output| output.role == NoteRole::Reservoir)
        );
        assert!(
            outputs
                .iter()
                .all(|output| output.value >= min_reservoir_value(&cfg))
        );
    }
}
