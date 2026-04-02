use anyhow::Result;
use futures::future::join_all;
use serde::Serialize;
use std::time::Duration;

use crate::config::{Config, select_instances};

#[derive(Debug, Serialize)]
pub struct NodeStatus {
    pub name: String,
    pub ip: String,
    pub height: Option<u64>,
    pub verification_progress: Option<f64>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub nodes: Vec<NodeStatus>,
    pub total: usize,
    pub reachable: usize,
    pub unreachable: usize,
}

pub async fn run(json: bool, directory: &str) -> Result<()> {
    let report = query(directory).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if report.nodes.is_empty() {
            println!("No active miners found.");
            return Ok(());
        }

        println!(
            "{:<30} {:<18} {:<10} {:<10}",
            "Name", "IP", "Height", "Status"
        );
        println!("{}", "-".repeat(68));

        for node in &report.nodes {
            let height = node
                .height
                .map(|h| h.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            println!(
                "{:<30} {:<18} {:<10} {:<10}",
                node.name, node.ip, height, node.status
            );
        }
    }

    Ok(())
}

pub async fn query(directory: &str) -> Result<StatusReport> {
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

    let futs: Vec<_> = active
        .iter()
        .map(|inst| {
            let name = inst.name.clone();
            let ip = inst.public_ip.clone();
            let client = client.clone();

            async move {
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
                                name,
                                ip,
                                height,
                                verification_progress: progress,
                                status,
                            }
                        }
                        Err(e) => NodeStatus {
                            name,
                            ip,
                            height: None,
                            verification_progress: None,
                            status: format!("error: {e}"),
                        },
                    },
                    Err(e) => NodeStatus {
                        name,
                        ip,
                        height: None,
                        verification_progress: None,
                        status: format!("unreachable: {e}"),
                    },
                }
            }
        })
        .collect();

    let nodes = join_all(futs).await;
    let reachable = nodes.iter().filter(|n| n.height.is_some()).count();
    let unreachable = total - reachable;

    Ok(StatusReport {
        nodes,
        total,
        reachable,
        unreachable,
    })
}
