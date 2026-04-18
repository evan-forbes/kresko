//! Monte Carlo-friendly reimplementation of the Zcash Difficulty Adjustment
//! Algorithm.
//!
//! Mirrors the math in `zebra-state/src/service/check/difficulty.rs` closely
//! enough that simulation results are representative, but trades some
//! CompactDifficulty round-trip fidelity for simplicity. For the purpose of
//! "will this calibration give me stable block times?" that's a fine trade.

use zebra_chain::work::difficulty::U256;

use crate::pow_tuning::PowRoundTuning;

/// One historical block observed by the simulator. Stored oldest-first.
#[derive(Debug, Clone, Copy)]
pub struct BlockState {
    /// Simulation time (seconds) at which this block was mined.
    pub time_secs: f64,
    /// The `difficulty_threshold` this block was mined at.
    pub target: U256,
    /// Wall-clock time since the previous block, for statistics.
    pub block_time_secs: f64,
    /// How many siblings this block's height had (orphans).
    pub orphan_siblings: u32,
}

/// Parameters the DAA needs. Stable across a simulation run.
#[derive(Debug, Clone)]
pub struct DaaParams {
    pub target_spacing_secs: u32,
    pub pow_limit: U256,
    pub tuning: PowRoundTuning,
}

impl DaaParams {
    /// Total number of historical blocks the DAA inspects
    /// (`pow_averaging_window + pow_median_block_span`).
    pub fn context_len(&self) -> usize {
        self.tuning.pow_averaging_window + self.tuning.pow_median_block_span
    }
}

/// Compute the expected `difficulty_threshold` for the next block, given the
/// previous blocks in chronological order (oldest first).
///
/// Returns `params.pow_limit` if we don't have enough history yet.
pub fn expected_target(history: &[BlockState], params: &DaaParams) -> U256 {
    let avg = params.tuning.pow_averaging_window;
    let span = params.tuning.pow_median_block_span;
    let total = avg + span;

    if history.len() < total {
        return params.pow_limit;
    }

    let recent = &history[history.len() - total..];

    // Zebra averages the `avg` newest targets (drops the oldest `span`).
    // Divide each contribution first to avoid a 256-bit overflow when targets
    // are close to 2^256 (e.g. pow_limit = 2^252 with avg=17 blocks would
    // overflow when summed). This matches zebra's intent; the small precision
    // loss is inherent to integer DAA arithmetic.
    let avg_u = U256::from(avg);
    let mut mean_target = U256::zero();
    for block in &recent[span..] {
        mean_target += block.target / avg_u;
    }

    // `newer_median` is the median of the last `span` block times.
    let newer_times: Vec<f64> = recent[avg..].iter().map(|b| b.time_secs).collect();
    let older_times: Vec<f64> = recent[..span].iter().map(|b| b.time_secs).collect();
    let newer_median = median_time(&newer_times);
    let older_median = median_time(&older_times);
    let actual_timespan_secs = newer_median - older_median;

    let expected_timespan_secs = avg as f64 * params.target_spacing_secs as f64;

    // Damping: `expected + (actual - expected) / damping`.
    let damped = expected_timespan_secs
        + (actual_timespan_secs - expected_timespan_secs) / params.tuning.pow_damping_factor as f64;

    // Bounds
    let up_pct = params.tuning.pow_max_adjust_up_percent as f64;
    let down_pct = params.tuning.pow_max_adjust_down_percent as f64;
    let min_timespan = expected_timespan_secs * (100.0 - up_pct) / 100.0;
    let max_timespan = expected_timespan_secs * (100.0 + down_pct) / 100.0;
    let bounded = damped.clamp(min_timespan, max_timespan);

    // Zebra does `(mean_target / expected) * bounded` in 256-bit integer
    // arithmetic. We follow the same divide-then-multiply order to match the
    // precision characteristics.
    let expected_int = expected_timespan_secs.round().max(1.0) as u64;
    let bounded_int = bounded.round().max(0.0) as u64;

    let scaled = (mean_target / U256::from(expected_int)) * U256::from(bounded_int);

    scaled.min(params.pow_limit)
}

/// Median of a small set of timestamps. Matches zebra's median: sorted
/// ascending, picks the middle element at `len / 2` (upper-middle for even
/// lengths).
pub fn median_time(times: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = times.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[sorted.len() / 2]
}

/// Parse a 64-character hex string (big-endian, no `0x` prefix) into a U256.
pub fn parse_pow_limit_hex(hex_str: &str) -> Result<U256, String> {
    let trimmed = hex_str.trim().trim_start_matches("0x");
    if trimmed.len() != 64 {
        return Err(format!(
            "target_difficulty_limit must be 64 hex chars (256 bits), got {}",
            trimmed.len()
        ));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let start = i * 2;
        *byte = u8::from_str_radix(&trimmed[start..start + 2], 16)
            .map_err(|e| format!("invalid hex in target_difficulty_limit: {e}"))?;
    }
    Ok(U256::from_big_endian(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pow_tuning::{PowProfile, PowRoundTuning};

    fn make_params() -> DaaParams {
        DaaParams {
            target_spacing_secs: 25,
            pow_limit: U256::from(1u64) << 246,
            tuning: PowRoundTuning::from_profile(PowProfile::Mainnet),
        }
    }

    #[test]
    fn insufficient_history_returns_pow_limit() {
        let params = make_params();
        let target = expected_target(&[], &params);
        assert_eq!(target, params.pow_limit);
    }

    #[test]
    fn on_target_blocks_stay_at_pow_limit() {
        // If all blocks are at the pow_limit and at exactly `target_spacing`
        // cadence, the DAA should compute a target very close to pow_limit.
        // (Exactly-equal check fails because of the divide-then-multiply
        // integer-precision loss that zebra normally recovers with a
        // CompactDifficulty round-trip; this simulator skips that round-trip.)
        let params = make_params();
        let mut history = Vec::new();
        for i in 0..params.context_len() {
            history.push(BlockState {
                time_secs: i as f64 * params.target_spacing_secs as f64,
                target: params.pow_limit,
                block_time_secs: params.target_spacing_secs as f64,
                orphan_siblings: 0,
            });
        }
        let target = expected_target(&history, &params);
        // Accept up to ~1 part per million of precision loss.
        let tolerance = params.pow_limit >> 20;
        let lo = params.pow_limit - tolerance;
        assert!(
            target >= lo && target <= params.pow_limit,
            "target {target:?} not within tolerance of pow_limit {:?}",
            params.pow_limit,
        );
    }

    #[test]
    fn too_fast_blocks_tighten_target() {
        // If blocks come at half the target spacing, the DAA should compute a
        // tighter (smaller) target.
        let params = make_params();
        let mut history = Vec::new();
        for i in 0..params.context_len() {
            history.push(BlockState {
                time_secs: i as f64 * (params.target_spacing_secs as f64 / 2.0),
                target: params.pow_limit,
                block_time_secs: params.target_spacing_secs as f64 / 2.0,
                orphan_siblings: 0,
            });
        }
        let target = expected_target(&history, &params);
        assert!(
            target < params.pow_limit,
            "target {target:?} should be below pow_limit {:?} when blocks are too fast",
            params.pow_limit,
        );
    }

    #[test]
    fn pow_limit_clamps_from_above() {
        // If blocks are WAY too fast but all at a difficulty HARDER than
        // pow_limit, the computed target should still be clamped to pow_limit
        // (the easiest allowed).
        let mut params = make_params();
        params.pow_limit = U256::from(1u64) << 200; // tighter pow_limit
        let easy_target = U256::from(1u64) << 240; // much easier than pow_limit
        let mut history = Vec::new();
        for i in 0..params.context_len() {
            history.push(BlockState {
                time_secs: i as f64 * 1.0, // 1 second apart - much too fast
                target: easy_target,
                block_time_secs: 1.0,
                orphan_siblings: 0,
            });
        }
        let target = expected_target(&history, &params);
        assert_eq!(
            target, params.pow_limit,
            "should clamp to pow_limit = easiest allowed"
        );
    }

    #[test]
    fn parse_pow_limit_hex_handles_calibrated_format() {
        let hex = "0040".to_string() + &"00".repeat(30);
        let u = parse_pow_limit_hex(&hex).expect("valid hex");
        assert_eq!(u, U256::from(1u64) << 246);
    }

    #[test]
    fn parse_pow_limit_hex_rejects_bad_length() {
        assert!(parse_pow_limit_hex("00").is_err());
    }
}
