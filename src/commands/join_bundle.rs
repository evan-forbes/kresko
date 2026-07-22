use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{
    Config, DaaConfig, Instance, LocalGenesisActivationHeights, LocalGenesisConfig, NetworkKind,
    NodeType, Provider,
};
use crate::zebra_config::{self, LocalTestnetParameters};

const JOIN_INSTALL_ROOT: &str = "/opt/nu7-testnet";
const JOIN_CHECKPOINTS_PATH: &str = "/opt/nu7-testnet/bundle/local_genesis/checkpoints.txt";
const DEFAULT_OBSERVER_MINER_ADDRESS: &str = "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinManifest {
    pub chain_id: String,
    pub genesis_hash: String,
    pub seeded_tip_hash: Option<String>,
    pub network_magic: [u8; 4],
    pub target_difficulty_limit: String,
    pub target_spacing_secs: Option<u32>,
    pub activation_heights: LocalGenesisActivationHeights,
    pub bootstrap_peers: Vec<String>,
    /// GitHub release the join script downloads the prebuilt zebrad from.
    pub zebra_release_repo: String,
    pub zebra_release_tag: String,
    /// GitHub release the join script downloads the prebuilt kresko from (--mine).
    pub kresko_release_repo: String,
    pub kresko_release_tag: String,
    pub generated_at_unix_secs: u64,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct PayloadPremineManifest {
    #[serde(default)]
    pow_start_height: Option<u32>,
}

pub fn run(
    run_dir: &str,
    zebra_release_repo: &str,
    zebra_release_tag: &str,
    kresko_release_repo: &str,
    kresko_release_tag: &str,
    out: &str,
) -> Result<()> {
    let run_dir = Path::new(run_dir);
    let out_dir = Path::new(out);
    let config = Config::load(run_dir)?;
    config.require_local_genesis("join-bundle")?;

    let local_genesis = config
        .local_genesis
        .as_ref()
        .context("config.json has no local_genesis; run `kresko genesis` first")?;
    let bootstrap_peers = bootstrap_peers(&config)?;
    if bootstrap_peers.is_empty() {
        anyhow::bail!("no active bootstrap peers found in config.json; run `kresko up` first");
    }

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let out_local_genesis = out_dir.join("local_genesis");
    if out_local_genesis.exists() {
        std::fs::remove_dir_all(&out_local_genesis)
            .with_context(|| format!("failed to clear {}", out_local_genesis.display()))?;
    }
    std::fs::create_dir_all(&out_local_genesis)
        .with_context(|| format!("failed to create {}", out_local_genesis.display()))?;

    let payload_local_genesis = run_dir.join("payload/local_genesis");
    for file_name in ["genesis.hex", "premine_blocks.hex", "checkpoints.txt"] {
        let source = payload_local_genesis.join(file_name);
        if !source.is_file() {
            anyhow::bail!(
                "missing required payload artifact {}; run `kresko genesis` first",
                source.display()
            );
        }
        std::fs::copy(&source, out_local_genesis.join(file_name))
            .with_context(|| format!("failed to copy {}", source.display()))?;
    }

    let zebrad_config = render_join_zebrad_config(run_dir, &config, local_genesis)?;
    let zebrad_config_path = out_dir.join("zebrad.join.toml");
    std::fs::write(&zebrad_config_path, zebrad_config)
        .with_context(|| format!("failed to write {}", zebrad_config_path.display()))?;

    // The bundle is data-only: the join script (scripts/join-nu7-testnet.sh) is
    // downloaded separately and reads these files plus the release coordinates
    // below to fetch the prebuilt zebrad/kresko binaries.
    let mut files = BTreeMap::new();
    for relative_path in [
        "zebrad.join.toml",
        "local_genesis/genesis.hex",
        "local_genesis/premine_blocks.hex",
        "local_genesis/checkpoints.txt",
    ] {
        files.insert(
            relative_path.to_string(),
            sha256_file(&out_dir.join(relative_path))?,
        );
    }

    let generated_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_secs();
    let manifest = JoinManifest {
        chain_id: config.chain_id.clone(),
        genesis_hash: local_genesis.genesis_hash.clone(),
        seeded_tip_hash: local_genesis.seeded_tip_hash.clone(),
        network_magic: local_genesis.network_magic,
        target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
        target_spacing_secs: local_genesis.target_spacing_secs,
        activation_heights: local_genesis.activation_heights.clone(),
        bootstrap_peers,
        zebra_release_repo: zebra_release_repo.to_string(),
        zebra_release_tag: zebra_release_tag.to_string(),
        kresko_release_repo: kresko_release_repo.to_string(),
        kresko_release_tag: kresko_release_tag.to_string(),
        generated_at_unix_secs,
        files,
    };
    let manifest_path = out_dir.join("join-manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    println!(
        "Join bundle generated in {} ({} bootstrap peers)",
        out_dir.display(),
        manifest.bootstrap_peers.len()
    );
    Ok(())
}

fn render_join_zebrad_config(
    run_dir: &Path,
    config: &Config,
    local_genesis: &LocalGenesisConfig,
) -> Result<String> {
    let template_path = run_dir.join("zebrad.toml");
    let template = if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .with_context(|| format!("failed to read {}", template_path.display()))?
    } else {
        zebra_config::template_for(config.network_kind)?
    };
    let toml_network = zebra_config::testnet_toml_parameters(&template)
        .with_context(|| format!("invalid testnet parameters in {}", template_path.display()))?;
    let daa = toml_network
        .daa
        .with_missing_from(config.daa)
        .with_missing_from(DaaConfig::tuned_25s_defaults());
    let pow_start_height = payload_pow_start_height(&run_dir.join("payload/local_genesis"))?;
    let local_testnet = LocalTestnetParameters {
        activates_nu7: local_genesis.activation_heights.nu7.is_some(),
        network_name: local_genesis.network_name.clone(),
        network_magic: local_genesis.network_magic,
        target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
        disable_pow: local_genesis.disable_pow,
        genesis_hash: local_genesis.genesis_hash.clone(),
        checkpoints_path: JOIN_CHECKPOINTS_PATH.to_string(),
        slow_start_interval: local_genesis.slow_start_interval,
        pre_blossom_halving_interval: local_genesis.pre_blossom_halving_interval,
        activation_height: local_genesis.activation_heights.overwinter,
        lockbox_disbursements: zebra_config::default_nu6_1_lockbox_disbursements()?,
        post_blossom_pow_target_spacing: None,
        daa,
        pow_start_height,
    };
    let observer = observer_instance();
    let active_instances = active_instances(config);
    let mut rendered = zebra_config::generate_node_config(
        &template,
        NetworkKind::LocalGenesis,
        &observer,
        &active_instances,
    )?;
    rendered = zebra_config::set_miner_address(&rendered, DEFAULT_OBSERVER_MINER_ADDRESS)?;
    rendered = zebra_config::apply_local_testnet_parameters(&rendered, &local_testnet)?;
    rendered = set_toml_string_in_section(
        &rendered,
        "state",
        "cache_dir",
        &format!("{JOIN_INSTALL_ROOT}/state"),
    )?;
    rendered = set_toml_string_in_section(&rendered, "rpc", "listen_addr", "127.0.0.1:18232")?;
    zebra_config::verify_local_testnet_parameters(&rendered, &local_testnet)
        .context("rendered invalid join zebrad.toml")?;
    Ok(rendered)
}

fn bootstrap_peers(config: &Config) -> Result<Vec<String>> {
    if config.network_kind != NetworkKind::LocalGenesis {
        anyhow::bail!("join bundles are only supported for local-genesis experiments");
    }

    Ok(active_instances(config)
        .iter()
        .map(|inst| format!("{}:{}", inst.public_ip, config.p2p_port()))
        .collect())
}

fn active_instances(config: &Config) -> Vec<Instance> {
    config
        .miners
        .iter()
        .filter(|inst| !inst.public_ip.is_empty() && inst.public_ip != "TBD")
        .cloned()
        .collect()
}

fn observer_instance() -> Instance {
    Instance {
        node_type: NodeType::Miner,
        public_ip: "TBD".to_string(),
        private_ip: "TBD".to_string(),
        provider: Provider::DigitalOcean,
        slug: "observer".to_string(),
        region: "local".to_string(),
        name: "__join_observer__".to_string(),
        tags: vec!["kresko".to_string(), "join-bundle".to_string()],
        tier: "observer".to_string(),
    }
}

fn payload_pow_start_height(local_genesis_dir: &Path) -> Result<Option<u32>> {
    let manifest_path = local_genesis_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: PayloadPremineManifest = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok(manifest.pow_start_height)
}

fn set_toml_string_in_section(
    config: &str,
    section: &str,
    key: &str,
    value: &str,
) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    let root = parsed
        .as_table_mut()
        .context("zebrad.toml root should be a TOML table")?;
    let section_table = root
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .with_context(|| format!("[{section}] should be a TOML table"))?;
    section_table.insert(key.to_string(), toml::Value::String(value.to_string()));
    toml::to_string_pretty(&parsed).context("failed to serialize zebrad.toml")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::{JoinManifest, set_toml_string_in_section};
    use crate::config::{
        Config, DaaConfig, EquihashParameterSet, Instance, LocalGenesisActivationHeights,
        LocalGenesisConfig, MiningMode, NetworkKind, NodeType, OrchardTxblastConfig, Provider,
    };

    fn miner(name: &str, ip: &str) -> Instance {
        Instance {
            node_type: NodeType::Miner,
            public_ip: ip.to_string(),
            private_ip: "10.0.0.1".to_string(),
            provider: Provider::DigitalOcean,
            slug: "s-1vcpu-1gb".to_string(),
            region: "nyc3".to_string(),
            name: name.to_string(),
            tags: vec!["kresko".to_string()],
            tier: "full".to_string(),
        }
    }

    fn test_config() -> Config {
        Config {
            miners: vec![
                miner("miner-0-abc", "1.1.1.1"),
                miner("miner-1-def", "2.2.2.2"),
                miner("miner-2-ghi", "TBD"),
            ],
            chain_id: "nu7-test".to_string(),
            experiment: "nu7".to_string(),
            ssh_pub_key_path: String::new(),
            ssh_key_name: String::new(),
            ssh_key_path: String::new(),
            provider: Provider::DigitalOcean,
            network_kind: NetworkKind::LocalGenesis,
            mining_mode: MiningMode::Pow,
            block_time_secs: Some(25),
            equihash_params: EquihashParameterSet::Regtest,
            daa: DaaConfig::tuned_25s_defaults(),
            orchard_txblast: OrchardTxblastConfig::default(),
            local_genesis: Some(LocalGenesisConfig {
                network_name: "Kresko_nu7".to_string(),
                network_magic: [1, 2, 3, 4],
                target_difficulty_limit: "0x0f".to_string(),
                target_spacing_secs: Some(25),
                disable_pow: false,
                genesis_hash: "00".repeat(32),
                seeded_tip_hash: Some("11".repeat(32)),
                genesis_hex: "abcd".to_string(),
                slow_start_interval: 0,
                pre_blossom_halving_interval: 144,
                activation_heights: LocalGenesisActivationHeights {
                    overwinter: 1,
                    sapling: 1,
                    blossom: 1,
                    heartwood: 1,
                    canopy: 1,
                    nu5: 1,
                    nu6: 1,
                    nu6_1: 1,
                    nu7: Some(1),
                },
                maturity_padding_block_count: 1,
                premine_block_count: 1,
                seeded_block_count: 2,
                bootstrap_treasury_key: None,
                funded_keys: Vec::new(),
            }),
        }
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "kresko-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        path
    }

    #[test]
    fn string_section_replacement_updates_existing_key() {
        let rendered = set_toml_string_in_section(
            "[rpc]\nlisten_addr = \"0.0.0.0:18232\"\n",
            "rpc",
            "listen_addr",
            "127.0.0.1:18232",
        )
        .expect("replacement should succeed");

        assert!(rendered.contains("listen_addr = \"127.0.0.1:18232\""));
        assert!(!rendered.contains("0.0.0.0:18232"));
    }

    #[test]
    fn generated_join_bundle_manifest_and_observer_config_are_data_only() {
        let run_dir = unique_temp_dir("join-run");
        let out_dir = unique_temp_dir("join-out");
        std::fs::create_dir_all(run_dir.join("payload/local_genesis"))
            .expect("should create payload dir");
        let config = test_config();
        std::fs::write(
            run_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).expect("config should serialize"),
        )
        .expect("should write config");
        std::fs::write(run_dir.join("payload/local_genesis/genesis.hex"), "abcd\n")
            .expect("should write genesis");
        std::fs::write(
            run_dir.join("payload/local_genesis/premine_blocks.hex"),
            "dcba\n",
        )
        .expect("should write premine");
        std::fs::write(
            run_dir.join("payload/local_genesis/checkpoints.txt"),
            format!("0 {}\n1 {}", "00".repeat(32), "11".repeat(32)),
        )
        .expect("should write checkpoints");

        super::run(
            run_dir.to_str().expect("temp path is utf8"),
            "valargroup/zebra",
            "nu7-testnet-v0.1.2",
            "valargroup/kresko",
            "v0.1.0",
            out_dir.to_str().expect("temp path is utf8"),
        )
        .expect("join bundle generation should succeed");

        let manifest_path = out_dir.join("join-manifest.json");
        let manifest: JoinManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest should exist"))
                .expect("manifest should parse");
        assert_eq!(manifest.chain_id, "nu7-test");
        assert_eq!(
            manifest.bootstrap_peers,
            vec!["1.1.1.1:18233", "2.2.2.2:18233"]
        );
        assert_eq!(manifest.zebra_release_repo, "valargroup/zebra");
        assert_eq!(manifest.zebra_release_tag, "nu7-testnet-v0.1.2");
        assert_eq!(manifest.kresko_release_repo, "valargroup/kresko");
        assert_eq!(manifest.kresko_release_tag, "v0.1.0");
        assert!(manifest.files.contains_key("local_genesis/genesis.hex"));
        assert!(!manifest.files.contains_key("join-manifest.json"));
        // The bundle is data-only: it must not ship the join script.
        assert!(!manifest.files.contains_key("join-nu7-testnet.sh"));
        assert!(!out_dir.join("join-nu7-testnet.sh").exists());

        let join_config =
            std::fs::read_to_string(out_dir.join("zebrad.join.toml")).expect("config should exist");
        let parsed: toml::Value = toml::from_str(&join_config).expect("join config should parse");
        let peers = parsed
            .get("network")
            .and_then(|network| network.get("initial_testnet_peers"))
            .and_then(toml::Value::as_array)
            .expect("peers should exist")
            .iter()
            .map(|value| value.as_str().expect("peer should be string"))
            .collect::<Vec<_>>();
        assert_eq!(peers, vec!["1.1.1.1:18233", "2.2.2.2:18233"]);
        assert_eq!(
            parsed
                .get("mining")
                .and_then(|mining| mining.get("miner_address"))
                .and_then(toml::Value::as_str),
            Some("t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v"),
        );
        assert_eq!(
            parsed
                .get("state")
                .and_then(|state| state.get("cache_dir"))
                .and_then(toml::Value::as_str),
            Some("/opt/nu7-testnet/state"),
        );
        assert!(!join_config.contains("secret_key_hex"));
        assert!(!out_dir.join("local_genesis/funded_keys.json").exists());

        let _ = std::fs::remove_dir_all(run_dir);
        let _ = std::fs::remove_dir_all(out_dir);
    }
}
