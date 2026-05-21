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
        }
    }

    toml::to_string_pretty(&config).context("failed to serialize Zebra-generated config")
}

fn zebra_default_config_value() -> Result<toml::Value> {
    toml::Value::try_from(zebrad::config::ZebradConfig::default())
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_NU6_1_LOCKBOX_ADDRESS, LocalTestnetParameters, LockboxDisbursement,
        apply_local_testnet_parameters, bootstrap_config_for_isolated_rpc,
        default_nu6_1_lockbox_disbursements, ensure_miner_address_is_set, generate_node_config,
        read_genesis_hash, read_miner_address, set_miner_address, strip_genesis_block_path,
        template_for, testnet_toml_parameters, verify_local_testnet_parameters,
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
                &toml::Value::try_from(zebrad::config::ZebradConfig::default())
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
            &toml::Value::try_from(zebrad::config::ZebradConfig::default())
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
