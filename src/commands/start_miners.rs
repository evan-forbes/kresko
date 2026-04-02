use anyhow::Result;
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::tmux;

pub async fn run(instances: &str, directory: &str) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, instances);

    if targets.is_empty() {
        println!("No matching instances found.");
        return Ok(());
    }

    println!("Starting PoW miners on {} nodes...", targets.len());

    let script = r#"#!/bin/bash
kresko mine --rpc-endpoint http://localhost:18232
"#;

    let owned_targets: Vec<_> = targets.into_iter().cloned().collect();
    let results = tmux::run_script_in_tmux(
        &owned_targets,
        &key,
        script,
        "mine",
        Duration::from_secs(30),
    )
    .await;

    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: miner started"),
            Err(e) => eprintln!("  {name}: failed: {e}"),
        }
    }

    Ok(())
}
