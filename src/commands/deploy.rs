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
    nodes: &str,
    workers: usize,
    ignore_failed_miners: bool,
    reuse_app_session: bool,
    restart_app_session: bool,
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

    let mut active_miners: Vec<_> = select_instances(&config.miners, nodes)
        .into_iter()
        .cloned()
        .collect();

    if active_miners.is_empty() {
        anyhow::bail!("No miners with assigned IPs. Run 'kresko up' first.");
    }

    println!(
        "Deploying to {} miner(s) matching '{nodes}' via S3...",
        active_miners.len()
    );

    let mut failed_miners = HashSet::new();
    let mut failure_details = Vec::new();

    // Payload is always distributed via S3. Direct SCP from the operator's
    // machine has been intentionally removed — nodes fetch from S3 only.
    let s3_cfg = S3Config::from_env()?;
    let s3_client = crate::s3::new_client(&s3_cfg).await?;
    let s3_key = format!("{}/payload.tar.gz", config.experiment);

    crate::s3::upload_file(&s3_client, &s3_cfg, &s3_cfg.bucket_name, &s3_key, &tar_path).await?;
    let download_url = crate::s3::presign_get_url(
        &s3_client,
        &s3_cfg.bucket_name,
        &s3_key,
        Duration::from_secs(3600),
    )
    .await?;

    let mut downloaded = HashSet::new();
    for chunk in active_miners.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let ip = inst.public_ip.clone();
                let name = inst.name.clone();
                let parsed_name = inst.parsed_hostname();
                let key = key.clone();
                let url = download_url.clone();

                async move {
                    println!("  {name}: downloading payload from S3...");
                    // Linode boots Ubuntu images with hostname=localhost, while DO/GCP
                    // set the instance name as the hostname. node_init.sh and several
                    // downstream consumers (txblast, tracing) derive per-node paths
                    // from $(hostname), so normalize it here before anything else runs.
                    let result = ssh::ssh_exec(
                        &ip,
                        &key,
                        &format!(
                            "hostnamectl set-hostname '{parsed_name}' && \
                             if ! command -v curl >/dev/null 2>&1; then \
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

    // Run node_init.sh via tmux on all eligible nodes
    if active_miners.is_empty() {
        eprintln!("No miners are eligible to start after payload distribution.");
    } else {
        println!("Starting nodes via tmux...");
        let script = std::fs::read_to_string(dir.join("scripts/node_init.sh"))
            .or_else(|_| std::fs::read_to_string(dir.join("payload/scripts/node_init.sh")))
            .or_else(|_| std::fs::read_to_string(dir.join("payload/node_init.sh")))
            .context("node_init.sh not found")?;

        let mut start_targets = Vec::new();
        let mut reused_sessions = HashSet::new();

        for chunk in active_miners.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let instance = inst.clone();
                    let key = key.clone();
                    async move {
                        let name = instance.name.clone();
                        if restart_app_session {
                            let _ = ssh::ssh_exec_timeout(
                                &instance.public_ip,
                                &key,
                                "tmux kill-session -t app 2>/dev/null || true",
                                Duration::from_secs(30),
                            )
                            .await;
                            return (name, Ok::<_, anyhow::Error>(SessionPreparation::StartFresh));
                        }

                        let result = ssh::ssh_exec_capture(
                            &instance.public_ip,
                            &key,
                            "tmux has-session -t app >/dev/null 2>&1",
                        )
                        .await
                        .and_then(|(code, _)| {
                            if code == 0 {
                                if reuse_app_session {
                                    Ok(SessionPreparation::ReuseExisting)
                                } else {
                                    anyhow::bail!(
                                        "app tmux session already exists; rerun with --reuse-app-session or --restart-app-session"
                                    )
                                }
                            } else {
                                Ok(SessionPreparation::StartFresh)
                            }
                        });
                        (name, result)
                    }
                })
                .collect();

            for (name, result) in join_all(futs).await {
                match result {
                    Ok(SessionPreparation::StartFresh) => {
                        if let Some(instance) = active_miners.iter().find(|inst| inst.name == name)
                        {
                            start_targets.push(instance.clone());
                        }
                    }
                    Ok(SessionPreparation::ReuseExisting) => {
                        println!("  {name}: reusing existing app session");
                        reused_sessions.insert(name);
                    }
                    Err(e) => {
                        let detail = e.to_string();
                        eprintln!("  {name}: failed to prepare app session: {detail}");
                        failed_miners.insert(name.clone());
                        failure_details
                            .push(format!("{name}: failed to prepare app session: {detail}"));
                    }
                }
            }
        }

        let results = tmux::run_script_in_tmux(
            &start_targets,
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

        if !reused_sessions.is_empty() {
            for name in reused_sessions {
                if !failed_miners.contains(&name) {
                    println!("  {name}: app session reused");
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

enum SessionPreparation {
    StartFresh,
    ReuseExisting,
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
