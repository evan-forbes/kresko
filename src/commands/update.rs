use anyhow::{Context, Result};
use futures::future::join_all;
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{Config, S3Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

pub async fn run(
    ssh_key_path: Option<&str>,
    nodes: &str,
    workers: usize,
    ignore_failed_miners: bool,
    kresko_binary: Option<&str>,
    directory: &str,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(ssh_key_path, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let binary_path = match kresko_binary {
        Some(path) => shellexpand(path).into(),
        None => std::env::current_exe()
            .context("failed to detect the running kresko binary; pass --kresko-binary")?,
    };
    if !binary_path.exists() {
        anyhow::bail!(
            "kresko binary not found at {}; pass --kresko-binary with a valid path",
            binary_path.display()
        );
    }
    if !binary_path.is_file() {
        anyhow::bail!(
            "kresko binary path is not a file: {}",
            binary_path.display()
        );
    }

    let active_miners: Vec<_> = select_instances(&config.miners, nodes)
        .into_iter()
        .cloned()
        .collect();

    if active_miners.is_empty() {
        anyhow::bail!("No miners with assigned IPs. Run 'kresko up' first.");
    }

    println!(
        "Updating kresko binary on {} miner(s) matching '{nodes}' via S3...",
        active_miners.len()
    );

    let s3_cfg = S3Config::from_env()?;
    let s3_client = crate::s3::new_client(&s3_cfg).await?;
    let update_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let s3_key = format!("{}/updates/kresko-{update_id}", config.experiment);

    crate::s3::upload_file(
        &s3_client,
        &s3_cfg,
        &s3_cfg.bucket_name,
        &s3_key,
        &binary_path,
    )
    .await?;
    let download_url = crate::s3::presign_get_url(
        &s3_client,
        &s3_cfg.bucket_name,
        &s3_key,
        Duration::from_secs(3600),
    )
    .await?;

    let mut failed_miners = HashSet::new();
    let mut failure_details = Vec::new();

    for chunk in active_miners.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let ip = inst.public_ip.clone();
                let name = inst.name.clone();
                let key = key.clone();
                let url = download_url.clone();

                async move {
                    println!("  {name}: downloading kresko binary from S3...");
                    let result = ssh::ssh_exec(
                        &ip,
                        &key,
                        &format!(
                            "if ! command -v curl >/dev/null 2>&1; then \
                                 apt-get -o DPkg::Lock::Timeout=300 update -y && \
                                 apt-get -o DPkg::Lock::Timeout=300 install -y curl; \
                             fi && \
                             curl -fsSL -o /tmp/kresko.new {} && \
                             install -m 0755 /tmp/kresko.new /usr/local/bin/kresko.new && \
                             mv -f /usr/local/bin/kresko.new /usr/local/bin/kresko && \
                             rm -f /tmp/kresko.new && \
                             test -x /usr/local/bin/kresko",
                            shell_single_quote(&url),
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
                Ok(()) => println!("  {name}: updated"),
                Err(e) => {
                    eprintln!("  Update failed for {name}: {e}");
                    failed_miners.insert(name.clone());
                    failure_details.push(format!("{name}: update failed: {e}"));
                }
            }
        }
    }

    if !failure_details.is_empty() {
        eprintln!(
            "Binary update completed with failures on {} miner(s):",
            failed_miners.len()
        );
        for detail in &failure_details {
            eprintln!("  - {detail}");
        }

        if !ignore_failed_miners {
            anyhow::bail!(
                "binary update encountered errors on {} miner(s); rerun with --ignore-failed-miners to suppress failure exit",
                failed_miners.len()
            );
        }
    }

    println!("Binary update complete.");
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
