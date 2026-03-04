use anyhow::Result;
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::tmux;

pub async fn run(session: &str, timeout: u64, directory: &str) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let active: Vec<_> = select_instances(&config.validators, "all")
        .into_iter()
        .cloned()
        .collect();

    if active.is_empty() {
        println!("No active validators found.");
        return Ok(());
    }

    println!("Killing session '{}' on {} nodes...", session, active.len());

    let results =
        tmux::stop_tmux_session(&active, &key, session, Duration::from_secs(timeout)).await;

    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: killed"),
            Err(e) => eprintln!("  {name}: {e}"),
        }
    }

    Ok(())
}

