use anyhow::Result;
use futures::future::join_all;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

#[derive(Debug, Clone, Copy, Default)]
pub struct QueryOptions {
    pub deep: bool,
}

#[derive(Debug, Serialize)]
pub struct NodeStatus {
    pub name: String,
    pub ip: String,
    pub height: Option<u64>,
    pub verification_progress: Option<f64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_reachable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session_present: Option<bool>,
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

pub async fn query(directory: &str) -> Result<StatusReport> {
    query_with_options(directory, QueryOptions::default(), None).await
}

pub async fn query_with_options(
    directory: &str,
    options: QueryOptions,
    ssh_key_path: Option<&str>,
) -> Result<StatusReport> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let active = select_instances(&config.miners, "all");
    let total = active.len();

    if active.is_empty() {
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
            &config.ssh_key_path,
        )))
    } else {
        None
    };

    let futs: Vec<_> = active
        .iter()
        .map(|inst| {
            let name = inst.name.clone();
            let ip = inst.public_ip.clone();
            let client = client.clone();
            let ssh_key = ssh_key.clone();

            async move {
                let mut node = fetch_rpc_status(&client, &name, &ip).await;
                if let Some(key) = ssh_key.as_deref() {
                    populate_deep_status(&mut node, key).await;
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

async fn fetch_rpc_status(client: &reqwest::Client, name: &str, ip: &str) -> NodeStatus {
    let url = format!("http://{ip}:18232");
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getblockchaininfo",
        "params": []
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => {
                let height = json["result"]["blocks"].as_u64();
                let progress = json["result"]["verificationprogress"].as_f64();
                let status = match progress {
                    Some(p) if p >= 0.9999 => "synced".to_string(),
                    Some(p) => format!("syncing ({:.1}%)", p * 100.0),
                    None => "unknown".to_string(),
                };
                NodeStatus {
                    name: name.to_string(),
                    ip: ip.to_string(),
                    height,
                    verification_progress: progress,
                    status,
                    ssh_reachable: None,
                    ssh_status: None,
                    tmux_session_present: None,
                    loopback_rpc_status: None,
                    recent_log_tail: None,
                }
            }
            Err(error) => NodeStatus {
                name: name.to_string(),
                ip: ip.to_string(),
                height: None,
                verification_progress: None,
                status: format!("error: {error}"),
                ssh_reachable: None,
                ssh_status: None,
                tmux_session_present: None,
                loopback_rpc_status: None,
                recent_log_tail: None,
            },
        },
        Err(error) => NodeStatus {
            name: name.to_string(),
            ip: ip.to_string(),
            height: None,
            verification_progress: None,
            status: format!("unreachable: {error}"),
            ssh_reachable: None,
            ssh_status: None,
            tmux_session_present: None,
            loopback_rpc_status: None,
            recent_log_tail: None,
        },
    }
}

async fn populate_deep_status(node: &mut NodeStatus, ssh_key: &str) {
    let command = r#"tmux_state="absent"
if tmux has-session -t app >/dev/null 2>&1; then
  tmux_state="present"
fi

loopback_rpc="no-curl"
if command -v curl >/dev/null 2>&1; then
  if curl -fsS --max-time 5 -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}' http://127.0.0.1:18232 >/dev/null 2>&1; then
    loopback_rpc="ok"
  else
    loopback_rpc="error"
  fi
fi

log_tail="missing"
if [ -f /root/kresko-app.log ]; then
  log_tail="$(tail -n 1 /root/kresko-app.log | tr '\n' ' ' | cut -c1-160)"
elif [ -f /root/logs ]; then
  log_tail="$(tail -n 1 /root/logs | tr '\n' ' ' | cut -c1-160)"
fi

printf 'tmux=%s\nloopback_rpc=%s\nlog_tail=%s\n' "$tmux_state" "$loopback_rpc" "$log_tail""#;

    match ssh::ssh_exec_capture(&node.ip, ssh_key, command).await {
        Ok((_, output)) => {
            node.ssh_reachable = Some(true);
            node.ssh_status = Some("reachable".to_string());

            for line in output.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "tmux" => node.tmux_session_present = Some(value.trim() == "present"),
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
        "{:<30} {:<18} {:<10} {:<24}",
        "Name", "IP", "Height", "Status"
    );
    println!("{}", "-".repeat(88));

    for node in &report.nodes {
        let height = node
            .height
            .map(|height| height.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        println!(
            "{:<30} {:<18} {:<10} {:<24}",
            node.name, node.ip, height, node.status
        );

        if deep {
            let ssh_status = match (node.ssh_reachable, node.ssh_status.as_deref()) {
                (Some(true), Some(status)) => status.to_string(),
                (Some(false), Some(status)) => format!("unreachable ({status})"),
                _ => "not checked".to_string(),
            };
            let tmux_status = match node.tmux_session_present {
                Some(true) => "present",
                Some(false) => "absent",
                None => "unknown",
            };
            let loopback_rpc = node.loopback_rpc_status.as_deref().unwrap_or("unknown");
            println!("  ssh: {ssh_status}; tmux app: {tmux_status}; loopback rpc: {loopback_rpc}");
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
