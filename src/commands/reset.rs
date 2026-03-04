use anyhow::Result;
use futures::future::join_all;
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;
use crate::tmux;

pub async fn run(validators: &str, workers: usize, directory: &str) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.validators, validators);

    if targets.is_empty() {
        println!("No matching validators found.");
        return Ok(());
    }

    println!("Resetting {} validators...", targets.len());

    // Kill tmux sessions
    let owned_targets: Vec<_> = targets.iter().map(|&inst| inst.clone()).collect();
    for session in &["app", "txblast"] {
        println!("  Killing {session} sessions...");
        tmux::stop_tmux_session(&owned_targets, &key, session, Duration::from_secs(30)).await;
    }

    // Clean up remote state in worker-sized chunks.
    for chunk in targets.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let ip = inst.public_ip.clone();
                let name = inst.name.clone();
                let key = key.clone();

                async move {
                    let cleanup = r#"
                        rm -rf /root/.cache/zebra
                        rm -rf /root/payload*
                        rm -f /root/logs*
                        rm -f /root/kresko-*.sh
                        rm -f /root/kresko-*.log
                        rm -f /usr/local/bin/zebrad
                        rm -f /usr/local/bin/kresko
                    "#;

                    match ssh::ssh_exec(&ip, &key, cleanup).await {
                        Ok(_) => println!("  {name}: reset complete"),
                        Err(e) => eprintln!("  {name}: reset failed: {e}"),
                    }
                }
            })
            .collect();

        join_all(futs).await;
    }

    println!("Reset complete.");
    Ok(())
}


