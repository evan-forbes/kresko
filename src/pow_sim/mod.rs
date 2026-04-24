//! Monte Carlo simulator for kresko PoW calibration.
//!
//! Self-contained under `src/pow_sim/` so it can be removed as a unit if
//! no longer useful. The only public coupling with the rest of kresko is
//! through `PowTuningInputs` / `PowRoundTuning` re-use.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::EquihashParameterSet;
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

/// Inputs provided on the CLI to `kresko pow-simulate-matrix`.
#[derive(Debug, Clone)]
pub struct PowSimulateMatrixCli {
    pub equihash_params: String,
    pub sol_per_sec: String,
    pub miners: String,
    pub target_spacing_secs: u32,
    pub blocks: u32,
    pub propagation_delays: String,
    pub pow_profile: PowProfile,
    pub headroom_bits: u8,
    pub seeds: String,
    pub csv_path: String,
}

#[derive(Debug, Clone)]
struct PreparedSimulation {
    inputs: SimulationInputs,
    pow_limit_hex: String,
    natural_bits: Option<u32>,
}

#[derive(Debug, Clone)]
struct SimulationSummary {
    stats: BlockTimeStats,
    total_orphans: u64,
    orphan_rate: f64,
    efficiency: f64,
    convergence_height: Option<u32>,
}

/// CLI entry point.
pub fn run(cli: PowSimulateCli) -> Result<()> {
    let prepared = prepare_simulation(&cli)?;
    let inputs = prepared.inputs;

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
        prepared.pow_limit_hex,
        match prepared.natural_bits {
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

/// CLI entry point for batch Monte Carlo runs.
pub fn run_matrix(cli: PowSimulateMatrixCli) -> Result<()> {
    let equihash_params = parse_equihash_list(&cli.equihash_params)?;
    let sol_rates = parse_sol_rate_list(&cli.sol_per_sec, &equihash_params)?;
    let miners = parse_usize_list(&cli.miners, "miners")?;
    let propagation_delays = parse_f64_list(&cli.propagation_delays, "propagation-delays")?;
    let seeds = parse_seed_list(&cli.seeds)?;

    if cli.blocks == 0 {
        anyhow::bail!("blocks must be > 0");
    }
    if cli.target_spacing_secs == 0 {
        anyhow::bail!("target-spacing must be > 0");
    }

    let total_runs = equihash_params.len() * miners.len() * propagation_delays.len() * seeds.len();
    println!("Running {total_runs} PoW simulations -> {}", cli.csv_path);

    let file = File::create(&cli.csv_path)?;
    let mut w = BufWriter::new(file);
    writeln!(
        w,
        "equihash_params,miners,sol_per_sec,target_spacing,propagation_delay,pow_profile,seed,mean,p10,p50,p90,p99,orphans,orphan_rate,efficiency,convergence_height"
    )?;

    let mut completed = 0usize;
    for (equihash_params, sol_per_sec) in equihash_params.iter().zip(sol_rates.iter()) {
        for &num_miners in &miners {
            for &propagation_delay_secs in &propagation_delays {
                for &seed in &seeds {
                    let cli_run = PowSimulateCli {
                        num_miners,
                        sol_per_sec_per_thread: *sol_per_sec,
                        target_spacing_secs: cli.target_spacing_secs,
                        blocks: cli.blocks,
                        propagation_delay_secs,
                        pow_profile: cli.pow_profile,
                        headroom_bits: cli.headroom_bits,
                        target_difficulty_limit_hex: None,
                        seed,
                        csv_path: None,
                    };
                    let prepared = prepare_simulation(&cli_run)?;
                    let result = simulate(&prepared.inputs);
                    let summary = summarize(&result, &prepared.inputs);
                    let convergence = summary
                        .convergence_height
                        .map(|h| h.to_string())
                        .unwrap_or_default();

                    writeln!(
                        w,
                        "{},{},{:.9},{},{:.6},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{},{:.9},{:.9},{}",
                        equihash_params,
                        num_miners,
                        sol_per_sec,
                        cli.target_spacing_secs,
                        propagation_delay_secs,
                        cli.pow_profile,
                        seed,
                        summary.stats.mean_secs,
                        summary.stats.p10_secs,
                        summary.stats.median_secs,
                        summary.stats.p90_secs,
                        summary.stats.p99_secs,
                        summary.total_orphans,
                        summary.orphan_rate,
                        summary.efficiency,
                        convergence,
                    )?;

                    completed += 1;
                    if completed == total_runs || completed % 100 == 0 {
                        println!("  completed {completed}/{total_runs}");
                    }
                }
            }
        }
    }

    w.flush()?;
    println!("Matrix CSV written to {}", cli.csv_path);

    Ok(())
}

fn prepare_simulation(cli: &PowSimulateCli) -> Result<PreparedSimulation> {
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

    Ok(PreparedSimulation {
        inputs: SimulationInputs {
            num_miners: cli.num_miners,
            sol_per_sec_per_thread: cli.sol_per_sec_per_thread,
            blocks: cli.blocks,
            propagation_delay_secs: cli.propagation_delay_secs,
            daa: DaaParams {
                target_spacing_secs: cli.target_spacing_secs,
                pow_limit,
                tuning,
            },
            seed: cli.seed,
        },
        pow_limit_hex,
        natural_bits,
    })
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
        "  block_time: min={:.2}s p10={:.2}s median={:.2}s mean={:.2}s p90={:.2}s p99={:.2}s max={:.2}s stddev={:.2}s",
        stats.min_secs,
        stats.p10_secs,
        stats.median_secs,
        stats.mean_secs,
        stats.p90_secs,
        stats.p99_secs,
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

fn summarize(result: &SimulationResult, inputs: &SimulationInputs) -> SimulationSummary {
    let target = inputs.daa.target_spacing_secs;
    let skip = inputs
        .daa
        .tuning
        .pow_averaging_window
        .min(result.blocks.len());
    let tail_blocks: Vec<BlockState> = result.blocks.iter().skip(skip).copied().collect();
    let stats = BlockTimeStats::from_blocks(&tail_blocks, target);
    let canonical_blocks = result.blocks.len().max(1) as f64;

    SimulationSummary {
        stats,
        total_orphans: result.total_orphans,
        orphan_rate: result.total_orphans as f64 / canonical_blocks,
        efficiency: result.effective_efficiency,
        convergence_height: first_convergence_height(
            &result.blocks,
            target,
            inputs.daa.tuning.pow_averaging_window,
        ),
    }
}

fn parse_equihash_list(raw: &str) -> Result<Vec<EquihashParameterSet>> {
    let values: Vec<EquihashParameterSet> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<Result<_>>()?;
    if values.is_empty() {
        anyhow::bail!("equihash-params must contain at least one value");
    }
    Ok(values)
}

fn parse_usize_list(raw: &str, name: &str) -> Result<Vec<usize>> {
    let values: Vec<usize> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<usize>()
                .with_context(|| format!("invalid {name} value: {s}"))
        })
        .collect::<Result<_>>()?;
    if values.is_empty() || values.contains(&0) {
        anyhow::bail!("{name} must contain positive integers");
    }
    Ok(values)
}

fn parse_f64_list(raw: &str, name: &str) -> Result<Vec<f64>> {
    let values: Vec<f64> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let value = s
                .parse::<f64>()
                .with_context(|| format!("invalid {name} value: {s}"))?;
            if !value.is_finite() || value < 0.0 {
                anyhow::bail!("{name} values must be non-negative finite numbers, got {value}");
            }
            Ok(value)
        })
        .collect::<Result<_>>()?;
    if values.is_empty() {
        anyhow::bail!("{name} must contain at least one value");
    }
    Ok(values)
}

fn parse_sol_rate_list(raw: &str, params: &[EquihashParameterSet]) -> Result<Vec<f64>> {
    if raw.contains('=') {
        let mut common = None;
        let mut regtest = None;
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = part
                .split_once('=')
                .with_context(|| format!("invalid sol-per-sec entry {part}; use common=1.0"))?;
            let key: EquihashParameterSet = key.trim().parse()?;
            let value = parse_positive_f64(value.trim(), "sol-per-sec")?;
            match key {
                EquihashParameterSet::Common => common = Some(value),
                EquihashParameterSet::Regtest => regtest = Some(value),
            }
        }

        return params
            .iter()
            .map(|param| match param {
                EquihashParameterSet::Common => {
                    common.with_context(|| "missing sol-per-sec for common Equihash parameters")
                }
                EquihashParameterSet::Regtest => {
                    regtest.with_context(|| "missing sol-per-sec for regtest Equihash parameters")
                }
            })
            .collect();
    }

    let rates: Vec<f64> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| parse_positive_f64(s, "sol-per-sec"))
        .collect::<Result<_>>()?;
    match rates.len() {
        0 => anyhow::bail!("sol-per-sec must contain at least one value"),
        1 => Ok(vec![rates[0]; params.len()]),
        n if n == params.len() => Ok(rates),
        n => anyhow::bail!(
            "sol-per-sec has {n} values but equihash-params has {}; use one value, matching positional values, or keyed values like common=1.0,regtest=100.0",
            params.len(),
        ),
    }
}

fn parse_positive_f64(raw: &str, name: &str) -> Result<f64> {
    let value = raw
        .parse::<f64>()
        .with_context(|| format!("invalid {name} value: {raw}"))?;
    if !value.is_finite() || value <= 0.0 {
        anyhow::bail!("{name} values must be positive finite numbers, got {value}");
    }
    Ok(value)
}

fn parse_seed_list(raw: &str) -> Result<Vec<u64>> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("seeds must contain at least one value");
    }

    if let Some((start, end)) = raw.split_once("..=") {
        return parse_seed_range(start, end);
    }
    if let Some((start, end)) = raw.split_once("..") {
        return parse_seed_range(start, end);
    }

    let values: Vec<u64> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u64>()
                .with_context(|| format!("invalid seed value: {s}"))
        })
        .collect::<Result<_>>()?;
    if values.is_empty() {
        anyhow::bail!("seeds must contain at least one value");
    }
    Ok(values)
}

fn parse_seed_range(start: &str, end: &str) -> Result<Vec<u64>> {
    let start = start
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid seed range start: {}", start.trim()))?;
    let end = end
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid seed range end: {}", end.trim()))?;
    if start > end {
        anyhow::bail!("seed range start {start} is greater than end {end}");
    }
    Ok((start..=end).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seed_lists_and_inclusive_ranges() {
        assert_eq!(parse_seed_list("1,3,5").unwrap(), vec![1, 3, 5]);
        assert_eq!(parse_seed_list("1..3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_seed_list("1..=3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parses_keyed_sol_rates_by_equihash_param() {
        let params = parse_equihash_list("common,regtest").unwrap();
        let rates = parse_sol_rate_list("regtest=100.0,common=1.5", &params).unwrap();
        assert_eq!(rates, vec![1.5, 100.0]);
    }
}
