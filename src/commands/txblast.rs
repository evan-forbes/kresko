use anyhow::Result;
use std::time::Duration;

use crate::config::{Config, TxType, resolve_value, select_instances, shellexpand};
use crate::tmux;

pub async fn run(
    instances: &str,
    tx_type: TxType,
    rate: u64,
    amount: f64,
    directory: &str,
) -> Result<()> {
    if !matches!(tx_type, TxType::Transparent) {
        anyhow::bail!("zebrad-compatible txblast currently supports only --tx-type transparent");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, instances);

    if targets.is_empty() {
        println!("No matching instances found.");
        return Ok(());
    }

    println!(
        "Starting txblast on {} nodes (type={tx_type}, rate={rate}/s, amount={amount})...",
        targets.len()
    );

    let script = format!(
        r#"#!/bin/bash
kresko txblast-local \
    --rpc-endpoint http://localhost:18232 \
    --tx-type {tx_type} \
    --rate {rate} \
    --amount {amount}
"#
    );

    let owned_targets: Vec<_> = targets.into_iter().cloned().collect();
    let results = tmux::run_script_in_tmux(
        &owned_targets,
        &key,
        &script,
        "txblast",
        Duration::from_secs(30),
    )
    .await;

    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: txblast started"),
            Err(e) => eprintln!("  {name}: failed: {e}"),
        }
    }

    Ok(())
}
