//! Event-driven Monte Carlo simulator for PoW block production.
//!
//! Each of the `N` miners is modeled as an independent Poisson process with
//! per-miner rate `λ_i = S × target / 2^256` (solutions per second × probability
//! any given solution satisfies the current difficulty target). The time until
//! the next canonical block is drawn from `Exponential(N × λ_i)`; the number
//! of siblings orphaned by propagation delay at each height is drawn from
//! `Poisson((N-1) × λ_i × propagation_delay)`.
//!
//! Fork model assumes first-found-globally wins — a conservative upper bound
//! on orphan rate relative to a gossip-aware model where majority reach matters.

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::Distribution;
use rand_distr::{Exp, Poisson};
use zebra_chain::work::difficulty::U256;

use super::daa::{BlockState, DaaParams, expected_target};

/// Full per-run configuration.
#[derive(Debug, Clone)]
pub struct SimulationInputs {
    /// Number of single-thread miners participating.
    pub num_miners: usize,
    /// Per-miner Equihash (200, 9) solution rate in sol/s.
    pub sol_per_sec_per_thread: f64,
    /// Blocks to simulate.
    pub blocks: u32,
    /// Average inter-miner block-propagation delay. Used only for fork
    /// counting; it does not feed back into DAA timing in this simulator.
    pub propagation_delay_secs: f64,
    /// DAA parameters.
    pub daa: DaaParams,
    /// RNG seed for reproducibility.
    pub seed: u64,
}

#[derive(Debug)]
pub struct SimulationResult {
    pub blocks: Vec<BlockState>,
    /// Total blocks orphaned to propagation-delay ties across the full run.
    pub total_orphans: u64,
    /// Effective network-hashpower fraction spent on the main chain.
    pub effective_efficiency: f64,
}

pub fn simulate(inputs: &SimulationInputs) -> SimulationResult {
    assert!(inputs.num_miners > 0, "need at least one miner to simulate");
    assert!(
        inputs.sol_per_sec_per_thread > 0.0,
        "sol_per_sec_per_thread must be positive"
    );
    assert!(inputs.blocks > 0, "need at least one block to simulate");
    assert!(
        inputs.propagation_delay_secs >= 0.0,
        "propagation delay must be non-negative"
    );

    let mut rng = StdRng::seed_from_u64(inputs.seed);
    let mut history: Vec<BlockState> = Vec::with_capacity(inputs.blocks as usize);
    let mut now = 0.0f64;
    let mut total_orphans = 0u64;

    for _height in 1..=inputs.blocks {
        let target = expected_target(&history, &inputs.daa);
        let target_fraction = target_to_fraction(target);
        let lambda_per_miner = inputs.sol_per_sec_per_thread * target_fraction;
        let lambda_total = lambda_per_miner * inputs.num_miners as f64;

        // Guard against absurdly tight targets that would make the simulator
        // wait "forever". If lambda is zero-ish, just skip ahead by a large
        // delta and record a block at pow_limit — this mirrors what happens
        // in practice when the min-difficulty-after-gap rule fires.
        let dt = if lambda_total > 1e-30 {
            Exp::new(lambda_total)
                .expect("lambda_total > 0 validated above")
                .sample(&mut rng)
        } else {
            1e9
        };
        now += dt;

        let lambda_others = lambda_per_miner * (inputs.num_miners.saturating_sub(1)) as f64;
        let mean_orphans = lambda_others * inputs.propagation_delay_secs;
        let orphans = if mean_orphans > 0.0 {
            Poisson::new(mean_orphans)
                .expect("mean_orphans > 0 validated above")
                .sample(&mut rng) as u32
        } else {
            0
        };
        total_orphans = total_orphans.saturating_add(u64::from(orphans));

        history.push(BlockState {
            time_secs: now,
            target,
            block_time_secs: dt,
            orphan_siblings: orphans,
        });
    }

    let canonical_blocks = inputs.blocks as u64;
    let effective_efficiency =
        canonical_blocks as f64 / (canonical_blocks as f64 + total_orphans as f64);

    SimulationResult {
        blocks: history,
        total_orphans,
        effective_efficiency,
    }
}

/// Convert a U256 target to its fractional form `target / 2^256` as f64.
///
/// We approximate by taking the top 53 bits (f64 mantissa) and applying the
/// appropriate 2^k scale. This is accurate enough for Poisson-rate purposes
/// across the full range of realistic targets.
fn target_to_fraction(target: U256) -> f64 {
    if target.is_zero() {
        return 0.0;
    }
    let bits = target.bits() as i32; // position of top set bit + 1
    // Extract top 53 bits into a u64.
    let shift = bits - 53;
    let mantissa: u64 = if shift > 0 {
        (target >> (shift as u32)).as_u64()
    } else {
        (target << ((-shift) as u32)).as_u64()
    };
    let scale = 2f64.powi(bits - 256 - 53);
    mantissa as f64 * scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pow_tuning::{PowProfile, PowRoundTuning};

    fn make_inputs(pow_limit_bits: u32) -> SimulationInputs {
        SimulationInputs {
            num_miners: 80,
            sol_per_sec_per_thread: 1.4,
            blocks: 200,
            propagation_delay_secs: 2.0,
            daa: DaaParams {
                target_spacing_secs: 25,
                pow_limit: U256::from(1u64) << pow_limit_bits,
                tuning: PowRoundTuning::from_profile(PowProfile::Mainnet),
            },
            seed: 42,
        }
    }

    #[test]
    fn target_to_fraction_round_trip() {
        // 2^246 / 2^256 = 2^-10 = 1/1024 = 0.0009765625.
        let target = U256::from(1u64) << 246;
        let frac = target_to_fraction(target);
        assert!((frac - 2f64.powi(-10)).abs() < 1e-12);

        // 2^200 / 2^256 = 2^-56.
        let target = U256::from(1u64) << 200;
        let frac = target_to_fraction(target);
        assert!((frac - 2f64.powi(-56)).abs() / 2f64.powi(-56) < 1e-6);
    }

    #[test]
    fn simulation_runs_deterministically_with_seed() {
        let a = simulate(&make_inputs(246));
        let b = simulate(&make_inputs(246));
        assert_eq!(a.blocks.len(), b.blocks.len());
        for (ba, bb) in a.blocks.iter().zip(b.blocks.iter()) {
            assert!((ba.time_secs - bb.time_secs).abs() < 1e-9);
        }
    }

    #[test]
    fn calibrated_pow_converges_toward_target() {
        // With headroom_bits=2 the pow_limit is 4× looser than the natural
        // target, so blocks start at ~T/4 (fast) and the DAA tightens over
        // ~8 windows down to ~T. Over a 200-block run the mean should land
        // somewhere between T/4 (no convergence) and T (fully converged).
        let inputs = make_inputs(246); // 80 miners × 1.4 sol/s × 25s, headroom 2
        let result = simulate(&inputs);
        let target = inputs.daa.target_spacing_secs as f64;

        let skip = inputs.daa.tuning.pow_averaging_window;
        assert!(result.blocks.len() > skip + 50);

        let later_block_times: Vec<f64> = result
            .blocks
            .iter()
            .skip(skip)
            .map(|b| b.block_time_secs)
            .collect();
        let mean = later_block_times.iter().sum::<f64>() / later_block_times.len() as f64;

        assert!(
            mean > target * 0.2 && mean < target * 1.2,
            "mean block time {mean:.2}s is outside [T/4, T] range {}..{} expected for a \
             calibrated run converging from headroom=2",
            target * 0.2,
            target * 1.2,
        );

        // Steady-state tail (last 50 blocks) should be well within ±30% of
        // target — convergence has had ~150 blocks to settle.
        let tail: Vec<f64> = result
            .blocks
            .iter()
            .rev()
            .take(50)
            .map(|b| b.block_time_secs)
            .collect();
        let tail_mean = tail.iter().sum::<f64>() / tail.len() as f64;
        assert!(
            tail_mean > target * 0.7 && tail_mean < target * 1.3,
            "steady-state mean {tail_mean:.2}s not within ±30% of target {target}s"
        );
    }

    #[test]
    fn propagation_delay_zero_produces_no_orphans() {
        let mut inputs = make_inputs(246);
        inputs.propagation_delay_secs = 0.0;
        let result = simulate(&inputs);
        assert_eq!(result.total_orphans, 0);
        assert!(result.effective_efficiency == 1.0);
    }
}
