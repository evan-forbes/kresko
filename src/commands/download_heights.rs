use anyhow::{Context, Result};
use futures::future::join_all;
use serde::Serialize;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Write;
use std::time::Duration;

use crate::config::{Config, select_instances};

#[derive(Debug, Serialize)]
struct HeightTraceEntry {
    node: String,
    ip: String,
    height: u64,
    hash: String,
    time: i64,
    size: u64,
}

pub async fn run(node_count: usize, workers: usize, directory: &str) -> Result<()> {
    if node_count == 0 {
        anyhow::bail!("node_count must be greater than 0");
    }

    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let active = select_instances(&config.miners, "all");
    let available = active.len();
    let targets: Vec<_> = active.into_iter().take(node_count).collect();

    if targets.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    if node_count > available {
        println!("Requested {node_count} nodes, but only {available} active nodes are available.");
    }

    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let out_path = data_dir.join("heights.jsonl");
    let mut out_file = File::create(&out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    println!(
        "Downloading block height traces from {} nodes...",
        targets.len()
    );

    let mut total_rows = 0usize;
    for chunk in targets.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let client = client.clone();
                let node = inst.name.clone();
                let ip = inst.public_ip.clone();

                async move { fetch_node_heights(&client, &node, &ip).await }
            })
            .collect();

        let results = join_all(futs).await;

        for result in results {
            match result {
                Ok(entries) => {
                    for entry in entries {
                        let line = serde_json::to_string(&entry)?;
                        writeln!(out_file, "{line}")?;
                        total_rows += 1;
                    }
                }
                Err(e) => {
                    eprintln!("  Warning: {e}");
                }
            }
        }
    }

    out_file.flush()?;
    println!(
        "Height trace download complete: {} rows -> {}",
        total_rows,
        out_path.display()
    );

    Ok(())
}

async fn fetch_node_heights(
    client: &reqwest::Client,
    node: &str,
    ip: &str,
) -> Result<Vec<HeightTraceEntry>> {
    let url = format!("http://{ip}:18232");
    let tip = rpc_call(client, &url, "getblockcount", json!([]))
        .await
        .with_context(|| format!("{node}: getblockcount failed"))?;
    let tip = tip
        .as_u64()
        .with_context(|| format!("{node}: invalid getblockcount response"))?;

    let mut entries = Vec::with_capacity((tip + 1) as usize);

    for height in 0..=tip {
        let block = rpc_call(client, &url, "getblock", json!([height.to_string(), 2]))
            .await
            .with_context(|| format!("{node}: getblock failed at height {height}"))?;

        let hash = block
            .get("hash")
            .and_then(Value::as_str)
            .with_context(|| format!("{node}: missing hash for height {height}"))?
            .to_string();
        let time = block
            .get("time")
            .and_then(Value::as_i64)
            .with_context(|| format!("{node}: missing time for height {height}"))?;
        let size = block
            .get("size")
            .and_then(Value::as_u64)
            .with_context(|| format!("{node}: missing size for height {height}"))?;

        entries.push(HeightTraceEntry {
            node: node.to_string(),
            ip: ip.to_string(),
            height,
            hash,
            time,
            size,
        });
    }

    println!("  {node}: downloaded {} heights", entries.len());
    Ok(entries)
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
