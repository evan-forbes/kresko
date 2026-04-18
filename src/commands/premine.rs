use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::pow_tuning::{self, PowTuningInputs};
use crate::premine::{self, CalibrationSignature, ResolveOutcome};

/// Resolve (or generate) the premine cache entry for the given calibration
/// inputs. Intended for ahead-of-time cache warming so the slow Equihash
/// mining step is paid down before an experiment launches. Safe to run in the
/// background; uses the same calibration algorithm as `kresko genesis` so the
/// resulting cache entry will be a hit when genesis runs with matching inputs.
pub fn run(
    mining_cpus: usize,
    block_time_secs: u32,
    pow_adjust: f64,
    cache_dir: Option<PathBuf>,
    solver_threads: Option<usize>,
) -> Result<()> {
    if mining_cpus == 0 {
        anyhow::bail!("--mining-cpus must be > 0");
    }
    if block_time_secs == 0 {
        anyhow::bail!("--block-time-secs must be > 0");
    }
    let solver_threads = solver_threads.unwrap_or_else(premine::default_solver_threads);
    if solver_threads == 0 {
        anyhow::bail!("--solver-threads must be > 0");
    }

    println!(
        "Benchmarking local Equihash sol/s ({} samples)...",
        pow_tuning::DEFAULT_BENCH_SAMPLES
    );
    let measured = pow_tuning::measure_local_sol_per_sec(pow_tuning::DEFAULT_BENCH_SAMPLES)
        .context("local sol/s benchmark failed")?;
    println!(
        "  local={:.3} sol/s ({} solves in {:.1}s) → assumed fleet={:.3} sol/s (÷{:.1})",
        measured.local_sol_per_sec,
        measured.total_solves,
        measured.elapsed_secs,
        measured.assumed_fleet_sol_per_sec,
        pow_tuning::LOCAL_TO_FLEET_DISCOUNT,
    );

    let calibration = pow_tuning::calibrate(&PowTuningInputs {
        num_miners: mining_cpus,
        target_spacing_secs: block_time_secs,
        target_adjust_fraction: pow_adjust,
        sol_per_sec_override: Some(measured.assumed_fleet_sol_per_sec),
        ..Default::default()
    })
    .context("PoW calibration failed")?;

    let signature = CalibrationSignature::new(
        calibration.target_difficulty_limit_hex.clone(),
        block_time_secs,
    )?;
    let cache_root = cache_dir.unwrap_or_else(premine::default_cache_root);

    println!(
        "Calibrated: target={} miners={} spacing={}s adjust={:+.3} natural_bits={} cache_key={}",
        signature.target_hex,
        calibration.num_miners,
        calibration.target_spacing_secs,
        calibration.target_adjust_fraction,
        calibration.natural_target_bits,
        signature.cache_key(),
    );
    println!(
        "Cache root: {} (solver_threads={})",
        cache_root.display(),
        solver_threads
    );

    let started = Instant::now();
    let (bundle, outcome) = premine::resolve_premine(&signature, &cache_root, solver_threads)?;
    let elapsed = started.elapsed();

    let entry_dir = signature.cache_dir(&cache_root);
    let manifest = bundle.manifest();

    println!(
        "Premine {} in {:.1}s: cache_dir={} seeded_blocks={} funded_keys={} genesis_hash={}",
        outcome,
        elapsed.as_secs_f64(),
        entry_dir.display(),
        manifest.seeded_block_count,
        manifest.funded_key_count,
        manifest.genesis_hash,
    );

    if outcome == ResolveOutcome::Miss {
        println!(
            "First-run cost amortizes across every experiment whose calibration \
             produces the same target_difficulty_limit and block_time_secs."
        );
    }

    Ok(())
}
