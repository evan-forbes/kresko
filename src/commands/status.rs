use anyhow::{Context, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::config::{Config, NetworkKind, resolve_value, select_instances, shellexpand};
use crate::ssh;

#[derive(Debug, Clone, Copy, Default)]
pub struct QueryOptions {
    pub deep: bool,
}

#[derive(Debug, Serialize)]
pub struct NodeStatus {
    pub name: String,
    pub ip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    pub height: Option<u64>,
    pub verification_progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_block_hash: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_reachable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session_present: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_sessions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loopback_rpc_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_log_tail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub nodes: Vec<NodeStatus>,
    pub total: usize,
    pub reachable: usize,
    pub unreachable: usize,
}

#[derive(Debug, Serialize)]
pub struct StatusSummary {
    pub total: usize,
    pub reachable: usize,
    pub unreachable: usize,
    pub highest_height: Option<u64>,
    pub lowest_height: Option<u64>,
    pub median_height: Option<u64>,
    pub height_buckets: Vec<HeightBucket>,
}

#[derive(Debug, Serialize)]
pub struct HeightBucket {
    pub height: u64,
    pub nodes: usize,
}

#[derive(Debug)]
struct StatusTargetSet {
    targets: Vec<StatusTarget>,
    network_kind: NetworkKind,
    rpc_port: u16,
    ssh_key_path: String,
}

#[derive(Debug)]
struct StatusTarget {
    name: String,
    ip: String,
}

#[derive(Debug, Deserialize)]
struct NodeSnapshot {
    name: String,
    public_ip: String,
    #[serde(default)]
    status: String,
}

pub async fn run(
    json: bool,
    summary: bool,
    deep: bool,
    ssh_key_path: Option<&str>,
    directory: &str,
) -> Result<()> {
    let report = query_with_options(directory, QueryOptions { deep }, ssh_key_path).await?;

    if summary {
        let aggregated = summarize(&report);
        if json {
            println!("{}", serde_json::to_string_pretty(&aggregated)?);
        } else {
            print_summary(&aggregated);
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report, deep);
    }

    Ok(())
}

#[allow(dead_code)]
pub async fn query(directory: &str) -> Result<StatusReport> {
    query_with_options(directory, QueryOptions::default(), None).await
}

pub async fn query_with_options(
    directory: &str,
    options: QueryOptions,
    ssh_key_path: Option<&str>,
) -> Result<StatusReport> {
    let dir = Path::new(directory);
    let target_set = load_status_targets(dir)?;
    let total = target_set.targets.len();
    let rpc_port = target_set.rpc_port;
    let network_kind = target_set.network_kind;

    if target_set.targets.is_empty() {
        return Ok(StatusReport {
            nodes: vec![],
            total: 0,
            reachable: 0,
            unreachable: 0,
        });
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let ssh_key = if options.deep {
        Some(shellexpand(&resolve_value(
            ssh_key_path,
            "KRESKO_SSH_KEY_PATH",
            &target_set.ssh_key_path,
        )))
    } else {
        None
    };

    let futs: Vec<_> = target_set
        .targets
        .iter()
        .map(|target| {
            let name = target.name.clone();
            let ip = target.ip.clone();
            let client = client.clone();
            let ssh_key = ssh_key.clone();

            async move {
                let mut node = fetch_rpc_status(&client, &name, &ip, rpc_port, network_kind).await;
                if let Some(key) = ssh_key.as_deref() {
                    populate_deep_status(&mut node, key, rpc_port).await;
                }
                node
            }
        })
        .collect();

    let nodes = join_all(futs).await;
    let reachable = nodes.iter().filter(|node| node.height.is_some()).count();
    let unreachable = total - reachable;

    Ok(StatusReport {
        nodes,
        total,
        reachable,
        unreachable,
    })
}

fn load_status_targets(dir: &Path) -> Result<StatusTargetSet> {
    let config_path = dir.join("config.json");
    if config_path.is_file() {
        let config = Config::load(dir)?;
        let targets = select_instances(&config.miners, "all")
            .into_iter()
            .map(|inst| StatusTarget {
                name: inst.name.clone(),
                ip: inst.public_ip.clone(),
            })
            .collect();

        return Ok(StatusTargetSet {
            targets,
            network_kind: config.network_kind,
            rpc_port: config.rpc_port(),
            ssh_key_path: config.ssh_key_path,
        });
    }

    let nodes_dir = dir.join("nodes");
    let mut targets = Vec::new();
    if nodes_dir.is_dir() {
        for entry in std::fs::read_dir(&nodes_dir)
            .with_context(|| format!("failed to read {}", nodes_dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", nodes_dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let node: NodeSnapshot = serde_json::from_str(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if node.status == "failed" || node.public_ip == "TBD" || node.public_ip.is_empty() {
                continue;
            }
            targets.push(StatusTarget {
                name: node.name,
                ip: node.public_ip,
            });
        }
    }

    targets.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(StatusTargetSet {
        targets,
        network_kind: NetworkKind::LocalGenesis,
        rpc_port: NetworkKind::LocalGenesis.rpc_port(),
        ssh_key_path: "~/.ssh/id_ed25519".to_string(),
    })
}

async fn fetch_rpc_status(
    client: &reqwest::Client,
    name: &str,
    ip: &str,
    rpc_port: u16,
    network_kind: NetworkKind,
) -> NodeStatus {
    let url = format!("http://{ip}:{rpc_port}");

    let count_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getblockcount",
        "params": []
    });
    let info_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getblockchaininfo",
        "params": []
    });

    let height = match client.post(&url).json(&count_body).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json["result"].as_u64(),
            Err(_) => None,
        },
        Err(_) => None,
    };

    let info_result = client.post(&url).json(&info_body).send().await;
    match info_result {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => status_from_blockchain_info(name, ip, json, height, network_kind),
            Err(error) => {
                status_from_height_fallback(name, ip, height, network_kind, Some(error.to_string()))
            }
        },
        Err(error) => NodeStatus {
            name: name.to_string(),
            ip: ip.to_string(),
            chain: None,
            height,
            verification_progress: height
                .and_then(|height| estimate_progress(height, network_kind)),
            best_block_hash: None,
            status: match height.and_then(|height| estimate_progress(height, network_kind)) {
                Some(progress) if progress >= 0.9999 => {
                    format!(
                        "synced? ({:.1}%; getblockchaininfo timed out)",
                        progress * 100.0
                    )
                }
                Some(progress) => {
                    format!(
                        "syncing (~{:.1}%; getblockchaininfo timed out)",
                        progress * 100.0
                    )
                }
                None if height.is_some() => {
                    format!("height ok; getblockchaininfo failed: {error}")
                }
                None => format!("unreachable: {error}"),
            },
            ssh_reachable: None,
            ssh_status: None,
            tmux_session_present: None,
            tmux_sessions: None,
            loopback_rpc_status: None,
            recent_log_tail: None,
        },
    }
}

fn status_from_blockchain_info(
    name: &str,
    ip: &str,
    json: serde_json::Value,
    fallback_height: Option<u64>,
    network_kind: NetworkKind,
) -> NodeStatus {
    let height = json["result"]["blocks"].as_u64().or(fallback_height);
    let chain = json["result"]["chain"].as_str().map(ToString::to_string);
    let best_block_hash = json["result"]["bestblockhash"]
        .as_str()
        .map(ToString::to_string);
    let progress = json["result"]["verificationprogress"]
        .as_f64()
        .or_else(|| height.and_then(|height| estimate_progress(height, network_kind)));
    let status = match progress {
        Some(p) if p >= 0.9999 => "synced".to_string(),
        Some(p) => format!("syncing ({:.1}%)", p * 100.0),
        None if height.is_some() => "height ok; progress unknown".to_string(),
        None => "unknown".to_string(),
    };

    NodeStatus {
        name: name.to_string(),
        ip: ip.to_string(),
        chain,
        height,
        verification_progress: progress,
        best_block_hash,
        status,
        ssh_reachable: None,
        ssh_status: None,
        tmux_session_present: None,
        tmux_sessions: None,
        loopback_rpc_status: None,
        recent_log_tail: None,
    }
}

fn status_from_height_fallback(
    name: &str,
    ip: &str,
    height: Option<u64>,
    network_kind: NetworkKind,
    error: Option<String>,
) -> NodeStatus {
    let progress = height.and_then(|height| estimate_progress(height, network_kind));
    let status = match (progress, height, error) {
        (Some(p), _, _) if p >= 0.9999 => format!("synced? (~{:.1}%)", p * 100.0),
        (Some(p), _, _) => format!("syncing (~{:.1}%)", p * 100.0),
        (None, Some(_), Some(error)) => format!("height ok; progress unavailable: {error}"),
        (None, Some(_), None) => "height ok; progress unavailable".to_string(),
        (None, None, Some(error)) => format!("error: {error}"),
        (None, None, None) => "unknown".to_string(),
    };

    NodeStatus {
        name: name.to_string(),
        ip: ip.to_string(),
        chain: None,
        height,
        verification_progress: progress,
        best_block_hash: None,
        status,
        ssh_reachable: None,
        ssh_status: None,
        tmux_session_present: None,
        tmux_sessions: None,
        loopback_rpc_status: None,
        recent_log_tail: None,
    }
}

fn estimate_progress(height: u64, network_kind: NetworkKind) -> Option<f64> {
    let estimated_tip = estimated_public_network_tip(network_kind)?;
    if estimated_tip == 0 {
        return None;
    }

    Some((height as f64 / estimated_tip as f64).min(1.0))
}

fn estimated_public_network_tip(network_kind: NetworkKind) -> Option<u64> {
    let now = chrono::Utc::now().timestamp();

    // Zcash mainnet and testnet both use a 150s target spacing before Blossom
    // and a 75s target spacing after Blossom. This fallback is only for status
    // display when Zebra's richer getblockchaininfo RPC is busy during sync.
    let (genesis_time, blossom_height) = match network_kind {
        NetworkKind::Mainnet => (1_477_612_800_i64, 653_600_i64),
        NetworkKind::PublicTestnet => (1_477_612_800_i64, 584_000_i64),
        NetworkKind::LocalGenesis => return None,
    };

    let blossom_time = genesis_time + (blossom_height * 150);
    let estimated_tip = if now <= blossom_time {
        (now - genesis_time).div_euclid(150)
    } else {
        blossom_height + (now - blossom_time).div_euclid(75)
    };

    u64::try_from(estimated_tip.max(0)).ok()
}

async fn populate_deep_status(node: &mut NodeStatus, ssh_key: &str, rpc_port: u16) {
    let command = format!(
        r#"tmux_state=""
if command -v tmux >/dev/null 2>&1; then
  if tmux has-session -t zebra >/dev/null 2>&1; then
    tmux_state="${{tmux_state}}zebra,"
  fi
  if tmux has-session -t mine >/dev/null 2>&1; then
    tmux_state="${{tmux_state}}mine,"
  fi
  if tmux has-session -t app >/dev/null 2>&1; then
    tmux_state="${{tmux_state}}app,"
  fi
fi
tmux_state="${{tmux_state%,}}"
[ -n "$tmux_state" ] || tmux_state="absent"

loopback_rpc="no-curl"
if command -v curl >/dev/null 2>&1; then
  if curl -fsS --max-time 5 -H 'content-type: application/json' --data '{{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}}' http://127.0.0.1:{rpc_port} >/dev/null 2>&1; then
    loopback_rpc="ok"
  else
    loopback_rpc="error"
  fi
fi

log_tail="missing"
if [ -f /root/kresko-mine.log ]; then
  log_tail="$(tail -n 1 /root/kresko-mine.log | tr '\n' ' ' | cut -c1-160)"
elif [ -f /root/logs/zebrad.log ]; then
  log_tail="$(tail -n 1 /root/logs/zebrad.log | tr '\n' ' ' | cut -c1-160)"
elif [ -f /root/kresko-app.log ]; then
  log_tail="$(tail -n 1 /root/kresko-app.log | tr '\n' ' ' | cut -c1-160)"
elif [ -f /root/logs ]; then
  log_tail="$(tail -n 1 /root/logs | tr '\n' ' ' | cut -c1-160)"
fi

printf 'tmux=%s\nloopback_rpc=%s\nlog_tail=%s\n' "$tmux_state" "$loopback_rpc" "$log_tail""#
    );

    match ssh::ssh_exec_timeout(&node.ip, ssh_key, &command, Duration::from_secs(15)).await {
        Ok(output) => {
            node.ssh_reachable = Some(true);
            node.ssh_status = Some("reachable".to_string());

            for line in output.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "tmux" => {
                        let sessions = value.trim();
                        node.tmux_session_present = Some(sessions != "absent");
                        node.tmux_sessions = Some(sessions.to_string());
                    }
                    "loopback_rpc" => node.loopback_rpc_status = Some(value.trim().to_string()),
                    "log_tail" => node.recent_log_tail = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
        Err(error) => {
            node.ssh_reachable = Some(false);
            node.ssh_status = Some(error.to_string());
        }
    }
}

fn print_report(report: &StatusReport, deep: bool) {
    if report.nodes.is_empty() {
        println!("No active miners found.");
        return;
    }

    println!(
        "{:<30} {:<18} {:<8} {:<10} {:<24}",
        "Name", "IP", "Chain", "Height", "Status"
    );
    println!("{}", "-".repeat(98));

    for node in &report.nodes {
        let height = node
            .height
            .map(|height| height.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        let chain = node.chain.as_deref().unwrap_or("-");
        println!(
            "{:<30} {:<18} {:<8} {:<10} {:<24}",
            node.name, node.ip, chain, height, node.status
        );

        if deep {
            let ssh_status = match (node.ssh_reachable, node.ssh_status.as_deref()) {
                (Some(true), Some(status)) => status.to_string(),
                (Some(false), Some(status)) => format!("unreachable ({status})"),
                _ => "not checked".to_string(),
            };
            let tmux_status = match node.tmux_session_present {
                Some(true) => node.tmux_sessions.as_deref().unwrap_or("present"),
                Some(false) => "absent",
                None => "unknown",
            };
            let loopback_rpc = node.loopback_rpc_status.as_deref().unwrap_or("unknown");
            println!("  ssh: {ssh_status}; tmux: {tmux_status}; loopback rpc: {loopback_rpc}");
            if let Some(hash) = node.best_block_hash.as_deref() {
                println!("  best block: {hash}");
            }
            if let Some(log_tail) = node.recent_log_tail.as_deref() {
                println!("  last log: {log_tail}");
            }
        }
    }
}

fn summarize(report: &StatusReport) -> StatusSummary {
    let mut heights: Vec<u64> = report.nodes.iter().filter_map(|node| node.height).collect();
    heights.sort_unstable();

    let mut buckets = BTreeMap::new();
    for height in &heights {
        *buckets.entry(*height).or_insert(0usize) += 1;
    }

    let median_height = if heights.is_empty() {
        None
    } else {
        Some(heights[heights.len() / 2])
    };

    StatusSummary {
        total: report.total,
        reachable: report.reachable,
        unreachable: report.unreachable,
        highest_height: heights.last().copied(),
        lowest_height: heights.first().copied(),
        median_height,
        height_buckets: buckets
            .into_iter()
            .rev()
            .map(|(height, nodes)| HeightBucket { height, nodes })
            .collect(),
    }
}

fn print_summary(summary: &StatusSummary) {
    println!(
        "Nodes: {} total, {} reachable, {} unreachable",
        summary.total, summary.reachable, summary.unreachable
    );

    if let (Some(low), Some(high)) = (summary.lowest_height, summary.highest_height) {
        let median = summary
            .median_height
            .map(|height| height.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        println!("Heights: low={low}, median={median}, high={high}");
    }

    if summary.height_buckets.is_empty() {
        return;
    }

    println!("Height buckets:");
    for bucket in &summary.height_buckets {
        println!("  {} node(s) at height {}", bucket.nodes, bucket.height);
    }
}
