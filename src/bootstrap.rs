use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::LocalGenesisFundedKey;

pub const DEFAULT_POW_BOOTSTRAP_ARTIFACT_ID: &str = "pow_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapManifest {
    pub artifact_id: String,
    pub seeded_block_count: u32,
    pub premine_block_count: u32,
    pub maturity_padding_block_count: u32,
    pub target_difficulty_limit: String,
    pub disable_pow: bool,
    pub genesis_hash: String,
    pub seeded_tip_hash: String,
    pub slow_start_interval: u32,
    pub pre_blossom_halving_interval: u32,
    pub activation_height: u32,
    pub treasury_address: String,
    pub treasury_public_key_hex: String,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct BootstrapBundle {
    root_dir: PathBuf,
    manifest: BootstrapManifest,
    treasury_key: LocalGenesisFundedKey,
}

impl BootstrapBundle {
    pub fn load(artifact_id: &str) -> Result<Self> {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("bootstrap")
            .join(artifact_id);
        let manifest_path = root_dir.join("manifest.json");
        let treasury_key_path = root_dir.join("treasury_key.json");

        let manifest = serde_json::from_slice::<BootstrapManifest>(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("failed to read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

        let treasury_key = serde_json::from_slice::<LocalGenesisFundedKey>(
            &std::fs::read(&treasury_key_path)
                .with_context(|| format!("failed to read {}", treasury_key_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", treasury_key_path.display()))?;

        if manifest.artifact_id != artifact_id {
            anyhow::bail!(
                "bootstrap artifact id mismatch: expected {artifact_id}, found {}",
                manifest.artifact_id
            );
        }
        if manifest.treasury_address != treasury_key.address {
            anyhow::bail!(
                "bootstrap treasury address mismatch between manifest and treasury_key.json"
            );
        }
        if manifest.treasury_public_key_hex != treasury_key.public_key_hex {
            anyhow::bail!(
                "bootstrap treasury public key mismatch between manifest and treasury_key.json"
            );
        }

        Ok(Self {
            root_dir,
            manifest,
            treasury_key,
        })
    }

    pub fn manifest(&self) -> &BootstrapManifest {
        &self.manifest
    }

    pub fn treasury_key(&self) -> &LocalGenesisFundedKey {
        &self.treasury_key
    }

    pub fn read_text_file(&self, file_name: &str) -> Result<String> {
        let path = self.root_dir.join(file_name);
        std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
    }

    pub fn copy_payload_files_to_vec(
        &self,
        destination: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<()> {
        for file_name in [
            "genesis.hex",
            "premine_blocks.hex",
            "checkpoints.txt",
            "manifest.json",
            "treasury_key.json",
        ] {
            let path = self.root_dir.join(file_name);
            destination.push((
                file_name.to_string(),
                std::fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            ));
        }
        Ok(())
    }
}
