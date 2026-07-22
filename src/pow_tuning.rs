//! Proof-of-Work parameter calibration for kresko testnets.
//!
//! The only input users typically need to think about is *how many mining
//! CPUs* their experiment has. The calibration multiplies that by a measured
//! per-CPU Equihash solution rate for the configured parameters and produces
//! a `target_difficulty_limit` that puts block production near the configured
//! target spacing from the moment the activation height is reached.
//!
//! The key identity is `target = 2^256 / (S × N × T)`, where `S` is per-CPU
//! sol/s, `N` is the number of mining CPUs, and `T` is the target block
//! spacing. `S` defaults to [`DEFAULT_SOL_PER_SEC_PER_CPU`] and is
//! deliberately biased low: underestimating `S` produces a looser target
//! so the DAA can tighten into steady state, whereas overestimating produces
//! a tighter target that the DAA cannot loosen past (it becomes `pow_limit`)
//! and the chain stalls. Pass `sol_per_sec_override` only if you've measured
//! the mining fleet's actual rate.

use std::{hint::black_box, time::Instant};

use anyhow::{Context, Result};
use hex::FromHex;
use zebra_chain::{
    block::{self, Header},
    fmt::HexDebug,
    parameters::Network,
    work::{
        difficulty::{CompactDifficulty, ExpandedDifficulty, U256},
        equihash::Solution,
    },
};

use crate::config::EquihashParameterSet;

/// Round-tuning presets that govern how the DAA reacts to jitter and miner
/// churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowProfile {
    /// Mainnet-like: 17/11 averaging window, 4x damping, 16%/32% caps.
    /// Smooth, slow to react; good when DAA behavior should not confound
    /// non-PoW experiments.
    #[default]
    Mainnet,
    /// Responsive: 8/6 averaging window, 2x damping, 32%/50% caps. Half the
    /// smoothing, ~2-3x faster reaction to miner churn, at the cost of more
    /// per-block jitter. Use when studying DAA dynamics.
    Responsive,
}

impl std::fmt::Display for PowProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PowProfile::Mainnet => f.write_str("mainnet"),
            PowProfile::Responsive => f.write_str("responsive"),
        }
    }
}

impl std::str::FromStr for PowProfile {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "mainnet" | "mainnet-like" | "default" => Ok(PowProfile::Mainnet),
            "responsive" | "fast" => Ok(PowProfile::Responsive),
            other => anyhow::bail!("unknown pow profile: {other}. Use mainnet or responsive."),
        }
    }
}

/// DAA round-tuning knobs consumed by the in-process PoW simulator.
#[derive(Debug, Clone, Copy)]
pub struct PowRoundTuning {
    pub pow_averaging_window: usize,
    pub pow_median_block_span: usize,
    pub pow_damping_factor: i32,
    pub pow_max_adjust_up_percent: i32,
    pub pow_max_adjust_down_percent: i32,
}

impl PowRoundTuning {
    pub fn from_profile(profile: PowProfile) -> Self {
        match profile {
            PowProfile::Mainnet => Self {
                pow_averaging_window: 17,
                pow_median_block_span: 11,
                pow_damping_factor: 4,
                pow_max_adjust_up_percent: 16,
                pow_max_adjust_down_percent: 32,
            },
            PowProfile::Responsive => Self {
                pow_averaging_window: 8,
                pow_median_block_span: 6,
                pow_damping_factor: 2,
                pow_max_adjust_up_percent: 32,
                pow_max_adjust_down_percent: 50,
            },
        }
    }
}

/// Inputs to the calibration routine.
#[derive(Debug, Clone)]
pub struct PowTuningInputs {
    /// Total number of mining CPUs (single-thread Equihash solvers)
    /// participating in the experiment.
    pub num_miners: usize,
    /// Target block spacing in seconds (post-Blossom).
    pub target_spacing_secs: u32,
    /// Fractional adjustment to the natural target. `+0.10` makes the
    /// target ~10% looser (≈10% faster initial blocks); `-0.10` makes it
    /// ~10% tighter. Applied before flooring to a power of two, so small
    /// values may not shift `target_bits`; use ±0.5 or larger to move by
    /// a full bit.
    pub target_adjust_fraction: f64,
    /// How many bits looser than the natural target to start. Kept for
    /// the pow-simulate path; the experiment-build path leaves this at 0
    /// and uses `target_adjust_fraction` instead.
    pub headroom_bits: u8,
    /// Per-CPU sol/s to calibrate against. Typical callers leave this
    /// `None` to use [`DEFAULT_SOL_PER_SEC_PER_CPU`]; override only when
    /// you've measured the mining fleet's actual rate (used by
    /// `pow-simulate`; experiment-build never overrides).
    pub sol_per_sec_override: Option<f64>,
}

impl Default for PowTuningInputs {
    fn default() -> Self {
        Self {
            num_miners: 1,
            target_spacing_secs: 75,
            target_adjust_fraction: 0.0,
            headroom_bits: 0,
            sol_per_sec_override: None,
        }
    }
}

/// Source of the per-CPU sol/s value used for calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolRateSource {
    /// Value supplied by the caller (e.g. measured on the mining fleet).
    Override,
    /// [`DEFAULT_SOL_PER_SEC_PER_CPU`].
    Default,
}

impl std::fmt::Display for SolRateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolRateSource::Override => f.write_str("override"),
            SolRateSource::Default => f.write_str("default-per-cpu"),
        }
    }
}

/// The calibrated PoW parameters kresko emits into the zebrad testnet TOML.
#[derive(Debug, Clone)]
pub struct PowCalibration {
    /// Hex-encoded 256-bit target difficulty limit (big-endian, no `0x` prefix).
    pub target_difficulty_limit_hex: String,
    /// The target spacing we calibrated for.
    pub target_spacing_secs: u32,
    /// The per-CPU Equihash rate used (solutions/second).
    pub sol_per_sec_per_thread: f64,
    /// Where `sol_per_sec_per_thread` came from.
    pub sol_rate_source: SolRateSource,
    /// Number of mining CPUs calibrated for.
    pub num_miners: usize,
    /// log2 of the natural target before adjust.
    pub natural_target_bits: u32,
    /// Fractional adjustment applied to the natural target.
    pub target_adjust_fraction: f64,
}

/// Calibrate PoW parameters for a kresko testnet.
pub fn calibrate(inputs: &PowTuningInputs) -> Result<PowCalibration> {
    if inputs.num_miners == 0 {
        anyhow::bail!("num_miners must be > 0 to calibrate PoW");
    }
    if inputs.target_spacing_secs == 0 {
        anyhow::bail!("target_spacing_secs must be > 0 to calibrate PoW");
    }
    if inputs.headroom_bits > 32 {
        anyhow::bail!(
            "headroom_bits {} is unreasonably large",
            inputs.headroom_bits
        );
    }
    if !inputs.target_adjust_fraction.is_finite() {
        anyhow::bail!(
            "target_adjust_fraction must be finite, got {}",
            inputs.target_adjust_fraction
        );
    }
    if inputs.target_adjust_fraction <= -1.0 {
        anyhow::bail!(
            "target_adjust_fraction must be > -1.0 (got {}); -1.0 would mean an infinitely loose target",
            inputs.target_adjust_fraction
        );
    }

    let (sol_per_sec, source) = if let Some(s) = inputs.sol_per_sec_override {
        if !s.is_finite() || s <= 0.0 {
            anyhow::bail!("sol_per_sec_override must be a positive finite number, got {s}");
        }
        (s, SolRateSource::Override)
    } else {
        (DEFAULT_SOL_PER_SEC_PER_CPU, SolRateSource::Default)
    };

    let raw_expected_sols =
        sol_per_sec * inputs.num_miners as f64 * inputs.target_spacing_secs as f64;
    // target = natural_target · (1 + adjust)  =>  expected_sols /= (1 + adjust)
    let expected_sols = raw_expected_sols / (1.0 + inputs.target_adjust_fraction);
    if !expected_sols.is_finite() || expected_sols <= 0.0 {
        anyhow::bail!(
            "invalid expected_sols_per_block = {expected_sols} (sol/s={sol_per_sec}, N={}, T={}, adjust={})",
            inputs.num_miners,
            inputs.target_spacing_secs,
            inputs.target_adjust_fraction,
        );
    }

    // target = 2^256 / expected_sols  =>  log2(target) = 256 - log2(expected_sols).
    // Floor so that the resulting target is <= natural target (i.e. slightly
    // tighter than ideal before headroom — headroom makes it looser again).
    let log2_ratio = expected_sols.log2();
    let natural_target_bits = (256.0 - log2_ratio).floor();
    if natural_target_bits < 1.0 {
        anyhow::bail!(
            "calibrated natural_target_bits = {natural_target_bits} is too small; \
             too much hashpower (S × N × T = {expected_sols}) for target_spacing_secs = {}",
            inputs.target_spacing_secs,
        );
    }
    let natural_target_bits = natural_target_bits as u32;

    let raw_target_bits = natural_target_bits.saturating_add(u32::from(inputs.headroom_bits));
    let target_bits = raw_target_bits.min(MAX_SAFE_TARGET_BITS);
    if raw_target_bits > MAX_SAFE_TARGET_BITS {
        eprintln!(
            "warning: calibrated target_bits={} exceeds Zebra's max safe pow_limit \
             (~2^{}); clamping to {}. The fleet is too small for the requested \
             spacing to actually constrain block production — the chain will run \
             at the loosest allowed pow_limit and the DAA will tighten from there.",
            raw_target_bits, MAX_SAFE_TARGET_BITS, MAX_SAFE_TARGET_BITS,
        );
    }
    let target_difficulty_limit_hex = bits_to_hex_target(target_bits);

    Ok(PowCalibration {
        target_difficulty_limit_hex,
        target_spacing_secs: inputs.target_spacing_secs,
        sol_per_sec_per_thread: sol_per_sec,
        sol_rate_source: source,
        num_miners: inputs.num_miners,
        natural_target_bits,
        target_adjust_fraction: inputs.target_adjust_fraction,
    })
}

/// Default per-CPU Equihash (200, 9) solution rate used when the caller
/// doesn't supply `sol_per_sec_override`.
///
/// The bias is intentionally on the low side: the calibrated
/// `target_difficulty_limit` becomes Zebra's `pow_limit`, which is the
/// EASIEST allowed target. The DAA can only tighten from there — it cannot
/// loosen past it. So:
///
/// - **Underestimate** S → pow_limit too loose → initial blocks arrive fast
///   → DAA tightens within its averaging window → **chain converges.**
/// - **Overestimate** S → pow_limit too tight → blocks slower than target
///   and the DAA is clamped at pow_limit → **chain stalls.**
///
/// We'd rather over-produce early blocks than stall the chain, so keep this
/// comfortably below any CPU we might realistically run on (laptops run
/// 2–5 sol/s, modest cloud VMs 0.5–1.5). If you're on something weaker,
/// pass `--pow-sol-per-sec` explicitly.
pub const DEFAULT_SOL_PER_SEC_PER_CPU: f64 = 1.0;

/// Largest `target_bits` the calibrated `target_difficulty_limit` is allowed
/// to take. Zebra's testnet `with_target_difficulty_limit` rejects any value
/// above `U256::MAX / POW_AVERAGING_WINDOW` (where `POW_AVERAGING_WINDOW=17`),
/// which works out to ~`2^251.91`. `1 << 252` overshoots, so 251 is the
/// effective ceiling for the `1 << b` targets [`bits_to_hex_target`] emits.
///
/// When the natural calibration would exceed this, the chain is too tiny for
/// the requested spacing to matter (e.g. a 4-miner toy fleet at 60s spacing).
/// We clamp to the ceiling — equivalent to the regtest-easy default — and let
/// the DAA tighten from there once mining begins.
pub const MAX_SAFE_TARGET_BITS: u32 = 251;

/// Discount applied to locally-measured Common Equihash candidates/sec when
/// estimating fleet per-miner rate.
pub const COMMON_LOCAL_TO_FLEET_DISCOUNT: f64 = 6.0;

/// Discount applied to locally-measured Regtest Equihash candidates/sec when
/// estimating fleet per-miner rate. Regtest has a much cheaper Equihash solve,
/// so per-solution overhead and CPU-class differences dominate more heavily.
pub const REGTEST_LOCAL_TO_FLEET_DISCOUNT: f64 = 16.0;

pub fn default_local_to_fleet_discount(equihash_params: EquihashParameterSet) -> f64 {
    match equihash_params {
        EquihashParameterSet::Common => COMMON_LOCAL_TO_FLEET_DISCOUNT,
        EquihashParameterSet::Regtest => REGTEST_LOCAL_TO_FLEET_DISCOUNT,
    }
}

/// Default minimum duration for the genesis-time local Equihash benchmark.
/// The run can exceed this because a single solver call is not interrupted.
pub const DEFAULT_BENCH_MIN_SECONDS: f64 = 10.0;

/// Inputs to the explicit Equihash solver benchmark.
#[derive(Debug, Clone, Copy)]
pub struct PowBenchInputs {
    /// Which compiled Equihash solver to run.
    pub equihash_params: EquihashParameterSet,
    /// Minimum wallclock time to spend benchmarking. The benchmark may run
    /// longer because a single solver call is not interrupted mid-run.
    pub min_seconds: f64,
}

/// Outcome of [`benchmark_equihash_solver`].
#[derive(Debug, Clone, Copy)]
pub struct PowBenchResult {
    pub equihash_params: EquihashParameterSet,
    /// Number of nonce solver rounds attempted.
    pub nonce_trials: usize,
    /// Number of valid Equihash solutions returned by the solver.
    pub equihash_solutions: usize,
    /// Number of solutions that were run through the same validation/hash path
    /// used by the live miner before applying the difficulty filter.
    pub mining_candidates: usize,
    pub elapsed_secs: f64,
    /// Nonce rounds per second. Useful for comparing raw solver throughput.
    pub nonce_trials_per_sec: f64,
    /// Raw Equihash solutions per second.
    pub sol_per_sec: f64,
    /// Live miner candidate checks per second. This is the rate to feed into
    /// calibration because Regtest's small Equihash solutions make per-solution
    /// validation and header hashing material.
    pub mining_candidates_per_sec: f64,
}

/// Benchmark the same compiled Equihash solver used by live mining.
///
/// This uses the same per-solution validation and header hash path as the live
/// miner, then reports both raw solver output and live mining candidate rate.
pub fn benchmark_equihash_solver(inputs: PowBenchInputs) -> Result<PowBenchResult> {
    if !inputs.min_seconds.is_finite() || inputs.min_seconds <= 0.0 {
        anyhow::bail!(
            "min_seconds must be a positive finite number, got {}",
            inputs.min_seconds
        );
    }

    if inputs.equihash_params != EquihashParameterSet::Common {
        anyhow::bail!("NU7 Zebra only supports common Equihash parameters for this benchmark");
    }
    let network = Network::new_default_testnet();
    let mut header = benchmark_header(&network)?;
    let max_target = ExpandedDifficulty::from(U256::MAX);

    let started = Instant::now();
    let mut next_nonce = 0u64;
    let mut nonce_trials = 0usize;
    let mut equihash_solutions = 0usize;
    let mut mining_candidates = 0usize;

    while started.elapsed().as_secs_f64() < inputs.min_seconds || mining_candidates == 0 {
        let nonce = nonce_from_counter(next_nonce);
        next_nonce = next_nonce.wrapping_add(1);
        header.nonce = HexDebug(nonce);
        nonce_trials = nonce_trials.saturating_add(1);

        let Ok(solved_headers) = Solution::solve(header, || Ok(())) else {
            continue;
        };
        equihash_solutions = equihash_solutions.saturating_add(solved_headers.len());
        for solved_header in solved_headers {
            let hash = solved_header.hash();
            if solved_header.solution.check(&solved_header, &network).is_ok() {
                black_box(hash <= max_target);
                mining_candidates = mining_candidates.saturating_add(1);
            }
        }
    }

    let elapsed_secs = started.elapsed().as_secs_f64();
    if elapsed_secs <= 0.0 || nonce_trials == 0 || mining_candidates == 0 {
        anyhow::bail!(
            "Equihash benchmark produced an unusable measurement (params={}, nonce_trials={}, solutions={}, candidates={}, elapsed={}s)",
            inputs.equihash_params,
            nonce_trials,
            equihash_solutions,
            mining_candidates,
            elapsed_secs,
        );
    }

    Ok(PowBenchResult {
        equihash_params: inputs.equihash_params,
        nonce_trials,
        equihash_solutions,
        mining_candidates,
        elapsed_secs,
        nonce_trials_per_sec: nonce_trials as f64 / elapsed_secs,
        sol_per_sec: equihash_solutions as f64 / elapsed_secs,
        mining_candidates_per_sec: mining_candidates as f64 / elapsed_secs,
    })
}

fn benchmark_header(_network: &Network) -> Result<Header> {
    Ok(Header {
        version: 4,
        previous_block_hash: block::Hash([0; 32]),
        merkle_root: block::merkle::Root([0; 32]),
        commitment_bytes: HexDebug([0; 32]),
        time: chrono::DateTime::from_timestamp(0, 0).context("invalid benchmark timestamp")?,
        difficulty_threshold: CompactDifficulty::from_hex("207fffff")
            .map_err(|e| anyhow::anyhow!("invalid benchmark difficulty: {e}"))?,
        nonce: HexDebug([0; 32]),
        solution: Solution::for_proposal(),
    })
}

fn nonce_from_counter(counter: u64) -> [u8; 32] {
    let mut nonce = [0u8; 32];
    nonce[24..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// Outcome of [`measure_local_sol_per_sec`].
#[derive(Debug, Clone, Copy)]
pub struct MeasuredSolRate {
    /// Which Equihash parameters were measured.
    pub equihash_params: EquihashParameterSet,
    /// Equihash solves per second observed on this machine, single-threaded.
    pub local_sol_per_sec: f64,
    /// Conservative estimate of fleet per-CPU sol/s
    /// (`local / fleet_discount`).
    pub assumed_fleet_sol_per_sec: f64,
    /// Discount applied to the local measurement.
    pub fleet_discount: f64,
    /// Total valid Equihash solutions counted across the benchmark.
    pub total_solves: usize,
    /// Wallclock seconds the benchmark consumed end-to-end.
    pub elapsed_secs: f64,
}

/// Time how fast this machine's single-threaded Equihash solver is for the
/// configured parameter set. The returned [`MeasuredSolRate`] carries both the
/// raw local rate and the discounted fleet estimate that callers should pass
/// into [`calibrate`].
pub fn measure_local_sol_per_sec(
    equihash_params: EquihashParameterSet,
    min_seconds: f64,
    fleet_discount_override: Option<f64>,
) -> Result<MeasuredSolRate> {
    if let Some(discount) = fleet_discount_override {
        if !discount.is_finite() || discount <= 0.0 {
            anyhow::bail!("fleet discount must be a positive finite number, got {discount}");
        }
    }
    let result = benchmark_equihash_solver(PowBenchInputs {
        equihash_params,
        min_seconds,
    })?;
    let elapsed_secs = result.elapsed_secs;
    let total_solves = result.mining_candidates;

    let local_sol_per_sec = result.mining_candidates_per_sec;
    let fleet_discount =
        fleet_discount_override.unwrap_or_else(|| default_local_to_fleet_discount(equihash_params));
    Ok(MeasuredSolRate {
        equihash_params,
        local_sol_per_sec,
        assumed_fleet_sol_per_sec: local_sol_per_sec / fleet_discount,
        fleet_discount,
        total_solves,
        elapsed_secs,
    })
}

/// Convert a bit position `b ∈ [0, 255]` into a 32-byte hex string
/// representing `1u256 << b` in big-endian display order.
fn bits_to_hex_target(b: u32) -> String {
    let b = b.min(255);
    // byte 0 holds bits 248..=255 (big-endian), byte 31 holds bits 0..=7.
    let byte_idx = 31 - (b / 8) as usize;
    let bit_in_byte = (b % 8) as u8;
    let mut bytes = [0u8; 32];
    bytes[byte_idx] = 1u8 << bit_in_byte;
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_to_hex_target_matches_plan_examples() {
        // From `art/inbox/initial_tuning_pow_kresko.md` worked-examples table.
        let prefix = |s: &str| s.chars().take(4).collect::<String>();

        // target_bits = 246  ->  "0040..."
        assert_eq!(prefix(&bits_to_hex_target(246)), "0040");
        // target_bits = 244  ->  "0010..."
        assert_eq!(prefix(&bits_to_hex_target(244)), "0010");
        // target_bits = 248  ->  "0100..."
        assert_eq!(prefix(&bits_to_hex_target(248)), "0100");
        // target_bits = 255  ->  "8000..."
        assert_eq!(prefix(&bits_to_hex_target(255)), "8000");
    }

    #[test]
    fn bits_to_hex_target_length_is_64_hex_chars() {
        for &b in &[1u32, 128, 200, 246, 255] {
            assert_eq!(bits_to_hex_target(b).len(), 64);
        }
    }

    #[test]
    fn calibrate_matches_plan_worked_example_25s_fast_cpu() {
        // 80 miners, 25s target, S = 2.0  ->  expected_sols = 4000,
        // natural_target_bits = 244, with headroom_bits = 2 -> 246 ("0040...").
        let c = calibrate(&PowTuningInputs {
            num_miners: 80,
            target_spacing_secs: 25,
            headroom_bits: 2,
            sol_per_sec_override: Some(2.0),
            ..Default::default()
        })
        .expect("calibration should succeed");
        assert_eq!(c.natural_target_bits, 244);
        assert_eq!(&c.target_difficulty_limit_hex[..4], "0040");
        assert_eq!(c.sol_rate_source, SolRateSource::Override);
    }

    #[test]
    fn calibrate_matches_plan_worked_example_75s_slow_cpu() {
        // 80 miners, 75s target, S = 0.5  ->  expected_sols = 3000,
        // natural_target_bits = 244, with headroom_bits = 2 -> 246 ("0040...").
        let c = calibrate(&PowTuningInputs {
            num_miners: 80,
            target_spacing_secs: 75,
            headroom_bits: 2,
            sol_per_sec_override: Some(0.5),
            ..Default::default()
        })
        .expect("calibration should succeed");
        assert_eq!(c.natural_target_bits, 244);
        assert_eq!(&c.target_difficulty_limit_hex[..4], "0040");
    }

    #[test]
    fn calibrate_matches_plan_worked_example_75s_fast_cpu() {
        // 80 miners, 75s target, S = 2.0  ->  expected_sols = 12000,
        // natural_target_bits = 242, with headroom_bits = 2 -> 244 ("0010...").
        let c = calibrate(&PowTuningInputs {
            num_miners: 80,
            target_spacing_secs: 75,
            headroom_bits: 2,
            sol_per_sec_override: Some(2.0),
            ..Default::default()
        })
        .expect("calibration should succeed");
        assert_eq!(c.natural_target_bits, 242);
        assert_eq!(&c.target_difficulty_limit_hex[..4], "0010");
    }

    #[test]
    fn calibrate_rejects_zero_miners() {
        let err = calibrate(&PowTuningInputs {
            num_miners: 0,
            sol_per_sec_override: Some(1.0),
            ..Default::default()
        })
        .expect_err("zero miners should fail");
        assert!(err.to_string().contains("num_miners"));
    }

    #[test]
    fn calibrate_rejects_zero_target_spacing() {
        let err = calibrate(&PowTuningInputs {
            num_miners: 1,
            target_spacing_secs: 0,
            sol_per_sec_override: Some(1.0),
            ..Default::default()
        })
        .expect_err("zero target spacing should fail");
        assert!(err.to_string().contains("target_spacing_secs"));
    }

    #[test]
    fn mainnet_profile_matches_mainnet_daa_constants() {
        let rt = PowRoundTuning::from_profile(PowProfile::Mainnet);
        assert_eq!(rt.pow_averaging_window, 17);
        assert_eq!(rt.pow_median_block_span, 11);
        assert_eq!(rt.pow_damping_factor, 4);
        assert_eq!(rt.pow_max_adjust_up_percent, 16);
        assert_eq!(rt.pow_max_adjust_down_percent, 32);
    }

    #[test]
    fn responsive_profile_is_snappier_than_mainnet() {
        let main = PowRoundTuning::from_profile(PowProfile::Mainnet);
        let resp = PowRoundTuning::from_profile(PowProfile::Responsive);
        assert!(resp.pow_averaging_window < main.pow_averaging_window);
        assert!(resp.pow_damping_factor < main.pow_damping_factor);
        assert!(resp.pow_max_adjust_up_percent > main.pow_max_adjust_up_percent);
        assert!(resp.pow_max_adjust_down_percent > main.pow_max_adjust_down_percent);
    }

    #[test]
    fn calibrate_clamps_at_max_safe_target_bits_for_tiny_fleets() {
        // Tiny fleet × very low sol/s would naturally calibrate above the
        // Zebra ceiling. Verify we clamp instead of producing an unrepresentable target.
        let c = calibrate(&PowTuningInputs {
            num_miners: 4,
            target_spacing_secs: 60,
            sol_per_sec_override: Some(0.029),
            ..Default::default()
        })
        .expect("calibration should clamp, not fail");
        // 4 × 60 × 0.029 = 6.96; floor(256 - log2(6.96)) = 253; clamped to 251.
        assert!(c.natural_target_bits >= MAX_SAFE_TARGET_BITS + 1);
        // Resulting hex is `1 << 251` = "0800...".
        assert_eq!(&c.target_difficulty_limit_hex[..4], "0800");
    }

    #[test]
    fn calibrate_uses_default_rate_when_no_override() {
        let c = calibrate(&PowTuningInputs {
            num_miners: 6,
            target_spacing_secs: 25,
            ..Default::default()
        })
        .expect("calibration should succeed");
        assert_eq!(c.sol_rate_source, SolRateSource::Default);
        assert_eq!(c.sol_per_sec_per_thread, DEFAULT_SOL_PER_SEC_PER_CPU);
    }

    #[test]
    fn local_measurement_uses_requested_equihash_params() {
        let measured = measure_local_sol_per_sec(EquihashParameterSet::Regtest, 0.001, None)
            .expect("regtest solver benchmark should produce a measurement");

        assert_eq!(measured.equihash_params, EquihashParameterSet::Regtest);
        assert!(measured.total_solves > 0);
        assert!(measured.local_sol_per_sec > 0.0);
        assert_eq!(
            measured.fleet_discount,
            default_local_to_fleet_discount(EquihashParameterSet::Regtest)
        );
    }
}
