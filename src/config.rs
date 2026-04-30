use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use zebra_chain::parameters::EquihashParams;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MiningMode {
    #[default]
    Generate,
    Pow,
}

impl std::fmt::Display for MiningMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MiningMode::Generate => write!(f, "generate"),
            MiningMode::Pow => write!(f, "pow"),
        }
    }
}

impl std::str::FromStr for MiningMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "generate" => Ok(MiningMode::Generate),
            "pow" => Ok(MiningMode::Pow),
            other => anyhow::bail!("unknown mining mode: {other}. Use generate or pow."),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquihashParameterSet {
    Common,
    #[default]
    Regtest,
}

impl std::fmt::Display for EquihashParameterSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EquihashParameterSet::Common => write!(f, "common"),
            EquihashParameterSet::Regtest => write!(f, "regtest"),
        }
    }
}

impl std::str::FromStr for EquihashParameterSet {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "common" | "mainnet" | "testnet" | "200_9" | "200,9" => {
                Ok(EquihashParameterSet::Common)
            }
            "regtest" | "easy" | "48_5" | "48,5" => Ok(EquihashParameterSet::Regtest),
            other => {
                anyhow::bail!("unknown equihash parameter set: {other}. Use common or regtest.")
            }
        }
    }
}

impl From<EquihashParameterSet> for EquihashParams {
    fn from(value: EquihashParameterSet) -> Self {
        match value {
            EquihashParameterSet::Common => EquihashParams::Common,
            EquihashParameterSet::Regtest => EquihashParams::Regtest,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkKind {
    #[default]
    LocalGenesis,
    PublicTestnet,
    Mainnet,
}

impl std::fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkKind::LocalGenesis => write!(f, "local-genesis"),
            NetworkKind::PublicTestnet => write!(f, "public-testnet"),
            NetworkKind::Mainnet => write!(f, "mainnet"),
        }
    }
}

impl std::str::FromStr for NetworkKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "local-genesis" | "local_genesis" | "local" => Ok(NetworkKind::LocalGenesis),
            "public-testnet" | "public_testnet" | "testnet" => Ok(NetworkKind::PublicTestnet),
            "mainnet" | "main" => Ok(NetworkKind::Mainnet),
            other => anyhow::bail!(
                "unknown network kind: {other}. Use local-genesis, public-testnet, or mainnet."
            ),
        }
    }
}

impl NetworkKind {
    pub fn is_local_genesis(self) -> bool {
        self == NetworkKind::LocalGenesis
    }

    pub fn is_public_network(self) -> bool {
        matches!(self, NetworkKind::PublicTestnet | NetworkKind::Mainnet)
    }

    pub fn rpc_port(self) -> u16 {
        match self {
            NetworkKind::Mainnet => 8232,
            NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => 18232,
        }
    }

    pub fn p2p_port(self) -> u16 {
        match self {
            NetworkKind::Mainnet => 8233,
            NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => 18233,
        }
    }

    pub fn zebra_network_string(self) -> &'static str {
        match self {
            NetworkKind::Mainnet => "Mainnet",
            NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => "Testnet",
        }
    }
}

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
    Linode,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::DigitalOcean => write!(f, "digitalocean"),
            Provider::GoogleCloud => write!(f, "googlecloud"),
            Provider::Linode => write!(f, "linode"),
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "digitalocean" | "do" => Ok(Provider::DigitalOcean),
            "googlecloud" | "gcp" | "google" => Ok(Provider::GoogleCloud),
            "linode" => Ok(Provider::Linode),
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
    #[serde(default = "default_tier")]
    pub tier: String,
}

pub fn default_tier() -> String {
    "full".into()
}

impl Instance {
    pub fn new_base(
        node_type: NodeType,
        provider: Provider,
        slug: &str,
        region: &str,
        name: &str,
        experiment: &str,
        tier: &str,
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
            tier: tier.to_string(),
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
    #[serde(default)]
    pub network_kind: NetworkKind,
    #[serde(default)]
    pub mining_mode: MiningMode,
    #[serde(default)]
    pub block_time_secs: Option<u32>,
    #[serde(default)]
    pub equihash_params: EquihashParameterSet,
    #[serde(default)]
    pub daa: DaaConfig,
    #[serde(default)]
    pub orchard_txblast: OrchardTxblastConfig,
    pub local_genesis: Option<LocalGenesisConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaaConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pow_averaging_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pow_median_block_span: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_blossom_pow_target_spacing: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pow_damping_factor: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pow_max_adjust_up_percent: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pow_max_adjust_down_percent: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testnet_min_difficulty_start_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testnet_min_difficulty_gap_multiplier: Option<i32>,
}

impl DaaConfig {
    pub fn tuned_25s_defaults() -> Self {
        Self {
            pow_averaging_window: Some(51),
            pow_median_block_span: Some(33),
            pre_blossom_pow_target_spacing: None,
            pow_damping_factor: Some(4),
            pow_max_adjust_up_percent: Some(16),
            pow_max_adjust_down_percent: Some(32),
            testnet_min_difficulty_start_height: None,
            testnet_min_difficulty_gap_multiplier: None,
        }
    }

    pub fn with_missing_from(self, defaults: Self) -> Self {
        Self {
            pow_averaging_window: self.pow_averaging_window.or(defaults.pow_averaging_window),
            pow_median_block_span: self
                .pow_median_block_span
                .or(defaults.pow_median_block_span),
            pre_blossom_pow_target_spacing: self
                .pre_blossom_pow_target_spacing
                .or(defaults.pre_blossom_pow_target_spacing),
            pow_damping_factor: self.pow_damping_factor.or(defaults.pow_damping_factor),
            pow_max_adjust_up_percent: self
                .pow_max_adjust_up_percent
                .or(defaults.pow_max_adjust_up_percent),
            pow_max_adjust_down_percent: self
                .pow_max_adjust_down_percent
                .or(defaults.pow_max_adjust_down_percent),
            testnet_min_difficulty_start_height: self
                .testnet_min_difficulty_start_height
                .or(defaults.testnet_min_difficulty_start_height),
            testnet_min_difficulty_gap_multiplier: self
                .testnet_min_difficulty_gap_multiplier
                .or(defaults.testnet_min_difficulty_gap_multiplier),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchardTxblastConfig {
    pub lanes_per_miner: usize,
    pub lane_value_zats: u64,
    pub fanout_source_value_zats: u64,
    pub fanout_outputs: usize,
}

impl Default for OrchardTxblastConfig {
    fn default() -> Self {
        Self {
            lanes_per_miner: 100,
            lane_value_zats: 30_000,
            fanout_source_value_zats: 500_000,
            fanout_outputs: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalGenesisConfig {
    pub network_name: String,
    pub network_magic: [u8; 4],
    pub target_difficulty_limit: String,
    /// Live post-Blossom PoW target spacing used by the generated testnet.
    #[serde(default)]
    pub target_spacing_secs: Option<u32>,
    pub disable_pow: bool,
    pub genesis_hash: String,
    #[serde(default)]
    pub seeded_tip_hash: Option<String>,
    pub genesis_hex: String,
    pub slow_start_interval: u32,
    pub pre_blossom_halving_interval: u32,
    pub activation_heights: LocalGenesisActivationHeights,
    #[serde(default)]
    pub maturity_padding_block_count: u32,
    #[serde(default)]
    pub premine_block_count: u32,
    #[serde(default)]
    pub seeded_block_count: u32,
    #[serde(default)]
    pub bootstrap_treasury_key: Option<LocalGenesisFundedKey>,
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

    pub fn is_local_genesis(&self) -> bool {
        self.network_kind.is_local_genesis()
    }

    pub fn is_public_network(&self) -> bool {
        self.network_kind.is_public_network()
    }

    pub fn rpc_port(&self) -> u16 {
        self.network_kind.rpc_port()
    }

    pub fn p2p_port(&self) -> u16 {
        self.network_kind.p2p_port()
    }

    pub fn zebra_network_string(&self) -> &'static str {
        self.network_kind.zebra_network_string()
    }

    pub fn require_local_genesis(&self, command_name: &str) -> Result<()> {
        if self.is_local_genesis() {
            Ok(())
        } else {
            anyhow::bail!(
                "`kresko {command_name}` only works with local-genesis experiments (current network: {}). Use public-network commands such as `kresko genesis-public` for public networks.",
                self.network_kind
            )
        }
    }

    pub fn require_public_network(&self, command_name: &str) -> Result<()> {
        if self.is_public_network() {
            Ok(())
        } else {
            anyhow::bail!(
                "`kresko {command_name}` only works with public-testnet or mainnet experiments (current network: {}). Use `kresko genesis` for local-genesis experiments.",
                self.network_kind
            )
        }
    }
}

pub fn provider_configs(base: &Config) -> Vec<Config> {
    let mut providers = Vec::new();

    if base.miners.is_empty() {
        providers.push(base.provider);
    } else {
        for instance in &base.miners {
            if !providers.contains(&instance.provider) {
                providers.push(instance.provider);
            }
        }
    }

    providers
        .into_iter()
        .map(|provider| {
            let mut config = base.clone();
            config.provider = provider;
            if !base.miners.is_empty() {
                config.miners = base
                    .miners
                    .iter()
                    .filter(|instance| instance.provider == provider)
                    .cloned()
                    .collect();
            }
            config
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::DaaConfig;

    #[test]
    fn tuned_25s_daa_defaults_match_expected_profile() {
        let daa = DaaConfig::tuned_25s_defaults();

        assert_eq!(daa.pow_averaging_window, Some(51));
        assert_eq!(daa.pow_median_block_span, Some(33));
        assert_eq!(daa.pow_damping_factor, Some(4));
        assert_eq!(daa.pow_max_adjust_up_percent, Some(16));
        assert_eq!(daa.pow_max_adjust_down_percent, Some(32));
    }

    #[test]
    fn daa_defaults_fill_missing_values_without_overwriting_explicit_values() {
        let daa = DaaConfig {
            pow_averaging_window: Some(17),
            ..DaaConfig::default()
        }
        .with_missing_from(DaaConfig::tuned_25s_defaults());

        assert_eq!(daa.pow_averaging_window, Some(17));
        assert_eq!(daa.pow_median_block_span, Some(33));
        assert_eq!(daa.pow_damping_factor, Some(4));
    }
}

// Default instance shapes/images per provider
/// DigitalOcean miner slugs in preference order. `add` walks this list per
/// region and assigns the first slug the region actually carries, so we
/// fall back to premium AMD / Intel variants when the basic Intel slug
/// isn't stocked in that datacenter.
pub const DO_FULL_MINER_SLUG_FALLBACKS: &[&str] =
    &["s-8vcpu-16gb", "s-8vcpu-16gb-amd", "s-8vcpu-16gb-intel"];
pub const DO_LOW_MINER_SLUG_FALLBACKS: &[&str] =
    &["s-4vcpu-8gb", "s-4vcpu-8gb-amd", "s-4vcpu-8gb-intel"];
pub const DO_PUBLIC_TESTNET_FULL_MINER_SLUG_FALLBACKS: &[&str] =
    &["s-4vcpu-8gb", "s-4vcpu-8gb-amd", "s-4vcpu-8gb-intel"];
pub const DO_MAINNET_FULL_MINER_SLUG_FALLBACKS: &[&str] = &["so-4vcpu-32gb"];
pub const DO_DEFAULT_IMAGE: &str = "ubuntu-22-04-x64";
pub const DO_REGIONS: &[&str] = &[
    "nyc1", "nyc3", "tor1", "sfo2", "sfo3", "ams3", "sgp1", "lon1", "fra1", "syd1",
];

pub const GCP_DEFAULT_MACHINE: &str = "c3d-highcpu-8";
pub const GCP_LOW_RESOURCE_MACHINE: &str = "c3d-highcpu-4";
pub const GCP_DEFAULT_DISK_SIZE_GB: u64 = 40;
pub fn gcp_disk_size_gb(network_kind: NetworkKind) -> u64 {
    match network_kind {
        NetworkKind::LocalGenesis => 40,
        NetworkKind::PublicTestnet => 100,
        NetworkKind::Mainnet => 500,
    }
}
pub const GCP_REGIONS: &[&str] = &[
    "us-central1",
    "us-east1",
    "us-east4",
    "asia-southeast1",
    "europe-west1",
    "asia-east1",
];

pub const LINODE_DEFAULT_MINER_TYPE: &str = "g6-dedicated-8";
pub const LINODE_LOW_RESOURCE_MINER_TYPE: &str = "g6-dedicated-4";
pub const LINODE_DEFAULT_IMAGE: &str = "linode/ubuntu22.04";
pub const LINODE_REGIONS: &[&str] = &[
    "us-east",
    "us-central",
    "us-west",
    "us-southeast",
    "us-ord",
    "us-iad",
    "us-lax",
    "us-mia",
    "us-sea",
    "ca-central",
    "br-gru",
    "eu-west",
    "eu-central",
    "gb-lon",
    "fr-par",
    "fr-par-2",
    "de-fra-2",
    "nl-ams",
    "es-mad",
    "it-mil",
    "se-sto",
    "ap-south",
    "ap-west",
    "ap-southeast",
    "ap-northeast",
    "in-maa",
    "in-bom-2",
    "jp-osa",
    "jp-tyo-3",
    "sg-sin-2",
    "id-cgk",
    "au-mel",
];
