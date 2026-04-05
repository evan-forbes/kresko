use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
    transparent,
};

use crate::bootstrap::{BootstrapBundle, DEFAULT_POW_BOOTSTRAP_ARTIFACT_ID};
use crate::config::{
    Config, LocalGenesisActivationHeights, LocalGenesisBootstrapMode, LocalGenesisConfig,
    LocalGenesisFundedKey, MiningMode, OrchardTxblastConfig,
};
use crate::zebra_config::{self, LocalTestnetParameters};

pub fn run(
    zebrad_binary: &str,
    kresko_binary: Option<&str>,
    build_dir: &str,
    maturity_padding_blocks: u32,
    bootstrap_mode: &str,
    orchard_lanes_per_miner: usize,
    orchard_lane_value_zats: u64,
    orchard_fanout_source_value_zats: u64,
    orchard_fanout_outputs: usize,
    directory: &str,
) -> Result<()> {
    let dir = Path::new(directory);
    let mut config = Config::load(dir)?;

    validate_orchard_txblast_config(
        orchard_lanes_per_miner,
        orchard_lane_value_zats,
        orchard_fanout_source_value_zats,
        orchard_fanout_outputs,
    )?;
    config.orchard_txblast = OrchardTxblastConfig {
        lanes_per_miner: orchard_lanes_per_miner,
        lane_value_zats: orchard_lane_value_zats,
        fanout_source_value_zats: orchard_fanout_source_value_zats,
        fanout_outputs: orchard_fanout_outputs,
    };

    let miner_names: Vec<String> = config
        .miners
        .iter()
        .map(|inst| inst.parsed_hostname())
        .collect();
    if miner_names.is_empty() {
        anyhow::bail!("No miners configured. Run 'kresko add -t miner -c <N>' first.");
    }

    let bootstrap_mode = resolve_bootstrap_mode(bootstrap_mode, config.mining_mode)?;
    let prepared = match bootstrap_mode {
        LocalGenesisBootstrapMode::Generated => {
            prepare_generated_local_genesis(&config, &miner_names, maturity_padding_blocks)?
        }
        LocalGenesisBootstrapMode::Cached => prepare_cached_local_genesis(&config, &miner_names)?,
    };

    config.local_genesis = Some(prepared.local_genesis.clone());
    config.save(dir)?;

    let payload_dir = dir.join("payload");
    if payload_dir.exists() {
        std::fs::remove_dir_all(&payload_dir)?;
    }
    std::fs::create_dir_all(&payload_dir)?;

    let local_genesis_dir = payload_dir.join("local_genesis");
    std::fs::create_dir_all(&local_genesis_dir)?;
    for (file_name, contents) in &prepared.payload_local_genesis_files {
        std::fs::write(local_genesis_dir.join(file_name), contents)?;
    }

    let funded_by_name: HashMap<String, LocalGenesisFundedKey> = prepared
        .runtime_funded_keys
        .iter()
        .cloned()
        .map(|key| (key.name.clone(), key))
        .collect();

    let template_path = dir.join("zebrad.toml");
    let template = if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .with_context(|| format!("failed to read template {}", template_path.display()))?
    } else {
        zebra_config::DEFAULT_ZEBRAD_TOML.to_string()
    };
    zebra_config::ensure_miner_address_is_set(&template).with_context(|| {
        format!(
            "invalid zebra config template at {}",
            template_path.display()
        )
    })?;

    println!("Generating per-node zebrad.toml configs...");
    for inst in &config.miners {
        let node_name = inst.parsed_hostname();
        let funded_key = funded_by_name
            .get(&node_name)
            .with_context(|| format!("missing funded key for node {node_name}"))?;

        let node_dir = payload_dir.join(&node_name);
        std::fs::create_dir_all(&node_dir)?;

        let mut node_config = zebra_config::generate_node_config(&template, inst, &config.miners)?;
        node_config = zebra_config::set_miner_address(&node_config, &funded_key.address);
        node_config =
            zebra_config::apply_local_testnet_parameters(&node_config, &prepared.local_testnet);
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;
        std::fs::write(
            node_dir.join("funded_key.json"),
            serde_json::to_vec_pretty(funded_key)?,
        )?;

        println!(
            "  {} -> {node_name}/zebrad.toml (runtime funded address: {})",
            inst.name, funded_key.address
        );
    }

    let scripts_dir = dir.join("scripts");
    if scripts_dir.exists() {
        for entry in std::fs::read_dir(&scripts_dir)? {
            let entry = entry?;
            let src = entry.path();
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                eprintln!(
                    "Skipping non-file script entry: {}",
                    src.strip_prefix(dir).unwrap_or(&src).display()
                );
                continue;
            }
            let dest = payload_dir.join(entry.file_name());
            std::fs::copy(&src, &dest)
                .with_context(|| format!("failed to copy script {}", src.display()))?;
        }
    }

    let bin_dir = payload_dir.join(build_dir);
    std::fs::create_dir_all(&bin_dir)?;

    let zebrad_path = Path::new(zebrad_binary);
    if !zebrad_path.exists() {
        anyhow::bail!("zebrad binary not found at {}", zebrad_binary);
    }
    std::fs::copy(zebrad_path, bin_dir.join("zebrad"))
        .with_context(|| format!("failed to copy zebrad from {}", zebrad_binary))?;
    println!("Copied zebrad binary from {zebrad_binary}");

    let kresko_binary_path = kresko_binary
        .map(std::path::PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .context("failed to detect the running kresko binary; pass --kresko-binary")
        })?;
    if !kresko_binary_path.exists() {
        anyhow::bail!(
            "kresko binary not found at {}; pass --kresko-binary with a valid path",
            kresko_binary_path.display()
        );
    }
    std::fs::copy(&kresko_binary_path, bin_dir.join("kresko")).with_context(|| {
        format!(
            "failed to copy kresko from {}",
            kresko_binary_path.display()
        )
    })?;
    println!("Copied kresko binary from {}", kresko_binary_path.display());

    let mut vars_content = format!(
        r#"#!/bin/bash
export CHAIN_ID="{}"
export KRESKO_MINING_MODE="{}"
export AWS_ACCESS_KEY_ID="{}"
export AWS_SECRET_ACCESS_KEY="{}"
export AWS_DEFAULT_REGION="{}"
export AWS_S3_BUCKET="{}"
export AWS_S3_ENDPOINT="{}"
export KRESKO_LOCAL_GENESIS_DIR="/root/payload/local_genesis"
"#,
        config.chain_id,
        config.mining_mode,
        std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "kresko-data".into()),
        std::env::var("AWS_S3_ENDPOINT").unwrap_or_default(),
    );
    if prepared.local_genesis.bootstrap_treasury_key.is_some() {
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_TREASURY_KEY_PATH=\"/root/payload/local_genesis/treasury_key.json\"\n",
        );
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_MANIFEST_PATH=\"/root/payload/local_genesis/manifest.json\"\n",
        );
    }
    if let Ok(trace_dir) = std::env::var("ZEBRA_P2P_TRACE_DIR") {
        if !trace_dir.is_empty() {
            vars_content.push_str(&format!("export ZEBRA_P2P_TRACE_DIR=\"{trace_dir}\"\n"));
        }
    }
    if let Ok(trace_file) = std::env::var("ZEBRA_P2P_TRACE_FILE") {
        if !trace_file.is_empty() {
            vars_content.push_str(&format!("export ZEBRA_P2P_TRACE_FILE=\"{trace_file}\"\n"));
        }
    }
    std::fs::write(payload_dir.join("vars.sh"), vars_content)?;

    println!(
        "Local genesis prepared: mode={:?}, network={}, funding_blocks={}, maturity_padding_blocks={}, seeded_blocks={}, runtime_funded_keys={}, orchard_lanes_per_miner={}",
        prepared.local_genesis.bootstrap_mode,
        prepared.local_genesis.network_name,
        prepared.local_genesis.premine_block_count,
        prepared.local_genesis.maturity_padding_block_count,
        prepared.local_genesis.seeded_block_count,
        prepared.local_genesis.funded_keys.len(),
        config.orchard_txblast.lanes_per_miner,
    );
    if let Some(artifact_id) = &prepared.local_genesis.bootstrap_artifact_id {
        println!("Bootstrap artifact: {artifact_id}");
    }
    println!("Genesis payload generated in {}", payload_dir.display());
    Ok(())
}

#[derive(Debug)]
struct PreparedLocalGenesis {
    local_genesis: LocalGenesisConfig,
    local_testnet: LocalTestnetParameters,
    runtime_funded_keys: Vec<LocalGenesisFundedKey>,
    payload_local_genesis_files: Vec<(String, Vec<u8>)>,
}

fn prepare_generated_local_genesis(
    config: &Config,
    miner_names: &[String],
    maturity_padding_blocks: u32,
) -> Result<PreparedLocalGenesis> {
    let mut options = LocalTestnetGenesisOptions::default();
    options.network_name = local_network_name(&config.chain_id);
    options.latest_network_upgrade = NetworkUpgrade::Nu6;
    options.maturity_padding_blocks = maturity_padding_blocks;

    if config.mining_mode == MiningMode::Pow {
        options.disable_pow = false;
    }
    if let Some(secs) = config.block_time_secs {
        options.target_spacing_secs = Some(secs);
    }

    let generated = generate_local_testnet_with_funded_keys(miner_names.to_vec(), options)
        .map_err(|e| anyhow::anyhow!("failed to generate local genesis chain artifact: {e}"))?;

    let network_params = generated
        .network
        .parameters()
        .context("generated local genesis did not produce testnet parameters")?;
    let activation_height = activation_height(&network_params, NetworkUpgrade::Nu6)?;
    let activation_heights = activation_heights(activation_height);

    let genesis_hex = generated
        .genesis_hex()
        .map_err(|e| anyhow::anyhow!("failed to serialize generated genesis block: {e}"))?;
    let runtime_funded_keys: Vec<LocalGenesisFundedKey> = generated
        .funded_keys
        .iter()
        .map(|key| LocalGenesisFundedKey {
            name: key.name.clone(),
            secret_key_hex: key.secret_key_hex.clone(),
            public_key_hex: key.public_key_hex.clone(),
            address: key.address.to_string(),
        })
        .collect();
    let seeded_tip_hash = generated
        .checkpoints
        .last()
        .map(|(_, hash)| hash.to_string());

    let pre_blossom_halving_interval: u32 = network_params
        .pre_blossom_halving_interval()
        .try_into()
        .context("pre_blossom_halving_interval does not fit in u32")?;
    let local_genesis = LocalGenesisConfig {
        bootstrap_mode: LocalGenesisBootstrapMode::Generated,
        bootstrap_artifact_id: None,
        network_name: network_params.network_name().to_string(),
        network_magic: network_params.network_magic().0,
        target_difficulty_limit: network_params.target_difficulty_limit().to_string(),
        disable_pow: network_params.disable_pow(),
        genesis_hash: network_params.genesis_hash().to_string(),
        seeded_tip_hash,
        genesis_hex: genesis_hex.clone(),
        slow_start_interval: network_params.slow_start_interval().0,
        pre_blossom_halving_interval,
        activation_heights,
        maturity_padding_block_count: maturity_padding_blocks,
        premine_block_count: runtime_funded_keys.len() as u32,
        seeded_block_count: generated.blocks.len().saturating_sub(1) as u32,
        bootstrap_treasury_key: None,
        funded_keys: runtime_funded_keys.clone(),
    };

    let mut seeded_blocks_hex = String::new();
    for block in generated.blocks.iter().skip(1) {
        let mut bytes = Vec::new();
        block
            .zcash_serialize(&mut bytes)
            .context("failed to serialize seeded block")?;
        seeded_blocks_hex.push_str(&to_hex(&bytes));
        seeded_blocks_hex.push('\n');
    }

    let checkpoints_content = generated
        .checkpoints
        .iter()
        .map(|(height, hash)| format!("{} {}", height.0, hash))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(PreparedLocalGenesis {
        local_testnet: LocalTestnetParameters {
            network_name: local_genesis.network_name.clone(),
            network_magic: local_genesis.network_magic,
            target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
            disable_pow: local_genesis.disable_pow,
            genesis_hash: local_genesis.genesis_hash.clone(),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: local_genesis.slow_start_interval,
            pre_blossom_halving_interval: local_genesis.pre_blossom_halving_interval,
            activation_height: local_genesis.activation_heights.overwinter,
            target_spacing_secs: config.block_time_secs,
        },
        runtime_funded_keys: runtime_funded_keys.clone(),
        payload_local_genesis_files: vec![
            ("genesis.hex".to_string(), genesis_hex.into_bytes()),
            (
                "premine_blocks.hex".to_string(),
                seeded_blocks_hex.into_bytes(),
            ),
            (
                "checkpoints.txt".to_string(),
                checkpoints_content.into_bytes(),
            ),
            (
                "funded_keys.json".to_string(),
                serde_json::to_vec_pretty(&runtime_funded_keys)?,
            ),
        ],
        local_genesis,
    })
}

fn prepare_cached_local_genesis(
    config: &Config,
    miner_names: &[String],
) -> Result<PreparedLocalGenesis> {
    let bundle = BootstrapBundle::load(DEFAULT_POW_BOOTSTRAP_ARTIFACT_ID)?;
    let manifest = bundle.manifest();
    let runtime_funded_keys = generate_runtime_funded_keys(miner_names)?;
    let network_magic = rand::random::<[u8; 4]>();
    let network_name = local_network_name(&config.chain_id);
    let activation_heights = activation_heights(manifest.activation_height);
    let genesis_hex = bundle.read_text_file("genesis.hex")?;

    let local_genesis = LocalGenesisConfig {
        bootstrap_mode: LocalGenesisBootstrapMode::Cached,
        bootstrap_artifact_id: Some(manifest.artifact_id.clone()),
        network_name: network_name.clone(),
        network_magic,
        target_difficulty_limit: manifest.target_difficulty_limit.clone(),
        disable_pow: manifest.disable_pow,
        genesis_hash: manifest.genesis_hash.clone(),
        seeded_tip_hash: Some(manifest.seeded_tip_hash.clone()),
        genesis_hex: genesis_hex.clone(),
        slow_start_interval: manifest.slow_start_interval,
        pre_blossom_halving_interval: manifest.pre_blossom_halving_interval,
        activation_heights,
        maturity_padding_block_count: manifest.maturity_padding_block_count,
        premine_block_count: manifest.premine_block_count,
        seeded_block_count: manifest.seeded_block_count,
        bootstrap_treasury_key: Some(bundle.treasury_key().clone()),
        funded_keys: runtime_funded_keys.clone(),
    };

    let mut payload_local_genesis_files = vec![(
        "funded_keys.json".to_string(),
        serde_json::to_vec_pretty(&runtime_funded_keys)?,
    )];
    bundle.copy_payload_files_to_vec(&mut payload_local_genesis_files)?;

    Ok(PreparedLocalGenesis {
        local_testnet: LocalTestnetParameters {
            network_name,
            network_magic,
            target_difficulty_limit: manifest.target_difficulty_limit.clone(),
            disable_pow: manifest.disable_pow,
            genesis_hash: manifest.genesis_hash.clone(),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: manifest.slow_start_interval,
            pre_blossom_halving_interval: manifest.pre_blossom_halving_interval,
            activation_height: manifest.activation_height,
            target_spacing_secs: config.block_time_secs,
        },
        runtime_funded_keys,
        payload_local_genesis_files,
        local_genesis,
    })
}

fn resolve_bootstrap_mode(
    mode: &str,
    mining_mode: MiningMode,
) -> Result<LocalGenesisBootstrapMode> {
    let resolved = match mode.to_ascii_lowercase().as_str() {
        "auto" => {
            if mining_mode == MiningMode::Pow {
                LocalGenesisBootstrapMode::Cached
            } else {
                LocalGenesisBootstrapMode::Generated
            }
        }
        "generated" => LocalGenesisBootstrapMode::Generated,
        "cached" => LocalGenesisBootstrapMode::Cached,
        other => anyhow::bail!("unknown bootstrap mode: {other}. Use auto, generated, or cached."),
    };

    if resolved == LocalGenesisBootstrapMode::Cached && mining_mode != MiningMode::Pow {
        anyhow::bail!("cached bootstrap mode currently only supports --mining-mode pow");
    }

    Ok(resolved)
}

fn activation_heights(activation_height: u32) -> LocalGenesisActivationHeights {
    LocalGenesisActivationHeights {
        overwinter: activation_height,
        sapling: activation_height,
        blossom: activation_height,
        heartwood: activation_height,
        canopy: activation_height,
        nu5: activation_height,
        nu6: activation_height,
        nu6_1: activation_height,
    }
}

fn generate_runtime_funded_keys(miner_names: &[String]) -> Result<Vec<LocalGenesisFundedKey>> {
    miner_names
        .iter()
        .cloned()
        .map(generate_transparent_key)
        .collect()
}

fn generate_transparent_key(name: String) -> Result<LocalGenesisFundedKey> {
    let secp = secp256k1::Secp256k1::new();
    let secret_key = loop {
        let secret_bytes = rand::random::<[u8; 32]>();
        if let Ok(secret_key) = secp256k1::SecretKey::from_slice(&secret_bytes) {
            break secret_key;
        }
    };
    let public_key = secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
    let pub_key_bytes = public_key.serialize();
    let pub_key_hash = hash160(&pub_key_bytes);
    let address = transparent::Address::from_pub_key_hash(
        zebra_chain::parameters::NetworkKind::Testnet,
        pub_key_hash,
    );

    Ok(LocalGenesisFundedKey {
        name,
        secret_key_hex: hex::encode(secret_key.secret_bytes()),
        public_key_hex: hex::encode(pub_key_bytes),
        address: address.to_string(),
    })
}

fn validate_orchard_txblast_config(
    orchard_lanes_per_miner: usize,
    orchard_lane_value_zats: u64,
    orchard_fanout_source_value_zats: u64,
    orchard_fanout_outputs: usize,
) -> Result<()> {
    if orchard_lanes_per_miner == 0 {
        anyhow::bail!("--orchard-lanes-per-miner must be greater than 0");
    }
    if orchard_lane_value_zats == 0 {
        anyhow::bail!("--orchard-lane-value-zats must be greater than 0");
    }
    if orchard_fanout_outputs < 2 {
        anyhow::bail!("--orchard-fanout-outputs must be at least 2");
    }

    let min_source_value = 10_000u64
        .saturating_add((orchard_fanout_outputs as u64).saturating_mul(orchard_lane_value_zats));
    if orchard_fanout_source_value_zats < min_source_value {
        anyhow::bail!(
            "--orchard-fanout-source-value-zats must be at least {} for {} outputs of {} zats plus fees",
            min_source_value,
            orchard_fanout_outputs,
            orchard_lane_value_zats,
        );
    }

    Ok(())
}

fn activation_height(
    network_params: &zebra_chain::parameters::testnet::Parameters,
    upgrade: NetworkUpgrade,
) -> Result<u32> {
    network_params
        .activation_heights()
        .iter()
        .find_map(|(height, configured_upgrade)| {
            if *configured_upgrade == upgrade {
                Some(height.0)
            } else {
                None
            }
        })
        .with_context(|| format!("missing activation height for {upgrade:?}"))
}

fn local_network_name(chain_id: &str) -> String {
    let cleaned: String = chain_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let mut name = if cleaned.is_empty() {
        "KreskoLocalGenesis".to_string()
    } else {
        format!("Kresko_{cleaned}")
    };

    if name.len() > 30 {
        name.truncate(30);
    }

    if matches!(
        name.as_str(),
        "Mainnet" | "Testnet" | "Regtest" | "MainnetKind" | "TestnetKind" | "RegtestKind"
    ) {
        return "KreskoLocalGenesis".to_string();
    }

    name
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hash160(data: &[u8]) -> [u8; 20] {
    use ripemd::Digest as _;

    let sha_hash = sha2::Sha256::digest(data);
    let ripemd_hash = ripemd::Ripemd160::digest(sha_hash);
    let mut result = [0u8; 20];
    result.copy_from_slice(&ripemd_hash);
    result
}
