use anyhow::Result;
use serde::Serialize;

use crate::commands::status;

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub healthy: bool,
    pub total_nodes: usize,
    pub reachable_nodes: usize,
    pub unreachable_nodes: usize,
    pub min_height: Option<u64>,
    pub max_height: Option<u64>,
    pub height_spread: Option<u64>,
    pub all_synced: bool,
    pub issues: Vec<String>,
}

pub async fn run(json: bool, directory: &str) -> Result<()> {
    let report = check(directory).await?;
    let exit_unhealthy = !report.healthy;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if report.healthy {
            println!("HEALTHY");
        } else {
            println!("UNHEALTHY");
        }
        println!(
            "  nodes: {}/{} reachable",
            report.reachable_nodes, report.total_nodes
        );
        if let (Some(min), Some(max)) = (report.min_height, report.max_height) {
            println!("  heights: {} - {} (spread: {})", min, max, max - min);
        }
        if !report.issues.is_empty() {
            println!("  issues:");
            for issue in &report.issues {
                println!("    - {issue}");
            }
        }
    }

    if exit_unhealthy {
        std::process::exit(1);
    }

    Ok(())
}

pub async fn check(directory: &str) -> Result<CheckReport> {
    let status = status::query(directory).await?;

    let mut issues = Vec::new();

    if status.total == 0 {
        return Ok(CheckReport {
            healthy: false,
            total_nodes: 0,
            reachable_nodes: 0,
            unreachable_nodes: 0,
            min_height: None,
            max_height: None,
            height_spread: None,
            all_synced: false,
            issues: vec!["no active nodes".to_string()],
        });
    }

    // Check for unreachable nodes
    let unreachable: Vec<_> = status
        .nodes
        .iter()
        .filter(|n| n.height.is_none())
        .collect();
    for node in &unreachable {
        issues.push(format!("{} ({}): {}", node.name, node.ip, node.status));
    }

    // Collect heights from reachable nodes
    let heights: Vec<u64> = status.nodes.iter().filter_map(|n| n.height).collect();

    let (min_height, max_height, height_spread) = if heights.is_empty() {
        (None, None, None)
    } else {
        let min = *heights.iter().min().unwrap();
        let max = *heights.iter().max().unwrap();
        (Some(min), Some(max), Some(max - min))
    };

    // Check for stuck nodes (height 0 while others have advanced)
    if let Some(max) = max_height {
        if max > 0 {
            for node in &status.nodes {
                if node.height == Some(0) {
                    issues.push(format!(
                        "{} ({}): stuck at height 0 while network is at {}",
                        node.name, node.ip, max
                    ));
                }
            }
        }
    }

    // Check for nodes falling behind (>10 blocks behind max)
    if let Some(max) = max_height {
        for node in &status.nodes {
            if let Some(h) = node.height {
                if h > 0 && max > h && max - h > 10 {
                    issues.push(format!(
                        "{} ({}): behind by {} blocks (height {} vs max {})",
                        node.name,
                        node.ip,
                        max - h,
                        h,
                        max
                    ));
                }
            }
        }
    }

    let all_synced = status.nodes.iter().all(|n| n.status == "synced");

    // Healthy if all nodes reachable and no major issues
    let healthy = status.unreachable == 0 && issues.is_empty();

    Ok(CheckReport {
        healthy,
        total_nodes: status.total,
        reachable_nodes: status.reachable,
        unreachable_nodes: status.unreachable,
        min_height,
        max_height,
        height_spread,
        all_synced,
        issues,
    })
}
