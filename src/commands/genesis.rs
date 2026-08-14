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
use crate::pow_tuning::{self, PowCalibration, PowTuningInputs};
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
    /// Per-miner solutions/second to calibrate against, skipping the local
    /// benchmark entirely.
    ///
    /// The benchmark measures *this* machine and divides by a fixed constant to
    /// guess at the fleet, so its answer moves with whatever else the machine
    /// happens to be doing — which makes a calibrated chain unreproducible for
    /// reasons that have nothing to do with the fleet. Once a fleet's real rate
    /// has been measured from a run
    /// (`2^256 / pow_limit / observed_spacing / miners`), stating it here is
    /// both accurate and repeatable.
    pub sol_per_sec: Option<f64>,
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

    let seeding = match config.mining_mode {
        MiningMode::Pow => {
            // Calibrate the difficulty the fleet can actually sustain, then seed
            // the chain at that difficulty with unsolved blocks. Live nodes start
            // enforcing proof-of-work one past the seeded tip, so the chain opens
            // at its equilibrium difficulty with no adjustment warm-up.
            let calibration = run_pow_calibration(&config, &pow_calibration, target_spacing_secs)?;
            report_calibration(&calibration);
            SeedingMode::EnforcePowAfterSeededTip {
                target_difficulty_limit: calibration_target_bytes(&calibration)?,
            }
        }
        _ => SeedingMode::PowDisabled,
    };
    let prepared = prepare_generated_local_genesis(
        &config,
        &miner_names,
        maturity_padding_blocks,
        target_spacing_secs,
        daa,
        seeding,
    )?;

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
    match std::env::var("KRESKO_LATEST_NETWORK_UPGRADE")
        .ok()
        .as_deref()
    {
        Some("nu5") | Some("Nu5") => NetworkUpgrade::Nu5,
        Some("nu6") | Some("Nu6") => NetworkUpgrade::Nu6,
        Some("nu6_1") | Some("Nu6_1") => NetworkUpgrade::Nu6_1,
        Some("nu6_2") | Some("Nu6_2") => NetworkUpgrade::Nu6_2,
        Some("nu6_3") | Some("Nu6_3") => NetworkUpgrade::Nu6_3,
        Some("nu7") | Some("Nu7") => NetworkUpgrade::Nu7,
        _ => NetworkUpgrade::Nu6_3,
    }
}

/// The chain's newest upgrade, named as the node config's activation table
/// spells it.
fn latest_upgrade_name() -> &'static str {
    match local_genesis_upgrade() {
        NetworkUpgrade::Nu5 => "NU5",
        NetworkUpgrade::Nu6 => "NU6",
        NetworkUpgrade::Nu6_1 => "NU6.1",
        NetworkUpgrade::Nu6_2 => "NU6.2",
        NetworkUpgrade::Nu6_3 => "NU6.3",
        NetworkUpgrade::Nu7 => "NU7",
        _ => "NU6.1",
    }
}

/// How the seeded chain relates to proof-of-work on the live network.
///
/// Seed blocks are never solved in either mode — that is what keeps seeding
/// instant. The difference is whether the live network then enforces
/// proof-of-work at all.
#[derive(Debug, Clone, Copy)]
enum SeedingMode {
    /// Proof-of-work stays off for the whole chain. Blocks are produced with the
    /// `generate` RPC rather than by mining.
    PowDisabled,
    /// Proof-of-work is enforced from one past the seeded tip, at the calibrated
    /// limit the seed blocks were written with.
    EnforcePowAfterSeededTip { target_difficulty_limit: [u8; 32] },
}

fn prepare_generated_local_genesis(
    config: &Config,
    miner_names: &[String],
    maturity_padding_blocks: u32,
    target_spacing_secs: u32,
    daa: DaaConfig,
    seeding: SeedingMode,
) -> Result<PreparedLocalGenesis> {
    // Seed blocks are always generated with Equihash solving off, so this is
    // cheap regardless of mode. Contextual difficulty still needs a target limit
    // that is safe for the configured DAA averaging window.
    let target_difficulty_limit = match seeding {
        SeedingMode::PowDisabled => LocalTestnetGenesisOptions::default().target_difficulty_limit,
        SeedingMode::EnforcePowAfterSeededTip {
            target_difficulty_limit,
        } => target_difficulty_limit,
    };
    let options = LocalTestnetGenesisOptions {
        network_name: local_network_name(&config.chain_id),
        latest_network_upgrade: local_genesis_upgrade(),
        disable_pow: true,
        enforce_pow_after_seeded_tip: matches!(
            seeding,
            SeedingMode::EnforcePowAfterSeededTip { .. }
        ),
        target_difficulty_limit,
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
            latest_upgrade: latest_upgrade_name().to_string(),
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
            // The seeded block timestamps are already spaced by this value, so
            // the running network has to target it too: otherwise the DAA aims
            // at the node's compiled-in 75s default and every non-75s
            // experiment silently measures a 75s chain.
            post_blossom_pow_target_spacing: Some(target_spacing_secs),
            daa,
            pow_start_height: network_params.pow_start_height().map(|height| height.0),
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
        nu6_2: (upgrade >= NetworkUpgrade::Nu6_2).then_some(activation_height),
        nu6_3: (upgrade >= NetworkUpgrade::Nu6_3).then_some(activation_height),
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

/// Benchmark this machine's Equihash solver, discount it to a conservative
/// fleet estimate, and turn that into the `pow_limit` the network should run at.
///
/// The benchmark is the slow step in the whole genesis pipeline; everything it
/// feeds is cheap. Underestimating the fleet's rate is the safe direction: it
/// produces a looser target that the adjustment can tighten into steady state,
/// whereas overestimating produces one the adjustment cannot loosen past, and
/// the chain stalls.
fn run_pow_calibration(
    config: &Config,
    cli: &PowCalibrationCli,
    target_spacing_secs: u32,
) -> Result<PowCalibration> {
    if let Some(sol_per_sec) = cli.sol_per_sec {
        println!("Using stated fleet rate {sol_per_sec:.3} sol/s per miner (no local benchmark)");
        return pow_tuning::calibrate(&PowTuningInputs {
            num_miners: config.miners.len(),
            target_spacing_secs,
            target_adjust_fraction: cli.adjust_fraction,
            sol_per_sec_override: Some(sol_per_sec),
            ..Default::default()
        })
        .context("PoW calibration failed");
    }

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
        "  params={} local={:.3} sol/s ({} solutions in {:.1}s) -> assumed fleet={:.3} sol/s (/{:.1})",
        measured.equihash_params,
        measured.local_sol_per_sec,
        measured.total_solves,
        measured.elapsed_secs,
        measured.assumed_fleet_sol_per_sec,
        measured.fleet_discount,
    );

    // `kresko mine` runs one single-threaded Equihash solver per node, so the
    // fleet's mining CPU count is its miner count.
    pow_tuning::calibrate(&PowTuningInputs {
        num_miners: config.miners.len(),
        target_spacing_secs,
        target_adjust_fraction: cli.adjust_fraction,
        sol_per_sec_override: Some(measured.assumed_fleet_sol_per_sec),
        ..Default::default()
    })
    .context("PoW calibration failed")
}

fn report_calibration(calibration: &PowCalibration) {
    println!(
        "calibrated pow_limit={} miners={} sol/s={:.3} ({}) spacing={}s adjust={:+.3} \
         natural_bits={}",
        calibration.target_difficulty_limit_hex,
        calibration.num_miners,
        calibration.sol_per_sec_per_thread,
        calibration.sol_rate_source,
        calibration.target_spacing_secs,
        calibration.target_adjust_fraction,
        calibration.natural_target_bits,
    );
}

fn calibration_target_bytes(calibration: &PowCalibration) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(&calibration.target_difficulty_limit_hex, &mut bytes).with_context(
        || {
            format!(
                "calibration produced a malformed target difficulty limit: {}",
                calibration.target_difficulty_limit_hex
            )
        },
    )?;
    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The PoW path must produce a chain the node will actually accept: seed
    /// blocks written at the calibrated limit, proof-of-work enabled on the live
    /// network, and a start height one past the seeded tip.
    ///
    /// Getting any one of these wrong only surfaces at `zakurad start` on the
    /// fleet, after the payload has already shipped.
    #[test]
    fn pow_seeding_renders_a_config_matching_the_generated_chain() {
        let miner_names: Vec<String> = (0..4).map(|i| format!("miner-{i}")).collect();
        let config = Config {
            chain_id: "calibration-test".to_string(),
            ..Config::default()
        };

        // A plausible calibrated limit: far harder than the default private
        // network limit, and safely below the averaging-window overflow ceiling.
        let mut target_difficulty_limit = [0u8; 32];
        target_difficulty_limit[0] = 0x04;
        target_difficulty_limit[1] = 0xec;

        let maturity_padding_blocks = 125;
        let prepared = prepare_generated_local_genesis(
            &config,
            &miner_names,
            maturity_padding_blocks,
            25,
            DaaConfig::default(),
            SeedingMode::EnforcePowAfterSeededTip {
                target_difficulty_limit,
            },
        )
        .expect("PoW seeding should prepare a local genesis");

        // One premine block per miner, plus the requested maturity padding.
        let seeded_block_count = miner_names.len() as u32 + 1 + maturity_padding_blocks;
        assert_eq!(
            prepared.local_genesis.maturity_padding_block_count,
            maturity_padding_blocks
        );
        assert_eq!(
            prepared.local_testnet.pow_start_height,
            Some(seeded_block_count)
        );
        assert!(
            !prepared.local_testnet.disable_pow,
            "the live network must enforce proof-of-work"
        );
        assert_eq!(
            prepared.local_testnet.post_blossom_pow_target_spacing,
            Some(25)
        );

        let template = zebra_config::template_for(crate::config::NetworkKind::LocalGenesis)
            .expect("template generation");
        let rendered =
            zebra_config::apply_local_testnet_parameters(&template, &prepared.local_testnet)
                .expect("rendering the node config");
        zebra_config::verify_local_testnet_parameters(&rendered, &prepared.local_testnet)
            .expect("the rendered config must match the generated chain");

        assert!(rendered.contains(&format!("pow_start_height = {seeded_block_count}")));
        assert!(rendered.contains("disable_pow = false"));
        assert!(
            rendered.contains(&prepared.local_testnet.target_difficulty_limit),
            "the rendered limit must be the calibrated one the seed blocks used"
        );
    }

    /// The non-PoW path is unchanged: no start height, proof-of-work off.
    #[test]
    fn pow_disabled_seeding_renders_no_start_height() {
        let config = Config {
            chain_id: "generate-mode".to_string(),
            ..Config::default()
        };

        let prepared = prepare_generated_local_genesis(
            &config,
            &["miner-0".to_string()],
            2,
            25,
            DaaConfig::default(),
            SeedingMode::PowDisabled,
        )
        .expect("generate-mode seeding should prepare a local genesis");

        assert_eq!(prepared.local_testnet.pow_start_height, None);
        assert!(prepared.local_testnet.disable_pow);

        let template = zebra_config::template_for(crate::config::NetworkKind::LocalGenesis)
            .expect("template generation");
        let rendered =
            zebra_config::apply_local_testnet_parameters(&template, &prepared.local_testnet)
                .expect("rendering the node config");
        assert!(!rendered.contains("pow_start_height"));
    }
}
