use anyhow::{Context, Result};
use futures::future::join_all;
use std::path::Path;
use std::time::Duration;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;
use crate::tmux;

pub async fn run(
    ssh_key_path: Option<&str>,
    direct_payload_upload: bool,
    workers: usize,
    ignore_failed_validators: bool,
    directory: &str,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(ssh_key_path, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let payload_dir = dir.join("payload");
    if !payload_dir.exists() {
        anyhow::bail!("Payload directory not found. Run 'kresko genesis' first.");
    }

    // Create tarball (skip if payload hasn't changed)
    let tar_path = dir.join("payload.tar.gz");
    if needs_rebuild(&tar_path, &payload_dir) {
        println!("Creating payload tarball...");
        let tar_output = tokio::process::Command::new("tar")
            .args([
                "-czf",
                tar_path.to_str().unwrap(),
                "-C",
                dir.to_str().unwrap(),
                "payload",
            ])
            .output()
            .await
            .context("failed to create tarball")?;

        if !tar_output.status.success() {
            anyhow::bail!(
                "tar failed: {}",
                String::from_utf8_lossy(&tar_output.stderr)
            );
        }
    } else {
        println!("Payload unchanged, reusing existing tarball.");
    }

    let active_validators = select_instances(&config.validators, "all");

    if active_validators.is_empty() {
        anyhow::bail!("No validators with assigned IPs. Run 'kresko up' first.");
    }

    println!(
        "Deploying to {} validators (direct={direct_payload_upload})...",
        active_validators.len()
    );

    if direct_payload_upload {
        // Direct SCP upload to each node
        let mut failed = Vec::new();
        for chunk in active_validators.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let ip = inst.public_ip.clone();
                    let name = inst.name.clone();
                    let key = key.clone();
                    let tar = tar_path.to_str().unwrap().to_string();

                    async move {
                        println!("  Uploading to {name} ({ip})...");
                        ssh::scp_upload(&ip, &key, &tar, "/root/payload.tar.gz").await?;
                        println!("  Uploaded to {name}");
                        Ok::<_, anyhow::Error>(name)
                    }
                })
                .collect();

            let results = join_all(futs).await;
            for r in &results {
                if let Err(e) = r {
                    eprintln!("  Upload failed: {e}");
                    failed.push(e.to_string());
                }
            }
        }

        if !failed.is_empty() && !ignore_failed_validators {
            anyhow::bail!("{} uploads failed", failed.len());
        }
    } else {
        // Upload to S3, then have nodes download
        let s3_client = crate::s3::new_client(&config.s3).await?;
        let s3_key = format!("{}/payload.tar.gz", config.experiment);

        crate::s3::upload_file(&s3_client, &config.s3.bucket_name, &s3_key, &tar_path).await?;
        let download_url = crate::s3::presign_get_url(
            &s3_client,
            &config.s3.bucket_name,
            &s3_key,
            Duration::from_secs(3600),
        )
        .await?;

        // SSH into each node to download from S3
        let mut failed = Vec::new();
        for chunk in active_validators.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let ip = inst.public_ip.clone();
                    let name = inst.name.clone();
                    let key = key.clone();
                    let url = download_url.clone();

                    async move {
                        println!("  {name}: downloading payload from S3...");
                        ssh::ssh_exec(
                            &ip,
                            &key,
                            &format!("curl -sL -o /root/payload.tar.gz '{url}'"),
                        )
                        .await?;
                        println!("  {name}: downloaded");
                        Ok::<_, anyhow::Error>(name)
                    }
                })
                .collect();

            let results = join_all(futs).await;
            for r in &results {
                if let Err(e) = r {
                    eprintln!("  Download failed: {e}");
                    failed.push(e.to_string());
                }
            }
        }

        if !failed.is_empty() && !ignore_failed_validators {
            anyhow::bail!("{} downloads failed", failed.len());
        }
    }

    // Run node_init.sh via tmux on all nodes
    println!("Starting nodes via tmux...");
    let script = std::fs::read_to_string(dir.join("scripts/node_init.sh"))
        .or_else(|_| std::fs::read_to_string(dir.join("payload/node_init.sh")))
        .context("node_init.sh not found")?;

    let owned_validators: Vec<_> = active_validators.into_iter().cloned().collect();
    let results = tmux::run_script_in_tmux(
        &owned_validators,
        &key,
        &script,
        "app",
        Duration::from_secs(600),
    )
    .await;

    let mut failed = 0;
    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: started"),
            Err(e) => {
                eprintln!("  {name}: failed to start: {e}");
                failed += 1;
            }
        }
    }

    if failed > 0 && !ignore_failed_validators {
        anyhow::bail!("{failed} nodes failed to start");
    }

    println!("Deployment complete.");
    Ok(())
}

/// Returns true if the tarball needs to be (re)created.
fn needs_rebuild(tar_path: &Path, payload_dir: &Path) -> bool {
    let tar_mtime = match std::fs::metadata(tar_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return true,
    };

    fn newest_mtime(dir: &Path) -> std::io::Result<std::time::SystemTime> {
        let mut newest = std::time::SystemTime::UNIX_EPOCH;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let mtime = meta.modified()?;
            if meta.is_dir() {
                let sub = newest_mtime(&entry.path())?;
                if sub > newest {
                    newest = sub;
                }
            }
            if mtime > newest {
                newest = mtime;
            }
        }
        Ok(newest)
    }

    match newest_mtime(payload_dir) {
        Ok(payload_mtime) => payload_mtime > tar_mtime,
        Err(_) => true,
    }
}

