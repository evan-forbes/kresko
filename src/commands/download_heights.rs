use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use crate::config::{Config, select_instances};

const HEIGHT_FETCH_RETRIES_PER_NODE: usize = 3;
const HEIGHT_FETCH_RETRY_DELAY_SECS: u64 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HeightTraceEntry {
    node: String,
    ip: String,
    height: u64,
    hash: String,
    time: i64,
    size: u64,
}

#[derive(Clone, Debug)]
struct HeightSource {
    name: String,
    ip: String,
    tip: u64,
}

type FailedNodeMap = HashMap<String, String>;

pub async fn run(
    nodes: &str,
    workers: usize,
    batch_size: Option<usize>,
    force: bool,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }
    let batch_size = batch_size.unwrap_or(workers);
    if batch_size == 0 {
        anyhow::bail!("batch_size must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let candidates = select_instances(&config.miners, nodes);

    if candidates.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    let data_dir = match data_subdir {
        Some(sub) => dir.join("data").join(sub),
        None => dir.join("data"),
    };
    std::fs::create_dir_all(&data_dir)?;
    let out_path = data_dir.join("heights.jsonl");
    let existing_entries = if force {
        Vec::new()
    } else {
        load_existing_entries(&out_path)?
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    println!(
        "Downloading block height trace from 1 node ({} candidates, {} block workers, batch size {})...",
        candidates.len(),
        workers,
        batch_size
    );
    if !force && !existing_entries.is_empty() {
        println!(
            "Reusing {} existing heights from {}",
            existing_entries.len(),
            out_path.display()
        );
    }
    println!(
        "Probing tips across {} candidate node(s)...",
        candidates.len()
    );

    let mut last_error = None;
    let mut failed_nodes = FailedNodeMap::new();
    let candidate_count = candidates.len();
    let tip_results: Vec<_> = stream::iter(candidates.into_iter())
        .map(|inst| {
            let client = client.clone();
            async move {
                let name = inst.name.clone();
                let ip = inst.public_ip.clone();
                let result = fetch_node_tip_with_retry(&client, &name, &ip).await;
                (name, ip, result)
            }
        })
        .buffer_unordered(candidate_count.max(1))
        .collect()
        .await;

    let mut reachable_sources = Vec::new();
    for (name, ip, result) in tip_results {
        match result {
            Ok(tip) => {
                println!("  {name}: tip {tip}");
                reachable_sources.push(HeightSource { name, ip, tip });
            }
            Err(err) => {
                eprintln!("  Warning: {err:#}");
                failed_nodes.insert(name, format!("{err:#}"));
                last_error = Some(err);
            }
        }
    }

    if reachable_sources.is_empty() {
        return match last_error {
            Some(err) => Err(err).context("failed to query tips from any selected node"),
            None => anyhow::bail!("failed to query tips from any selected node"),
        };
    }

    reachable_sources.sort_by(|a, b| b.tip.cmp(&a.tip).then_with(|| a.name.cmp(&b.name)));

    let entries = fetch_heights_with_failover(
        &client,
        &reachable_sources,
        existing_entries,
        workers,
        batch_size,
        &mut failed_nodes,
    )
    .await
    .context("failed to download heights from selected nodes")?;
    write_entries(&out_path, &entries)?;
    println!(
        "Height trace download complete: {} rows -> {}",
        entries.len(),
        out_path.display()
    );
    Ok(())
}

fn load_existing_entries(out_path: &std::path::Path) -> Result<Vec<HeightTraceEntry>> {
    if !out_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(out_path)
        .with_context(|| format!("failed to open existing {}", out_path.display()))?;
    let reader = BufReader::new(file);
    let mut entries_by_height = HashMap::new();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read line {} from {}",
                line_idx + 1,
                out_path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: HeightTraceEntry = serde_json::from_str(&line).with_context(|| {
            format!(
                "failed to parse line {} from {}",
                line_idx + 1,
                out_path.display()
            )
        })?;
        entries_by_height.insert(entry.height, entry);
    }

    let mut entries: Vec<_> = entries_by_height.into_values().collect();
    entries.sort_unstable_by_key(|entry| entry.height);
    Ok(entries)
}

fn write_entries(out_path: &std::path::Path, entries: &[HeightTraceEntry]) -> Result<()> {
    let mut out_file = File::create(out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;

    for entry in entries {
        let line = serde_json::to_string(entry)?;
        writeln!(out_file, "{line}")?;
    }

    out_file.flush()?;
    Ok(())
}

async fn fetch_node_tip_with_retry(client: &reqwest::Client, node: &str, ip: &str) -> Result<u64> {
    let mut last_error = None;

    for attempt in 1..=HEIGHT_FETCH_RETRIES_PER_NODE {
        match fetch_node_tip(client, node, ip).await {
            Ok(tip) => {
                if attempt > 1 {
                    println!("  {node}: recovered on attempt {attempt}");
                }
                return Ok(tip);
            }
            Err(err) => {
                eprintln!(
                    "  {node}: attempt {attempt}/{HEIGHT_FETCH_RETRIES_PER_NODE} failed: {err:#}"
                );
                last_error = Some(err);

                if attempt < HEIGHT_FETCH_RETRIES_PER_NODE {
                    tokio::time::sleep(Duration::from_secs(HEIGHT_FETCH_RETRY_DELAY_SECS)).await;
                }
            }
        }
    }

    match last_error {
        Some(err) => Err(err).context(format!(
            "{node}: exhausted {HEIGHT_FETCH_RETRIES_PER_NODE} attempts"
        )),
        None => anyhow::bail!("{node}: exhausted retries without a recorded error"),
    }
}

async fn fetch_node_tip(client: &reqwest::Client, node: &str, ip: &str) -> Result<u64> {
    let url = format!("http://{ip}:18232");
    let tip = rpc_call(client, &url, "getblockcount", json!([]))
        .await
        .with_context(|| format!("{node}: getblockcount failed"))?;
    tip.as_u64()
        .with_context(|| format!("{node}: invalid getblockcount response"))
}

async fn fetch_heights_with_failover(
    client: &reqwest::Client,
    sources: &[HeightSource],
    existing_entries: Vec<HeightTraceEntry>,
    workers: usize,
    batch_size: usize,
    failed_nodes: &mut FailedNodeMap,
) -> Result<Vec<HeightTraceEntry>> {
    let tip_from_sources = sources
        .iter()
        .map(|source| source.tip)
        .max()
        .context("no reachable height sources available")?;
    let existing_tip = existing_entries.iter().map(|entry| entry.height).max();
    let target_tip = existing_tip.map_or(tip_from_sources, |tip| tip.max(tip_from_sources));
    let mut entries = Vec::with_capacity((target_tip + 1) as usize);
    entries.resize_with((target_tip + 1) as usize, || None);

    for entry in existing_entries {
        let idx = entry.height as usize;
        if idx < entries.len() {
            entries[idx] = Some(entry);
        }
    }

    let mut missing_heights: Vec<u64> = entries
        .iter()
        .enumerate()
        .filter_map(|(height, entry)| entry.is_none().then_some(height as u64))
        .collect();
    let mut last_error = None;

    for source in sources {
        if missing_heights.is_empty() {
            break;
        }

        if let Some(reason) = failed_nodes.get(&source.name) {
            println!("  {}: skipping failed node ({reason})", source.name);
            continue;
        }

        let eligible_heights: Vec<u64> = missing_heights
            .iter()
            .copied()
            .filter(|height| *height <= source.tip)
            .collect();

        if eligible_heights.is_empty() {
            println!(
                "  {}: skipping, tip {} is below remaining target heights",
                source.name, source.tip
            );
            continue;
        }

        println!(
            "  {}: fetching {} heights up to tip {}",
            source.name,
            eligible_heights.len(),
            source.tip
        );

        let (successes, failed, source_error) = fetch_height_subset_until_failure(
            client,
            source,
            &eligible_heights,
            workers,
            batch_size,
        )
        .await;

        for entry in successes {
            let idx = entry.height as usize;
            entries[idx] = Some(entry);
        }

        if !failed.is_empty() {
            eprintln!(
                "  {}: switching away after {} unresolved heights",
                source.name,
                failed.len()
            );
        }
        if let Some(err) = source_error {
            eprintln!("  Warning: {err:#}");
            failed_nodes.insert(source.name.clone(), format!("{err:#}"));
            last_error = Some(err);
        }

        missing_heights.retain(|height| entries[*height as usize].is_none());
    }

    if !missing_heights.is_empty() {
        let sample = missing_heights
            .iter()
            .take(8)
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if missing_heights.len() > 8 {
            ", ..."
        } else {
            ""
        };
        return match last_error {
            Some(err) => Err(err).context(format!(
                "missing {} heights after failover; sample: {sample}{suffix}",
                missing_heights.len()
            )),
            None => anyhow::bail!(
                "missing {} heights after failover; sample: {sample}{suffix}",
                missing_heights.len()
            ),
        };
    }

    let mut resolved = Vec::with_capacity((target_tip + 1) as usize);
    for entry in entries {
        resolved.push(entry.context("missing height entry after failover resolution")?);
    }

    let used_sources = resolved
        .iter()
        .map(|entry| entry.node.as_str())
        .collect::<HashSet<_>>()
        .len();
    println!("  completed height trace using {used_sources} node(s)");
    Ok(resolved)
}

async fn fetch_height_subset_until_failure(
    client: &reqwest::Client,
    source: &HeightSource,
    heights: &[u64],
    workers: usize,
    batch_size: usize,
) -> (Vec<HeightTraceEntry>, Vec<u64>, Option<anyhow::Error>) {
    let mut successes = Vec::with_capacity(heights.len());
    let mut start = 0usize;

    while start < heights.len() {
        let end = (start + batch_size.max(1)).min(heights.len());
        let batch = &heights[start..end];
        let (mut fetched, mut failed, mut errors) =
            fetch_height_batch(client, source, batch, workers).await;

        successes.append(&mut fetched);

        if !failed.is_empty() {
            let failed_count = failed.len();
            failed.extend_from_slice(&heights[end..]);
            let error = errors.drain(..).next().or_else(|| {
                Some(anyhow::anyhow!(
                    "{}: {} heights failed; marking node failed",
                    source.name,
                    failed_count
                ))
            });
            return (successes, failed, error);
        }

        start = end;
    }

    (successes, Vec::new(), None)
}

async fn fetch_height_batch(
    client: &reqwest::Client,
    source: &HeightSource,
    heights: &[u64],
    workers: usize,
) -> (Vec<HeightTraceEntry>, Vec<u64>, Vec<anyhow::Error>) {
    let url = format!("http://{}:18232", source.ip);
    let results: Vec<_> = stream::iter(heights.iter().copied())
        .map(|height| {
            let client = client.clone();
            let node = source.name.clone();
            let ip = source.ip.clone();
            let url = url.clone();

            async move {
                let block = rpc_call(&client, &url, "getblock", json!([height.to_string(), 2]))
                    .await
                    .with_context(|| format!("{node}: getblock failed at height {height}"))?;

                let hash = block
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("{node}: missing hash for height {height}"))?
                    .to_string();
                let size = block
                    .get("size")
                    .and_then(Value::as_u64)
                    .with_context(|| format!("{node}: missing size for height {height}"))?;
                let time = block
                    .get("time")
                    .and_then(Value::as_i64)
                    .with_context(|| format!("{node}: missing time for height {height}"))?;

                Ok::<_, anyhow::Error>((
                    height,
                    HeightTraceEntry {
                        node,
                        ip,
                        height,
                        hash,
                        time,
                        size,
                    },
                ))
            }
        })
        .buffer_unordered(workers.max(1))
        .collect()
        .await;

    let mut successes = Vec::with_capacity(results.len());
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok((_, entry)) => successes.push(entry),
            Err(err) => {
                eprintln!("  Warning: {err:#}");
                errors.push(err);
            }
        }
    }

    let success_heights: HashSet<u64> = successes.iter().map(|entry| entry.height).collect();
    for height in heights {
        if !success_heights.contains(height) {
            failed.push(*height);
        }
    }
    successes.sort_unstable_by_key(|entry| entry.height);
    failed.sort_unstable();
    failed.dedup();
    (successes, failed, errors)
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let response = client.post(url).json(&body).send().await?;
    let payload: Value = response.json().await?;

    if let Some(error) = payload.get("error").filter(|v| !v.is_null()) {
        anyhow::bail!("RPC error from {url} method={method}: {error}");
    }

    payload
        .get("result")
        .cloned()
        .with_context(|| format!("missing result in RPC response for method={method}"))
}

#[cfg(test)]
mod tests {
    use super::{HeightTraceEntry, load_existing_entries};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn height_trace_entry_includes_block_time_but_omits_wall_clock_fields() {
        let entry = HeightTraceEntry {
            node: "miner-0".to_string(),
            ip: "127.0.0.1".to_string(),
            height: 42,
            hash: "abc".to_string(),
            time: 1_700_000_000,
            size: 1234,
        };

        let value = serde_json::to_value(entry).expect("test entry should serialize");

        assert_eq!(value["node"], "miner-0");
        assert_eq!(value["ip"], "127.0.0.1");
        assert_eq!(value["height"], 42);
        assert_eq!(value["hash"], "abc");
        assert_eq!(value["time"], 1_700_000_000);
        assert_eq!(value["size"], 1234);
        assert!(value.get("wall_time").is_none());
        assert!(value.get("wall_time_unix_ms").is_none());
    }

    #[test]
    fn load_existing_entries_sorts_and_deduplicates_by_height() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kresko-heights-{unique}.jsonl"));
        let body = concat!(
            "{\"node\":\"miner-1\",\"ip\":\"127.0.0.2\",\"height\":2,\"hash\":\"old\",\"time\":200,\"size\":20}\n",
            "{\"node\":\"miner-0\",\"ip\":\"127.0.0.1\",\"height\":0,\"hash\":\"genesis\",\"time\":100,\"size\":10}\n",
            "{\"node\":\"miner-1\",\"ip\":\"127.0.0.3\",\"height\":2,\"hash\":\"new\",\"time\":201,\"size\":21}\n"
        );
        fs::write(&path, body).expect("should write temp heights file");

        let loaded = load_existing_entries(&path).expect("should parse existing heights file");
        fs::remove_file(&path).expect("should remove temp heights file");

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].height, 0);
        assert_eq!(loaded[0].hash, "genesis");
        assert_eq!(loaded[1].height, 2);
        assert_eq!(loaded[1].hash, "new");
        assert_eq!(loaded[1].time, 201);
    }
}
