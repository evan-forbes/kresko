//! Premine generation for PoW experiments.
//!
//! When `mining_mode == Pow`, kresko seeds the live network with a chain of
//! genesis + premine + maturity-padding blocks. That chain is generated with
//! `disable_pow: true`, anchored to `SystemTime::now()`, and live PoW
//! enforcement starts at the seeded tip via zebra's `pow_start_height`.
//!
//! Premine generation is cheap (V1 transparent coinbases, no shielded proofs,
//! no Equihash) — roughly ~13 ms for 256 blocks on a single core — so every
//! call regenerates from scratch. That keeps every seeded block's timestamp
//! close to wall-clock-now, so the first live-mined block has no multi-
//! thousand-second gap at the seeded→live boundary.
//!
//! The premine has a *fixed* number of funded keys ([`FUNDED_KEY_COUNT`])
//! followed by [`MATURITY_PADDING_BLOCKS`] empty blocks; `funded_keys[0]` is
//! the treasury. Any experiment up to `FUNDED_KEY_COUNT` miners can share the
//! same signature.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use zebra_chain::{
    block::Block,
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
};

use crate::config::LocalGenesisFundedKey;

/// Number of funded premine keys produced by every premine bundle.
pub const FUNDED_KEY_COUNT: usize = 128;

/// Empty blocks appended after the funded premine so every premine coinbase
/// is spendable well before experiments begin. Zcash coinbase maturity is
/// 100 blocks; 128 leaves a comfortable margin.
pub const MATURITY_PADDING_BLOCKS: u32 = 128;

/// The inputs that determine which premine a PoW experiment needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationSignature {
    /// Big-endian `pow_limit` as 64 lowercase hex characters (no `0x`). This
    /// is the loosest target allowed by the generated network.
    pub target_hex: String,
    /// Target block spacing the live network will advertise, in seconds.
    pub block_time_secs: u32,
}

impl CalibrationSignature {
    pub fn new(target_hex: impl Into<String>, block_time_secs: u32) -> Result<Self> {
        let target_hex = target_hex.into().trim().to_ascii_lowercase();
        if target_hex.len() != 64 || !target_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!(
                "target_hex must be exactly 64 lowercase hex characters; got {:?}",
                target_hex
            );
        }
        if block_time_secs == 0 {
            anyhow::bail!("block_time_secs must be > 0");
        }
        Ok(Self {
            target_hex,
            block_time_secs,
        })
    }

    pub fn target_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&self.target_hex, &mut bytes)
            .expect("target_hex validated by CalibrationSignature::new");
        bytes
    }
}

/// Diagnostic description of the generated premine. Serialized into the
/// payload as `manifest.json` for operator inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PremineManifest {
    pub target_difficulty_limit: String,
    pub block_time_secs: u32,
    pub target_spacing_secs: u32,
    pub seeded_block_count: u32,
    pub premine_block_count: u32,
    pub maturity_padding_block_count: u32,
    pub disable_pow: bool,
    pub pow_start_height: Option<u32>,
    pub genesis_hash: String,
    pub seeded_tip_hash: String,
    pub seeded_genesis_time: i64,
    pub seeded_tip_time: i64,
    pub observed_min_spacing_secs: u32,
    pub observed_max_spacing_secs: u32,
    pub slow_start_interval: u32,
    pub pre_blossom_halving_interval: u32,
    pub activation_height: u32,
    /// Address of the first funded key. Kept in the manifest so downstream
    /// tooling (`fund_runtime_keys`) that treats the first key as the treasury
    /// doesn't need to load `funded_keys.json`.
    pub treasury_address: String,
    pub treasury_public_key_hex: String,
    pub funded_key_count: u32,
}

/// An in-memory premine bundle. Generated fresh on every call to
/// [`generate`]; never loaded from disk.
#[derive(Debug, Clone)]
pub struct PremineBundle {
    manifest: PremineManifest,
    treasury_key: LocalGenesisFundedKey,
    funded_keys: Vec<LocalGenesisFundedKey>,
    genesis_hex: String,
    premine_blocks_hex: String,
    checkpoints_content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeededTimeSummary {
    seeded_block_count: u32,
    seeded_genesis_time: i64,
    seeded_tip_time: i64,
    observed_min_spacing_secs: u32,
    observed_max_spacing_secs: u32,
}

fn summarize_seeded_block_times(
    blocks: &[Block],
    target_spacing_secs: u32,
) -> Result<SeededTimeSummary> {
    let first = blocks
        .first()
        .context("seeded chain must include a genesis block")?;
    let last = blocks
        .last()
        .context("seeded chain must include a tip block")?;
    let target_spacing = i64::from(target_spacing_secs);

    let times: Vec<i64> = blocks
        .iter()
        .map(|block| block.header.time.timestamp())
        .collect();
    let deltas: Vec<i64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();

    let (observed_min_spacing_secs, observed_max_spacing_secs) = if deltas.is_empty() {
        (0_u32, 0_u32)
    } else {
        let min_delta = *deltas.iter().min().expect("non-empty deltas has min");
        let max_delta = *deltas.iter().max().expect("non-empty deltas has max");
        if min_delta != target_spacing || max_delta != target_spacing {
            anyhow::bail!(
                "seeded block timestamps do not match target spacing: expected {}s, observed min={}s max={}s",
                target_spacing_secs,
                min_delta,
                max_delta,
            );
        }

        (
            u32::try_from(min_delta).context("negative min seeded spacing is invalid")?,
            u32::try_from(max_delta).context("negative max seeded spacing is invalid")?,
        )
    };

    Ok(SeededTimeSummary {
        seeded_block_count: blocks.len().saturating_sub(1) as u32,
        seeded_genesis_time: first.header.time.timestamp(),
        seeded_tip_time: last.header.time.timestamp(),
        observed_min_spacing_secs,
        observed_max_spacing_secs,
    })
}

impl PremineBundle {
    pub fn manifest(&self) -> &PremineManifest {
        &self.manifest
    }

    pub fn treasury_key(&self) -> &LocalGenesisFundedKey {
        &self.treasury_key
    }

    pub fn funded_keys(&self) -> &[LocalGenesisFundedKey] {
        &self.funded_keys
    }

    pub fn genesis_hex(&self) -> &str {
        &self.genesis_hex
    }

    /// Files to be copied verbatim into `payload/local_genesis/` so live nodes
    /// and downstream tooling (`fund_runtime_keys`) can consume the premine.
    pub fn payload_files(&self) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(vec![
            (
                "genesis.hex".to_string(),
                self.genesis_hex.as_bytes().to_vec(),
            ),
            (
                "premine_blocks.hex".to_string(),
                self.premine_blocks_hex.as_bytes().to_vec(),
            ),
            (
                "checkpoints.txt".to_string(),
                self.checkpoints_content.as_bytes().to_vec(),
            ),
            (
                "manifest.json".to_string(),
                serde_json::to_vec_pretty(&self.manifest)?,
            ),
            (
                "treasury_key.json".to_string(),
                serde_json::to_vec_pretty(&self.treasury_key)?,
            ),
            (
                "funded_keys.json".to_string(),
                serde_json::to_vec_pretty(&self.funded_keys)?,
            ),
        ])
    }
}

/// Generate a fresh premine bundle for `sig`. PoW is disabled during
/// generation (every block header is unsolved), so this is cheap — no
/// Equihash, no shielded proofs. Live nodes enforce PoW at `pow_start_height`,
/// which equals the total seeded block count (genesis + premine + padding).
pub fn generate(sig: &CalibrationSignature) -> Result<PremineBundle> {
    let miner_names: Vec<String> = (0..FUNDED_KEY_COUNT)
        .map(|i| format!("funded-key-{i:03}"))
        .collect();

    let options = LocalTestnetGenesisOptions {
        network_name: "KreskoPremine".to_string(),
        latest_network_upgrade: NetworkUpgrade::Nu6,
        disable_pow: true,
        target_spacing_secs: sig.block_time_secs,
        seeded_tip_time: None,
        maturity_padding_blocks: MATURITY_PADDING_BLOCKS,
        target_difficulty_limit: sig.target_bytes(),
        num_solver_threads: 1,
    };

    let generated = generate_local_testnet_with_funded_keys(miner_names, options)
        .map_err(|e| anyhow::anyhow!("premine generation failed: {e}"))?;

    let network_params = generated
        .network
        .parameters()
        .context("generated premine did not produce testnet parameters")?;
    let activation_height = network_params
        .activation_heights()
        .iter()
        .find_map(|(height, upgrade)| (*upgrade == NetworkUpgrade::Nu6).then_some(height.0))
        .context("missing activation height for NU6")?;

    let genesis_hex = generated
        .genesis_hex()
        .map_err(|e| anyhow::anyhow!("failed to serialize generated genesis block: {e}"))?;

    let funded_keys: Vec<LocalGenesisFundedKey> = generated
        .funded_keys
        .iter()
        .map(|key| LocalGenesisFundedKey {
            name: key.name.clone(),
            secret_key_hex: key.secret_key_hex.clone(),
            public_key_hex: key.public_key_hex.clone(),
            address: key.address.to_string(),
        })
        .collect();
    let treasury = funded_keys
        .first()
        .context("premine produced zero funded keys")?
        .clone();

    let mut premine_blocks_hex = String::new();
    for block in generated.blocks.iter().skip(1) {
        let mut bytes = Vec::new();
        block
            .zcash_serialize(&mut bytes)
            .context("failed to serialize seeded block")?;
        premine_blocks_hex.push_str(&hex::encode(&bytes));
        premine_blocks_hex.push('\n');
    }

    let checkpoints_content = generated
        .checkpoints
        .iter()
        .map(|(height, hash)| format!("{} {}", height.0, hash))
        .collect::<Vec<_>>()
        .join("\n");
    let seeded_tip_hash = generated
        .checkpoints
        .last()
        .map(|(_, hash)| hash.to_string())
        .context("generated premine has no checkpoints")?;
    let pre_blossom_halving_interval: u32 = network_params
        .pre_blossom_halving_interval()
        .try_into()
        .context("pre_blossom_halving_interval does not fit in u32")?;

    let timing_summary = summarize_seeded_block_times(&generated.blocks, sig.block_time_secs)
        .context("generated premine has invalid seeded timestamps")?;

    let manifest = PremineManifest {
        target_difficulty_limit: sig.target_hex.clone(),
        block_time_secs: sig.block_time_secs,
        target_spacing_secs: sig.block_time_secs,
        seeded_block_count: timing_summary.seeded_block_count,
        premine_block_count: FUNDED_KEY_COUNT as u32,
        maturity_padding_block_count: MATURITY_PADDING_BLOCKS,
        disable_pow: network_params.disable_pow(),
        pow_start_height: network_params.pow_start_height().map(|h| h.0),
        genesis_hash: network_params.genesis_hash().to_string(),
        seeded_tip_hash,
        seeded_genesis_time: timing_summary.seeded_genesis_time,
        seeded_tip_time: timing_summary.seeded_tip_time,
        observed_min_spacing_secs: timing_summary.observed_min_spacing_secs,
        observed_max_spacing_secs: timing_summary.observed_max_spacing_secs,
        slow_start_interval: network_params.slow_start_interval().0,
        pre_blossom_halving_interval,
        activation_height,
        treasury_address: treasury.address.clone(),
        treasury_public_key_hex: treasury.public_key_hex.clone(),
        funded_key_count: funded_keys.len() as u32,
    };

    Ok(PremineBundle {
        manifest,
        treasury_key: treasury,
        funded_keys,
        genesis_hex,
        premine_blocks_hex,
        checkpoints_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zebra_chain::local_genesis::generate_local_testnet_with_funded_keys;

    #[test]
    fn signature_rejects_bad_hex() {
        assert!(CalibrationSignature::new("not hex", 75).is_err());
        assert!(CalibrationSignature::new("00", 75).is_err());
        assert!(CalibrationSignature::new("0".repeat(63), 75).is_err());
        assert!(CalibrationSignature::new("0".repeat(65), 75).is_err());
    }

    #[test]
    fn signature_rejects_zero_block_time() {
        let target = "0".repeat(64);
        assert!(CalibrationSignature::new(target, 0).is_err());
    }

    #[test]
    fn signature_normalizes_case_and_whitespace() {
        let sig = CalibrationSignature::new(format!("  {}  ", "0F".repeat(32)), 75).unwrap();
        assert_eq!(sig.target_hex, "0f".repeat(32));
    }

    #[test]
    fn target_bytes_round_trips() {
        let sig = CalibrationSignature::new("0080".to_string() + &"00".repeat(30), 75).unwrap();
        let b = sig.target_bytes();
        assert_eq!(b[0], 0x00);
        assert_eq!(b[1], 0x80);
        for &x in &b[2..] {
            assert_eq!(x, 0);
        }
    }

    #[test]
    fn seeded_timestamp_summary_accepts_matching_spacing() {
        let generated = generate_local_testnet_with_funded_keys(
            vec!["alice".to_string(), "bob".to_string()],
            LocalTestnetGenesisOptions {
                target_spacing_secs: 25,
                maturity_padding_blocks: 2,
                ..Default::default()
            },
        )
        .unwrap();

        let summary = summarize_seeded_block_times(&generated.blocks, 25).unwrap();
        assert_eq!(summary.seeded_block_count, 4);
        assert_eq!(summary.observed_min_spacing_secs, 25);
        assert_eq!(summary.observed_max_spacing_secs, 25);
    }

    /// Generating a fresh premine should produce exactly-uniform spacing and
    /// a tip time close to wall-clock-now. This is the regression test for
    /// the seeded-tip drift bug.
    #[test]
    fn generated_bundle_has_uniform_spacing_anchored_to_now() {
        let target = "0f".repeat(32);
        let sig = CalibrationSignature::new(target, 25).unwrap();

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let bundle = generate(&sig).expect("premine should generate");
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let m = bundle.manifest();
        assert_eq!(m.observed_min_spacing_secs, 25);
        assert_eq!(m.observed_max_spacing_secs, 25);
        assert_eq!(m.block_time_secs, 25);
        assert!(m.disable_pow);
        assert_eq!(
            m.pow_start_height,
            Some(m.seeded_block_count + 1),
            "pow_start_height must equal total block count (genesis + premine + padding)",
        );
        assert!(
            m.seeded_tip_time >= before && m.seeded_tip_time <= after,
            "seeded tip time {} should fall within [{}, {}]",
            m.seeded_tip_time,
            before,
            after,
        );
    }
}
