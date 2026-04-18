//! Monte Carlo simulator for kresko PoW calibration.
//!
//! Self-contained under `src/pow_sim/` so it can be removed as a unit if
//! no longer useful. The only public coupling with the rest of kresko is
//! through `PowTuningInputs` / `PowRoundTuning` re-use.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use anyhow::{Context, Result};

use crate::pow_tuning::{self, PowProfile, PowTuningInputs};

pub mod daa;
pub mod simulate;
pub mod stats;

pub use daa::{BlockState, DaaParams, parse_pow_limit_hex};
pub use simulate::{SimulationInputs, SimulationResult, simulate};
pub use stats::{BlockTimeStats, first_convergence_height, steady_state_mean};

/// Inputs provided on the CLI to `kresko pow-simulate`.
#[derive(Debug, Clone)]
pub struct PowSimulateCli {
    pub num_miners: usize,
    pub sol_per_sec_per_thread: f64,
    pub target_spacing_secs: u32,
    pub blocks: u32,
    pub propagation_delay_secs: f64,
    pub pow_profile: PowProfile,
    pub headroom_bits: u8,
    /// If set, use this target_difficulty_limit hex instead of recalibrating.
    pub target_difficulty_limit_hex: Option<String>,
    pub seed: u64,
    /// If set, write per-block CSV to this path.
    pub csv_path: Option<String>,
}

/// CLI entry point.
pub fn run(cli: PowSimulateCli) -> Result<()> {
    let tuning = pow_tuning::PowRoundTuning::from_profile(cli.pow_profile);

    let (pow_limit_hex, natural_bits) = match cli.target_difficulty_limit_hex.clone() {
        Some(hex) => (hex, None),
        None => {
            let cal = pow_tuning::calibrate(&PowTuningInputs {
                num_miners: cli.num_miners,
                target_spacing_secs: cli.target_spacing_secs,
                headroom_bits: cli.headroom_bits,
                sol_per_sec_override: Some(cli.sol_per_sec_per_thread),
                ..Default::default()
            })?;
            (
                cal.target_difficulty_limit_hex,
                Some(cal.natural_target_bits),
            )
        }
    };
    let pow_limit = parse_pow_limit_hex(&pow_limit_hex)
        .map_err(|e| anyhow::anyhow!("bad target_difficulty_limit: {e}"))?;

    let daa = DaaParams {
        target_spacing_secs: cli.target_spacing_secs,
        pow_limit,
        tuning,
    };

    let inputs = SimulationInputs {
        num_miners: cli.num_miners,
        sol_per_sec_per_thread: cli.sol_per_sec_per_thread,
        blocks: cli.blocks,
        propagation_delay_secs: cli.propagation_delay_secs,
        daa,
        seed: cli.seed,
    };

    println!(
        "PoW simulation setup: profile={}, miners={}, sol/s per thread={:.3}, target_spacing={}s, \
         blocks={}, propagation_delay={}s, seed={}",
        cli.pow_profile,
        cli.num_miners,
        cli.sol_per_sec_per_thread,
        cli.target_spacing_secs,
        cli.blocks,
        cli.propagation_delay_secs,
        cli.seed,
    );
    println!(
        "  target_difficulty_limit = \"{}\"{}",
        pow_limit_hex,
        match natural_bits {
            Some(b) => format!("  (natural_bits={b}, headroom_bits={})", cli.headroom_bits),
            None => String::new(),
        },
    );
    println!(
        "  round_tuning: avg_window={}, median_span={}, damping={}, up={}%, down={}%",
        inputs.daa.tuning.pow_averaging_window,
        inputs.daa.tuning.pow_median_block_span,
        inputs.daa.tuning.pow_damping_factor,
        inputs.daa.tuning.pow_max_adjust_up_percent,
        inputs.daa.tuning.pow_max_adjust_down_percent,
    );

    let start = std::time::Instant::now();
    let result = simulate(&inputs);
    let elapsed = start.elapsed();
    println!(
        "Simulated {} blocks in {:.3}s",
        cli.blocks,
        elapsed.as_secs_f64()
    );
    println!();

    report(&result, &inputs);

    if let Some(path) = cli.csv_path {
        write_csv(&result, Path::new(&path))
            .with_context(|| format!("failed to write CSV to {path}"))?;
        println!("Per-block CSV written to {path}");
    }

    Ok(())
}

fn report(result: &SimulationResult, inputs: &SimulationInputs) {
    let target = inputs.daa.target_spacing_secs;
    let skip = inputs.daa.tuning.pow_averaging_window;

    let full_stats = BlockTimeStats::from_blocks(&result.blocks, target);
    let tail_blocks: Vec<BlockState> = result.blocks.iter().skip(skip).copied().collect();
    let tail_stats = BlockTimeStats::from_blocks(&tail_blocks, target);

    println!("=== Overall (all {} blocks) ===", full_stats.count);
    print_stats(&full_stats, target);
    println!();
    println!("=== Post-warmup (skipping first {skip} DAA-warming blocks) ===");
    print_stats(&tail_stats, target);
    println!();

    let convergence = first_convergence_height(&result.blocks, target, skip);
    match convergence {
        Some(h) => println!(
            "Convergence: first height where running-mean of last {skip} blocks is within \
             ±10% of target_spacing: {h}"
        ),
        None => println!(
            "Convergence: running-mean never settled to within ±10% of target_spacing; \
             try more blocks or a more aggressive profile"
        ),
    }

    let steady_tail = steady_state_mean(&result.blocks, 50.min(result.blocks.len()));
    println!(
        "Steady-state mean (last {}): {steady_tail:.2}s",
        50.min(result.blocks.len())
    );

    println!();
    println!("=== Forks ===");
    println!(
        "Total orphaned blocks: {} ({:.1}% of canonical blocks)",
        result.total_orphans,
        100.0 * result.total_orphans as f64 / result.blocks.len().max(1) as f64,
    );
    println!(
        "Effective-hashpower efficiency: {:.1}%",
        100.0 * result.effective_efficiency
    );
}

fn print_stats(stats: &BlockTimeStats, target: u32) {
    println!(
        "  block_time: min={:.2}s p10={:.2}s median={:.2}s mean={:.2}s p90={:.2}s max={:.2}s stddev={:.2}s",
        stats.min_secs,
        stats.p10_secs,
        stats.median_secs,
        stats.mean_secs,
        stats.p90_secs,
        stats.max_secs,
        stats.stddev_secs,
    );
    println!(
        "  % within ±20% of {target}s target: {:.1}%",
        100.0 * stats.pct_within_20pct
    );
    println!(
        "  % within ±50% of {target}s target: {:.1}%",
        100.0 * stats.pct_within_50pct
    );
}

fn write_csv(result: &SimulationResult, path: &Path) -> Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    writeln!(
        w,
        "height,time_secs,block_time_secs,target_hex,orphan_siblings"
    )?;
    for (i, block) in result.blocks.iter().enumerate() {
        let target_bytes = block.target.to_big_endian();
        writeln!(
            w,
            "{},{:.6},{:.6},{},{}",
            i + 1,
            block.time_secs,
            block.block_time_secs,
            hex::encode(target_bytes),
            block.orphan_siblings,
        )?;
    }
    w.flush()?;
    Ok(())
}
