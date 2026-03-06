use anyhow::{Context, Result};
use futures::future::join_all;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::config::{Config, S3Config, resolve_value, select_instances, shellexpand};
use crate::ssh;
use crate::tmux;

pub async fn run(
    ssh_key_path: Option<&str>,
    direct_payload_upload: bool,
    workers: usize,
    ignore_failed_miners: bool,
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

    let mut active_miners: Vec<_> = select_instances(&config.miners, "all")
        .into_iter()
        .cloned()
        .collect();

    if active_miners.is_empty() {
        anyhow::bail!("No miners with assigned IPs. Run 'kresko up' first.");
    }

    println!(
        "Deploying to {} miners (direct={direct_payload_upload})...",
        active_miners.len()
    );

    let mut failed_miners = HashSet::new();
    let mut failure_details = Vec::new();

    if direct_payload_upload {
        // Direct SCP upload to each node
        let mut uploaded = HashSet::new();
        for chunk in active_miners.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let ip = inst.public_ip.clone();
                    let name = inst.name.clone();
                    let key = key.clone();
                    let tar = tar_path.to_str().unwrap().to_string();

                    async move {
                        println!("  Uploading to {name} ({ip})...");
                        let result = ssh::scp_upload(&ip, &key, &tar, "/root/payload.tar.gz").await;
                        (name, result)
                    }
                })
                .collect();

            for (name, result) in join_all(futs).await {
                match result {
                    Ok(()) => {
                        println!("  Uploaded to {name}");
                        uploaded.insert(name);
                    }
                    Err(e) => {
                        eprintln!("  Upload failed for {name}: {e}");
                        failed_miners.insert(name.clone());
                        failure_details.push(format!("{name}: upload failed: {e}"));
                    }
                }
            }
        }

        active_miners.retain(|inst| uploaded.contains(&inst.name));
    } else {
        // Upload to S3, then have nodes download
        let s3_cfg = S3Config::from_env()?;
        let s3_client = crate::s3::new_client(&s3_cfg).await?;
        let s3_key = format!("{}/payload.tar.gz", config.experiment);

        crate::s3::upload_file(&s3_client, &s3_cfg.bucket_name, &s3_key, &tar_path).await?;
        let download_url = crate::s3::presign_get_url(
            &s3_client,
            &s3_cfg.bucket_name,
            &s3_key,
            Duration::from_secs(3600),
        )
        .await?;

        // SSH into each node to download from S3
        let mut downloaded = HashSet::new();
        for chunk in active_miners.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let ip = inst.public_ip.clone();
                    let name = inst.name.clone();
                    let key = key.clone();
                    let url = download_url.clone();

                    async move {
                        println!("  {name}: downloading payload from S3...");
                        let result = ssh::ssh_exec(
                            &ip,
                            &key,
                            &format!(
                                "if ! command -v curl >/dev/null 2>&1; then \
                                     apt-get -o DPkg::Lock::Timeout=300 update -y && \
                                     apt-get -o DPkg::Lock::Timeout=300 install -y curl; \
                                 fi && \
                                 curl -fsSL -o /root/payload.tar.gz '{url}'"
                            ),
                        )
                        .await
                        .map(|_| ());
                        (name, result)
                    }
                })
                .collect();

            for (name, result) in join_all(futs).await {
                match result {
                    Ok(()) => {
                        println!("  {name}: downloaded");
                        downloaded.insert(name);
                    }
                    Err(e) => {
                        eprintln!("  Download failed for {name}: {e}");
                        failed_miners.insert(name.clone());
                        failure_details.push(format!("{name}: download failed: {e}"));
                    }
                }
            }
        }

        active_miners.retain(|inst| downloaded.contains(&inst.name));
    }

    // Run node_init.sh via tmux on all nodes
    if active_miners.is_empty() {
        eprintln!("No miners are eligible to start after payload distribution.");
    } else {
        println!("Starting nodes via tmux...");
        let script = std::fs::read_to_string(dir.join("scripts/node_init.sh"))
            .or_else(|_| std::fs::read_to_string(dir.join("payload/node_init.sh")))
            .context("node_init.sh not found")?;

        let results = tmux::run_script_in_tmux(
            &active_miners,
            &key,
            &script,
            "app",
            Duration::from_secs(600),
        )
        .await;

        for (name, result) in &results {
            match result {
                Ok(()) => println!("  {name}: started"),
                Err(e) => {
                    eprintln!("  {name}: failed to start: {e}");
                    failed_miners.insert(name.clone());
                    failure_details.push(format!("{name}: failed to start: {e}"));
                }
            }
        }
    }

    if !failure_details.is_empty() {
        eprintln!(
            "Deployment completed with failures on {} miner(s):",
            failed_miners.len()
        );
        for detail in &failure_details {
            eprintln!("  - {detail}");
        }

        if !ignore_failed_miners {
            anyhow::bail!(
                "deployment encountered errors on {} miner(s); rerun with --ignore-failed-miners to suppress failure exit",
                failed_miners.len()
            );
        }
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
