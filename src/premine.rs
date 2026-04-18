//! Premine generation, caching, and loading.
//!
//! When `mining_mode == Pow`, kresko seeds the live network with a precomputed
//! chain of genesis + premine + maturity-padding blocks. That chain must be
//! mined against the exact `pow_limit` the live zebrad config will advertise,
//! otherwise zebrad rejects the chain at height 0 (see
//! `art/inbox/refactor_pre_mine_generation.md`).
//!
//! This module is the **only** path by which a premine enters the PoW genesis
//! flow. There is no flag, mode, or env var that produces a premine by another
//! route. The core of the design is:
//!
//! - [`CalibrationSignature`] is the cache key: `(target_hex, block_time_secs)`.
//!   `target_hex` determines the chain bytes; `block_time_secs` is a correctness
//!   fence so we never reuse a premine across experiments whose live networks
//!   advertise different target spacing.
//! - [`resolve_premine`] returns a [`PremineBundle`] that is guaranteed to
//!   match the signature (verified by reading the written manifest back). On
//!   cache miss or any mismatch, it regenerates.
//! - [`generate`] mines the chain in-process and writes a fully-populated cache
//!   entry (genesis, premine blocks, checkpoints, manifest, funded keys).
//!
//! The generated premine has a *fixed* number of funded keys
//! ([`FUNDED_KEY_COUNT`]) followed by [`MATURITY_PADDING_BLOCKS`] empty blocks;
//! the fixed size means the signature does not need to vary with the experiment's
//! miner count. Any experiment up to `FUNDED_KEY_COUNT` miners can share the
//! same cache entry.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use zebra_chain::{
    block::Block,
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::{ZcashDeserialize, ZcashSerialize},
};

use crate::config::LocalGenesisFundedKey;

/// Number of funded premine keys produced by every cache entry.
pub const FUNDED_KEY_COUNT: usize = 128;

/// Empty blocks appended after the funded premine so every premine coinbase
/// is spendable well before experiments begin. Zcash coinbase maturity is
/// 100 blocks; 128 leaves a comfortable margin.
pub const MATURITY_PADDING_BLOCKS: u32 = 128;

/// Default premine cache key used by `kresko genesis` when the caller does
/// not pass `--premine-cache-key`. Encodes `<block_time_secs>-<funded_key_count>`
/// of the canonical premine bundle shipped with the repo.
///
/// Generating a fresh premine for every experiment is prohibitively slow
/// (Equihash mines hundreds of blocks against a tight target); the workflow
/// is "pre-mine once, reuse many times". The default lets the common case
/// (block_time_secs=25, 128 funded keys) work without any flag.
pub const DEFAULT_CACHE_KEY: &str = "25-128";

/// The inputs that determine which premine a PoW experiment needs. Two
/// experiments with the same signature share the same cache entry; two with
/// different signatures get different cache entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationSignature {
    /// Big-endian `pow_limit` as 64 lowercase hex characters (no `0x`). This
    /// is the loosest target allowed by the generated network and every
    /// premine block is mined against it.
    pub target_hex: String,
    /// Target block spacing the live network will advertise, in seconds.
    /// Not baked into chain bytes, but part of the cache key so mismatched
    /// spacing cannot silently reuse a premine.
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

    /// Stable hex key derived from the signature fields. First 16 hex chars of
    /// SHA-256; short enough to be a readable directory name.
    pub fn cache_key(&self) -> String {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(self.target_hex.as_bytes());
        hasher.update(b"|");
        hasher.update(self.block_time_secs.to_string().as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..8])
    }

    pub fn cache_dir(&self, root: &Path) -> PathBuf {
        root.join(self.cache_key())
    }
}

/// On-disk description of a single cache entry. Written next to the premine
/// artifacts so the bundle can be loaded and cross-validated against the
/// signature that generated it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PremineManifest {
    pub cache_key: String,
    pub target_difficulty_limit: String,
    pub block_time_secs: u32,
    pub target_spacing_secs: u32,
    pub seeded_block_count: u32,
    pub premine_block_count: u32,
    pub maturity_padding_block_count: u32,
    pub disable_pow: bool,
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
    pub notes: String,
}

/// An in-memory view of a loaded premine cache entry.
#[derive(Debug, Clone)]
pub struct PremineBundle {
    root_dir: PathBuf,
    manifest: PremineManifest,
    treasury_key: LocalGenesisFundedKey,
    funded_keys: Vec<LocalGenesisFundedKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeededTimeSummary {
    seeded_block_count: u32,
    seeded_genesis_time: i64,
    seeded_tip_time: i64,
    observed_min_spacing_secs: u32,
    observed_max_spacing_secs: u32,
}

fn decode_hex_block(hex_str: &str, context: &str) -> Result<Block> {
    let block_bytes = hex::decode(hex_str.trim())
        .with_context(|| format!("failed to decode hex block for {context}"))?;
    Block::zcash_deserialize(&block_bytes[..])
        .with_context(|| format!("failed to deserialize block for {context}"))
}

fn load_seeded_blocks_from_dir(cache_dir: &Path) -> Result<Vec<Block>> {
    let genesis_hex_path = cache_dir.join("genesis.hex");
    let premine_hex_path = cache_dir.join("premine_blocks.hex");

    let genesis_hex = std::fs::read_to_string(&genesis_hex_path)
        .with_context(|| format!("failed to read {}", genesis_hex_path.display()))?;
    let premine_hex = std::fs::read_to_string(&premine_hex_path)
        .with_context(|| format!("failed to read {}", premine_hex_path.display()))?;

    let mut blocks = vec![decode_hex_block(
        &genesis_hex,
        &genesis_hex_path.display().to_string(),
    )?];
    for (line_idx, line) in premine_hex.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        blocks.push(decode_hex_block(
            trimmed,
            &format!("{} line {}", premine_hex_path.display(), line_idx + 1),
        )?);
    }

    Ok(blocks)
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

fn validate_manifest_timestamps(
    manifest: &PremineManifest,
    blocks: &[Block],
) -> Result<SeededTimeSummary> {
    if manifest.block_time_secs != manifest.target_spacing_secs {
        anyhow::bail!(
            "premine manifest spacing mismatch: block_time_secs={} target_spacing_secs={}",
            manifest.block_time_secs,
            manifest.target_spacing_secs,
        );
    }

    let summary = summarize_seeded_block_times(blocks, manifest.target_spacing_secs)?;

    if summary.seeded_block_count != manifest.seeded_block_count {
        anyhow::bail!(
            "premine manifest seeded_block_count {} disagrees with serialized chain {}",
            manifest.seeded_block_count,
            summary.seeded_block_count,
        );
    }
    if summary.seeded_genesis_time != manifest.seeded_genesis_time {
        anyhow::bail!(
            "premine manifest seeded_genesis_time {} disagrees with serialized chain {}",
            manifest.seeded_genesis_time,
            summary.seeded_genesis_time,
        );
    }
    if summary.seeded_tip_time != manifest.seeded_tip_time {
        anyhow::bail!(
            "premine manifest seeded_tip_time {} disagrees with serialized chain {}",
            manifest.seeded_tip_time,
            summary.seeded_tip_time,
        );
    }
    if summary.observed_min_spacing_secs != manifest.observed_min_spacing_secs
        || summary.observed_max_spacing_secs != manifest.observed_max_spacing_secs
    {
        anyhow::bail!(
            "premine manifest observed spacing [{}, {}] disagrees with serialized chain [{}, {}]",
            manifest.observed_min_spacing_secs,
            manifest.observed_max_spacing_secs,
            summary.observed_min_spacing_secs,
            summary.observed_max_spacing_secs,
        );
    }

    Ok(summary)
}

impl PremineBundle {
    pub fn load_from_dir(cache_dir: &Path) -> Result<Self> {
        let manifest_path = cache_dir.join("manifest.json");
        let treasury_key_path = cache_dir.join("treasury_key.json");
        let funded_keys_path = cache_dir.join("funded_keys.json");

        let manifest: PremineManifest = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("failed to read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

        let treasury_key: LocalGenesisFundedKey = serde_json::from_slice(
            &std::fs::read(&treasury_key_path)
                .with_context(|| format!("failed to read {}", treasury_key_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", treasury_key_path.display()))?;

        let funded_keys: Vec<LocalGenesisFundedKey> = serde_json::from_slice(
            &std::fs::read(&funded_keys_path)
                .with_context(|| format!("failed to read {}", funded_keys_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", funded_keys_path.display()))?;

        if manifest.treasury_address != treasury_key.address {
            anyhow::bail!(
                "premine treasury address mismatch between manifest and treasury_key.json in {}",
                cache_dir.display()
            );
        }
        if manifest.treasury_public_key_hex != treasury_key.public_key_hex {
            anyhow::bail!(
                "premine treasury public key mismatch between manifest and treasury_key.json in {}",
                cache_dir.display()
            );
        }
        if funded_keys.is_empty() || funded_keys[0].address != treasury_key.address {
            anyhow::bail!(
                "premine funded_keys[0] must equal treasury_key in {}",
                cache_dir.display()
            );
        }
        if (funded_keys.len() as u32) != manifest.funded_key_count {
            anyhow::bail!(
                "premine funded_keys.json length {} disagrees with manifest.funded_key_count {}",
                funded_keys.len(),
                manifest.funded_key_count
            );
        }
        let blocks = load_seeded_blocks_from_dir(cache_dir)?;
        validate_manifest_timestamps(&manifest, &blocks).with_context(|| {
            format!(
                "premine cache entry at {} failed timestamp validation",
                cache_dir.display()
            )
        })?;

        Ok(Self {
            root_dir: cache_dir.to_path_buf(),
            manifest,
            treasury_key,
            funded_keys,
        })
    }

    pub fn manifest(&self) -> &PremineManifest {
        &self.manifest
    }

    pub fn treasury_key(&self) -> &LocalGenesisFundedKey {
        &self.treasury_key
    }

    pub fn funded_keys(&self) -> &[LocalGenesisFundedKey] {
        &self.funded_keys
    }

    pub fn read_text_file(&self, file_name: &str) -> Result<String> {
        let path = self.root_dir.join(file_name);
        std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
    }

    pub fn copy_payload_files_to_vec(
        &self,
        destination: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<()> {
        for file_name in [
            "genesis.hex",
            "premine_blocks.hex",
            "checkpoints.txt",
            "manifest.json",
            "treasury_key.json",
            "funded_keys.json",
        ] {
            let path = self.root_dir.join(file_name);
            destination.push((
                file_name.to_string(),
                std::fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            ));
        }
        Ok(())
    }
}

/// Outcome of a [`resolve_premine`] call. Useful for the caller's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// An existing cache entry matched the signature and was loaded.
    Hit,
    /// The cache missed (or held a corrupt / mismatched entry), so a premine
    /// was generated.
    Miss,
}

impl std::fmt::Display for ResolveOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveOutcome::Hit => f.write_str("hit"),
            ResolveOutcome::Miss => f.write_str("miss"),
        }
    }
}

/// Try to load an existing cache entry by name. Returns `Ok(None)` if no
/// entry exists at `cache_root/key/`; returns `Err` if an entry exists but
/// fails to parse (callers should treat that as fatal — silently regenerating
/// over a corrupt entry hides bugs).
pub fn try_load_by_key(cache_root: &Path, key: &str) -> Result<Option<PremineBundle>> {
    let cache_dir = cache_root.join(key);
    if !cache_dir.join("manifest.json").exists() {
        return Ok(None);
    }
    let bundle = PremineBundle::load_from_dir(&cache_dir).with_context(|| {
        format!(
            "premine cache entry at {} exists but failed to load",
            cache_dir.display()
        )
    })?;
    Ok(Some(bundle))
}

/// Get a [`PremineBundle`] matching `sig`, generating it if necessary, under
/// the cache directory named `key` (typically the signature's hash key, but
/// callers may supply a stable human-readable name like `"25-128"`).
///
/// Guarantees: the returned bundle's manifest has `target_difficulty_limit`
/// equal to `sig.target_hex` and `block_time_secs` equal to `sig.block_time_secs`.
/// Any other state on disk under `cache_root/key/` is deleted and regenerated.
pub fn resolve_premine_with_key(
    sig: &CalibrationSignature,
    key: &str,
    cache_root: &Path,
    num_solver_threads: usize,
) -> Result<(PremineBundle, ResolveOutcome)> {
    let cache_dir = cache_root.join(key);
    let manifest_path = cache_dir.join("manifest.json");

    if manifest_path.exists() {
        match PremineBundle::load_from_dir(&cache_dir) {
            Ok(bundle) => {
                let m = bundle.manifest();
                if m.target_difficulty_limit == sig.target_hex
                    && m.block_time_secs == sig.block_time_secs
                    && m.target_spacing_secs == sig.block_time_secs
                {
                    return Ok((bundle, ResolveOutcome::Hit));
                }
                eprintln!(
                    "premine cache entry at {} disagrees with signature (\
                     manifest target={}, block_time_secs={}; sig target={}, block_time_secs={}); regenerating",
                    cache_dir.display(),
                    m.target_difficulty_limit,
                    m.block_time_secs,
                    sig.target_hex,
                    sig.block_time_secs,
                );
            }
            Err(e) => {
                eprintln!(
                    "premine cache entry at {} failed to load ({e}); regenerating",
                    cache_dir.display()
                );
            }
        }
    }

    if cache_dir.exists() {
        std::fs::remove_dir_all(&cache_dir).with_context(|| {
            format!(
                "failed to clear stale cache entry at {}",
                cache_dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create cache dir at {}", cache_dir.display()))?;

    generate(sig, key, &cache_dir, num_solver_threads)?;
    let bundle = PremineBundle::load_from_dir(&cache_dir)?;
    if bundle.manifest().target_difficulty_limit != sig.target_hex {
        anyhow::bail!(
            "internal error: freshly generated premine at {} has target {} but signature demanded {}",
            cache_dir.display(),
            bundle.manifest().target_difficulty_limit,
            sig.target_hex,
        );
    }
    Ok((bundle, ResolveOutcome::Miss))
}

/// Backwards-compatible variant of [`resolve_premine_with_key`] that uses
/// the signature's hash-derived cache key.
pub fn resolve_premine(
    sig: &CalibrationSignature,
    cache_root: &Path,
    num_solver_threads: usize,
) -> Result<(PremineBundle, ResolveOutcome)> {
    let key = sig.cache_key();
    resolve_premine_with_key(sig, &key, cache_root, num_solver_threads)
}

/// Mine a fresh premine for `sig` and write the full cache layout into
/// `output_dir`. The directory must already exist and should be empty; the
/// caller is responsible for creating and clearing it (see [`resolve_premine`]).
///
/// `num_solver_threads` controls how many OS threads search disjoint nonce
/// partitions per block. `1` mines single-threaded; higher counts cut wallclock
/// roughly linearly until Equihash's ~144 MB-per-thread memory footprint
/// saturates RAM bandwidth (typically around 8–16 threads).
pub fn generate(
    sig: &CalibrationSignature,
    cache_key: &str,
    output_dir: &Path,
    num_solver_threads: usize,
) -> Result<()> {
    let miner_names: Vec<String> = (0..FUNDED_KEY_COUNT)
        .map(|i| format!("funded-key-{i:03}"))
        .collect();

    let options = LocalTestnetGenesisOptions {
        network_name: "KreskoPremine".to_string(),
        latest_network_upgrade: NetworkUpgrade::Nu6,
        disable_pow: false,
        target_spacing_secs: sig.block_time_secs,
        seeded_tip_time: None,
        maturity_padding_blocks: MATURITY_PADDING_BLOCKS,
        target_difficulty_limit: sig.target_bytes(),
        num_solver_threads,
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
        cache_key: cache_key.to_string(),
        target_difficulty_limit: sig.target_hex.clone(),
        block_time_secs: sig.block_time_secs,
        target_spacing_secs: sig.block_time_secs,
        seeded_block_count: timing_summary.seeded_block_count,
        premine_block_count: FUNDED_KEY_COUNT as u32,
        maturity_padding_block_count: MATURITY_PADDING_BLOCKS,
        disable_pow: network_params.disable_pow(),
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
        notes: format!(
            "Kresko premine for target={}, block_time_secs={}, tip_anchored=true. {} funded keys, {} maturity padding blocks.",
            sig.target_hex,
            sig.block_time_secs,
            funded_keys.len(),
            MATURITY_PADDING_BLOCKS,
        ),
    };

    std::fs::write(output_dir.join("genesis.hex"), &genesis_hex)?;
    std::fs::write(output_dir.join("premine_blocks.hex"), &premine_blocks_hex)?;
    std::fs::write(output_dir.join("checkpoints.txt"), &checkpoints_content)?;
    std::fs::write(
        output_dir.join("treasury_key.json"),
        serde_json::to_vec_pretty(&treasury)?,
    )?;
    std::fs::write(
        output_dir.join("funded_keys.json"),
        serde_json::to_vec_pretty(&funded_keys)?,
    )?;
    std::fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(())
}

/// Sensible default for `num_solver_threads`: detect available CPUs, cap at 8.
/// Equihash (200, 9) needs ~144 MB per thread, so 8 threads is roughly the
/// point where additional cores stop helping due to RAM bandwidth saturation.
pub fn default_solver_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}

/// Default cache root for resolve_premine: repo-local, gitignored.
pub fn default_cache_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bootstrap")
        .join("cache")
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
    fn cache_key_stable_and_differs_across_inputs() {
        let a = CalibrationSignature::new("0f".repeat(32), 75).unwrap();
        let b = CalibrationSignature::new("0f".repeat(32), 75).unwrap();
        let c = CalibrationSignature::new("0f".repeat(32), 60).unwrap();
        let d = CalibrationSignature::new("08".repeat(32), 75).unwrap();
        assert_eq!(a.cache_key(), b.cache_key());
        assert_ne!(a.cache_key(), c.cache_key());
        assert_ne!(a.cache_key(), d.cache_key());
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

    #[test]
    fn manifest_timestamp_validation_fails_if_spacing_disagrees() {
        let generated = generate_local_testnet_with_funded_keys(
            vec!["alice".to_string()],
            LocalTestnetGenesisOptions {
                target_spacing_secs: 25,
                maturity_padding_blocks: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let summary = summarize_seeded_block_times(&generated.blocks, 25).unwrap();
        let manifest = PremineManifest {
            cache_key: "test".to_string(),
            target_difficulty_limit: "0f".repeat(32),
            block_time_secs: 24,
            target_spacing_secs: 24,
            seeded_block_count: summary.seeded_block_count,
            premine_block_count: 1,
            maturity_padding_block_count: 1,
            disable_pow: true,
            genesis_hash: "00".repeat(32),
            seeded_tip_hash: "11".repeat(32),
            seeded_genesis_time: summary.seeded_genesis_time,
            seeded_tip_time: summary.seeded_tip_time,
            observed_min_spacing_secs: 24,
            observed_max_spacing_secs: 24,
            slow_start_interval: 0,
            pre_blossom_halving_interval: 144,
            activation_height: 3,
            treasury_address: "tmTest".to_string(),
            treasury_public_key_hex: "02".repeat(33),
            funded_key_count: 1,
            notes: "test".to_string(),
        };

        let err = validate_manifest_timestamps(&manifest, &generated.blocks).unwrap_err();
        assert!(err.to_string().contains("target spacing"));
    }
}
