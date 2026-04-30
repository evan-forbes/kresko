use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
    work::difficulty::U256,
};

use crate::config::{
    Config, DaaConfig, LocalGenesisActivationHeights, LocalGenesisConfig, LocalGenesisFundedKey,
    MiningMode, OrchardTxblastConfig,
};
use crate::pow_tuning::{self, PowCalibration, PowTuningInputs};
use crate::premine::{self, CalibrationSignature};
use crate::zebra_config::{self, LocalTestnetParameters};

const DEFAULT_ZEBRA_TRACE_DIR: &str = "/root/.cache/zebra/traces";
const DEFAULT_TARGET_SPACING_SECS: u32 = 25;

/// PoW calibration inputs sourced from CLI flags and forwarded into the
/// genesis pipeline.
#[derive(Debug, Clone, Default)]
pub struct PowCalibrationCli {
    /// Fractional adjustment to the natural target. `+0.10` = ~10% looser
    /// (faster initial blocks), `-0.10` = ~10% tighter. Usually 0.
    pub adjust_fraction: f64,
    /// Optional divisor applied to the local benchmark before calibration.
    /// Higher values produce a looser initial pow_limit.
    pub fleet_discount: Option<f64>,
}

pub fn run(
    zebrad_binary: &str,
    kresko_binary: Option<&str>,
    build_dir: &str,
    maturity_padding_blocks: u32,
    orchard_lanes_per_miner: usize,
    orchard_lane_value_zats: u64,
    orchard_fanout_source_value_zats: u64,
    orchard_fanout_outputs: usize,
    scripts_dir: &str,
    pow_calibration: PowCalibrationCli,
    directory: &str,
) -> Result<()> {
    let dir = Path::new(directory);
    let mut config = Config::load(dir)?;
    config.require_local_genesis("genesis")?;

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
    let toml_network = zebra_config::testnet_toml_parameters(&template)
        .with_context(|| format!("invalid testnet parameters in {}", template_path.display()))?;
    let target_spacing_secs = toml_network
        .post_blossom_pow_target_spacing
        .or(config.block_time_secs)
        .unwrap_or(DEFAULT_TARGET_SPACING_SECS);
    let daa = toml_network
        .daa
        .with_missing_from(config.daa)
        .with_missing_from(DaaConfig::tuned_25s_defaults());

    let prepared = match config.mining_mode {
        MiningMode::Pow => prepare_premine_local_genesis(
            &config,
            &miner_names,
            &pow_calibration,
            target_spacing_secs,
            daa,
        )?,
        _ => prepare_generated_local_genesis(
            &config,
            &miner_names,
            maturity_padding_blocks,
            target_spacing_secs,
            daa,
        )?,
    };
    validate_target_spacing_consistency(&prepared)?;

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

    println!("Generating per-node zebrad.toml configs...");
    for inst in &config.miners {
        let node_name = inst.parsed_hostname();
        let funded_key = funded_by_name
            .get(&node_name)
            .with_context(|| format!("missing funded key for node {node_name}"))?;

        let node_dir = payload_dir.join(&node_name);
        std::fs::create_dir_all(&node_dir)?;

        let mut node_config = zebra_config::generate_node_config(
            &template,
            config.network_kind,
            inst,
            &config.miners,
        )?;
        node_config = zebra_config::set_miner_address(&node_config, &funded_key.address);
        node_config =
            zebra_config::apply_local_testnet_parameters(&node_config, &prepared.local_testnet);
        zebra_config::verify_local_testnet_parameters(&node_config, &prepared.local_testnet)
            .with_context(|| format!("rendered invalid zebrad.toml for node {node_name}"))?;
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;
        std::fs::write(
            node_dir.join("funded_key.json"),
            serde_json::to_vec_pretty(funded_key)?,
        )?;
        std::fs::write(node_dir.join("tier"), format!("{}\n", inst.tier))?;

        println!(
            "  {} -> {node_name}/zebrad.toml (runtime funded address: {})",
            inst.name, funded_key.address
        );
    }

    let scripts_src = if Path::new(scripts_dir).is_absolute() {
        std::path::PathBuf::from(scripts_dir)
    } else {
        dir.join(scripts_dir)
    };
    if scripts_src.exists() {
        let payload_scripts_dir = payload_dir.join("scripts");
        copy_dir_recursive(&scripts_src, &payload_scripts_dir).with_context(|| {
            format!(
                "failed to copy scripts tree from {} into {}",
                scripts_src.display(),
                payload_scripts_dir.display()
            )
        })?;
        // Backwards compat: also flat-copy root-level files into payload/ so
        // the existing `payload/node_init.sh` lookup keeps working.
        for entry in std::fs::read_dir(&scripts_src)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if entry.file_name() == "vars.sh" {
                    continue;
                }
                std::fs::copy(entry.path(), payload_dir.join(entry.file_name()))?;
            }
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
    if let Some(target_spacing_secs) = prepared.local_testnet.post_blossom_pow_target_spacing {
        vars_content.push_str(&format!(
            "export KRESKO_TARGET_BLOCK_TIME_SECS=\"{}\"\n",
            target_spacing_secs
        ));
    }
    if prepared.local_genesis.bootstrap_treasury_key.is_some() {
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_TREASURY_KEY_PATH=\"/root/payload/local_genesis/treasury_key.json\"\n",
        );
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_MANIFEST_PATH=\"/root/payload/local_genesis/manifest.json\"\n",
        );
    }
    append_zebra_trace_exports(&mut vars_content);
    std::fs::write(payload_dir.join("vars.sh"), vars_content)?;

    println!(
        "Local genesis prepared: mining_mode={}, network={}, funding_blocks={}, maturity_padding_blocks={}, seeded_blocks={}, runtime_funded_keys={}, orchard_lanes_per_miner={}",
        config.mining_mode,
        prepared.local_genesis.network_name,
        prepared.local_genesis.premine_block_count,
        prepared.local_genesis.maturity_padding_block_count,
        prepared.local_genesis.seeded_block_count,
        prepared.local_genesis.funded_keys.len(),
        config.orchard_txblast.lanes_per_miner,
    );
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

fn validate_target_spacing_consistency(prepared: &PreparedLocalGenesis) -> Result<()> {
    let rendered_spacing = prepared
        .local_testnet
        .post_blossom_pow_target_spacing
        .context("generated local testnet is missing post_blossom_pow_target_spacing")?;
    let metadata_spacing = prepared
        .local_genesis
        .target_spacing_secs
        .context("generated local_genesis metadata is missing target_spacing_secs")?;

    if metadata_spacing != rendered_spacing {
        anyhow::bail!(
            "target spacing mismatch before payload render: local_genesis={}s, zebrad.toml={}s",
            metadata_spacing,
            rendered_spacing,
        );
    }

    Ok(())
}

fn prepare_generated_local_genesis(
    config: &Config,
    miner_names: &[String],
    maturity_padding_blocks: u32,
    target_spacing_secs: u32,
    daa: DaaConfig,
) -> Result<PreparedLocalGenesis> {
    // Non-PoW path: zebra-chain disables Equihash solving so we can seed blocks
    // cheaply, but contextual difficulty still needs a target limit that is safe
    // for the configured DAA averaging window.
    let options = LocalTestnetGenesisOptions {
        network_name: local_network_name(&config.chain_id),
        latest_network_upgrade: NetworkUpgrade::Nu6,
        target_spacing_secs,
        seeded_tip_time: None,
        maturity_padding_blocks,
        target_difficulty_limit: safe_target_difficulty_limit_for_daa(daa),
        ..LocalTestnetGenesisOptions::default()
    };

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
        network_name: network_params.network_name().to_string(),
        network_magic: network_params.network_magic().0,
        target_difficulty_limit: network_params.target_difficulty_limit().to_string(),
        target_spacing_secs: Some(network_params.post_blossom_pow_target_spacing()),
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
            post_blossom_pow_target_spacing: Some(network_params.post_blossom_pow_target_spacing()),
            equihash_params: config.equihash_params.into(),
            daa,
            pow_start_height: None,
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

fn safe_target_difficulty_limit_for_daa(daa: DaaConfig) -> [u8; 32] {
    let averaging_window = daa.pow_averaging_window.unwrap_or(17).max(1);
    let divisor = U256::from(averaging_window.saturating_add(1));

    (U256::MAX / divisor).to_big_endian()
}

fn prepare_premine_local_genesis(
    config: &Config,
    miner_names: &[String],
    pow_calibration: &PowCalibrationCli,
    block_time_secs: u32,
    daa: DaaConfig,
) -> Result<PreparedLocalGenesis> {
    // Calibrate the target from measured local Equihash sol/s, then generate
    // a fresh premine. Premine generation itself is cheap (disable_pow = true,
    // no Equihash solves); the calibration benchmark is the only slow step
    // and its output determines the live network's pow_limit.
    let calibration = run_pow_calibration(config, pow_calibration, block_time_secs)?;
    let signature = CalibrationSignature::new(
        calibration.target_difficulty_limit_hex.clone(),
        block_time_secs,
    )?;

    let started = std::time::Instant::now();
    let bundle = premine::generate(&signature)?;
    report_calibration(&calibration, &signature, started.elapsed());

    // The premine bundle's funded_keys[0] is the bootstrap treasury; the
    // remaining keys each received exactly one premine coinbase in blocks
    // 1..=premine_block_count and are already mature at the seeded tip.
    // Assigning them directly to each miner gives each node a premine-backed
    // transparent bootstrap key. Shielded txblast can still fan funds out via
    // a runtime funding transaction when it needs confirmed non-coinbase
    // transparent inputs across the fleet.
    let available_miner_keys = bundle.funded_keys().len().saturating_sub(1);
    if miner_names.len() > available_miner_keys {
        anyhow::bail!(
            "experiment has {} miners but the premine only provides {} \
             non-treasury funded keys (see src/premine.rs::FUNDED_KEY_COUNT).",
            miner_names.len(),
            available_miner_keys,
        );
    }

    let runtime_funded_keys: Vec<LocalGenesisFundedKey> = bundle
        .funded_keys()
        .iter()
        .skip(1)
        .zip(miner_names.iter())
        .map(|(premine_key, miner_name)| LocalGenesisFundedKey {
            name: miner_name.clone(),
            secret_key_hex: premine_key.secret_key_hex.clone(),
            public_key_hex: premine_key.public_key_hex.clone(),
            address: premine_key.address.clone(),
        })
        .collect();
    let network_magic = rand::random::<[u8; 4]>();
    let network_name = local_network_name(&config.chain_id);
    let manifest = bundle.manifest();
    let activation_heights = activation_heights(manifest.activation_height);
    let genesis_hex = bundle.genesis_hex().to_string();

    let local_genesis = LocalGenesisConfig {
        network_name: network_name.clone(),
        network_magic,
        target_difficulty_limit: manifest.target_difficulty_limit.clone(),
        target_spacing_secs: Some(manifest.target_spacing_secs),
        disable_pow: manifest.disable_pow,
        genesis_hash: manifest.genesis_hash.clone(),
        seeded_tip_hash: Some(manifest.seeded_tip_hash.clone()),
        genesis_hex,
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
        "runtime_funded_keys.json".to_string(),
        serde_json::to_vec_pretty(&runtime_funded_keys)?,
    )];
    payload_local_genesis_files.extend(bundle.payload_files()?);

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
            post_blossom_pow_target_spacing: Some(manifest.target_spacing_secs),
            equihash_params: config.equihash_params.into(),
            daa,
            pow_start_height: manifest.pow_start_height,
        },
        runtime_funded_keys,
        payload_local_genesis_files,
        local_genesis,
    })
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
    if orchard_fanout_outputs == 0 {
        anyhow::bail!("--orchard-fanout-outputs must be greater than 0");
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

pub(crate) fn append_zebra_trace_exports(vars_content: &mut String) {
    for (name, value) in collect_zebra_trace_env_vars(std::env::vars()) {
        vars_content.push_str(&format!("export {name}={}\n", shell_single_quote(&value)));
    }
}

fn collect_zebra_trace_env_vars<I>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut trace_vars = BTreeMap::new();
    for (name, value) in vars {
        if is_zebra_trace_var(&name) && !value.is_empty() {
            trace_vars.insert(name, value);
        }
    }

    trace_vars
        .entry("ZEBRA_P2P_TRACE_DIR".to_owned())
        .or_insert_with(|| DEFAULT_ZEBRA_TRACE_DIR.to_owned());
    trace_vars
        .entry("ZEBRA_TRACE_DIR".to_owned())
        .or_insert_with(|| DEFAULT_ZEBRA_TRACE_DIR.to_owned());
    trace_vars
        .entry("ZEBRA_FORK_TRACE_ENABLE".to_owned())
        .or_insert_with(|| "1".to_owned());

    trace_vars
}

fn is_zebra_trace_var(name: &str) -> bool {
    name.starts_with("ZEBRA_") && (name.contains("TRACE") || name.contains("TRACING"))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
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

fn run_pow_calibration(
    config: &Config,
    cli: &PowCalibrationCli,
    block_time_secs: u32,
) -> Result<PowCalibration> {
    println!(
        "Benchmarking local Equihash sol/s (params={}, min {:.1}s)...",
        config.equihash_params,
        pow_tuning::DEFAULT_BENCH_MIN_SECONDS
    );
    let measured = pow_tuning::measure_local_sol_per_sec(
        config.equihash_params,
        pow_tuning::DEFAULT_BENCH_MIN_SECONDS,
        cli.fleet_discount,
    )
    .context("local sol/s benchmark failed")?;
    println!(
        "  params={} local={:.3} candidates/s ({} candidates in {:.1}s) → assumed fleet={:.3} candidates/s (÷{:.1})",
        measured.equihash_params,
        measured.local_sol_per_sec,
        measured.total_solves,
        measured.elapsed_secs,
        measured.assumed_fleet_sol_per_sec,
        measured.fleet_discount,
    );

    let inputs = PowTuningInputs {
        num_miners: config.miners.len(),
        target_spacing_secs: block_time_secs,
        target_adjust_fraction: cli.adjust_fraction,
        sol_per_sec_override: Some(measured.assumed_fleet_sol_per_sec),
        ..Default::default()
    };

    pow_tuning::calibrate(&inputs).context("PoW calibration failed")
}

fn report_calibration(
    calibration: &PowCalibration,
    signature: &CalibrationSignature,
    generation_elapsed: std::time::Duration,
) {
    println!(
        "calibrated pow_limit={} miners={} sol/s={:.3} ({}) spacing={}s adjust={:+.3} \
         natural_bits={}; premine generated in {:.3}s (target={})",
        calibration.target_difficulty_limit_hex,
        calibration.num_miners,
        calibration.sol_per_sec_per_thread,
        calibration.sol_rate_source,
        calibration.target_spacing_secs,
        calibration.target_adjust_fraction,
        calibration.natural_target_bits,
        generation_elapsed.as_secs_f64(),
        signature.target_hex,
    );
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ZEBRA_TRACE_DIR, collect_zebra_trace_env_vars, is_zebra_trace_var,
        shell_single_quote,
    };

    #[test]
    fn zebra_trace_var_matcher_accepts_trace_and_tracing_names() {
        assert!(is_zebra_trace_var("ZEBRA_P2P_TRACE_DIR"));
        assert!(is_zebra_trace_var("ZEBRA_RUNTIME_TRACING_FILE"));
        assert!(!is_zebra_trace_var("ZEBRA_NETWORK"));
        assert!(!is_zebra_trace_var("KRESKO_TRACE_DIR"));
    }

    #[test]
    fn collect_zebra_trace_env_vars_preserves_new_trace_vars_and_defaults() {
        let vars = vec![
            ("ZEBRA_P2P_TRACE_DIR".to_owned(), "/tmp/p2p".to_owned()),
            (
                "ZEBRA_RUNTIME_TRACE_DIR".to_owned(),
                "/tmp/runtime".to_owned(),
            ),
            (
                "ZEBRA_RUNTIME_TRACING_FILE".to_owned(),
                "/tmp/runtime/events.log".to_owned(),
            ),
            ("UNRELATED".to_owned(), "ignored".to_owned()),
        ];

        let trace_vars = collect_zebra_trace_env_vars(vars);

        assert_eq!(
            trace_vars.get("ZEBRA_P2P_TRACE_DIR").map(String::as_str),
            Some("/tmp/p2p")
        );
        assert_eq!(
            trace_vars
                .get("ZEBRA_RUNTIME_TRACE_DIR")
                .map(String::as_str),
            Some("/tmp/runtime")
        );
        assert_eq!(
            trace_vars
                .get("ZEBRA_RUNTIME_TRACING_FILE")
                .map(String::as_str),
            Some("/tmp/runtime/events.log")
        );
        assert_eq!(
            trace_vars.get("ZEBRA_TRACE_DIR").map(String::as_str),
            Some(DEFAULT_ZEBRA_TRACE_DIR)
        );
        assert_eq!(
            trace_vars
                .get("ZEBRA_FORK_TRACE_ENABLE")
                .map(String::as_str),
            Some("1")
        );
        assert!(!trace_vars.contains_key("UNRELATED"));
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\"'\"'b'");
    }
}
