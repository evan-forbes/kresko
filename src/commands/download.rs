use anyhow::Result;
use futures::future::join_all;

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

pub async fn run(nodes: &str, workers: usize, no_compress: bool, directory: &str) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, nodes);

    if targets.is_empty() {
        println!("No matching nodes found.");
        return Ok(());
    }

    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir)?;

    println!("Downloading logs from {} nodes...", targets.len());

    // Process in chunks of `workers`
    for chunk in targets.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let ip = inst.public_ip.clone();
                let name = inst.name.clone();
                let key = key.clone();
                let node_dir = data_dir.join(&name);
                let no_compress = no_compress;

                async move {
                    std::fs::create_dir_all(&node_dir)?;

                    if !no_compress {
                        // Compress logs on remote first
                        let _ =
                            ssh::ssh_exec(&ip, &key, "xz -z /root/logs 2>/dev/null || true").await;

                        // Download compressed log
                        let remote = "/root/logs.xz";
                        let local = node_dir.join("logs.xz");
                        match ssh::sftp_download(&ip, &key, remote, local.to_str().unwrap()).await {
                            Ok(()) => println!("  {name}: downloaded logs.xz"),
                            Err(_) => {
                                // Try uncompressed
                                let remote = "/root/logs";
                                let local = node_dir.join("logs");
                                ssh::sftp_download(&ip, &key, remote, local.to_str().unwrap())
                                    .await?;
                                println!("  {name}: downloaded logs");
                            }
                        }
                    } else {
                        let remote = "/root/logs";
                        let local = node_dir.join("logs");
                        ssh::sftp_download(&ip, &key, remote, local.to_str().unwrap()).await?;
                        println!("  {name}: downloaded logs");
                    }

                    Ok::<_, anyhow::Error>(())
                }
            })
            .collect();

        let results = join_all(futs).await;
        for r in results {
            if let Err(e) = r {
                eprintln!("  Warning: {e}");
            }
        }
    }

    println!("Downloads complete. Data saved to {}", data_dir.display());
    Ok(())
}
