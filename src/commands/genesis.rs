use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
};

use crate::config::{
    Config, LocalGenesisActivationHeights, LocalGenesisConfig, LocalGenesisFundedKey, MiningMode,
    OrchardTxblastConfig,
};
use crate::pow_tuning::{self, PowCalibration, PowTuningInputs};
use crate::premine::{self, CalibrationSignature, ResolveOutcome};
use crate::zebra_config::{self, LocalTestnetParameters};

/// PoW calibration inputs sourced from CLI flags and forwarded into the
/// genesis pipeline.
#[derive(Debug, Clone, Default)]
pub struct PowCalibrationCli {
    /// Fractional adjustment to the natural target. `+0.10` = ~10% looser
    /// (faster initial blocks), `-0.10` = ~10% tighter. Usually 0.
    pub adjust_fraction: f64,
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
    premine_cache_key: &str,
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

    let prepared = match config.mining_mode {
        MiningMode::Pow => prepare_premine_local_genesis(
            &config,
            &miner_names,
            &pow_calibration,
            premine_cache_key,
        )?,
        _ => prepare_generated_local_genesis(&config, &miner_names, maturity_padding_blocks)?,
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
    if prepared.local_genesis.bootstrap_treasury_key.is_some() {
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_TREASURY_KEY_PATH=\"/root/payload/local_genesis/treasury_key.json\"\n",
        );
        vars_content.push_str(
            "export KRESKO_BOOTSTRAP_MANIFEST_PATH=\"/root/payload/local_genesis/manifest.json\"\n",
        );
    }
    let trace_dir = std::env::var("ZEBRA_P2P_TRACE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/root/.cache/zebra/traces".to_owned());
    vars_content.push_str(&format!("export ZEBRA_P2P_TRACE_DIR=\"{trace_dir}\"\n"));
    if let Ok(trace_file) = std::env::var("ZEBRA_P2P_TRACE_FILE") {
        if !trace_file.is_empty() {
            vars_content.push_str(&format!("export ZEBRA_P2P_TRACE_FILE=\"{trace_file}\"\n"));
        }
    }
    let fork_trace_dir = std::env::var("ZEBRA_TRACE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/root/.cache/zebra/traces".to_owned());
    let fork_trace_enable = std::env::var("ZEBRA_FORK_TRACE_ENABLE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "1".to_owned());
    vars_content.push_str(&format!(
        "export ZEBRA_FORK_TRACE_ENABLE=\"{fork_trace_enable}\"\n"
    ));
    vars_content.push_str(&format!("export ZEBRA_TRACE_DIR=\"{fork_trace_dir}\"\n"));
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
    if let Some(cache_key) = &prepared.local_genesis.premine_cache_key {
        println!("Premine cache key: {cache_key}");
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
    // Non-PoW path: zebra-chain's default disables PoW entirely, which skips
    // Equihash solves and lets us seed any number of blocks cheaply. The
    // regtest-easy 0x0f target the default options carry is fine here — PoW
    // is off, so the target isn't enforced on incoming blocks.
    let options = LocalTestnetGenesisOptions {
        network_name: local_network_name(&config.chain_id),
        latest_network_upgrade: NetworkUpgrade::Nu6,
        target_spacing_secs: config.block_time_secs.unwrap_or(1),
        seeded_tip_time: None,
        maturity_padding_blocks,
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
        premine_cache_key: None,
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

fn prepare_premine_local_genesis(
    config: &Config,
    miner_names: &[String],
    pow_calibration: &PowCalibrationCli,
    premine_cache_key: &str,
) -> Result<PreparedLocalGenesis> {
    // PoW mode requires an explicit target block spacing.
    let block_time_secs = config.block_time_secs.context(
        "block_time_secs must be set in config when mining_mode = pow; \
         edit config.json or pass --block-time-secs to 'kresko add'",
    )?;

    let cache_root = premine::default_cache_root();

    // Try to load by key first. On hit, we skip the (slow) Equihash benchmark
    // entirely and read every parameter we need from the manifest. The whole
    // point of the premine cache is "mine once, reuse forever"; running
    // calibration just to compute a cache key defeats that.
    let bundle = match premine::try_load_by_key(&cache_root, premine_cache_key)? {
        Some(bundle) => {
            let m = bundle.manifest();
            if m.block_time_secs != block_time_secs {
                anyhow::bail!(
                    "premine cache entry '{}' was mined for block_time_secs={} but this \
                     experiment is configured with block_time_secs={}. Pick a different \
                     --premine-cache-key, or change the experiment's block_time_secs to match.",
                    premine_cache_key,
                    m.block_time_secs,
                    block_time_secs,
                );
            }
            println!(
                "Premine cache HIT: key={} target={} block_time_secs={} funded_keys={} (no benchmark needed)",
                premine_cache_key, m.target_difficulty_limit, m.block_time_secs, m.funded_key_count,
            );
            bundle
        }
        None => generate_premine_with_warning(
            config,
            pow_calibration,
            block_time_secs,
            premine_cache_key,
            &cache_root,
        )?,
    };

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
            "experiment has {} miners but premine cache entry '{}' only provides {} \
             non-treasury funded keys. Pick a premine with more funded keys \
             (see src/premine.rs::FUNDED_KEY_COUNT).",
            miner_names.len(),
            premine_cache_key,
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
    let genesis_hex = bundle.read_text_file("genesis.hex")?;

    let local_genesis = LocalGenesisConfig {
        premine_cache_key: Some(manifest.cache_key.clone()),
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
        "runtime_funded_keys.json".to_string(),
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
        },
        runtime_funded_keys,
        payload_local_genesis_files,
        local_genesis,
    })
}

/// Cache miss path. Loudly warns the operator (generating a fresh premine
/// per experiment is the slow road we're trying to avoid), then runs the
/// Equihash benchmark + calibration to derive a target, mines a fresh
/// premine bundle, and stores it under `premine_cache_key` so subsequent
/// runs hit the cache.
fn generate_premine_with_warning(
    config: &Config,
    pow_calibration: &PowCalibrationCli,
    block_time_secs: u32,
    premine_cache_key: &str,
    cache_root: &std::path::Path,
) -> Result<premine::PremineBundle> {
    eprintln!();
    eprintln!("================================================================");
    eprintln!("⚠️  WARNING: premine cache MISS for key '{premine_cache_key}'");
    eprintln!("⚠️");
    eprintln!(
        "⚠️  No entry at {}",
        cache_root.join(premine_cache_key).display()
    );
    eprintln!("⚠️  Falling back to fresh Equihash benchmark + premine generation.");
    eprintln!("⚠️  This takes several minutes and is strongly discouraged per experiment.");
    eprintln!("⚠️");
    eprintln!("⚠️  Pre-warm the cache once with:");
    eprintln!(
        "⚠️    kresko premine --mining-cpus <N> --block-time-secs {} \\\n\
         ⚠️      --premine-cache-key {}",
        block_time_secs, premine_cache_key,
    );
    eprintln!("⚠️");
    eprintln!("⚠️  Then every subsequent 'kresko genesis' will be instant.");
    eprintln!("================================================================");
    eprintln!();

    let calibration = run_pow_calibration(config, pow_calibration, block_time_secs)?;
    let signature = CalibrationSignature::new(
        calibration.target_difficulty_limit_hex.clone(),
        block_time_secs,
    )?;

    let solver_threads = premine::default_solver_threads();
    let (bundle, outcome) = premine::resolve_premine_with_key(
        &signature,
        premine_cache_key,
        cache_root,
        solver_threads,
    )?;

    report_calibration(&calibration, &signature, outcome);
    Ok(bundle)
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

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
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
        "Benchmarking local Equihash sol/s ({} samples)...",
        pow_tuning::DEFAULT_BENCH_SAMPLES
    );
    let measured = pow_tuning::measure_local_sol_per_sec(pow_tuning::DEFAULT_BENCH_SAMPLES)
        .context("local sol/s benchmark failed")?;
    println!(
        "  local={:.3} sol/s ({} solves in {:.1}s) → assumed fleet={:.3} sol/s (÷{:.1})",
        measured.local_sol_per_sec,
        measured.total_solves,
        measured.elapsed_secs,
        measured.assumed_fleet_sol_per_sec,
        pow_tuning::LOCAL_TO_FLEET_DISCOUNT,
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
    outcome: ResolveOutcome,
) {
    println!(
        "calibrated pow_limit={} miners={} sol/s={:.3} ({}) spacing={}s adjust={:+.3} \
         natural_bits={}; premine {} key={}",
        calibration.target_difficulty_limit_hex,
        calibration.num_miners,
        calibration.sol_per_sec_per_thread,
        calibration.sol_rate_source,
        calibration.target_spacing_secs,
        calibration.target_adjust_fraction,
        calibration.natural_target_bits,
        outcome,
        signature.cache_key(),
    );
}
