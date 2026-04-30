use anyhow::{Context, Result};

use crate::config::Instance;
use crate::config::{DaaConfig, NetworkKind};
use zebra_chain::parameters::EquihashParams;

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
    /// Custom post-Blossom spacing for experiment networks. If omitted, Zebra
    /// falls back to its public Testnet/Mainnet default of 75 seconds.
    pub post_blossom_pow_target_spacing: Option<u32>,
    /// Equihash parameter set used by live PoW solving and validation.
    pub equihash_params: EquihashParams,
    /// Optional difficulty adjustment parameters for local experiment networks.
    pub daa: DaaConfig,
    /// If `Some(h)`, live nodes skip Equihash and difficulty checks for
    /// blocks below height `h`. Set to one past the seeded tip so the
    /// cached pre-mined blocks pass validation but every live-mined block
    /// must solve PoW.
    pub pow_start_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestnetTomlParameters {
    pub post_blossom_pow_target_spacing: Option<u32>,
    pub daa: DaaConfig,
}

/// Default zebrad.toml template.
pub const DEFAULT_ZEBRAD_TOML: &str = r#"[consensus]
checkpoint_sync = true

[mempool]
eviction_memory_time = "1h"
tx_cost_limit = 80000000
debug_enable_at_height = 0

[mining]
# Use "auto" to generate a wallet-owned miner address on each node at startup.
miner_address = "auto"

[network]
network = "Testnet"
listen_addr = "0.0.0.0:18233"
# Zebra's default target initial peer set size. Increase or decrease this in
# an initialized experiment's zebrad.toml before running `kresko genesis`.
peerset_initial_target_size = 25
initial_testnet_peers = []

[network.testnet_parameters]
target_difficulty_limit = "0x04ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec4ec"
post_blossom_pow_target_spacing = 25
pow_averaging_window = 51
pow_median_block_span = 33
pow_damping_factor = 4
pow_max_adjust_up_percent = 16
pow_max_adjust_down_percent = 32

[network.testnet_parameters.activation_heights]
Overwinter = 1
Sapling = 1
Blossom = 1
Heartwood = 1
Canopy = 1
NU5 = 1
NU6 = 1
"NU6.1" = 1

[state]
cache_dir = "/root/.cache/zebra"

[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false

[tracing]
use_color = false
"#;

pub const PUBLIC_TESTNET_ZEBRAD_TOML: &str = r#"[consensus]
checkpoint_sync = true

[mempool]
eviction_memory_time = "1h"
tx_cost_limit = 80000000
debug_enable_at_height = 0

[network]
network = "Testnet"
listen_addr = "0.0.0.0:18233"
peerset_initial_target_size = 25
initial_testnet_peers = ["dnsseed.testnet.z.cash:18233", "testnet.seeder.zfnd.org:18233"]

[state]
cache_dir = "/root/.cache/zebra"

[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false

[tracing]
use_color = false
"#;

pub const MAINNET_ZEBRAD_TOML: &str = r#"[consensus]
checkpoint_sync = true

[mempool]
eviction_memory_time = "1h"
tx_cost_limit = 80000000
debug_enable_at_height = 0

[network]
network = "Mainnet"
listen_addr = "0.0.0.0:8233"
peerset_initial_target_size = 25
initial_mainnet_peers = ["dnsseed.str4d.xyz:8233", "dnsseed.z.cash:8233", "mainnet.seeder.shieldedinfra.net:8233", "mainnet.seeder.zfnd.org:8233"]

[state]
cache_dir = "/root/.cache/zebra"

[rpc]
listen_addr = "0.0.0.0:8232"
enable_cookie_auth = false

[tracing]
use_color = false
"#;

pub fn template_for(network_kind: NetworkKind) -> &'static str {
    match network_kind {
        NetworkKind::LocalGenesis => DEFAULT_ZEBRAD_TOML,
        NetworkKind::PublicTestnet => PUBLIC_TESTNET_ZEBRAD_TOML,
        NetworkKind::Mainnet => MAINNET_ZEBRAD_TOML,
    }
}

pub fn set_post_blossom_pow_target_spacing(config: &str, spacing_secs: u32) -> Result<String> {
    let mut result = String::new();
    let mut replaced = false;

    for line in config.lines() {
        if line.trim().starts_with("post_blossom_pow_target_spacing") {
            result.push_str(&format!("post_blossom_pow_target_spacing = {spacing_secs}"));
            replaced = true;
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    if !replaced {
        anyhow::bail!("default zebrad.toml template is missing post_blossom_pow_target_spacing");
    }

    Ok(result)
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

    let peer_list = format!(
        "[{}]",
        peer_values
            .iter()
            .map(|peer| format!("\"{peer}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut result = String::new();
    let mut in_network = false;
    let mut replaced_peers = false;
    let mut wrote_external_addr = false;
    let external_addr = match network_kind {
        NetworkKind::LocalGenesis => None,
        NetworkKind::PublicTestnet | NetworkKind::Mainnet => {
            if current_node.public_ip == "TBD" || current_node.public_ip.is_empty() {
                anyhow::bail!(
                    "cannot render external_addr for {} without a public IP",
                    current_node.name
                );
            }
            Some(format!(
                "external_addr = \"{}:{p2p_port}\"",
                current_node.public_ip
            ))
        }
    };

    for line in template.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_network && !wrote_external_addr {
                if let Some(external_addr) = &external_addr {
                    result.push_str(external_addr);
                    result.push('\n');
                    wrote_external_addr = true;
                }
            }
            in_network = trimmed == "[network]";
        }

        if trimmed.starts_with(peer_key) {
            result.push_str(&format!("{peer_key} = {peer_list}"));
            replaced_peers = true;
        } else if in_network && trimmed.starts_with("external_addr") {
            if let Some(external_addr) = &external_addr {
                result.push_str(external_addr);
                wrote_external_addr = true;
            }
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    if in_network && !wrote_external_addr {
        if let Some(external_addr) = &external_addr {
            result.push_str(external_addr);
            result.push('\n');
        }
    }
    if !replaced_peers {
        anyhow::bail!("zebrad.toml template is missing {peer_key}");
    }

    Ok(result)
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

/// Set `mining.miner_address` to a concrete address in a rendered zebrad.toml.
pub fn set_miner_address(config: &str, miner_address: &str) -> String {
    let mut result = String::new();
    let mut replaced = false;

    for line in config.lines() {
        if line.trim().starts_with("miner_address") {
            result.push_str(&format!("miner_address = \"{miner_address}\""));
            replaced = true;
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    if !replaced {
        result.push('\n');
        result.push_str("[mining]\n");
        result.push_str(&format!("miner_address = \"{miner_address}\"\n"));
    }

    result
}

/// Inject custom `[network.testnet_parameters]` for locally generated chains.
pub fn apply_local_testnet_parameters(config: &str, params: &LocalTestnetParameters) -> String {
    let stripped = strip_testnet_parameter_sections(config);
    let mut result = stripped.trim_end().to_string();
    result.push('\n');
    result.push('\n');
    result.push_str("[network.testnet_parameters]\n");
    result.push_str(&format!("network_name = \"{}\"\n", params.network_name));
    result.push_str(&format!(
        "network_magic = [{}, {}, {}, {}]\n",
        params.network_magic[0],
        params.network_magic[1],
        params.network_magic[2],
        params.network_magic[3],
    ));
    result.push_str(&format!(
        "target_difficulty_limit = \"{}\"\n",
        params.target_difficulty_limit
    ));
    result.push_str(&format!("disable_pow = {}\n", params.disable_pow));
    if let Some(h) = params.pow_start_height {
        result.push_str(&format!("pow_start_height = {h}\n"));
    }
    result.push_str(&format!("genesis_hash = \"{}\"\n", params.genesis_hash));
    result.push_str(&format!(
        "slow_start_interval = {}\n",
        params.slow_start_interval
    ));
    result.push_str(&format!(
        "pre_blossom_halving_interval = {}\n",
        params.pre_blossom_halving_interval
    ));
    if let Some(spacing_secs) = params.post_blossom_pow_target_spacing {
        result.push_str(&format!(
            "post_blossom_pow_target_spacing = {}\n",
            spacing_secs
        ));
    }
    result.push_str(&format!(
        "equihash_params = \"{}\"\n",
        equihash_params_name(params.equihash_params)
    ));
    append_daa_parameters(&mut result, params.daa);
    result.push_str("lockbox_disbursements = []\n");
    // Local genesis generation clears funding streams; mirror that here to avoid
    // default Testnet recipient validation for short custom halving intervals.
    result.push_str("pre_nu6_funding_streams = { recipients = [] }\n");
    result.push_str("post_nu6_funding_streams = { recipients = [] }\n");
    result.push_str(&format!("checkpoints = \"{}\"\n", params.checkpoints_path));
    result.push('\n');
    result.push_str("[network.testnet_parameters.activation_heights]\n");
    result.push_str(&format!("Overwinter = {}\n", params.activation_height));
    result.push_str(&format!("Sapling = {}\n", params.activation_height));
    result.push_str(&format!("Blossom = {}\n", params.activation_height));
    result.push_str(&format!("Heartwood = {}\n", params.activation_height));
    result.push_str(&format!("Canopy = {}\n", params.activation_height));
    result.push_str(&format!("NU5 = {}\n", params.activation_height));
    result.push_str(&format!("NU6 = {}\n", params.activation_height));

    result
}

/// Parse and validate the rendered config so experiment-specific testnet
/// parameters cannot be silently dropped during templating.
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

    if let Some(expected_spacing) = params.post_blossom_pow_target_spacing {
        let actual_spacing = testnet_params
            .get("post_blossom_pow_target_spacing")
            .and_then(toml::Value::as_integer)
            .context("missing network.testnet_parameters.post_blossom_pow_target_spacing")?;
        let actual_spacing = u32::try_from(actual_spacing)
            .context("post_blossom_pow_target_spacing does not fit in u32")?;
        if actual_spacing != expected_spacing {
            anyhow::bail!(
                "rendered post_blossom_pow_target_spacing mismatch: expected {}, got {}",
                expected_spacing,
                actual_spacing,
            );
        }
    }

    if let Some(expected_pow_start_height) = params.pow_start_height {
        let actual_pow_start_height = testnet_params
            .get("pow_start_height")
            .and_then(toml::Value::as_integer)
            .context("missing network.testnet_parameters.pow_start_height")?;
        let actual_pow_start_height = u32::try_from(actual_pow_start_height)
            .context("pow_start_height does not fit in u32")?;
        if actual_pow_start_height != expected_pow_start_height {
            anyhow::bail!(
                "rendered pow_start_height mismatch: expected {}, got {}",
                expected_pow_start_height,
                actual_pow_start_height,
            );
        }
    }

    let actual_equihash_params = testnet_params
        .get("equihash_params")
        .and_then(toml::Value::as_str)
        .context("missing network.testnet_parameters.equihash_params")?;
    let expected_equihash_params = equihash_params_name(params.equihash_params);
    if actual_equihash_params != expected_equihash_params {
        anyhow::bail!(
            "rendered equihash_params mismatch: expected {}, got {}",
            expected_equihash_params,
            actual_equihash_params,
        );
    }

    verify_optional_usize(
        testnet_params,
        "pow_averaging_window",
        params.daa.pow_averaging_window,
    )?;
    verify_optional_usize(
        testnet_params,
        "pow_median_block_span",
        params.daa.pow_median_block_span,
    )?;
    verify_optional_i64(
        testnet_params,
        "pre_blossom_pow_target_spacing",
        params.daa.pre_blossom_pow_target_spacing,
    )?;
    verify_optional_i32(
        testnet_params,
        "pow_damping_factor",
        params.daa.pow_damping_factor,
    )?;
    verify_optional_i32(
        testnet_params,
        "pow_max_adjust_up_percent",
        params.daa.pow_max_adjust_up_percent,
    )?;
    verify_optional_i32(
        testnet_params,
        "pow_max_adjust_down_percent",
        params.daa.pow_max_adjust_down_percent,
    )?;
    verify_optional_u32(
        testnet_params,
        "testnet_min_difficulty_start_height",
        params.daa.testnet_min_difficulty_start_height,
    )?;
    verify_optional_i32(
        testnet_params,
        "testnet_min_difficulty_gap_multiplier",
        params.daa.testnet_min_difficulty_gap_multiplier,
    )?;

    Ok(())
}

fn append_daa_parameters(result: &mut String, daa: DaaConfig) {
    if let Some(value) = daa.pow_averaging_window {
        result.push_str(&format!("pow_averaging_window = {value}\n"));
    }
    if let Some(value) = daa.pow_median_block_span {
        result.push_str(&format!("pow_median_block_span = {value}\n"));
    }
    if let Some(value) = daa.pre_blossom_pow_target_spacing {
        result.push_str(&format!("pre_blossom_pow_target_spacing = {value}\n"));
    }
    if let Some(value) = daa.pow_damping_factor {
        result.push_str(&format!("pow_damping_factor = {value}\n"));
    }
    if let Some(value) = daa.pow_max_adjust_up_percent {
        result.push_str(&format!("pow_max_adjust_up_percent = {value}\n"));
    }
    if let Some(value) = daa.pow_max_adjust_down_percent {
        result.push_str(&format!("pow_max_adjust_down_percent = {value}\n"));
    }
    if let Some(value) = daa.testnet_min_difficulty_start_height {
        result.push_str(&format!("testnet_min_difficulty_start_height = {value}\n"));
    }
    if let Some(value) = daa.testnet_min_difficulty_gap_multiplier {
        result.push_str(&format!(
            "testnet_min_difficulty_gap_multiplier = {value}\n"
        ));
    }
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

fn verify_optional_usize(
    testnet_params: &toml::Value,
    key: &str,
    expected: Option<usize>,
) -> Result<()> {
    if let Some(expected) = expected {
        let actual = testnet_params
            .get(key)
            .and_then(toml::Value::as_integer)
            .with_context(|| format!("missing network.testnet_parameters.{key}"))?;
        let actual =
            usize::try_from(actual).with_context(|| format!("{key} does not fit in usize"))?;
        if actual != expected {
            anyhow::bail!("{key} mismatch: expected {expected}, got {actual}");
        }
    }

    Ok(())
}

fn verify_optional_u32(
    testnet_params: &toml::Value,
    key: &str,
    expected: Option<u32>,
) -> Result<()> {
    if let Some(expected) = expected {
        let actual = testnet_params
            .get(key)
            .and_then(toml::Value::as_integer)
            .with_context(|| format!("missing network.testnet_parameters.{key}"))?;
        let actual = u32::try_from(actual).with_context(|| format!("{key} does not fit in u32"))?;
        if actual != expected {
            anyhow::bail!("{key} mismatch: expected {expected}, got {actual}");
        }
    }

    Ok(())
}

fn verify_optional_i64(
    testnet_params: &toml::Value,
    key: &str,
    expected: Option<i64>,
) -> Result<()> {
    if let Some(expected) = expected {
        let actual = testnet_params
            .get(key)
            .and_then(toml::Value::as_integer)
            .with_context(|| format!("missing network.testnet_parameters.{key}"))?;
        if actual != expected {
            anyhow::bail!("{key} mismatch: expected {expected}, got {actual}");
        }
    }

    Ok(())
}

fn verify_optional_i32(
    testnet_params: &toml::Value,
    key: &str,
    expected: Option<i32>,
) -> Result<()> {
    if let Some(expected) = expected {
        let actual = testnet_params
            .get(key)
            .and_then(toml::Value::as_integer)
            .with_context(|| format!("missing network.testnet_parameters.{key}"))?;
        let actual = i32::try_from(actual).with_context(|| format!("{key} does not fit in i32"))?;
        if actual != expected {
            anyhow::bail!("{key} mismatch: expected {expected}, got {actual}");
        }
    }

    Ok(())
}

fn equihash_params_name(params: EquihashParams) -> &'static str {
    match params {
        EquihashParams::Common => "common",
        EquihashParams::Regtest => "regtest",
    }
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

fn strip_testnet_parameter_sections(config: &str) -> String {
    let mut result = String::new();
    let mut in_testnet_params = false;

    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if trimmed == "[network.testnet_parameters]"
                || trimmed == "[network.testnet_parameters.activation_heights]"
            {
                in_testnet_params = true;
                continue;
            }

            if in_testnet_params {
                in_testnet_params = false;
            }
        }

        if !in_testnet_params {
            result.push_str(line);
            result.push('\n');
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ZEBRAD_TOML, LocalTestnetParameters, MAINNET_ZEBRAD_TOML,
        PUBLIC_TESTNET_ZEBRAD_TOML, apply_local_testnet_parameters, ensure_miner_address_is_set,
        generate_node_config, set_miner_address, set_post_blossom_pow_target_spacing,
        testnet_toml_parameters, verify_local_testnet_parameters,
    };
    use crate::config::{DaaConfig, Instance, NetworkKind, NodeType, Provider};
    use zebra_chain::parameters::EquihashParams;

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

    #[test]
    fn accepts_auto_miner_address() {
        ensure_miner_address_is_set(DEFAULT_ZEBRAD_TOML)
            .expect("default template should use auto miner address");
    }

    #[test]
    fn default_template_contains_25s_daa_profile() {
        let params =
            testnet_toml_parameters(DEFAULT_ZEBRAD_TOML).expect("default template should parse");

        assert_eq!(params.post_blossom_pow_target_spacing, Some(25));
        assert_eq!(params.daa.pow_averaging_window, Some(51));
        assert_eq!(params.daa.pow_median_block_span, Some(33));
        assert_eq!(params.daa.pow_damping_factor, Some(4));
        assert_eq!(params.daa.pow_max_adjust_up_percent, Some(16));
        assert_eq!(params.daa.pow_max_adjust_down_percent, Some(32));
    }

    #[test]
    fn block_time_override_updates_default_template_spacing() {
        let config = set_post_blossom_pow_target_spacing(DEFAULT_ZEBRAD_TOML, 42)
            .expect("default template has post blossom spacing");
        let params = testnet_toml_parameters(&config).expect("updated template should parse");

        assert_eq!(params.post_blossom_pow_target_spacing, Some(42));
        assert_eq!(params.daa.pow_averaging_window, Some(51));
        assert_eq!(params.daa.pow_median_block_span, Some(33));
    }

    #[test]
    fn accepts_non_auto_miner_address() {
        let config = DEFAULT_ZEBRAD_TOML.replace(
            "miner_address = \"auto\"",
            "miner_address = \"tmFakeAddress\"",
        );
        ensure_miner_address_is_set(&config).expect("non-empty miner address should pass");
    }

    #[test]
    fn replaces_peers_for_each_node() {
        let config = DEFAULT_ZEBRAD_TOML.replace(
            "miner_address = \"auto\"",
            "miner_address = \"tmFakeAddress\"",
        );
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
            miner("miner-2-ghi", "TBD"),
        ];

        let generated =
            generate_node_config(&config, NetworkKind::LocalGenesis, &miners[0], &miners)
                .expect("config generation");
        assert!(generated.contains("peerset_initial_target_size = 25"));
        assert!(generated.contains("initial_testnet_peers = [\"2.2.2.2:18233\"]"));
    }

    #[test]
    fn public_testnet_preserves_seeders_and_adds_fleet_peers() {
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
        ];

        let generated = generate_node_config(
            PUBLIC_TESTNET_ZEBRAD_TOML,
            NetworkKind::PublicTestnet,
            &miners[0],
            &miners,
        )
        .expect("config generation");

        assert!(
            generated.contains("initial_testnet_peers = [\"dnsseed.testnet.z.cash:18233\", \"testnet.seeder.zfnd.org:18233\", \"2.2.2.2:18233\"]")
        );
        assert!(generated.contains("external_addr = \"1.1.1.1:18233\""));
        assert!(!generated.contains("[network.testnet_parameters]"));
    }

    #[test]
    fn mainnet_preserves_seeders_and_adds_external_addr() {
        let miners = vec![
            miner("miner-0-abc", "1.1.1.1"),
            miner("miner-1-def", "2.2.2.2"),
        ];

        let generated = generate_node_config(
            MAINNET_ZEBRAD_TOML,
            NetworkKind::Mainnet,
            &miners[0],
            &miners,
        )
        .expect("config generation");

        assert!(generated.contains("initial_mainnet_peers = [\"dnsseed.str4d.xyz:8233\""));
        assert!(generated.contains("\"2.2.2.2:8233\""));
        assert!(generated.contains("external_addr = \"1.1.1.1:8233\""));
    }

    #[test]
    fn rejects_placeholder_miner_address() {
        let config =
            DEFAULT_ZEBRAD_TOML.replace("miner_address = \"auto\"", "miner_address = \"todo\"");
        let err = ensure_miner_address_is_set(&config)
            .expect_err("placeholder values should fail validation");
        assert!(err.to_string().contains("mining.miner_address"));
    }

    #[test]
    fn sets_miner_address() {
        let generated = set_miner_address(DEFAULT_ZEBRAD_TOML, "tmTestAddress");
        assert!(generated.contains("miner_address = \"tmTestAddress\""));
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
            post_blossom_pow_target_spacing: Some(25),
            equihash_params: EquihashParams::Regtest,
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

        let generated = apply_local_testnet_parameters(DEFAULT_ZEBRAD_TOML, &params);
        assert!(generated.contains("[network.testnet_parameters]"));
        assert!(generated.contains("network_name = \"LocalGenesisNet\""));
        assert!(generated.contains("pow_start_height = 257"));
        assert!(generated.contains("post_blossom_pow_target_spacing = 25"));
        assert!(generated.contains("equihash_params = \"regtest\""));
        assert!(generated.contains("pow_averaging_window = 8"));
        assert!(generated.contains("pow_median_block_span = 6"));
        assert!(generated.contains("pre_blossom_pow_target_spacing = 50"));
        assert!(generated.contains("pow_damping_factor = 3"));
        assert!(generated.contains("pow_max_adjust_up_percent = 20"));
        assert!(generated.contains("pow_max_adjust_down_percent = 40"));
        assert!(generated.contains("testnet_min_difficulty_start_height = 100"));
        assert!(generated.contains("testnet_min_difficulty_gap_multiplier = 4"));
        assert!(!generated.contains("genesis_block_path"));
        assert!(
            generated.contains("checkpoints = \"/root/payload/local_genesis/checkpoints.txt\"")
        );
        assert!(generated.contains("pre_nu6_funding_streams = { recipients = [] }"));
        assert!(generated.contains("post_nu6_funding_streams = { recipients = [] }"));
        assert!(generated.contains("NU6 = 1"));
        verify_local_testnet_parameters(&generated, &params)
            .expect("rendered config should preserve local testnet parameters");
    }
}
