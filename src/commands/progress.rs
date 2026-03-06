use anyhow::Result;
use futures::future::join_all;
use rand::seq::SliceRandom;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Config, Instance, select_instances};

#[derive(Debug, Serialize)]
struct ProgressLogEntry {
    ts_unix_ms: u128,
    tick: u64,
    mode: String,
    miner: String,
    ip: String,
    ok: bool,
    latency_ms: u128,
    status_code: Option<u16>,
    block_hash: Option<String>,
    error: Option<String>,
}

pub async fn run(block_time: u64, random: bool, concurrent: usize, directory: &str) -> Result<()> {
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
    let log_path = dir.join("progress.log.jsonl");
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    println!(
        "Progress loop started (miners={}, mode={}, block_time={}s, concurrent={}).",
        miners.len(),
        mode,
        block_time,
        effective_concurrency
    );
    println!("Logging results to {}", log_path.display());
    println!("Press Ctrl-C to stop.");

    let mut ticker = tokio::time::interval(Duration::from_secs(block_time));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut tick: u64 = 0;
    let mut next_idx: usize = 0;
    let mut rng = rand::rng();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping progress loop.");
                break;
            }
            _ = ticker.tick() => {
                tick = tick.saturating_add(1);

                let selected = if random {
                    pick_random_miners(&miners, effective_concurrency, &mut rng)
                } else {
                    pick_round_robin_miners(&miners, effective_concurrency, &mut next_idx)
                };

                let futs: Vec<_> = selected
                    .into_iter()
                    .map(|miner| generate_block(&client, miner, tick, mode))
                    .collect();

                let results = join_all(futs).await;
                for entry in results {
                    print_log_entry(&entry);
                    let line = serde_json::to_string(&entry)?;
                    writeln!(log_file, "{line}")?;
                }
                log_file.flush()?;
            }
        }
    }

    Ok(())
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
        miner: miner.name.clone(),
        ip: miner.public_ip.clone(),
        ok: false,
        latency_ms: 0,
        status_code: None,
        block_hash: None,
        error: None,
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
