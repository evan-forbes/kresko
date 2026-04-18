use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::commands::{exec::ExecResult, run};

#[derive(Debug, Deserialize)]
struct QueueFile {
    runs: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueState {
    next_index: usize,
    results: Vec<QueueRunResult>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueueRunResult {
    manifest: String,
    init_failed_nodes: Vec<String>,
    download_failed: bool,
}

const STATE_FILE: &str = ".kresko-queue-state.json";

pub async fn run_queue(
    queue_path: &str,
    workers: usize,
    directory: &str,
    resume: bool,
    halt_on_failure: bool,
) -> Result<()> {
    let dir = Path::new(directory);
    let state_path = dir.join(STATE_FILE);

    let queue_raw = std::fs::read_to_string(queue_path)
        .with_context(|| format!("failed to read queue file at {queue_path}"))?;
    let queue: QueueFile = toml::from_str(&queue_raw)
        .with_context(|| format!("failed to parse queue file at {queue_path}"))?;

    let mut state: QueueState = if resume && state_path.exists() {
        let raw = std::fs::read_to_string(&state_path)?;
        serde_json::from_str(&raw).context("failed to parse queue state file")?
    } else {
        QueueState::default()
    };

    while state.next_index < queue.runs.len() {
        let manifest = queue.runs[state.next_index].clone();
        println!(
            "\n>>> queue step {}/{}: {manifest}",
            state.next_index + 1,
            queue.runs.len()
        );

        match run::run(&manifest, workers, directory).await {
            Ok(()) => {
                let init_summary = read_init_summary(dir, &manifest).unwrap_or_default();
                state.results.push(QueueRunResult {
                    manifest: manifest.clone(),
                    init_failed_nodes: init_summary.failed_nodes,
                    download_failed: false,
                });
            }
            Err(e) => {
                eprintln!("!!! catastrophic run failure: {e:#}");
                state.results.push(QueueRunResult {
                    manifest: manifest.clone(),
                    init_failed_nodes: vec![],
                    download_failed: true,
                });
                if halt_on_failure {
                    state.next_index += 1;
                    persist(&state_path, &state)?;
                    return Err(e);
                }
            }
        }

        state.next_index += 1;
        persist(&state_path, &state)?;
    }

    println!("\n=== queue complete ===");
    for r in &state.results {
        let status = if r.download_failed {
            "FAILED"
        } else if !r.init_failed_nodes.is_empty() {
            "completed with init failures"
        } else {
            "ok"
        };
        println!("  {}: {status}", r.manifest);
    }
    Ok(())
}

fn read_init_summary(dir: &Path, manifest_path: &str) -> Option<ExecResult> {
    // The run name is also the data subdirectory; the manifest's `name` field
    // drives that. Read the manifest to discover the name, then pull the
    // init_summary.json out of data/<name>/.
    let manifest = crate::run_manifest::RunManifest::load(Path::new(manifest_path)).ok()?;
    let summary_path = dir
        .join("data")
        .join(&manifest.name)
        .join("init_summary.json");
    let raw = std::fs::read_to_string(summary_path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn persist(path: &Path, state: &QueueState) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("failed to write queue state to {}", path.display()))?;
    Ok(())
}
