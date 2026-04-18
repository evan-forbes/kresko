use anyhow::{Context, Result};
use std::path::Path;
use tokio::time::{Duration, sleep};

use crate::commands::{download, exec, progress, reset};
use crate::run_manifest::RunManifest;

pub async fn run(manifest_path: &str, workers: usize, directory: &str) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = Path::new(directory);
    let manifest_pb = std::path::PathBuf::from(manifest_path);
    let manifest = RunManifest::load(&manifest_pb)?;

    let data_dir = dir.join("data").join(&manifest.name);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create {}", data_dir.display()))?;

    // Freeze the manifest alongside its data.
    std::fs::copy(&manifest_pb, data_dir.join("manifest.toml"))
        .with_context(|| format!("failed to copy manifest to {}", data_dir.display()))?;

    println!("=== run: {} ===", manifest.name);

    // 1. Exec init_script on all miners. Per-node failures don't abort the run.
    let init_target = format!("/root/payload/{}", manifest.init_script);
    let init_env = run_env(&manifest.name);
    let init_result = exec::run_remote_path(
        "all",
        workers,
        directory,
        &init_target,
        false,
        false,
        &init_env,
    )
    .await?;

    if init_result.failed_nodes.is_empty() {
        println!("  init ok on all {} miners", init_result.total);
    } else {
        println!(
            "  init failed on {} / {} miners (continuing): {:?}",
            init_result.failed_nodes.len(),
            init_result.total,
            init_result.failed_nodes
        );
    }

    // 2. Optional background progress logger.
    let progress_handle = if let Some(bt) = manifest.progress_block_time_secs {
        Some(progress::spawn_background(dir, bt, &manifest.name).await?)
    } else {
        None
    };

    // 3. Wallclock window. Operator can intervene early via `kresko exec`.
    println!("  running for {}s...", manifest.duration_secs);
    sleep(Duration::from_secs(manifest.duration_secs)).await;

    // 4. Stop the workload.
    match &manifest.stop_script {
        Some(script) => {
            let target = format!("/root/payload/{}", script);
            let stop_env = run_env(&manifest.name);
            let _ =
                exec::run_remote_path("all", workers, directory, &target, false, false, &stop_env)
                    .await;
        }
        None => {
            reset::kill_known_sessions("all", workers, directory).await?;
        }
    }

    if let Some(handle) = progress_handle {
        handle.abort();
    }

    // 5. Download artifacts into data/<name>/.
    let subdir = manifest.name.as_str();
    if let Err(e) = download::run_logs("all", workers, false, directory, Some(subdir)).await {
        eprintln!("  log download failed: {e:#}");
    }
    if let Err(e) = download::run_traces("all", workers, "all", directory, Some(subdir)).await {
        eprintln!("  trace download failed: {e:#}");
    }

    // 6. Persist init summary alongside the data.
    std::fs::write(
        data_dir.join("init_summary.json"),
        serde_json::to_string_pretty(&init_result)?,
    )
    .with_context(|| {
        format!(
            "failed to write init_summary.json in {}",
            data_dir.display()
        )
    })?;

    println!(
        "=== run {} complete -- data in {} ===",
        manifest.name,
        data_dir.display()
    );
    Ok(())
}

fn run_env(run_name: &str) -> Vec<(String, String)> {
    vec![
        ("KRESKO_RUN_NAME".to_string(), run_name.to_string()),
        (
            "KRESKO_PAYLOAD_DIR".to_string(),
            "/root/payload".to_string(),
        ),
    ]
}
