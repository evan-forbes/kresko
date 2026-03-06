use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
};

use crate::config::{
    Config, LocalGenesisActivationHeights, LocalGenesisConfig, LocalGenesisFundedKey,
};
use crate::zebra_config::{self, LocalTestnetParameters};

pub fn run(
    zebrad_binary: &str,
    txblast_binary: Option<&str>,
    build_dir: &str,
    directory: &str,
) -> Result<()> {
    let dir = Path::new(directory);
    let mut config = Config::load(dir)?;

    let miner_names: Vec<String> = config
        .miners
        .iter()
        .map(|inst| inst.parsed_hostname())
        .collect();
    if miner_names.is_empty() {
        anyhow::bail!("No miners configured. Run 'kresko add -t miner -c <N>' first.");
    }

    let mut options = LocalTestnetGenesisOptions::default();
    options.network_name = local_network_name(&config.chain_id);
    options.latest_network_upgrade = NetworkUpgrade::Nu6_1;

    let generated = generate_local_testnet_with_funded_keys(miner_names.clone(), options)
        .context("failed to generate local genesis chain artifact")?;

    let network_params = generated
        .network
        .parameters()
        .context("generated local genesis did not produce testnet parameters")?;
    let activation_height = activation_height(&network_params, NetworkUpgrade::Nu6_1)?;
    let activation_heights = LocalGenesisActivationHeights {
        overwinter: activation_height,
        sapling: activation_height,
        blossom: activation_height,
        heartwood: activation_height,
        canopy: activation_height,
        nu5: activation_height,
        nu6: activation_height,
        nu6_1: activation_height,
    };

    let genesis_hex = generated
        .genesis_hex()
        .context("failed to serialize generated genesis block")?;
    let funded_keys: Vec<LocalGenesisFundedKey> = generated
        .funded_keys
        .iter()
        .map(|key| LocalGenesisFundedKey {
            name: key.name.clone(),
            secret_key_hex: key.secret_key_hex.clone(),
            public_key_hex: key.public_key_hex.clone(),
            address: key.address.to_string(),
        })
        .collect();

    let pre_blossom_halving_interval: u32 = network_params
        .pre_blossom_halving_interval()
        .try_into()
        .context("pre_blossom_halving_interval does not fit in u32")?;
    let local_genesis = LocalGenesisConfig {
        network_name: network_params.network_name().to_string(),
        network_magic: network_params.network_magic().0,
        target_difficulty_limit: network_params.target_difficulty_limit().to_string(),
        disable_pow: network_params.disable_pow(),
        genesis_hash: network_params.genesis_hash().to_string(),
        genesis_hex: genesis_hex.clone(),
        slow_start_interval: network_params.slow_start_interval().0,
        pre_blossom_halving_interval,
        activation_heights,
        premine_block_count: generated.blocks.len().saturating_sub(1) as u32,
        funded_keys: funded_keys.clone(),
    };
    config.local_genesis = Some(local_genesis.clone());
    config.save(dir)?;

    let payload_dir = dir.join("payload");

    // Clean old payload
    if payload_dir.exists() {
        std::fs::remove_dir_all(&payload_dir)?;
    }
    std::fs::create_dir_all(&payload_dir)?;

    // Write local genesis artifact files
    let local_genesis_dir = payload_dir.join("local_genesis");
    std::fs::create_dir_all(&local_genesis_dir)?;
    std::fs::write(local_genesis_dir.join("genesis.hex"), &genesis_hex)?;

    let mut premine_blocks_hex = String::new();
    for block in generated.blocks.iter().skip(1) {
        let mut bytes = Vec::new();
        block
            .zcash_serialize(&mut bytes)
            .context("failed to serialize premine block")?;
        premine_blocks_hex.push_str(&to_hex(&bytes));
        premine_blocks_hex.push('\n');
    }
    std::fs::write(
        local_genesis_dir.join("premine_blocks.hex"),
        premine_blocks_hex,
    )?;

    let checkpoints_content = generated
        .checkpoints
        .iter()
        .map(|(height, hash)| format!("{} {}", height.0, hash))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        local_genesis_dir.join("checkpoints.txt"),
        checkpoints_content,
    )?;
    std::fs::write(
        local_genesis_dir.join("funded_keys.json"),
        serde_json::to_vec_pretty(&funded_keys)?,
    )?;

    let funded_by_name: HashMap<String, LocalGenesisFundedKey> = funded_keys
        .into_iter()
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

    let local_testnet_params = LocalTestnetParameters {
        network_name: local_genesis.network_name.clone(),
        network_magic: local_genesis.network_magic,
        target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
        disable_pow: local_genesis.disable_pow,
        genesis_hash: local_genesis.genesis_hash.clone(),
        checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
        slow_start_interval: local_genesis.slow_start_interval,
        pre_blossom_halving_interval: local_genesis.pre_blossom_halving_interval,
        activation_height: local_genesis.activation_heights.overwinter,
    };

    // Generate per-node configs
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
            zebra_config::apply_local_testnet_parameters(&node_config, &local_testnet_params);
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;
        std::fs::write(
            node_dir.join("funded_key.json"),
            serde_json::to_vec_pretty(funded_key)?,
        )?;

        println!(
            "  {} -> {node_name}/zebrad.toml (premine address: {})",
            inst.name, funded_key.address
        );
    }

    // Copy scripts
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

    // Copy binaries
    let bin_dir = payload_dir.join(build_dir);
    std::fs::create_dir_all(&bin_dir)?;

    let zebrad_path = Path::new(zebrad_binary);
    if !zebrad_path.exists() {
        anyhow::bail!("zebrad binary not found at {}", zebrad_binary);
    }
    std::fs::copy(zebrad_path, bin_dir.join("zebrad"))
        .with_context(|| format!("failed to copy zebrad from {}", zebrad_binary))?;
    println!("Copied zebrad binary from {zebrad_binary}");

    if let Some(txblast_path) = txblast_binary {
        let src = Path::new(txblast_path);
        if src.exists() {
            std::fs::copy(src, bin_dir.join("kresko"))?;
            println!("Copied txblast binary from {txblast_path}");
        }
    }

    // Write vars.sh with credentials
    let vars_content = format!(
        r#"#!/bin/bash
export CHAIN_ID="{}"
export AWS_ACCESS_KEY_ID="{}"
export AWS_SECRET_ACCESS_KEY="{}"
export AWS_DEFAULT_REGION="{}"
export AWS_S3_BUCKET="{}"
export AWS_S3_ENDPOINT="{}"
export KRESKO_LOCAL_GENESIS_DIR="/root/payload/local_genesis"
"#,
        config.chain_id,
        std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "kresko-data".into()),
        std::env::var("AWS_S3_ENDPOINT").unwrap_or_default(),
    );
    std::fs::write(payload_dir.join("vars.sh"), vars_content)?;

    println!(
        "Local genesis generated: network={}, premine_blocks={}, funded_keys={}",
        local_genesis.network_name,
        local_genesis.premine_block_count,
        local_genesis.funded_keys.len()
    );
    println!("Genesis payload generated in {}", payload_dir.display());
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
