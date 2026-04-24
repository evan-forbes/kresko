//! Summary statistics over a simulation run.

use super::daa::BlockState;

#[derive(Debug, Clone)]
pub struct BlockTimeStats {
    pub count: usize,
    pub min_secs: f64,
    pub p10_secs: f64,
    pub median_secs: f64,
    pub mean_secs: f64,
    pub p90_secs: f64,
    pub p99_secs: f64,
    pub max_secs: f64,
    pub stddev_secs: f64,
    /// Fraction of blocks whose block_time lies within ±20% of target_spacing.
    pub pct_within_20pct: f64,
    /// Fraction of blocks whose block_time lies within ±50% of target_spacing.
    pub pct_within_50pct: f64,
}

impl BlockTimeStats {
    /// Compute summary stats over the block_time field of the given blocks.
    pub fn from_blocks(blocks: &[BlockState], target_spacing_secs: u32) -> Self {
        let mut times: Vec<f64> = blocks.iter().map(|b| b.block_time_secs).collect();
        let target = f64::from(target_spacing_secs);
        let lo20 = target * 0.8;
        let hi20 = target * 1.2;
        let lo50 = target * 0.5;
        let hi50 = target * 1.5;

        let pct_within_20 = times.iter().filter(|t| **t >= lo20 && **t <= hi20).count() as f64
            / times.len().max(1) as f64;
        let pct_within_50 = times.iter().filter(|t| **t >= lo50 && **t <= hi50).count() as f64
            / times.len().max(1) as f64;

        let sum: f64 = times.iter().sum();
        let mean = sum / times.len().max(1) as f64;
        let var = times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / times.len().max(1) as f64;
        let stddev = var.sqrt();

        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p = |q: f64| percentile(&times, q);

        Self {
            count: blocks.len(),
            min_secs: times.first().copied().unwrap_or(0.0),
            p10_secs: p(0.10),
            median_secs: p(0.50),
            mean_secs: mean,
            p90_secs: p(0.90),
            p99_secs: p(0.99),
            max_secs: times.last().copied().unwrap_or(0.0),
            stddev_secs: stddev,
            pct_within_20pct: pct_within_20,
            pct_within_50pct: pct_within_50,
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (q * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// First block height at which the running mean of the previous
/// `avg_window` block_times is within ±10% of `target_spacing`. `None` if
/// the simulation never reached that state.
pub fn first_convergence_height(
    blocks: &[BlockState],
    target_spacing_secs: u32,
    avg_window: usize,
) -> Option<u32> {
    if blocks.len() < avg_window {
        return None;
    }
    let target = f64::from(target_spacing_secs);
    let lo = target * 0.9;
    let hi = target * 1.1;
    for i in avg_window..=blocks.len() {
        let slice = &blocks[i - avg_window..i];
        let mean = slice.iter().map(|b| b.block_time_secs).sum::<f64>() / avg_window as f64;
        if mean >= lo && mean <= hi {
            return Some(i as u32);
        }
    }
    None
}

/// Mean block_time over the tail of the simulation (last `tail_count` blocks).
pub fn steady_state_mean(blocks: &[BlockState], tail_count: usize) -> f64 {
    if blocks.is_empty() {
        return 0.0;
    }
    let n = tail_count.min(blocks.len());
    let slice = &blocks[blocks.len() - n..];
    slice.iter().map(|b| b.block_time_secs).sum::<f64>() / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_blocks(times: &[f64]) -> Vec<BlockState> {
        use zebra_chain::work::difficulty::U256;
        let mut blocks = Vec::new();
        let mut now = 0.0;
        for &t in times {
            now += t;
            blocks.push(BlockState {
                time_secs: now,
                target: U256::zero(),
                block_time_secs: t,
                orphan_siblings: 0,
            });
        }
        blocks
    }

    #[test]
    fn stats_cover_constant_block_times() {
        let blocks = make_blocks(&[25.0; 50]);
        let stats = BlockTimeStats::from_blocks(&blocks, 25);
        assert_eq!(stats.count, 50);
        assert!((stats.mean_secs - 25.0).abs() < 1e-9);
        assert!(stats.stddev_secs < 1e-9);
        assert_eq!(stats.pct_within_20pct, 1.0);
    }

    #[test]
    fn convergence_detects_settled_mean() {
        let mut times = vec![100.0; 10];
        times.extend(vec![25.0; 30]);
        let blocks = make_blocks(&times);
        let h = first_convergence_height(&blocks, 25, 10);
        assert!(h.is_some());
        assert!(h.unwrap() >= 20);
    }
}
