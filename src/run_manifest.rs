use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    /// Run name; also the data subdirectory under `data/`.
    pub name: String,

    /// Path of the init script inside the unpacked payload, e.g.
    /// "scripts/init_scripts/cubic_fq.sh". Executed on every miner before
    /// the workload window opens.
    pub init_script: String,

    /// Wallclock duration (seconds) after init_script returns, before stop+collect.
    pub duration_secs: u64,

    /// Optional payload-relative path to a shutdown script run at the end of
    /// the duration. If unset, kresko kills the canonical tmux sessions.
    #[serde(default)]
    pub stop_script: Option<String>,

    /// Optional: enable local progress logging during this run. Output goes
    /// to data/<name>/progress.log.jsonl.
    #[serde(default)]
    pub progress_block_time_secs: Option<u64>,
}

impl RunManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read run manifest at {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("failed to parse run manifest at {}", path.display()))
    }
}
