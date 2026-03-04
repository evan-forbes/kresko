use anyhow::Result;
use futures::future::join_all;
use std::time::Duration;

use crate::config::{Config, select_instances};

pub async fn run(directory: &str) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let active = select_instances(&config.validators, "all");

    if active.is_empty() {
        println!("No active validators found.");
        return Ok(());
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
                            let height = json["result"]["blocks"]
                                .as_u64()
                                .unwrap_or(0)
                                .to_string();
                            let progress = json["result"]["verificationprogress"]
                                .as_f64()
                                .unwrap_or(0.0);
                            let status = if progress >= 0.9999 {
                                "synced".to_string()
                            } else {
                                format!("syncing ({:.1}%)", progress * 100.0)
                            };
                            (name, ip, height, status)
                        }
                        Err(e) => (name, ip, "N/A".to_string(), format!("error: {e}")),
                    },
                    Err(e) => (name, ip, "N/A".to_string(), format!("unreachable: {e}")),
                }
            }
        })
        .collect();

    let results = join_all(futs).await;

    println!(
        "{:<30} {:<18} {:<10} {:<10}",
        "Name", "IP", "Height", "Status"
    );
    println!("{}", "-".repeat(68));

    for (name, ip, height, status) in results {
        println!("{:<30} {:<18} {:<10} {:<10}", name, ip, height, status);
    }

    Ok(())
}
