use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Miner,
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeType::Miner => write!(f, "miner"),
        }
    }
}

impl std::str::FromStr for NodeType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "miner" => Ok(NodeType::Miner),
            other => anyhow::bail!("unknown node type: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    DigitalOcean,
    GoogleCloud,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::DigitalOcean => write!(f, "digitalocean"),
            Provider::GoogleCloud => write!(f, "googlecloud"),
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "digitalocean" | "do" => Ok(Provider::DigitalOcean),
            "googlecloud" | "gcp" | "google" => Ok(Provider::GoogleCloud),
            other => anyhow::bail!("unknown provider: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub node_type: NodeType,
    pub public_ip: String,
    pub private_ip: String,
    pub provider: Provider,
    pub slug: String,
    pub region: String,
    pub name: String,
    pub tags: Vec<String>,
}

impl Instance {
    pub fn new_base(
        node_type: NodeType,
        provider: Provider,
        slug: &str,
        region: &str,
        name: &str,
        experiment: &str,
    ) -> Self {
        Self {
            node_type,
            public_ip: "TBD".to_string(),
            private_ip: "TBD".to_string(),
            provider,
            slug: slug.to_string(),
            region: region.to_string(),
            name: name.to_string(),
            tags: vec!["kresko".to_string(), experiment_tag(experiment)],
        }
    }

    pub fn parsed_hostname(&self) -> String {
        let parts: Vec<&str> = self.name.split('-').collect();
        if parts.len() >= 2 {
            format!("{}-{}", parts[0], parts[1])
        } else {
            self.name.clone()
        }
    }
}

pub fn experiment_tag(experiment: &str) -> String {
    format!("kresko-{experiment}")
}

#[derive(Debug, Clone, Default)]
pub struct S3Config {
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket_name: String,
    pub endpoint: String,
}

impl S3Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            region: std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
            access_key_id: require_env("AWS_ACCESS_KEY_ID")?,
            secret_access_key: require_env("AWS_SECRET_ACCESS_KEY")?,
            bucket_name: std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "kresko-data".into()),
            endpoint: std::env::var("AWS_S3_ENDPOINT").unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub miners: Vec<Instance>,
    pub chain_id: String,
    pub experiment: String,
    pub ssh_pub_key_path: String,
    pub ssh_key_name: String,
    pub ssh_key_path: String,
    pub provider: Provider,
    pub local_genesis: Option<LocalGenesisConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalGenesisConfig {
    pub network_name: String,
    pub network_magic: [u8; 4],
    pub target_difficulty_limit: String,
    pub disable_pow: bool,
    pub genesis_hash: String,
    pub genesis_hex: String,
    pub slow_start_interval: u32,
    pub pre_blossom_halving_interval: u32,
    pub activation_heights: LocalGenesisActivationHeights,
    pub premine_block_count: u32,
    pub funded_keys: Vec<LocalGenesisFundedKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalGenesisFundedKey {
    pub name: String,
    pub secret_key_hex: String,
    pub public_key_hex: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalGenesisActivationHeights {
    pub overwinter: u32,
    pub sapling: u32,
    pub blossom: u32,
    pub heartwood: u32,
    pub canopy: u32,
    pub nu5: u32,
    pub nu6: u32,
    pub nu6_1: u32,
}

pub fn require_env(var: &str) -> Result<String> {
    let val = std::env::var(var).unwrap_or_default();
    if val.is_empty() {
        anyhow::bail!("{var} is not set. Add it to your .env file.");
    }
    Ok(val)
}

impl Config {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("config.json");
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        serde_json::from_str(&data).context("failed to parse config.json")
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join("config.json");
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)
            .with_context(|| format!("failed to write config to {}", path.display()))?;
        Ok(())
    }
}

/// Resolve a value with priority: flag > env > config
pub fn resolve_value(flag: Option<&str>, env_var: &str, config_val: &str) -> String {
    if let Some(v) = flag {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(v) = std::env::var(env_var) {
        if !v.is_empty() {
            return v;
        }
    }
    config_val.to_string()
}

/// Expand `~/` to $HOME in a path string.
pub fn shellexpand(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}{}", &path[1..]);
        }
    }
    path.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxType {
    Transparent,
    Shielded,
    Both,
}

impl std::fmt::Display for TxType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxType::Transparent => write!(f, "transparent"),
            TxType::Shielded => write!(f, "shielded"),
            TxType::Both => write!(f, "both"),
        }
    }
}

impl std::str::FromStr for TxType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "transparent" => Ok(TxType::Transparent),
            "shielded" => Ok(TxType::Shielded),
            "both" => Ok(TxType::Both),
            other => anyhow::bail!("unknown tx type: {other}. Use transparent, shielded, or both."),
        }
    }
}

/// Select active instances by pattern. Supports:
/// - "all" or "*" to select all active instances
/// - comma-separated indices: "0,2,5"
/// - comma-separated wildcard name patterns: "miner-0-*,miner-1-*"
pub fn select_instances<'a>(instances: &'a [Instance], pattern: &str) -> Vec<&'a Instance> {
    let active: Vec<_> = instances.iter().filter(|i| i.public_ip != "TBD").collect();

    if pattern == "all" || pattern == "*" {
        return active;
    }

    let parts: Vec<&str> = pattern
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    // If all parts parse as numbers, treat as indices
    let indices: Vec<usize> = parts.iter().filter_map(|s| s.parse().ok()).collect();
    if indices.len() == parts.len() {
        return active
            .into_iter()
            .enumerate()
            .filter(|(idx, _)| indices.contains(idx))
            .map(|(_, inst)| inst)
            .collect();
    }

    // Otherwise treat as wildcard name patterns
    active
        .into_iter()
        .filter(|i| parts.iter().any(|p| wildcard_match(p, &i.name)))
        .collect()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_idx, mut match_idx) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// Default slugs/regions per provider
pub const DO_DEFAULT_MINER_SLUG: &str = "s-8vcpu-16gb";
pub const DO_DEFAULT_IMAGE: &str = "ubuntu-22-04-x64";
pub const DO_REGIONS: &[&str] = &[
    "nyc1", "nyc3", "tor1", "sfo2", "sfo3", "ams3", "sgp1", "lon1", "fra1", "syd1",
];

pub const GCP_DEFAULT_MACHINE: &str = "c3d-highcpu-16";
pub const GCP_DEFAULT_DISK_SIZE_GB: u64 = 400;
pub const GCP_REGIONS: &[&str] = &[
    "us-central1",
    "us-east1",
    "us-east4",
    "asia-southeast1",
    "europe-west1",
    "asia-east1",
];
