use anyhow::Result;
use futures::future::join_all;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

use crate::config::{Config, Instance, MiningMode, select_instances};

#[derive(Debug, Serialize)]
struct ProgressLogEntry {
    ts_unix_ms: u128,
    tick: u64,
    mode: String,
    mining_mode: String,
    miner: String,
    ip: String,
    ok: bool,
    latency_ms: u128,
    status_code: Option<u16>,
    block_hash: Option<String>,
    error: Option<String>,
    // PoW observer fields (None in generate mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    propagation_delay_ms: Option<u128>,
}

pub async fn run(
    block_time: u64,
    random: bool,
    concurrent: usize,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<()> {
    if block_time == 0 {
        anyhow::bail!("block-time must be greater than 0 seconds");
    }
    if concurrent == 0 {
        anyhow::bail!("concurrent must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let miners: Vec<Instance> = select_instances(&config.miners, "all")
        .into_iter()
        .cloned()
        .collect();

    if miners.is_empty() {
        println!("No active miners found.");
        return Ok(());
    }

    let log_path = resolve_log_path(dir, data_subdir)?;

    match config.mining_mode {
        MiningMode::Pow => run_observer(block_time, &miners, &log_path, true).await,
        MiningMode::Generate => {
            run_generate(block_time, random, concurrent, &miners, &log_path, true).await
        }
    }
}

/// Spawn a background progress task for use by `kresko run`. The returned
/// JoinHandle's `abort()` stops the task. Output goes to
/// `data/<data_subdir>/progress.log.jsonl`.
pub async fn spawn_background(
    directory: &Path,
    block_time: u64,
    data_subdir: &str,
) -> Result<JoinHandle<()>> {
    if block_time == 0 {
        anyhow::bail!("block-time must be greater than 0 seconds");
    }

    let config = Config::load(directory)?;
    let miners: Vec<Instance> = select_instances(&config.miners, "all")
        .into_iter()
        .cloned()
        .collect();

    if miners.is_empty() {
        anyhow::bail!("No active miners found for background progress logging.");
    }

    let log_path = resolve_log_path(directory, Some(data_subdir))?;
    let mining_mode = config.mining_mode;

    let handle = tokio::spawn(async move {
        let result = match mining_mode {
            MiningMode::Pow => run_observer(block_time, &miners, &log_path, false).await,
            MiningMode::Generate => {
                run_generate(block_time, false, 1, &miners, &log_path, false).await
            }
        };
        if let Err(e) = result {
            eprintln!("background progress task error: {e}");
        }
    });

    Ok(handle)
}

fn resolve_log_path(dir: &Path, data_subdir: Option<&str>) -> Result<PathBuf> {
    let path = match data_subdir {
        Some(sub) => {
            let target_dir = dir.join("data").join(sub);
            std::fs::create_dir_all(&target_dir)?;
            target_dir.join("progress.log.jsonl")
        }
        None => dir.join("progress.log.jsonl"),
    };
    // Touch so OpenOptions::append always finds an existing file.
    if !path.exists() {
        let _ = File::create(&path)?;
    }
    Ok(path)
}

/// Generate mode: drive block production via the `generate` RPC (PoW disabled).
async fn run_generate(
    block_time: u64,
    random: bool,
    concurrent: usize,
    miners: &[Instance],
    log_path: &Path,
    respond_to_ctrl_c: bool,
) -> Result<()> {
    let effective_concurrency = concurrent.min(miners.len());
    if effective_concurrency != concurrent {
        println!(
            "Requested concurrency {} exceeds active miners {}; using {}.",
            concurrent,
            miners.len(),
            effective_concurrency
        );
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let mode = if random { "random" } else { "round_robin" };
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    if respond_to_ctrl_c {
        println!(
            "Progress loop started (miners={}, mode={}, block_time={}s, concurrent={}).",
            miners.len(),
            mode,
            block_time,
            effective_concurrency
        );
        println!("Logging results to {}", log_path.display());
        println!("Press Ctrl-C to stop.");
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(block_time));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut tick: u64 = 0;
    let mut next_idx: usize = 0;
    let mut rng = StdRng::from_os_rng();

    loop {
        if respond_to_ctrl_c {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("Stopping progress loop.");
                    break;
                }
                _ = ticker.tick() => {
                    generate_tick(
                        &client, miners, &mut log_file, &mut tick, mode, random,
                        effective_concurrency, &mut next_idx, &mut rng, respond_to_ctrl_c,
                    ).await?;
                }
            }
        } else {
            ticker.tick().await;
            generate_tick(
                &client,
                miners,
                &mut log_file,
                &mut tick,
                mode,
                random,
                effective_concurrency,
                &mut next_idx,
                &mut rng,
                respond_to_ctrl_c,
            )
            .await?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn generate_tick(
    client: &reqwest::Client,
    miners: &[Instance],
    log_file: &mut std::fs::File,
    tick: &mut u64,
    mode: &str,
    random: bool,
    effective_concurrency: usize,
    next_idx: &mut usize,
    rng: &mut impl rand::Rng,
    print_entries: bool,
) -> Result<()> {
    *tick = tick.saturating_add(1);

    let selected = if random {
        pick_random_miners(miners, effective_concurrency, rng)
    } else {
        pick_round_robin_miners(miners, effective_concurrency, next_idx)
    };

    let futs: Vec<_> = selected
        .into_iter()
        .map(|miner| generate_block(client, miner, *tick, mode))
        .collect();

    let results = join_all(futs).await;
    for entry in results {
        if print_entries {
            print_log_entry(&entry);
        }
        let line = serde_json::to_string(&entry)?;
        writeln!(log_file, "{line}")?;
    }
    log_file.flush()?;
    Ok(())
}

/// Observer mode: poll nodes for block height changes (PoW enabled, blocks mined by nodes).
async fn run_observer(
    block_time: u64,
    miners: &[Instance],
    log_path: &Path,
    respond_to_ctrl_c: bool,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    if respond_to_ctrl_c {
        println!(
            "Progress observer started (miners={}, poll_interval={}s, mining_mode=pow).",
            miners.len(),
            block_time,
        );
        println!("Logging results to {}", log_path.display());
        println!("Press Ctrl-C to stop.");
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(block_time));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut tick: u64 = 0;
    let mut last_heights: HashMap<String, u64> = HashMap::new();
    // Track when we first saw each block height (for propagation delay).
    let mut height_first_seen: HashMap<u64, (u128, String)> = HashMap::new();

    loop {
        if respond_to_ctrl_c {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("Stopping progress observer.");
                    break;
                }
                _ = ticker.tick() => {
                    observer_tick(
                        &client, miners, &mut log_file, &mut tick,
                        &mut last_heights, &mut height_first_seen, respond_to_ctrl_c,
                    ).await?;
                }
            }
        } else {
            ticker.tick().await;
            observer_tick(
                &client,
                miners,
                &mut log_file,
                &mut tick,
                &mut last_heights,
                &mut height_first_seen,
                respond_to_ctrl_c,
            )
            .await?;
        }
    }

    Ok(())
}

async fn observer_tick(
    client: &reqwest::Client,
    miners: &[Instance],
    log_file: &mut std::fs::File,
    tick: &mut u64,
    last_heights: &mut HashMap<String, u64>,
    height_first_seen: &mut HashMap<u64, (u128, String)>,
    print_entries: bool,
) -> Result<()> {
    *tick = tick.saturating_add(1);

    let futs: Vec<_> = miners
        .iter()
        .map(|miner| observe_node(client, miner, *tick))
        .collect();

    let results = join_all(futs).await;
    for (miner_name, mut entry, current_height) in results {
        let last = last_heights.get(&miner_name).copied().unwrap_or(0);
        if current_height > last || last == 0 {
            last_heights.insert(miner_name.clone(), current_height);

            if current_height > 0 {
                if let Some((first_ts, first_miner)) = height_first_seen.get(&current_height) {
                    entry.discovered_by = Some(first_miner.clone());
                    entry.propagation_delay_ms = Some(entry.ts_unix_ms.saturating_sub(*first_ts));
                } else {
                    height_first_seen
                        .insert(current_height, (entry.ts_unix_ms, miner_name.clone()));
                    entry.discovered_by = Some(miner_name);
                    entry.propagation_delay_ms = Some(0);
                }
            }

            if print_entries {
                print_log_entry(&entry);
            }
            let line = serde_json::to_string(&entry)?;
            writeln!(log_file, "{line}")?;
        }
    }
    log_file.flush()?;

    if let Some(&max_h) = last_heights.values().max() {
        height_first_seen.retain(|h, _| *h + 100 >= max_h);
    }
    Ok(())
}

async fn observe_node(
    client: &reqwest::Client,
    miner: &Instance,
    tick: u64,
) -> (String, ProgressLogEntry, u64) {
    let start = Instant::now();
    let ts_unix_ms = now_unix_ms();
    let url = format!("http://{}:18232", miner.public_ip);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": tick,
        "method": "getblockchaininfo",
        "params": []
    });

    let mut entry = ProgressLogEntry {
        ts_unix_ms,
        tick,
        mode: "observer".to_string(),
        mining_mode: "pow".to_string(),
        miner: miner.name.clone(),
        ip: miner.public_ip.clone(),
        ok: false,
        latency_ms: 0,
        status_code: None,
        block_hash: None,
        error: None,
        height: None,
        discovered_by: None,
        propagation_delay_ms: None,
    };

    let mut height: u64 = 0;

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            let status = resp.status();
            entry.status_code = Some(status.as_u16());
            match resp.json::<serde_json::Value>().await {
                Ok(payload) => {
                    if let Some(err) = payload.get("error").filter(|v| !v.is_null()) {
                        entry.error = Some(format!("rpc error: {err}"));
                    } else if let Some(result) = payload.get("result") {
                        entry.ok = true;
                        height = result["blocks"].as_u64().unwrap_or(0);
                        entry.height = Some(height);
                        entry.block_hash = result["bestblockhash"].as_str().map(String::from);
                    } else {
                        entry.error = Some("missing result in RPC response".to_string());
                    }
                }
                Err(e) => {
                    entry.error = Some(format!("failed to parse RPC JSON response: {e}"));
                }
            }
        }
        Err(e) => {
            entry.error = Some(format!("request failed: {e}"));
        }
    }

    entry.latency_ms = start.elapsed().as_millis();
    (miner.name.clone(), entry, height)
}

fn pick_round_robin_miners<'a>(
    miners: &'a [Instance],
    concurrent: usize,
    next_idx: &mut usize,
) -> Vec<&'a Instance> {
    let mut selected = Vec::with_capacity(concurrent);
    for _ in 0..concurrent {
        let idx = *next_idx % miners.len();
        selected.push(&miners[idx]);
        *next_idx = (*next_idx + 1) % miners.len();
    }
    selected
}

fn pick_random_miners<'a, R: rand::Rng + ?Sized>(
    miners: &'a [Instance],
    concurrent: usize,
    rng: &mut R,
) -> Vec<&'a Instance> {
    let mut idxs: Vec<usize> = (0..miners.len()).collect();
    idxs.shuffle(rng);
    idxs.truncate(concurrent);
    idxs.into_iter().map(|idx| &miners[idx]).collect()
}

async fn generate_block(
    client: &reqwest::Client,
    miner: &Instance,
    tick: u64,
    mode: &str,
) -> ProgressLogEntry {
    let start = Instant::now();
    let ts_unix_ms = now_unix_ms();
    let url = format!("http://{}:18232", miner.public_ip);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": tick,
        "method": "generate",
        "params": [1]
    });

    let mut entry = ProgressLogEntry {
        ts_unix_ms,
        tick,
        mode: mode.to_string(),
        mining_mode: "generate".to_string(),
        miner: miner.name.clone(),
        ip: miner.public_ip.clone(),
        ok: false,
        latency_ms: 0,
        status_code: None,
        block_hash: None,
        error: None,
        height: None,
        discovered_by: None,
        propagation_delay_ms: None,
    };

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            let status = resp.status();
            entry.status_code = Some(status.as_u16());
            let status_ok = status.is_success();
            match resp.json::<serde_json::Value>().await {
                Ok(payload) => {
                    if let Some(err) = payload.get("error").filter(|v| !v.is_null()) {
                        entry.error = Some(format!("rpc error: {err}"));
                    } else if !status_ok {
                        entry.error = Some(format!("http status {status}"));
                    } else if let Some(hash) = extract_block_hash(payload.get("result")) {
                        entry.ok = true;
                        entry.block_hash = Some(hash);
                    } else {
                        entry.error = Some("missing result in RPC response".to_string());
                    }
                }
                Err(e) => {
                    entry.error = Some(format!("failed to parse RPC JSON response: {e}"));
                }
            }
        }
        Err(e) => {
            entry.error = Some(format!("request failed: {e}"));
        }
    }

    entry.latency_ms = start.elapsed().as_millis();
    entry
}

fn extract_block_hash(result: Option<&serde_json::Value>) -> Option<String> {
    match result {
        Some(serde_json::Value::Array(blocks)) => blocks
            .first()
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        Some(serde_json::Value::String(hash)) => Some(hash.clone()),
        _ => None,
    }
}

fn print_log_entry(entry: &ProgressLogEntry) {
    if entry.ok {
        println!(
            "[tick {:>6}] {:<30} {:<18} OK    hash={} latency={}ms",
            entry.tick,
            entry.miner,
            entry.ip,
            entry.block_hash.as_deref().unwrap_or("-"),
            entry.latency_ms
        );
    } else {
        println!(
            "[tick {:>6}] {:<30} {:<18} FAIL  {} (latency={}ms)",
            entry.tick,
            entry.miner,
            entry.ip,
            entry.error.as_deref().unwrap_or("unknown error"),
            entry.latency_ms
        );
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
