use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
};

use crate::config::{
    Config, DaaConfig, LocalGenesisActivationHeights, LocalGenesisConfig, LocalGenesisFundedKey,
    MiningMode, OrchardTxblastConfig,
};
use crate::zebra_config::{self, LocalTestnetParameters};

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
    _pow_calibration: PowCalibrationCli,
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
        zebra_config::template_for(config.network_kind)?
    };
    zebra_config::ensure_miner_address_is_set(&template).with_context(|| {
        format!(
            "invalid zebra config template at {}",
            template_path.display()
        )
    })?;
    let toml_network = zebra_config::testnet_toml_parameters(&template)
        .with_context(|| format!("invalid testnet parameters in {}", template_path.display()))?;
    let target_spacing_secs = config
        .block_time_secs
        .unwrap_or(DEFAULT_TARGET_SPACING_SECS);
    let daa = toml_network.daa.with_missing_from(config.daa);

    let prepared = match config.mining_mode {
        MiningMode::Pow => prepare_generated_local_genesis(
            &config,
            &miner_names,
            0,
            target_spacing_secs,
            daa,
            false,
        )?,
        _ => prepare_generated_local_genesis(
            &config,
            &miner_names,
            maturity_padding_blocks,
            target_spacing_secs,
            daa,
            true,
        )?,
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
        node_config = zebra_config::set_miner_address(&node_config, &funded_key.address)?;
        node_config =
            zebra_config::apply_local_testnet_parameters(&node_config, &prepared.local_testnet)?;
        zebra_config::verify_local_testnet_parameters(&node_config, &prepared.local_testnet)
            .with_context(|| format!("rendered invalid zebrad.toml for node {node_name}"))?;
        // Strip optional fields that older zebrad versions reject. Doing
        // this in Rust (TOML-aware) replaces a sed line in node_init.sh.
        node_config = zebra_config::strip_genesis_block_path(&node_config)?;
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;
        // Pre-render the isolated-RPC bootstrap config alongside zebrad.toml
        // so node_init.sh doesn't have to munge TOML at runtime.
        let bootstrap_config = zebra_config::bootstrap_config_for_isolated_rpc(&node_config)
            .with_context(|| {
                format!("failed to render bootstrap zebrad.toml for node {node_name}")
            })?;
        std::fs::write(node_dir.join("zebrad.bootstrap.toml"), &bootstrap_config)?;
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
    let zebrad_dest = bin_dir.join("zebrad");
    std::fs::copy(zebrad_path, &zebrad_dest)
        .with_context(|| format!("failed to copy zebrad from {}", zebrad_binary))?;
    let zebrad_sha256 = sha256_file(&zebrad_dest)
        .with_context(|| format!("failed to hash zebrad at {}", zebrad_dest.display()))?;
    println!("Copied zebrad binary from {zebrad_binary} (sha256={zebrad_sha256})");

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
    let kresko_dest = bin_dir.join("kresko");
    std::fs::copy(&kresko_binary_path, &kresko_dest).with_context(|| {
        format!(
            "failed to copy kresko from {}",
            kresko_binary_path.display()
        )
    })?;
    let kresko_sha256 = sha256_file(&kresko_dest)
        .with_context(|| format!("failed to hash kresko at {}", kresko_dest.display()))?;
    println!(
        "Copied kresko binary from {} (sha256={kresko_sha256})",
        kresko_binary_path.display()
    );

    // Manifest read by node_init.sh to verify what was installed matches what
    // genesis built. Sha256 in hex; flat keys so awk/jq parsing in shell stays
    // trivial without pulling in a TOML reader on the node.
    let manifest_lines = format!(
        "zebrad_sha256={}\nkresko_sha256={}\nzebrad_source={}\nkresko_source={}\n",
        zebrad_sha256,
        kresko_sha256,
        zebrad_path.display(),
        kresko_binary_path.display(),
    );
    std::fs::write(bin_dir.join("manifest.txt"), manifest_lines)
        .context("failed to write payload binary manifest")?;

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

/// Latest network upgrade to activate on the generated local chain.
///
/// Defaults to Nu6_3: a release-profile zakurad compiles the Nu7 consensus
/// branch id out entirely (it is test-gated pending librustzcash), so a chain
/// that activates Nu7 cannot be mined -- every block at the activation height
/// is rejected as WrongTransactionConsensusBranchId. Override with
/// KRESKO_LATEST_NETWORK_UPGRADE=nu7 against a build that carries it.
fn local_genesis_upgrade() -> NetworkUpgrade {
    match std::env::var("KRESKO_LATEST_NETWORK_UPGRADE").ok().as_deref() {
        Some("nu5") | Some("Nu5") => NetworkUpgrade::Nu5,
        Some("nu6") | Some("Nu6") => NetworkUpgrade::Nu6,
        Some("nu6_1") | Some("Nu6_1") => NetworkUpgrade::Nu6_1,
        Some("nu6_2") | Some("Nu6_2") => NetworkUpgrade::Nu6_2,
        Some("nu6_3") | Some("Nu6_3") => NetworkUpgrade::Nu6_3,
        Some("nu7") | Some("Nu7") => NetworkUpgrade::Nu7,
        _ => NetworkUpgrade::Nu6_3,
    }
}

fn prepare_generated_local_genesis(
    config: &Config,
    miner_names: &[String],
    maturity_padding_blocks: u32,
    target_spacing_secs: u32,
    daa: DaaConfig,
    disable_pow: bool,
) -> Result<PreparedLocalGenesis> {
    // Non-PoW path: zebra-chain disables Equihash solving so we can seed blocks
    // cheaply, but contextual difficulty still needs a target limit that is safe
    // for the configured DAA averaging window.
    let options = LocalTestnetGenesisOptions {
        network_name: local_network_name(&config.chain_id),
        latest_network_upgrade: local_genesis_upgrade(),
        disable_pow,
        target_spacing_secs,
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
    let activation_height = activation_height(&network_params, local_genesis_upgrade())?;
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
        target_spacing_secs: Some(target_spacing_secs),
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
            activates_nu7: local_genesis.activation_heights.nu7.is_some(),
            network_name: local_genesis.network_name.clone(),
            network_magic: local_genesis.network_magic,
            target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
            disable_pow: local_genesis.disable_pow,
            genesis_hash: local_genesis.genesis_hash.clone(),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: local_genesis.slow_start_interval,
            pre_blossom_halving_interval: local_genesis.pre_blossom_halving_interval,
            activation_height: local_genesis.activation_heights.overwinter,
            lockbox_disbursements: zebra_config::default_nu6_1_lockbox_disbursements()?,
            post_blossom_pow_target_spacing: None,
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

fn activation_heights(activation_height: u32) -> LocalGenesisActivationHeights {
    let upgrade = local_genesis_upgrade();
    LocalGenesisActivationHeights {
        overwinter: activation_height,
        sapling: activation_height,
        blossom: activation_height,
        heartwood: activation_height,
        canopy: activation_height,
        nu5: activation_height,
        nu6: activation_height,
        nu6_1: activation_height,
        nu7: (upgrade >= NetworkUpgrade::Nu7).then_some(activation_height),
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

/// SHA-256 a file's contents and return the lowercase hex digest. Streams
/// in 64 KiB chunks so we don't need to load the binary into memory.
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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
