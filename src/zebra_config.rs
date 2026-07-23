use anyhow::{Context, Result};
use std::str::FromStr;
use toml::map::Map;
use zebra_chain::transparent::Address as TransparentAddress;

use crate::config::Instance;
use crate::config::{DaaConfig, NetworkKind};

/// Synthetic P2SH recipient used for local-genesis NU6.1 activation.
///
/// Kresko local genesis does not fund the NU6.1 lockbox, so the default
/// disbursement amount is zero. Zebra still needs an explicit config entry
/// when NU6.1 is activated before NU7.
pub const DEFAULT_NU6_1_LOCKBOX_ADDRESS: &str = "t26ovBdKAJLtrvBsE2QGF4nqBkEuptuPFZz";
pub const PUBLIC_BLOCK_SYNC_PEER_TARGET: i64 = 100;
pub const PUBLIC_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT: i64 = 100;

/// A single one-time NU6.1 lockbox disbursement entry, matching zebra's
/// `[[network.testnet_parameters.lockbox_disbursements]]` schema.
///
/// Validation enforces P2SH on construction — P2PKH disbursements caused
/// zebrad to panic during NU6.1 activation, so no non-P2SH entry should
/// ever leave kresko.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockboxDisbursement {
    pub address: String,
    pub amount_zats: u64,
}

impl LockboxDisbursement {
    /// Construct a disbursement, asserting the address is a transparent P2SH
    /// address (`t3...` mainnet / `t2...` testnet).
    pub fn new_p2sh(address: impl Into<String>, amount_zats: u64) -> Result<Self> {
        let address = address.into();
        ensure_address_is_p2sh(&address)?;
        Ok(Self {
            address,
            amount_zats,
        })
    }
}

pub fn default_nu6_1_lockbox_disbursements() -> Result<Vec<LockboxDisbursement>> {
    Ok(vec![LockboxDisbursement::new_p2sh(
        DEFAULT_NU6_1_LOCKBOX_ADDRESS,
        0,
    )?])
}

/// Reject anything that isn't a transparent P2SH address. P2PKH (`t1`/`tm`)
/// makes zebrad panic at NU6.1 activation; Tex addresses are a separate
/// shape that zebra's lockbox validator does not accept.
fn ensure_address_is_p2sh(address: &str) -> Result<()> {
    let parsed = TransparentAddress::from_str(address).with_context(|| {
        format!("lockbox disbursement address {address} is not a valid transparent address")
    })?;
    match parsed {
        TransparentAddress::PayToScriptHash { .. } => Ok(()),
        TransparentAddress::PayToPublicKeyHash { .. } => anyhow::bail!(
            "lockbox disbursement address {address} is P2PKH; zebrad panics on \
             non-P2SH disbursements at NU6.1 activation. Use a P2SH (t2/t3) address."
        ),
        TransparentAddress::Tex { .. } => anyhow::bail!(
            "lockbox disbursement address {address} is a Tex address; only P2SH \
             (t2/t3) is accepted in lockbox_disbursements."
        ),
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LocalTestnetParameters {
    pub network_name: String,
    pub network_magic: [u8; 4],
    pub target_difficulty_limit: String,
    pub disable_pow: bool,
    pub genesis_hash: String,
    pub checkpoints_path: String,
    pub slow_start_interval: u32,
    pub pre_blossom_halving_interval: u32,
    pub activation_height: u32,
    /// Newest upgrade the generated chain activates, as it appears in the
    /// config's activation table (e.g. "NU6.3"). Everything below it is
    /// activated too; nothing above it is written.
    pub latest_upgrade: String,
    /// One-time NU6.1 lockbox disbursements emitted in
    /// `[[network.testnet_parameters.lockbox_disbursements]]`. Kresko's
    /// default local-genesis path uses a zero-zat synthetic P2SH entry so
    /// Zebra has explicit NU6.1 disbursement config before NU7.
    pub lockbox_disbursements: Vec<LockboxDisbursement>,
    /// Legacy knobs kept in metadata for old generated configs. Zebra's NU7
    /// testnet config does not accept these fields, so rendering skips them.
    pub post_blossom_pow_target_spacing: Option<u32>,
    pub daa: DaaConfig,
    pub pow_start_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestnetTomlParameters {
    pub post_blossom_pow_target_spacing: Option<u32>,
    pub daa: DaaConfig,
}

/// Network upgrades a local-genesis chain activates, newest last.
///
/// The writer and the verifier must agree exactly, so both derive the list
/// here. NU7 is included only when the generated chain activates it: a node
/// build without the NU7 consensus branch id cannot mine a chain that
/// declares an NU7 activation.
/// Local-genesis activation table, oldest first.
///
/// Everything up to and including the chain's newest upgrade is activated at
/// the same height. The tail is optional because a node build without an
/// upgrade's consensus branch id cannot mine a chain that declares it.
const LOCAL_GENESIS_UPGRADES: &[&str] = &[
    "Overwinter",
    "Sapling",
    "Blossom",
    "Heartwood",
    "Canopy",
    "NU5",
    "NU6",
    "NU6.1",
    "NU6.2",
    "NU6.3",
    "NU7",
];

fn local_genesis_upgrade_names(latest: &str) -> Vec<&'static str> {
    let end = LOCAL_GENESIS_UPGRADES
        .iter()
        .position(|name| *name == latest)
        .unwrap_or(LOCAL_GENESIS_UPGRADES.len() - 1);
    LOCAL_GENESIS_UPGRADES[..=end].to_vec()
}

pub fn template_for(network_kind: NetworkKind) -> Result<String> {
    let mut config = zebra_default_config_value()?;

    match network_kind {
        NetworkKind::LocalGenesis => {
            set_path(&mut config, &["network", "network"], "Testnet".into());
            set_path(
                &mut config,
                &["network", "listen_addr"],
                "0.0.0.0:18233".into(),
            );
            set_path(
                &mut config,
                &["network", "initial_testnet_peers"],
                toml::Value::Array(Vec::new()),
            );
            set_path(
                &mut config,
                &["network", "peerset_initial_target_size"],
                4.into(),
            );
            set_path(
                &mut config,
                &["mempool", "debug_enable_at_height"],
                0.into(),
            );
            set_path(&mut config, &["mining", "miner_address"], "auto".into());
            set_path(
                &mut config,
                &["state", "cache_dir"],
                "/root/.cache/zebra".into(),
            );
            set_path(&mut config, &["rpc", "listen_addr"], "0.0.0.0:18232".into());
            set_path(&mut config, &["rpc", "enable_cookie_auth"], false.into());
            set_path(
                &mut config,
                &["rpc", "debug_force_finished_sync"],
                true.into(),
            );
            set_path(&mut config, &["rpc", "parallel_cpu_threads"], 1.into());
            set_path(&mut config, &["sync", "parallel_cpu_threads"], 1.into());
        }
        NetworkKind::PublicTestnet => {
            set_path(&mut config, &["network", "network"], "Testnet".into());
            set_path(
                &mut config,
                &["network", "listen_addr"],
                "0.0.0.0:18233".into(),
            );
            set_path(
                &mut config,
                &["state", "cache_dir"],
                "/root/.cache/zebra".into(),
            );
            set_path(&mut config, &["rpc", "listen_addr"], "0.0.0.0:18232".into());
            set_path(&mut config, &["rpc", "enable_cookie_auth"], false.into());
            tune_public_block_sync(&mut config);
        }
        NetworkKind::Mainnet => {
            set_path(&mut config, &["network", "network"], "Mainnet".into());
            set_path(
                &mut config,
                &["network", "listen_addr"],
                "0.0.0.0:8233".into(),
            );
            set_path(
                &mut config,
                &["state", "cache_dir"],
                "/root/.cache/zebra".into(),
            );
            set_path(&mut config, &["rpc", "listen_addr"], "0.0.0.0:8232".into());
            set_path(&mut config, &["rpc", "enable_cookie_auth"], false.into());
            tune_public_block_sync(&mut config);
        }
    }

    toml::to_string_pretty(&config).context("failed to serialize Zebra-generated config")
}

fn tune_public_block_sync(config: &mut toml::Value) {
    set_path(
        config,
        &["network", "peerset_initial_target_size"],
        PUBLIC_BLOCK_SYNC_PEER_TARGET.into(),
    );
    set_path(
        config,
        &["sync", "download_concurrency_limit"],
        PUBLIC_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT.into(),
    );
}

fn zebra_default_config_value() -> Result<toml::Value> {
    toml::Value::try_from(zebrad::config::ZakuradConfig::default())
        .context("failed to serialize Zebra default config")
}

fn set_path(config: &mut toml::Value, path: &[&str], value: toml::Value) {
    let mut current = config;
    for key in &path[..path.len() - 1] {
        let table = current
            .as_table_mut()
            .expect("Zebra config root and sections should be TOML tables");
        current = table
            .entry((*key).to_string())
            .or_insert_with(|| toml::Value::Table(Map::new()));
    }
    current
        .as_table_mut()
        .expect("Zebra config section should be a TOML table")
        .insert(path[path.len() - 1].to_string(), value);
}

pub fn testnet_toml_parameters(config: &str) -> Result<TestnetTomlParameters> {
    let parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    let Some(testnet_params) = parsed
        .get("network")
        .and_then(|network| network.get("testnet_parameters"))
    else {
        return Ok(TestnetTomlParameters::default());
    };

    Ok(TestnetTomlParameters {
        post_blossom_pow_target_spacing: optional_u32(
            testnet_params,
            "post_blossom_pow_target_spacing",
        )?,
        daa: DaaConfig {
            pow_averaging_window: optional_usize(testnet_params, "pow_averaging_window")?,
            pow_median_block_span: optional_usize(testnet_params, "pow_median_block_span")?,
            pre_blossom_pow_target_spacing: optional_i64(
                testnet_params,
                "pre_blossom_pow_target_spacing",
            )?,
            pow_damping_factor: optional_i32(testnet_params, "pow_damping_factor")?,
            pow_max_adjust_up_percent: optional_i32(testnet_params, "pow_max_adjust_up_percent")?,
            pow_max_adjust_down_percent: optional_i32(
                testnet_params,
                "pow_max_adjust_down_percent",
            )?,
            testnet_min_difficulty_start_height: optional_u32(
                testnet_params,
                "testnet_min_difficulty_start_height",
            )?,
            testnet_min_difficulty_gap_multiplier: optional_i32(
                testnet_params,
                "testnet_min_difficulty_gap_multiplier",
            )?,
        },
    })
}

/// Generate a per-node zebrad.toml with the correct peer list.
///
/// Takes the template content as a string, replaces the `initial_testnet_peers`
/// line with the actual peer IPs (excluding the current node).
pub fn generate_node_config(
    template: &str,
    network_kind: NetworkKind,
    current_node: &Instance,
    all_instances: &[Instance],
) -> Result<String> {
    let mut parsed: toml::Value =
        toml::from_str(template).context("failed to parse zebrad.toml")?;
    let p2p_port = match network_kind {
        NetworkKind::Mainnet => 8233,
        NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => 18233,
    };
    let peers: Vec<String> = all_instances
        .iter()
        .filter(|inst| inst.name != current_node.name)
        .filter(|inst| inst.public_ip != "TBD")
        .map(|inst| format!("{}:{p2p_port}", inst.public_ip))
        .collect();

    let peer_key = match network_kind {
        NetworkKind::LocalGenesis | NetworkKind::PublicTestnet => "initial_testnet_peers",
        NetworkKind::Mainnet => "initial_mainnet_peers",
    };
    let peer_values = match network_kind {
        NetworkKind::LocalGenesis => peers,
        NetworkKind::PublicTestnet | NetworkKind::Mainnet => {
            let mut values = initial_peers_from_template(template, peer_key)?;
            for peer in peers {
                if !values.contains(&peer) {
                    values.push(peer);
                }
            }
            values
        }
    };

    let external_addr = match network_kind {
        NetworkKind::LocalGenesis => None,
        NetworkKind::PublicTestnet | NetworkKind::Mainnet => {
            if current_node.public_ip == "TBD" || current_node.public_ip.is_empty() {
                anyhow::bail!(
                    "cannot render external_addr for {} without a public IP",
                    current_node.name
                );
            }
            Some(format!("{}:{p2p_port}", current_node.public_ip))
        }
    };

    set_path(
        &mut parsed,
        &["network", peer_key],
        toml::Value::Array(peer_values.into_iter().map(toml::Value::String).collect()),
    );
    if let Some(external_addr) = external_addr {
        set_path(
            &mut parsed,
            &["network", "external_addr"],
            external_addr.into(),
        );
    }
    if network_kind == NetworkKind::LocalGenesis {
        set_path(
            &mut parsed,
            &["network", "peerset_initial_target_size"],
            (all_instances.len() as i64).into(),
        );
    }

    toml::to_string_pretty(&parsed).context("failed to serialize rendered zebrad.toml")
}

fn initial_peers_from_template(template: &str, peer_key: &str) -> Result<Vec<String>> {
    let parsed: toml::Value = toml::from_str(template).context("failed to parse zebrad.toml")?;
    let peers = parsed
        .get("network")
        .and_then(|network| network.get(peer_key))
        .and_then(|value| value.as_array())
        .with_context(|| format!("zebrad.toml template is missing network.{peer_key}"))?;

    peers
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .with_context(|| format!("network.{peer_key} contains a non-string peer"))
        })
        .collect()
}

/// Produce a bootstrap variant of `config` with all P2P peering disabled.
///
/// The bootstrap config is what node_init.sh hands to a short-lived `zebrad`
/// process when it needs to do RPC work (e.g. generate a wallet address or
/// seed local-genesis blocks) before joining the fleet. We need to:
///
/// - Bind the P2P listener to `127.0.0.1:0` so the bootstrap node never
///   announces itself on the public IP.
/// - Empty `initial_testnet_peers` / `initial_mainnet_peers` so it doesn't
///   try to dial the (still-being-provisioned) fleet.
///
/// Doing this in Rust at payload-build time means node_init.sh no longer
/// has to munge TOML with awk.
pub fn bootstrap_config_for_isolated_rpc(config: &str) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    set_path(
        &mut parsed,
        &["network", "listen_addr"],
        "127.0.0.1:0".into(),
    );
    set_path(
        &mut parsed,
        &["network", "initial_testnet_peers"],
        toml::Value::Array(Vec::new()),
    );
    set_path(
        &mut parsed,
        &["network", "initial_mainnet_peers"],
        toml::Value::Array(Vec::new()),
    );
    toml::to_string_pretty(&parsed).context("failed to serialize bootstrap zebrad.toml")
}

/// Strip the optional `network.testnet_parameters.genesis_block_path` key
/// from a rendered config. Older zebrad versions reject the field, so we
/// remove it for forward/backward compatibility.
pub fn strip_genesis_block_path(config: &str) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    if let Some(testnet_params) = parsed
        .get_mut("network")
        .and_then(|n| n.get_mut("testnet_parameters"))
        .and_then(|tp| tp.as_table_mut())
    {
        testnet_params.remove("genesis_block_path");
    }
    toml::to_string_pretty(&parsed).context("failed to serialize zebrad.toml")
}

/// Read `network.testnet_parameters.genesis_hash` from a rendered config.
pub fn read_genesis_hash(config: &str) -> Result<Option<String>> {
    let parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    Ok(parsed
        .get("network")
        .and_then(|n| n.get("testnet_parameters"))
        .and_then(|tp| tp.get("genesis_hash"))
        .and_then(toml::Value::as_str)
        .map(str::to_lowercase))
}

/// Read `mining.miner_address` from a rendered config.
pub fn read_miner_address(config: &str) -> Result<Option<String>> {
    let parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    Ok(parsed
        .get("mining")
        .and_then(|m| m.get("miner_address"))
        .and_then(toml::Value::as_str)
        .map(str::to_string))
}

/// Set `mining.miner_address` to a concrete address in a rendered zebrad.toml.
pub fn set_miner_address(config: &str, miner_address: &str) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    set_path(
        &mut parsed,
        &["mining", "miner_address"],
        miner_address.into(),
    );
    toml::to_string_pretty(&parsed).context("failed to serialize rendered zebrad.toml")
}

/// Inject custom `[network.testnet_parameters]` for locally generated chains.
pub fn apply_local_testnet_parameters(
    config: &str,
    params: &LocalTestnetParameters,
) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    let mut testnet_params = Map::new();

    testnet_params.insert(
        "network_name".to_string(),
        params.network_name.clone().into(),
    );
    testnet_params.insert(
        "network_magic".to_string(),
        toml::Value::Array(
            params
                .network_magic
                .iter()
                .map(|byte| toml::Value::Integer(i64::from(*byte)))
                .collect(),
        ),
    );
    testnet_params.insert(
        "target_difficulty_limit".to_string(),
        params.target_difficulty_limit.clone().into(),
    );
    testnet_params.insert("disable_pow".to_string(), params.disable_pow.into());
    testnet_params.insert(
        "genesis_hash".to_string(),
        params.genesis_hash.clone().into(),
    );
    testnet_params.insert(
        "slow_start_interval".to_string(),
        i64::from(params.slow_start_interval).into(),
    );
    testnet_params.insert(
        "pre_blossom_halving_interval".to_string(),
        i64::from(params.pre_blossom_halving_interval).into(),
    );
    let mut disbursement_entries: Vec<toml::Value> =
        Vec::with_capacity(params.lockbox_disbursements.len());
    for disbursement in &params.lockbox_disbursements {
        // Belt-and-braces: even if the caller built the struct directly,
        // refuse to write a config that zebrad will refuse to load.
        ensure_address_is_p2sh(&disbursement.address).with_context(|| {
            format!(
                "lockbox disbursement {} failed P2SH validation",
                disbursement.address
            )
        })?;
        let mut entry = Map::new();
        entry.insert("address".into(), disbursement.address.clone().into());
        entry.insert(
            "amount".into(),
            i64::try_from(disbursement.amount_zats)
                .with_context(|| {
                    format!(
                        "lockbox disbursement amount {} does not fit in i64",
                        disbursement.amount_zats
                    )
                })?
                .into(),
        );
        disbursement_entries.push(toml::Value::Table(entry));
    }
    testnet_params.insert(
        "lockbox_disbursements".to_string(),
        toml::Value::Array(disbursement_entries),
    );
    // Local genesis generation clears funding streams; mirror that here to avoid
    // default Testnet recipient validation for short custom halving intervals.
    testnet_params.insert(
        "pre_nu6_funding_streams".to_string(),
        empty_recipients_table(),
    );
    testnet_params.insert(
        "post_nu6_funding_streams".to_string(),
        empty_recipients_table(),
    );
    testnet_params.insert(
        "checkpoints".to_string(),
        params.checkpoints_path.clone().into(),
    );

    let mut activation_heights = Map::new();
    // NU6.1 is the one-time ZIP-271 lockbox disbursement event. Local genesis
    // activates it at the same height as NU7 with a zero-zat synthetic
    // disbursement so Zebra's NU6.1 config validation is explicit.
    for upgrade in local_genesis_upgrade_names(&params.latest_upgrade) {
        activation_heights.insert(
            upgrade.to_string(),
            toml::Value::Integer(i64::from(params.activation_height)),
        );
    }
    testnet_params.insert(
        "activation_heights".to_string(),
        toml::Value::Table(activation_heights),
    );

    set_path(
        &mut parsed,
        &["network", "testnet_parameters"],
        toml::Value::Table(testnet_params),
    );

    toml::to_string_pretty(&parsed).context("failed to serialize rendered zebrad.toml")
}

fn empty_recipients_table() -> toml::Value {
    let mut table = Map::new();
    table.insert("recipients".to_string(), toml::Value::Array(Vec::new()));
    toml::Value::Table(table)
}

/// Parse and validate the rendered config so experiment-specific testnet
/// parameters cannot be silently dropped during templating.
///
/// Catches three classes of inconsistency that would otherwise only surface
/// at `zebrad start` time:
///
/// 1. The rendered target_difficulty_limit / genesis_hash do not match the
///    values the genesis builder produced — i.e. the on-disk chain does not
///    match the on-disk config.
/// 2. The activation_heights table is missing entries the genesis builder
///    relied on (e.g. NU7).
/// 3. Any lockbox disbursement entry that survived rendering is non-P2SH.
pub fn verify_local_testnet_parameters(
    config: &str,
    params: &LocalTestnetParameters,
) -> Result<()> {
    let parsed: toml::Value =
        toml::from_str(config).context("failed to parse rendered zebrad.toml")?;
    let testnet_params = parsed
        .get("network")
        .and_then(|network| network.get("testnet_parameters"))
        .context("missing [network.testnet_parameters] section in rendered zebrad.toml")?;

    let actual_target = testnet_params
        .get("target_difficulty_limit")
        .and_then(toml::Value::as_str)
        .context("missing network.testnet_parameters.target_difficulty_limit")?;
    if actual_target != params.target_difficulty_limit {
        anyhow::bail!(
            "rendered target_difficulty_limit mismatch: expected {}, got {}",
            params.target_difficulty_limit,
            actual_target,
        );
    }

    let actual_genesis = testnet_params
        .get("genesis_hash")
        .and_then(toml::Value::as_str)
        .context("missing network.testnet_parameters.genesis_hash")?;
    if !actual_genesis.eq_ignore_ascii_case(&params.genesis_hash) {
        anyhow::bail!(
            "rendered genesis_hash mismatch: generated chain has {}, but rendered \
             config has {}. Re-run `kresko genesis` so the config matches the chain.",
            params.genesis_hash,
            actual_genesis,
        );
    }

    let activation = testnet_params
        .get("activation_heights")
        .and_then(toml::Value::as_table)
        .context("missing network.testnet_parameters.activation_heights table")?;
    for upgrade in local_genesis_upgrade_names(&params.latest_upgrade) {
        let height = activation
            .get(upgrade)
            .and_then(toml::Value::as_integer)
            .with_context(|| {
                format!("missing activation height for {upgrade} in rendered config")
            })?;
        if height != i64::from(params.activation_height) {
            anyhow::bail!(
                "rendered activation height for {upgrade} is {height}, expected {}",
                params.activation_height,
            );
        }
    }

    if let Some(rendered) = testnet_params
        .get("lockbox_disbursements")
        .and_then(toml::Value::as_array)
    {
        if rendered.len() != params.lockbox_disbursements.len() {
            anyhow::bail!(
                "rendered lockbox_disbursements length mismatch: expected {}, got {}",
                params.lockbox_disbursements.len(),
                rendered.len(),
            );
        }
        for (entry, expected) in rendered.iter().zip(&params.lockbox_disbursements) {
            let address = entry
                .get("address")
                .and_then(toml::Value::as_str)
                .context("rendered lockbox disbursement entry missing address")?;
            ensure_address_is_p2sh(address).with_context(|| {
                format!("rendered config has non-P2SH lockbox disbursement {address}")
            })?;
            if address != expected.address {
                anyhow::bail!(
                    "rendered lockbox disbursement address mismatch: expected {}, got {}",
                    expected.address,
                    address,
                );
            }
            let amount = entry
                .get("amount")
                .and_then(toml::Value::as_integer)
                .context("rendered lockbox disbursement entry missing amount")?;
            if amount != i64::try_from(expected.amount_zats)? {
                anyhow::bail!(
                    "rendered lockbox disbursement amount mismatch: expected {}, got {}",
                    expected.amount_zats,
                    amount,
                );
            }
        }
    }

    Ok(())
}

fn optional_usize(testnet_params: &toml::Value, key: &str) -> Result<Option<usize>> {
    let Some(actual) = testnet_params.get(key).and_then(toml::Value::as_integer) else {
        return Ok(None);
    };

    Ok(Some(
        usize::try_from(actual).with_context(|| format!("{key} does not fit in usize"))?,
    ))
}

fn optional_u32(testnet_params: &toml::Value, key: &str) -> Result<Option<u32>> {
    let Some(actual) = testnet_params.get(key).and_then(toml::Value::as_integer) else {
        return Ok(None);
    };

    Ok(Some(
        u32::try_from(actual).with_context(|| format!("{key} does not fit in u32"))?,
    ))
}

fn optional_i64(testnet_params: &toml::Value, key: &str) -> Result<Option<i64>> {
    Ok(testnet_params.get(key).and_then(toml::Value::as_integer))
}

fn optional_i32(testnet_params: &toml::Value, key: &str) -> Result<Option<i32>> {
    let Some(actual) = testnet_params.get(key).and_then(toml::Value::as_integer) else {
        return Ok(None);
    };

    Ok(Some(
        i32::try_from(actual).with_context(|| format!("{key} does not fit in i32"))?,
    ))
}

/// Ensure the template has a non-empty mining.miner_address value.
pub fn ensure_miner_address_is_set(template: &str) -> Result<()> {
    let Some(address) = extract_miner_address(template) else {
        anyhow::bail!(
            "missing `mining.miner_address` in zebrad.toml. Set it to `auto` or a valid Zcash address"
        );
    };

    // "auto" is supported and means node_init.sh will generate a wallet-owned address.
    if is_auto_miner_address(&address) {
        return Ok(());
    }

    if matches!(
        address.to_ascii_lowercase().as_str(),
        "todo" | "changeme" | "replace_me" | "<address>" | "<miner_address>"
    ) {
        anyhow::bail!(
            "`mining.miner_address` is a placeholder in zebrad.toml. Set it to `auto` or a valid Zcash address"
        );
    }

    Ok(())
}

fn is_auto_miner_address(address: &str) -> bool {
    matches!(
        address.trim().to_ascii_lowercase().as_str(),
        "" | "auto" | "__auto__" | "__auto_miner_address__"
    )
}

fn extract_miner_address(template: &str) -> Option<String> {
    for line in template.lines() {
        let without_comment = line.split('#').next()?.trim();
        if !without_comment.starts_with("miner_address") {
            continue;
        }

        let (_, value) = without_comment.split_once('=')?;
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value)
            .trim();
        return Some(value.to_string());
    }

    None
}

// ---------------------------------------------------------------------------- #
// Local-fleet config rewriting (formerly the harness's prepare_node_dirs)
//
// The mempool-load harness runs N zakurad nodes on one host, each on a distinct
// 127.0.0.x loopback, so Kresko's one-node-per-host template (0.0.0.0 binds,
// shared /root state dir) has to be re-pointed per node. This used to live as
// ~350 lines of line-oriented TOML rewriting in mempool-load-lab.py.
//
// It is kept LINE-ORIENTED here on purpose rather than round-tripping through
// `toml::Value`: Kresko renders each config one key per line with
// `toml::to_string_pretty`, and preserving that layout keeps the rendered
// per-node config byte-identical to the former Python output — the strongest
// possible regression check for a refactor that must not change node behavior.
// ---------------------------------------------------------------------------- #

// Kresko's fixed per-host ports (see template_for). The harness varies the bind
// address rather than these, so localized configs need no port rewriting.
const LOCAL_FLEET_P2P_PORT: u16 = 18233;
const LOCAL_FLEET_RPC_PORT: u16 = 18232;
// Prometheus scrape endpoint. The mempool backpressure counters the harness
// grades on are only exposed here, not over JSON-RPC.
const LOCAL_FLEET_METRICS_PORT: u16 = 19999;
// The Zakura p2p stack's own listener: present in the config even when the
// default stack leaves it unused, and it still binds a wildcard port every node
// would otherwise contend for.
const LOCAL_FLEET_ZAKURA_P2P_PORT: u16 = 18234;

// Public seed-peer arrays emptied on an isolated local chain. Cleared by bare
// key name (not dotted path) to match every section they may appear in, exactly
// as the harness did. The loopback `initial_testnet_peers` list is preserved.
const LOCAL_FLEET_PUBLIC_PEER_KEYS: [&str; 2] = ["initial_mainnet_peers", "bootstrap_peers"];

/// One node's placement in a local fleet, addressing the harness's on-disk
/// layout so the rendered config is byte-identical to the old Python output.
#[derive(Debug, Clone)]
pub struct LocalFleetNode {
    /// Loopback address this node binds (e.g. `127.0.0.101`).
    pub ip: String,
    /// Absolute per-node directory (`<lab>/nodes/<name>`), whose subdirs hold
    /// this node's own state DB, peer cache, identity, and cookie.
    pub node_dir: std::path::PathBuf,
    /// Whether this node runs zakurad's internal miner (only the designated
    /// miners produce blocks; the rest are pure relay/mempool peers).
    pub is_miner: bool,
    /// The other nodes' `ip:p2p` addresses this node dials.
    pub peers: Vec<String>,
}

/// Rewrite one generated node config for local-fleet use.
///
/// Mirrors the harness's per-file rewrite: re-point every remote-deployment
/// absolute path and 0.0.0.0 bind at this node's own loopback and directories,
/// insert the metrics endpoint and internal-miner flag, empty the public
/// seed-peer lists, and (for the running config) set the loopback peer list.
///
/// `bootstrap` selects the P2P-disabled variant used for seeding: it keeps
/// Kresko's own `network.listen_addr` and omits the loopback peer list, matching
/// how the harness treats `zebrad.bootstrap.toml`.
pub fn localize_local_fleet_config(
    config: &str,
    node: &LocalFleetNode,
    checkpoints_path: &str,
    bootstrap: bool,
) -> Result<String> {
    let dir = |sub: &str| quote(&node.node_dir.join(sub).display().to_string());

    // Re-point every remote-deployment absolute path and shared bind at this
    // node's own directory, so N nodes never share a state DB, peer cache,
    // identity, cookie, or listener. These keys must already exist; a missing
    // one means Kresko's template drifted and is surfaced loudly.
    let mut updates: Vec<(String, String)> = vec![
        ("network.cache_dir".into(), dir("peer-cache")),
        ("network.identity_dir".into(), dir("identity")),
        ("state.cache_dir".into(), dir("state")),
        ("rpc.cookie_dir".into(), dir("cookie")),
        (
            "rpc.listen_addr".into(),
            quote(&format!("{}:{LOCAL_FLEET_RPC_PORT}", node.ip)),
        ),
        (
            "network.testnet_parameters.checkpoints".into(),
            quote(checkpoints_path),
        ),
        (
            "network.zakura.listen_addr".into(),
            quote(&format!("{}:{LOCAL_FLEET_ZAKURA_P2P_PORT}", node.ip)),
        ),
    ];
    // The bootstrap config runs P2P-disabled on an isolated RPC, so it keeps
    // Kresko's own network.listen_addr handling.
    if !bootstrap {
        updates.push((
            "network.listen_addr".into(),
            quote(&format!("{}:{LOCAL_FLEET_P2P_PORT}", node.ip)),
        ));
    }
    let mut text = set_toml_values(config, &updates, false)?;

    // Inserted rather than replaced: Kresko writes neither key.
    text = set_toml_values(
        &text,
        &[
            (
                "metrics.endpoint_addr".into(),
                quote(&format!("{}:{LOCAL_FLEET_METRICS_PORT}", node.ip)),
            ),
            (
                "mining.internal_miner".into(),
                (if node.is_miner { "true" } else { "false" }).to_string(),
            ),
        ],
        true,
    )?;

    let cleared: Vec<(&str, Vec<String>)> = LOCAL_FLEET_PUBLIC_PEER_KEYS
        .iter()
        .map(|key| (*key, Vec::new()))
        .collect();
    text = set_toml_arrays(&text, &cleared, false)?;

    // Kresko bakes the peer list at genesis time from config.json's addresses.
    // Regenerating it here from the live addressing keeps the run correct even
    // if genesis ran with a different node count or address base; a stale list
    // silently yields a 0-peer network that measures nothing.
    if !bootstrap {
        text = set_toml_arrays(
            &text,
            &[("initial_testnet_peers", node.peers.clone())],
            true,
        )?;
    }

    Ok(text)
}

fn quote(value: &str) -> String {
    format!("\"{value}\"")
}

/// Split into lines like Python's `str.splitlines()` for `\n`-delimited text:
/// a single trailing newline does not yield an empty final element. The rewrite
/// helpers rejoin with `"\n"` and re-add one trailing `\n`, so this keeps the
/// line count exact and the output byte-identical to the former Python output.
fn splitlines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

/// Set `section.key = value` entries in a rendered config, addressed by full
/// dotted path. Line-oriented so untouched lines keep their exact bytes.
///
/// Addressing is by dotted `section.key` rather than bare key because the same
/// key recurs across sections (`cache_dir` is both the peer cache and the state
/// DB; `listen_addr` appears in `[network]`, `[rpc]`, and `[network.zakura]`).
/// Missing keys are an error unless `insert_missing`, so Kresko template drift
/// fails loudly instead of leaving a node on a default binding.
pub fn set_toml_values(
    text: &str,
    updates: &[(String, String)],
    insert_missing: bool,
) -> Result<String> {
    let mut remaining: std::collections::HashMap<String, String> =
        updates.iter().cloned().collect();
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    // Where each section's body ends in `out`, so a missing key can be inserted.
    let mut section_end: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for line in splitlines(text) {
        let stripped = line.trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            section = stripped.trim_matches(|c| c == '[' || c == ']').to_string();
        } else if stripped.contains('=') && !stripped.starts_with('#') {
            let key = stripped
                .split_once('=')
                .map(|(k, _)| k.trim())
                .unwrap_or("");
            let path = if section.is_empty() {
                key.to_string()
            } else {
                format!("{section}.{key}")
            };
            if let Some(value) = remaining.remove(&path) {
                out.push(format!("{key} = {value}"));
                section_end.insert(section.clone(), out.len());
                continue;
            }
        }
        if !section.is_empty() {
            section_end.insert(section.clone(), out.len() + 1);
        }
        out.push(line.to_string());
    }

    if !remaining.is_empty() && !insert_missing {
        let mut missing: Vec<&String> = remaining.keys().collect();
        missing.sort();
        anyhow::bail!(
            "keys not found in generated config (Kresko template drift?): {}",
            missing
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Insert leftovers at the end of their section, or append the section.
    // Sorted in reverse so multiple appended sections land in a stable order,
    // matching the harness's `sorted(..., reverse=True)`.
    let mut leftovers: Vec<(String, String)> = remaining.into_iter().collect();
    leftovers.sort_by(|a, b| b.0.cmp(&a.0));
    for (path, value) in leftovers {
        let (target_section, key) = match path.rsplit_once('.') {
            Some((s, k)) => (s.to_string(), k.to_string()),
            None => (String::new(), path.clone()),
        };
        if let Some(&idx) = section_end.get(&target_section) {
            out.insert(idx, format!("{key} = {value}"));
        } else {
            out.push(String::new());
            out.push(format!("[{target_section}]"));
            out.push(format!("{key} = {value}"));
        }
    }

    Ok(out.join("\n") + "\n")
}

/// Replace whole `key = [...]` arrays, single- or multi-line, addressed by bare
/// key. Handles both forms Kresko emits, and errors for a missing key when
/// `require` — a silently absent peer list yields a 0-peer network.
pub fn set_toml_arrays(
    text: &str,
    arrays: &[(&str, Vec<String>)],
    require: bool,
) -> Result<String> {
    let lookup: std::collections::HashMap<&str, &Vec<String>> =
        arrays.iter().map(|(k, v)| (*k, v)).collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut consuming = false;

    for line in splitlines(text) {
        let stripped = line.trim();
        if consuming {
            // Drop the old entries up to the closing bracket.
            if stripped.starts_with(']') {
                consuming = false;
            }
            continue;
        }
        let key = if stripped.contains('=') {
            stripped
                .split_once('=')
                .map(|(k, _)| k.trim())
                .unwrap_or("")
        } else {
            ""
        };
        // Match against the array keys (bare key, any section) and record the
        // slice's own `&str` so `seen` shares its lifetime, not the line's.
        let matched = arrays.iter().find(|(k, _)| *k == key).map(|(k, _)| *k);
        if let Some(matched) = matched {
            let items = lookup[matched];
            seen.insert(matched);
            if items.is_empty() {
                out.push(format!("{key} = []"));
            } else {
                out.push(format!("{key} = ["));
                for item in items {
                    out.push(format!("    \"{item}\","));
                }
                out.push("]".to_string());
            }
            // A single-line `key = [...]` is fully consumed by this line.
            consuming = !stripped.trim_end().ends_with(']');
            continue;
        }
        out.push(line.to_string());
    }

    if require {
        let missing: Vec<&str> = arrays
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| !seen.contains(k))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!("array key(s) not found in generated config: {missing:?}");
        }
    }

    Ok(out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_NU6_1_LOCKBOX_ADDRESS, LocalTestnetParameters, LockboxDisbursement,
        PUBLIC_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT, PUBLIC_BLOCK_SYNC_PEER_TARGET,
        apply_local_testnet_parameters, bootstrap_config_for_isolated_rpc,
        default_nu6_1_lockbox_disbursements, ensure_miner_address_is_set, generate_node_config,
        local_genesis_upgrade_names, read_genesis_hash, read_miner_address, set_miner_address,
        strip_genesis_block_path, template_for, testnet_toml_parameters,
        verify_local_testnet_parameters,
    };
    use crate::config::{DaaConfig, Instance, NetworkKind, NodeType, Provider};

    // Known-valid testnet P2SH and P2PKH addresses borrowed from
    // zebra-chain's own test vectors.
    const P2SH_TESTNET: &str = DEFAULT_NU6_1_LOCKBOX_ADDRESS;
    const P2PKH_TESTNET: &str = "tmTc6trRhbv96kGfA99i7vrFwb5p7BVFwc3";

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

    fn parsed(config: &str) -> toml::Value {
        toml::from_str(config).expect("config should parse as TOML")
    }

    fn string_array_at<'a>(config: &'a toml::Value, path: &[&str]) -> Vec<&'a str> {
        let mut value = config;
        for key in path {
            value = value
                .get(*key)
                .unwrap_or_else(|| panic!("missing TOML key {key}"));
        }
        value
            .as_array()
            .expect("TOML value should be an array")
            .iter()
            .map(|value| value.as_str().expect("array element should be a string"))
            .collect()
    }

    #[test]
    fn accepts_auto_miner_address() {
        let config = template_for(NetworkKind::LocalGenesis).expect("template generation");
        ensure_miner_address_is_set(&config)
            .expect("default template should use auto miner address");
    }

    #[test]
    fn generated_local_template_does_not_override_daa() {
        let config = template_for(NetworkKind::LocalGenesis).expect("template generation");
        let params = testnet_toml_parameters(&config).expect("default template should parse");

        assert_eq!(params.post_blossom_pow_target_spacing, None);
        assert_eq!(params.daa.pow_averaging_window, None);
        assert_eq!(params.daa.pow_median_block_span, None);
        assert_eq!(params.daa.pow_damping_factor, None);
        assert_eq!(params.daa.pow_max_adjust_up_percent, None);
        assert_eq!(params.daa.pow_max_adjust_down_percent, None);
    }

    #[test]
    fn generated_local_template_forces_finished_sync_for_mining() {
        let config = template_for(NetworkKind::LocalGenesis).expect("template generation");
        let config = parsed(&config);

        assert_eq!(
            config
                .get("rpc")
                .and_then(|rpc| rpc.get("debug_force_finished_sync"))
                .and_then(toml::Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn generated_local_template_does_not_overlay_tracing_section() {
        // Trust ZebradConfig::default() for the [tracing] section. Kresko
        // should not enable, disable, or recolour tracing on the testnet
        // path unless an experiment explicitly asks for it.
        let default_config = parsed(
            &toml::to_string_pretty(
                &toml::Value::try_from(zebrad::config::ZakuradConfig::default())
                    .expect("zebra default should serialize"),
            )
            .expect("zebra default should re-serialize"),
        );
        let generated =
            parsed(&template_for(NetworkKind::LocalGenesis).expect("template generation"));
        let default_tracing = default_config
            .get("tracing")
            .expect("zebra default has [tracing]");
        let generated_tracing = generated
            .get("tracing")
            .expect("generated template has [tracing]");
        assert_eq!(
            default_tracing, generated_tracing,
            "kresko must not overlay zebra's default [tracing] section",
        );
    }

    #[test]
    fn generated_local_template_enables_mempool_for_mining() {
        let config = template_for(NetworkKind::LocalGenesis).expect("template generation");
        let config = parsed(&config);

        assert_eq!(
            config
                .get("mempool")
                .and_then(|mempool| mempool.get("debug_enable_at_height"))
                .and_then(toml::Value::as_integer),
            Some(0),
        );
    }

    #[test]
    fn accepts_non_auto_miner_address() {
        let config = set_miner_address(
            &template_for(NetworkKind::LocalGenesis).expect("template generation"),
            "tmFakeAddress",
        )
        .expect("set miner address");
        ensure_miner_address_is_set(&config).expect("non-empty miner address should pass");
    }

    #[test]
    fn replaces_peers_for_each_node() {
        let config = set_miner_address(
            &template_for(NetworkKind::LocalGenesis).expect("template generation"),
            "tmFakeAddress",
        )
        .expect("set miner address");
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
            miner("miner-2-ghi", "TBD"),
        ];

        let generated =
            generate_node_config(&config, NetworkKind::LocalGenesis, &miners[0], &miners)
                .expect("config generation");
        let generated = parsed(&generated);
        assert_eq!(
            generated
                .get("network")
                .and_then(|network| network.get("peerset_initial_target_size"))
                .and_then(toml::Value::as_integer),
            Some(3)
        );
        assert_eq!(
            string_array_at(&generated, &["network", "initial_testnet_peers"]),
            vec!["2.2.2.2:18233"]
        );
    }

    #[test]
    fn public_testnet_preserves_seeders_and_adds_fleet_peers() {
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
        ];

        let generated = generate_node_config(
            &template_for(NetworkKind::PublicTestnet).expect("template generation"),
            NetworkKind::PublicTestnet,
            &miners[0],
            &miners,
        )
        .expect("config generation");

        let generated = parsed(&generated);
        assert_eq!(
            string_array_at(&generated, &["network", "initial_testnet_peers"]),
            vec![
                "dnsseed.testnet.z.cash:18233",
                "testnet.seeder.zfnd.org:18233",
                "2.2.2.2:18233"
            ]
        );
        assert_eq!(
            generated
                .get("network")
                .and_then(|network| network.get("external_addr"))
                .and_then(toml::Value::as_str),
            Some("1.1.1.1:18233")
        );
        assert!(
            generated
                .get("network")
                .and_then(|network| network.get("testnet_parameters"))
                .is_none()
        );
    }

    #[test]
    fn mainnet_preserves_seeders_and_adds_external_addr() {
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
        ];

        let generated = generate_node_config(
            &template_for(NetworkKind::Mainnet).expect("template generation"),
            NetworkKind::Mainnet,
            &miners[0],
            &miners,
        )
        .expect("config generation");

        let generated = parsed(&generated);
        let peers = string_array_at(&generated, &["network", "initial_mainnet_peers"]);
        assert!(peers.contains(&"dnsseed.str4d.xyz:8233"));
        assert!(peers.contains(&"2.2.2.2:8233"));
        assert_eq!(
            generated
                .get("network")
                .and_then(|network| network.get("external_addr"))
                .and_then(toml::Value::as_str),
            Some("1.1.1.1:8233")
        );
    }

    #[test]
    fn public_network_templates_use_fast_block_sync_settings() {
        for network_kind in [NetworkKind::PublicTestnet, NetworkKind::Mainnet] {
            let generated = parsed(&template_for(network_kind).expect("template generation"));
            assert_eq!(
                generated
                    .get("network")
                    .and_then(|network| network.get("peerset_initial_target_size"))
                    .and_then(toml::Value::as_integer),
                Some(PUBLIC_BLOCK_SYNC_PEER_TARGET),
                "{network_kind} should target enough peers for fast public block sync",
            );
            assert_eq!(
                generated
                    .get("sync")
                    .and_then(|sync| sync.get("download_concurrency_limit"))
                    .and_then(toml::Value::as_integer),
                Some(PUBLIC_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT),
                "{network_kind} should allow matching concurrent block downloads",
            );
        }
    }

    #[test]
    fn rejects_placeholder_miner_address() {
        let config = set_miner_address(
            &template_for(NetworkKind::LocalGenesis).expect("template generation"),
            "todo",
        )
        .expect("set miner address");
        let err = ensure_miner_address_is_set(&config)
            .expect_err("placeholder values should fail validation");
        assert!(err.to_string().contains("mining.miner_address"));
    }

    #[test]
    fn sets_miner_address() {
        let generated = set_miner_address(
            &template_for(NetworkKind::LocalGenesis).expect("template generation"),
            "tmTestAddress",
        )
        .expect("set miner address");
        assert!(generated.contains("miner_address = \"tmTestAddress\""));
    }

    #[test]
    fn bootstrap_config_isolates_rpc_from_fleet() {
        let template =
            template_for(NetworkKind::PublicTestnet).expect("public testnet template generation");
        let bootstrap =
            bootstrap_config_for_isolated_rpc(&template).expect("bootstrap config generation");
        let parsed = parsed(&bootstrap);
        assert_eq!(
            parsed
                .get("network")
                .and_then(|n| n.get("listen_addr"))
                .and_then(toml::Value::as_str),
            Some("127.0.0.1:0"),
        );
        assert!(
            parsed
                .get("network")
                .and_then(|n| n.get("initial_testnet_peers"))
                .and_then(toml::Value::as_array)
                .map(Vec::is_empty)
                .unwrap_or(false),
            "initial_testnet_peers must be empty in bootstrap config",
        );
        assert!(
            parsed
                .get("network")
                .and_then(|n| n.get("initial_mainnet_peers"))
                .and_then(toml::Value::as_array)
                .map(Vec::is_empty)
                .unwrap_or(false),
            "initial_mainnet_peers must be empty in bootstrap config",
        );
    }

    #[test]
    fn bootstrap_config_handles_multiline_peer_arrays() {
        // A previous shell-based bootstrap path crashed on multiline peer
        // arrays because it sed-deleted only the first line of the array.
        // The TOML-aware path must round-trip a multiline array cleanly.
        let multiline = r#"[network]
network = "Testnet"
listen_addr = "0.0.0.0:18233"
initial_testnet_peers = [
    "1.1.1.1:18233",
    "2.2.2.2:18233",
    "3.3.3.3:18233",
]
initial_mainnet_peers = []
"#;
        let bootstrap = bootstrap_config_for_isolated_rpc(multiline)
            .expect("bootstrap from multiline-peer config");
        let parsed = parsed(&bootstrap);
        assert_eq!(
            parsed
                .get("network")
                .and_then(|n| n.get("initial_testnet_peers"))
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(0),
            "multiline peer array must be cleared in bootstrap, not partially edited",
        );
        assert_eq!(
            parsed
                .get("network")
                .and_then(|n| n.get("listen_addr"))
                .and_then(toml::Value::as_str),
            Some("127.0.0.1:0"),
        );
    }

    #[test]
    fn strip_genesis_block_path_is_idempotent() {
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let mut config = template.clone();
        config.push_str("[network.testnet_parameters]\ngenesis_block_path = \"/tmp/genesis\"\n");
        let stripped = strip_genesis_block_path(&config).expect("strip");
        assert!(!stripped.contains("genesis_block_path"));
        // Idempotent: stripping again is a no-op.
        let again = strip_genesis_block_path(&stripped).expect("strip again");
        assert!(!again.contains("genesis_block_path"));
    }

    #[test]
    fn read_miner_address_and_genesis_hash_round_trip_through_toml() {
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let with_address =
            set_miner_address(&template, "tmExampleAddress").expect("set miner address");
        assert_eq!(
            read_miner_address(&with_address).expect("read miner address"),
            Some("tmExampleAddress".to_string()),
        );
        let params = LocalTestnetParameters {
            latest_upgrade: "NU7".to_string(),
            network_name: "ReadTestNet".to_string(),
            network_magic: [1, 2, 3, 4],
            target_difficulty_limit: "0x0f".to_string(),
            disable_pow: false,
            genesis_hash: "AB".repeat(32),
            checkpoints_path: "/tmp/checkpoints".to_string(),
            slow_start_interval: 0,
            pre_blossom_halving_interval: 144,
            activation_height: 1,
            lockbox_disbursements: Vec::new(),
            post_blossom_pow_target_spacing: None,
            daa: DaaConfig::default(),
            pow_start_height: None,
        };
        let with_params = apply_local_testnet_parameters(&with_address, &params)
            .expect("apply local testnet parameters");
        // Lowercased so the equality check matches `expected_genesis_hash`
        // computed by node_init.sh's RPC response normalisation.
        assert_eq!(
            read_genesis_hash(&with_params).expect("read genesis hash"),
            Some("ab".repeat(32)),
        );
    }

    #[test]
    fn template_is_built_from_zebrad_default_config() {
        // Trust upstream Zebra to define what fields a config has — kresko
        // only ever overlays a few mining-friendly knobs.
        let default_serialized = toml::to_string_pretty(
            &toml::Value::try_from(zebrad::config::ZakuradConfig::default())
                .expect("zebra default should serialize"),
        )
        .expect("zebra default should re-serialize");
        let default_config = parsed(&default_serialized);
        let generated =
            parsed(&template_for(NetworkKind::LocalGenesis).expect("template generation"));
        // Every section that ZebradConfig::default() declares must still
        // exist in the generated template, so we never drop sections by
        // accident.
        let default_table = default_config
            .as_table()
            .expect("zebra default is a TOML table");
        for (section, _) in default_table {
            assert!(
                generated.get(section).is_some(),
                "kresko template dropped Zebra default section [{section}]",
            );
        }
    }

    #[test]
    fn local_testnet_preserves_required_fields() {
        // Required testnet identity (network_name, magic, genesis hash,
        // target limit, checkpoints, activation heights) must round-trip
        // through apply_local_testnet_parameters intact.
        let params = LocalTestnetParameters {
            latest_upgrade: "NU7".to_string(),
            network_name: "RequiredFieldsNet".to_string(),
            network_magic: [9, 8, 7, 6],
            target_difficulty_limit: "0x1f".to_string(),
            disable_pow: false,
            genesis_hash: "11".repeat(32),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: 0,
            pre_blossom_halving_interval: 144,
            activation_height: 5,
            lockbox_disbursements: Vec::new(),
            post_blossom_pow_target_spacing: None,
            daa: DaaConfig::default(),
            pow_start_height: None,
        };
        let template = template_for(NetworkKind::LocalGenesis).expect("template generation");
        let generated = apply_local_testnet_parameters(&template, &params)
            .expect("apply_local_testnet_parameters");
        let parsed = parsed(&generated);
        let testnet_params = parsed
            .get("network")
            .and_then(|n| n.get("testnet_parameters"))
            .expect("testnet_parameters present");
        assert_eq!(
            testnet_params
                .get("network_name")
                .and_then(toml::Value::as_str),
            Some("RequiredFieldsNet"),
        );
        assert_eq!(
            testnet_params
                .get("target_difficulty_limit")
                .and_then(toml::Value::as_str),
            Some("0x1f"),
        );
        assert_eq!(
            testnet_params
                .get("genesis_hash")
                .and_then(toml::Value::as_str),
            Some("11".repeat(32).as_str()),
        );
        assert_eq!(
            testnet_params
                .get("checkpoints")
                .and_then(toml::Value::as_str),
            Some("/root/payload/local_genesis/checkpoints.txt"),
        );
        let magic_bytes: Vec<i64> = testnet_params
            .get("network_magic")
            .and_then(toml::Value::as_array)
            .expect("network_magic")
            .iter()
            .map(|v| v.as_integer().expect("byte"))
            .collect();
        assert_eq!(magic_bytes, vec![9, 8, 7, 6]);
        // Activation heights table must exist and cover Overwinter..NU7,
        // including the explicit NU6.1 lockbox event.
        let activation = testnet_params
            .get("activation_heights")
            .and_then(toml::Value::as_table)
            .expect("activation_heights");
        for upgrade in [
            "Overwinter",
            "Sapling",
            "Blossom",
            "Heartwood",
            "Canopy",
            "NU5",
            "NU6",
            "NU6.1",
            "NU7",
        ] {
            assert!(
                activation.contains_key(upgrade),
                "activation height {upgrade} must be set",
            );
        }
    }

    fn local_testnet_params_with(
        genesis_hash: &str,
        activation_height: u32,
        lockbox: Vec<LockboxDisbursement>,
    ) -> LocalTestnetParameters {
        LocalTestnetParameters {
            latest_upgrade: "NU7".to_string(),
            network_name: "CrossValTestNet".to_string(),
            network_magic: [4, 3, 2, 1],
            target_difficulty_limit: "0x0f".to_string(),
            disable_pow: false,
            genesis_hash: genesis_hash.to_string(),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: 0,
            pre_blossom_halving_interval: 144,
            activation_height,
            lockbox_disbursements: lockbox,
            post_blossom_pow_target_spacing: None,
            daa: DaaConfig::default(),
            pow_start_height: None,
        }
    }

    #[test]
    fn lockbox_disbursement_constructor_rejects_p2pkh() {
        // P2PKH crashes zebrad at NU6.1 activation, so the constructor must
        // refuse to build one.
        let err = LockboxDisbursement::new_p2sh(P2PKH_TESTNET, 0)
            .expect_err("P2PKH must not be accepted as a disbursement address");
        assert!(err.to_string().contains("P2PKH"), "{err}");
    }

    #[test]
    fn lockbox_disbursement_constructor_accepts_p2sh() {
        LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0)
            .expect("P2SH testnet address must be accepted");
    }

    #[test]
    fn default_nu6_1_lockbox_disbursement_is_zero_zat_p2sh() {
        let entries = default_nu6_1_lockbox_disbursements()
            .expect("default NU6.1 lockbox disbursement must be valid");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, P2SH_TESTNET);
        assert_eq!(entries[0].amount_zats, 0);
    }

    #[test]
    fn apply_local_testnet_parameters_emits_lockbox_disbursements_as_toml_array_of_tables() {
        let params = local_testnet_params_with(
            &"aa".repeat(32),
            5,
            vec![LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0).unwrap()],
        );
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let rendered = apply_local_testnet_parameters(&template, &params)
            .expect("apply with one P2SH disbursement");
        let parsed = parsed(&rendered);
        let entries = parsed
            .get("network")
            .and_then(|n| n.get("testnet_parameters"))
            .and_then(|tp| tp.get("lockbox_disbursements"))
            .and_then(toml::Value::as_array)
            .expect("lockbox_disbursements rendered as array");
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_table().expect("entry is a table");
        assert_eq!(
            entry.get("address").and_then(toml::Value::as_str),
            Some(P2SH_TESTNET)
        );
        assert_eq!(
            entry.get("amount").and_then(toml::Value::as_integer),
            Some(0)
        );
    }

    #[test]
    fn apply_local_testnet_parameters_rejects_p2pkh_lockbox() {
        // Belt-and-braces: even if a caller sneaks a P2PKH entry past the
        // constructor (e.g. via a serde round-trip), the renderer must
        // reject it.
        let mut params = local_testnet_params_with(&"aa".repeat(32), 5, Vec::new());
        params.lockbox_disbursements.push(LockboxDisbursement {
            address: P2PKH_TESTNET.to_string(),
            amount_zats: 0,
        });
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let err = apply_local_testnet_parameters(&template, &params)
            .expect_err("rendering must fail on a P2PKH lockbox entry");
        assert!(err.to_string().contains("P2SH validation"), "{err}");
    }

    #[test]
    fn verify_detects_genesis_hash_drift_between_chain_and_config() {
        let chain_hash = "11".repeat(32);
        let drifted_hash = "22".repeat(32);
        let params = local_testnet_params_with(&chain_hash, 5, Vec::new());
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        // Render the config with the *drifted* hash, simulating a config
        // that no longer matches the chain on disk.
        let mut drifted_params = params.clone();
        drifted_params.genesis_hash = drifted_hash;
        let rendered =
            apply_local_testnet_parameters(&template, &drifted_params).expect("apply parameters");
        let err = verify_local_testnet_parameters(&rendered, &params)
            .expect_err("verify must catch the drift");
        assert!(err.to_string().contains("genesis_hash mismatch"), "{err}");
    }

    #[test]
    fn verify_detects_activation_height_drift_between_chain_and_config() {
        let params = local_testnet_params_with(&"aa".repeat(32), 5, Vec::new());
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let mut drifted = params.clone();
        drifted.activation_height = 12;
        let rendered =
            apply_local_testnet_parameters(&template, &drifted).expect("apply parameters");
        let err = verify_local_testnet_parameters(&rendered, &params)
            .expect_err("verify must catch the activation drift");
        assert!(err.to_string().contains("activation height"), "{err}");
    }

    #[test]
    fn activation_table_stops_at_the_configured_upgrade() {
        assert_eq!(
            local_genesis_upgrade_names("NU6.1").last(),
            Some(&"NU6.1"),
            "nothing above the configured upgrade may be declared"
        );
        assert!(!local_genesis_upgrade_names("NU6.1").contains(&"NU6.2"));
        assert!(!local_genesis_upgrade_names("NU6.1").contains(&"NU7"));
    }

    #[test]
    fn nu6_3_activates_everything_below_it() {
        // NU6.3 gates the Ironwood shielded pool, so a chain that stops short
        // of it cannot exercise Ironwood at all.
        let names = local_genesis_upgrade_names("NU6.3");
        for expected in ["Overwinter", "NU5", "NU6", "NU6.1", "NU6.2", "NU6.3"] {
            assert!(names.contains(&expected), "{expected} must be active");
        }
        assert!(!names.contains(&"NU7"));
    }

    #[test]
    fn nu6_3_survives_the_writer_and_the_verifier() {
        let mut params = local_testnet_params_with(
            &"aa".repeat(32),
            5,
            vec![LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0).unwrap()],
        );
        params.latest_upgrade = "NU6.3".to_string();
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let rendered =
            apply_local_testnet_parameters(&template, &params).expect("apply parameters");
        assert!(rendered.contains("NU6.3"));
        assert!(!rendered.contains("NU7"));
        verify_local_testnet_parameters(&rendered, &params)
            .expect("an Ironwood-capable chain must pass cross-validation");
    }

    #[test]
    fn writer_and_verifier_agree_when_the_chain_omits_nu7() {
        // The writer and verifier each used to carry their own hardcoded
        // upgrade list. When NU7 became optional only the writer was updated,
        // so a chain without NU7 rendered a config the verifier then rejected
        // with "missing activation height for NU7". Both now derive the list
        // from local_genesis_upgrade_names().
        let mut params = local_testnet_params_with(
            &"aa".repeat(32),
            5,
            vec![LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0).unwrap()],
        );
        params.latest_upgrade = "NU6.1".to_string();
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let rendered =
            apply_local_testnet_parameters(&template, &params).expect("apply parameters");
        assert!(
            !rendered.contains("NU7"),
            "a chain that does not activate NU7 must not declare it"
        );
        verify_local_testnet_parameters(&rendered, &params)
            .expect("writer output must satisfy the verifier when NU7 is omitted");
    }

    #[test]
    fn nu7_is_declared_when_the_chain_activates_it() {
        let params = local_testnet_params_with(
            &"aa".repeat(32),
            5,
            vec![LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0).unwrap()],
        );
        assert_eq!(params.latest_upgrade, "NU7");
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let rendered =
            apply_local_testnet_parameters(&template, &params).expect("apply parameters");
        assert!(rendered.contains("NU7"));
        verify_local_testnet_parameters(&rendered, &params).expect("cross-validation");
    }

    #[test]
    fn verify_passes_when_chain_and_config_agree() {
        let params = local_testnet_params_with(
            &"aa".repeat(32),
            5,
            vec![LockboxDisbursement::new_p2sh(P2SH_TESTNET, 0).unwrap()],
        );
        let template = template_for(NetworkKind::LocalGenesis).expect("template");
        let rendered =
            apply_local_testnet_parameters(&template, &params).expect("apply parameters");
        verify_local_testnet_parameters(&rendered, &params)
            .expect("matching chain + config must pass cross-validation");
    }

    #[test]
    fn injects_local_testnet_parameters() {
        let params = LocalTestnetParameters {
            latest_upgrade: "NU7".to_string(),
            network_name: "LocalGenesisNet".to_string(),
            network_magic: [1, 2, 3, 4],
            target_difficulty_limit: "0x0f".to_string(),
            disable_pow: true,
            genesis_hash: "00".repeat(32),
            checkpoints_path: "/root/payload/local_genesis/checkpoints.txt".to_string(),
            slow_start_interval: 0,
            pre_blossom_halving_interval: 144,
            activation_height: 1,
            lockbox_disbursements: Vec::new(),
            post_blossom_pow_target_spacing: Some(25),
            daa: DaaConfig {
                pow_averaging_window: Some(8),
                pow_median_block_span: Some(6),
                pre_blossom_pow_target_spacing: Some(50),
                pow_damping_factor: Some(3),
                pow_max_adjust_up_percent: Some(20),
                pow_max_adjust_down_percent: Some(40),
                testnet_min_difficulty_start_height: Some(100),
                testnet_min_difficulty_gap_multiplier: Some(4),
            },
            pow_start_height: Some(257),
        };

        let template = template_for(NetworkKind::LocalGenesis).expect("template generation");
        let generated =
            apply_local_testnet_parameters(&template, &params).expect("set testnet parameters");
        assert!(generated.contains("[network.testnet_parameters]"));
        assert!(generated.contains("network_name = \"LocalGenesisNet\""));
        assert!(!generated.contains("pow_start_height"));
        assert!(!generated.contains("post_blossom_pow_target_spacing"));
        assert!(!generated.contains("equihash_params"));
        assert!(!generated.contains("pow_averaging_window"));
        assert!(!generated.contains("pow_median_block_span"));
        assert!(!generated.contains("pre_blossom_pow_target_spacing"));
        assert!(!generated.contains("pow_damping_factor"));
        assert!(!generated.contains("pow_max_adjust_up_percent"));
        assert!(!generated.contains("pow_max_adjust_down_percent"));
        assert!(!generated.contains("testnet_min_difficulty_start_height"));
        assert!(!generated.contains("testnet_min_difficulty_gap_multiplier"));
        assert!(!generated.contains("genesis_block_path"));
        assert!(
            generated.contains("checkpoints = \"/root/payload/local_genesis/checkpoints.txt\"")
        );
        let parsed = parsed(&generated);
        let testnet_params = parsed
            .get("network")
            .and_then(|network| network.get("testnet_parameters"))
            .expect("testnet parameters");
        assert!(
            testnet_params
                .get("pre_nu6_funding_streams")
                .and_then(|streams| streams.get("recipients"))
                .and_then(toml::Value::as_array)
                .is_some_and(Vec::is_empty)
        );
        assert!(
            testnet_params
                .get("post_nu6_funding_streams")
                .and_then(|streams| streams.get("recipients"))
                .and_then(toml::Value::as_array)
                .is_some_and(Vec::is_empty)
        );
        let activation = testnet_params
            .get("activation_heights")
            .and_then(toml::Value::as_table)
            .expect("activation_heights");
        assert_eq!(
            activation.get("NU6").and_then(toml::Value::as_integer),
            Some(1)
        );
        assert_eq!(
            activation.get("NU6.1").and_then(toml::Value::as_integer),
            Some(1)
        );
        assert_eq!(
            activation.get("NU7").and_then(toml::Value::as_integer),
            Some(1)
        );
        verify_local_testnet_parameters(&generated, &params)
            .expect("rendered config should preserve local testnet parameters");
    }
}

#[cfg(test)]
mod local_fleet_tests {
    use super::{
        LOCAL_FLEET_METRICS_PORT, LOCAL_FLEET_P2P_PORT, LOCAL_FLEET_RPC_PORT,
        LOCAL_FLEET_ZAKURA_P2P_PORT, LocalFleetNode, localize_local_fleet_config, set_toml_arrays,
        set_toml_values,
    };
    use std::path::PathBuf;

    // A trimmed copy of what `kresko genesis` renders, keeping the structural
    // features that broke a naive rewriter in the harness's first real run:
    //   - `cache_dir` in both [network] (peer cache) and [state] (RocksDB)
    //   - `listen_addr` in [network], [rpc], and [network.zakura]
    //   - no internal_miner and no [metrics] section at all
    //   - multi-line public seed-peer arrays
    // Kept identical to the fixture in the harness's test_mempool_load.py so the
    // ported Rust rewrite is checked against the same properties.
    const GENERATED_CONFIG: &str = r#"[mempool]
debug_enable_at_height = 0

[mining]
miner_address = "tmExampleAddress"

[network]
cache_dir = "/root/.cache/zebra-peers"
identity_dir = "/root/.zakura"
initial_mainnet_peers = [
    "dnsseed.z.cash:8233",
    "mainnet.seeder.zfnd.org:8233",
]
initial_testnet_peers = [
    "127.0.0.1:18233",
    "127.0.0.3:18233",
]
listen_addr = "0.0.0.0:18233"
network = "Testnet"
p2p_stack = "default"

[network.testnet_parameters]
checkpoints = "/root/payload/local_genesis/checkpoints.txt"
disable_pow = true

[network.zakura]
bootstrap_peers = [
    "abc@165.22.54.66:8234",
    "def@104.131.184.123:8234",
]
listen_addr = "0.0.0.0:8234"

[rpc]
cookie_dir = "/root/.cache/zakura"
enable_cookie_auth = false
listen_addr = "0.0.0.0:18232"

[state]
cache_dir = "/root/.cache/zebra"
ephemeral = false
"#;

    fn node_ip(index: usize) -> String {
        format!("127.0.0.{}", 101 + index)
    }

    /// Body of `[name]`, up to the next section header.
    fn section_of(text: &str, name: &str) -> String {
        let body = text
            .split_once(&format!("[{name}]\n"))
            .unwrap_or_else(|| panic!("section [{name}] not found"))
            .1;
        let mut lines = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') {
                break;
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    fn values(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn localize_running(index: usize, miner: bool, peers: &[&str]) -> String {
        let node = LocalFleetNode {
            ip: node_ip(index),
            node_dir: PathBuf::from(format!("/lab/miner-{index}")),
            is_miner: miner,
            peers: peers.iter().map(|p| p.to_string()).collect(),
        };
        localize_local_fleet_config(GENERATED_CONFIG, &node, "/lab/checkpoints.txt", false)
            .expect("localize running config")
    }

    #[test]
    fn same_key_in_two_sections_is_addressed_independently() {
        // `cache_dir` exists in both [network] and [state]; a bare-key rewrite
        // would hit only the first, leaving every node sharing one RocksDB.
        let out = set_toml_values(
            GENERATED_CONFIG,
            &values(&[
                ("network.cache_dir", "\"/lab/peers\""),
                ("state.cache_dir", "\"/lab/state\""),
            ]),
            false,
        )
        .unwrap();
        assert!(section_of(&out, "network").contains("cache_dir = \"/lab/peers\""));
        assert!(section_of(&out, "state").contains("cache_dir = \"/lab/state\""));
        assert!(!out.contains("/root/.cache/zebra"));
    }

    #[test]
    fn listen_addr_is_set_per_section() {
        let out = localize_running(1, false, &[]);
        assert!(section_of(&out, "network").contains(&format!(
            "listen_addr = \"{}:{LOCAL_FLEET_P2P_PORT}\"",
            node_ip(1)
        )));
        assert!(section_of(&out, "rpc").contains(&format!(
            "listen_addr = \"{}:{LOCAL_FLEET_RPC_PORT}\"",
            node_ip(1)
        )));
        assert!(section_of(&out, "network.zakura").contains(&format!(
            "listen_addr = \"{}:{LOCAL_FLEET_ZAKURA_P2P_PORT}\"",
            node_ip(1)
        )));
    }

    #[test]
    fn missing_key_is_a_loud_failure() {
        // Template drift must fail rather than silently leave a default binding.
        let err = set_toml_values(
            GENERATED_CONFIG,
            &values(&[("state.nonexistent", "1")]),
            false,
        );
        assert!(err.is_err());
    }

    #[test]
    fn wrong_section_is_also_a_loud_failure() {
        // cache_dir exists, but not in [rpc]; this must not silently no-op.
        let err = set_toml_values(
            GENERATED_CONFIG,
            &values(&[("rpc.cache_dir", "\"/x\"")]),
            false,
        );
        assert!(err.is_err());
    }

    #[test]
    fn dotted_subsection_keys_are_addressable() {
        let out = set_toml_values(
            GENERATED_CONFIG,
            &values(&[(
                "network.testnet_parameters.checkpoints",
                "\"/lab/checkpoints.txt\"",
            )]),
            false,
        )
        .unwrap();
        assert!(out.contains("checkpoints = \"/lab/checkpoints.txt\""));
        assert!(!out.contains("/root/payload"));
    }

    #[test]
    fn internal_miner_is_inserted_into_the_mining_section() {
        let out = localize_running(0, true, &[]);
        assert!(section_of(&out, "mining").contains("internal_miner = true"));
    }

    #[test]
    fn relay_nodes_do_not_mine() {
        let out = localize_running(0, false, &[]);
        assert!(section_of(&out, "mining").contains("internal_miner = false"));
    }

    #[test]
    fn metrics_section_is_created_when_absent() {
        // Kresko emits no [metrics] section, but the backpressure counters the
        // harness grades are Prometheus-only.
        assert!(!GENERATED_CONFIG.contains("[metrics]"));
        let out = localize_running(1, false, &[]);
        assert!(out.contains("[metrics]"));
        assert!(section_of(&out, "metrics").contains(&format!(
            "endpoint_addr = \"{}:{LOCAL_FLEET_METRICS_PORT}\"",
            node_ip(1)
        )));
    }

    #[test]
    fn insertion_is_idempotent() {
        let once = localize_running(1, false, &[]);
        let twice =
            set_toml_values(&once, &values(&[("mining.internal_miner", "false")]), true).unwrap();
        assert_eq!(twice.matches("internal_miner").count(), 1);
        assert_eq!(twice.matches("[metrics]").count(), 1);
    }

    #[test]
    fn public_seed_peers_are_emptied() {
        let out = localize_running(1, false, &["127.0.0.101:18233"]);
        assert!(out.contains("initial_mainnet_peers = []"));
        assert!(out.contains("bootstrap_peers = []"));
        for host in [
            "dnsseed.z.cash",
            "zfnd.org",
            "165.22.54.66",
            "104.131.184.123",
        ] {
            assert!(!out.contains(host), "public host {host} survived");
        }
    }

    #[test]
    fn clearing_public_peers_preserves_the_loopback_list() {
        // Emptying the public arrays must not disturb initial_testnet_peers.
        let out = set_toml_arrays(
            GENERATED_CONFIG,
            &[
                ("initial_mainnet_peers", Vec::new()),
                ("bootstrap_peers", Vec::new()),
            ],
            false,
        )
        .unwrap();
        assert!(out.contains("127.0.0.1:18233"));
        assert!(out.contains("127.0.0.3:18233"));
    }

    #[test]
    fn peer_list_is_regenerated_from_live_addressing() {
        // genesis baked 127.0.0.1/.3 into the list; the fleet moved to
        // 127.0.0.101+, so the running config must carry the live peers and
        // drop the stale ones (a stale list silently measures nothing).
        let out = localize_running(0, true, &["127.0.0.102:18233", "127.0.0.103:18233"]);
        assert!(out.contains("\"127.0.0.102:18233\""));
        assert!(out.contains("\"127.0.0.103:18233\""));
        assert!(!out.contains("127.0.0.3:18233"));
    }

    #[test]
    fn peer_list_reflects_exactly_the_supplied_peers() {
        // The command excludes self when building `peers`; localization writes
        // that list verbatim into initial_testnet_peers. Scope the check to the
        // array — the node's own bind (127.0.0.102) legitimately appears in its
        // listen_addr lines, so a whole-file check would be meaningless here.
        let out = localize_running(1, true, &["127.0.0.101:18233", "127.0.0.103:18233"]);
        let array = out
            .split_once("initial_testnet_peers = [")
            .expect("peer list present")
            .1
            .split_once(']')
            .expect("peer list closes")
            .0;
        assert!(array.contains("\"127.0.0.101:18233\""));
        assert!(array.contains("\"127.0.0.103:18233\""));
        assert!(
            !array.contains("127.0.0.102"),
            "self must not appear in the peer list"
        );
    }

    #[test]
    fn missing_peer_key_is_a_loud_failure() {
        let err = set_toml_arrays(
            "[network]\nlisten_addr = \"x\"\n",
            &[("initial_testnet_peers", vec!["127.0.0.1:18233".to_string()])],
            true,
        );
        assert!(err.is_err());
    }

    #[test]
    fn fully_rewritten_config_has_no_wildcard_binds_or_shared_paths() {
        let out = localize_running(1, false, &["127.0.0.101:18233"]);
        // A leftover 0.0.0.0 bind means two nodes collide on one port.
        assert!(!out.contains("0.0.0.0"), "wildcard bind survived");
        // A leftover /root path means nodes share state or read a droplet-only path.
        assert!(!out.contains("/root/"), "shared /root path survived");
    }

    #[test]
    fn bootstrap_config_keeps_kresko_listen_addr_and_no_peer_list() {
        // The bootstrap variant runs P2P-disabled: it must not get a fleet peer
        // list and keeps Kresko's own network.listen_addr handling.
        let node = LocalFleetNode {
            ip: node_ip(0),
            node_dir: PathBuf::from("/lab/miner-0"),
            is_miner: true,
            peers: vec!["127.0.0.102:18233".to_string()],
        };
        let out =
            localize_local_fleet_config(GENERATED_CONFIG, &node, "/lab/checkpoints.txt", true)
                .unwrap();
        // network.listen_addr is untouched by the bootstrap path.
        assert!(section_of(&out, "network").contains("listen_addr = \"0.0.0.0:18233\""));
        // No live fleet peer was injected.
        assert!(!out.contains("127.0.0.102:18233"));
        // But rpc/dirs/metrics are still localized.
        assert!(section_of(&out, "rpc").contains(&format!(
            "listen_addr = \"{}:{LOCAL_FLEET_RPC_PORT}\"",
            node_ip(0)
        )));
        assert!(out.contains("[metrics]"));
    }

    #[test]
    fn trailing_newline_is_preserved_exactly_once() {
        // Byte-identity depends on not gaining or losing the final newline.
        let out = set_toml_values(
            GENERATED_CONFIG,
            &values(&[("state.ephemeral", "true")]),
            false,
        )
        .unwrap();
        assert!(out.ends_with("\n"));
        assert!(!out.ends_with("\n\n"));
    }
}
