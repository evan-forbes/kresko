use anyhow::{Context, Result};

use crate::config::Instance;
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
    /// If `Some(h)`, live nodes skip Equihash and difficulty checks for
    /// blocks below height `h`. Set to one past the seeded tip so the
    /// cached pre-mined blocks pass validation but every live-mined block
    /// must solve PoW.
    pub pow_start_height: Option<u32>,
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

/// Generate a per-node zebrad.toml with the correct peer list.
///
/// Takes the template content as a string, replaces the `initial_testnet_peers`
/// line with the actual peer IPs (excluding the current node).
pub fn generate_node_config(
    template: &str,
    current_node: &Instance,
    all_instances: &[Instance],
) -> Result<String> {
    let peers: Vec<String> = all_instances
        .iter()
        .filter(|inst| inst.name != current_node.name)
        .filter(|inst| inst.public_ip != "TBD")
        .map(|inst| format!("\"{}:18233\"", inst.public_ip))
        .collect();

    let peer_list = format!("[{}]", peers.join(", "));

    // Replace the initial_testnet_peers line
    let mut result = String::new();
    for line in template.lines() {
        if line.trim().starts_with("initial_testnet_peers") {
            result.push_str(&format!("initial_testnet_peers = {peer_list}"));
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    Ok(result)
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
        DEFAULT_ZEBRAD_TOML, LocalTestnetParameters, apply_local_testnet_parameters,
        ensure_miner_address_is_set, generate_node_config, set_miner_address,
        verify_local_testnet_parameters,
    };
    use crate::config::{Instance, NodeType, Provider};
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
            generate_node_config(&config, &miners[0], &miners).expect("config generation");
        assert!(generated.contains("peerset_initial_target_size = 25"));
        assert!(generated.contains("initial_testnet_peers = [\"2.2.2.2:18233\"]"));
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
            pow_start_height: Some(257),
        };

        let generated = apply_local_testnet_parameters(DEFAULT_ZEBRAD_TOML, &params);
        assert!(generated.contains("[network.testnet_parameters]"));
        assert!(generated.contains("network_name = \"LocalGenesisNet\""));
        assert!(generated.contains("pow_start_height = 257"));
        assert!(generated.contains("post_blossom_pow_target_spacing = 25"));
        assert!(generated.contains("equihash_params = \"regtest\""));
        assert!(!generated.contains("genesis_block_path"));
        assert!(
            generated.contains("checkpoints = \"/root/payload/local_genesis/checkpoints.txt\"")
        );
        assert!(generated.contains("pre_nu6_funding_streams = { recipients = [] }"));
        assert!(generated.contains("post_nu6_funding_streams = { recipients = [] }"));
        assert!(generated.contains("NU6 = 1"));
        verify_local_testnet_parameters(&generated, &params)
            .expect("rendered config should preserve local testnet parameters");
        assert!(!generated.contains("pre_blossom_pow_target_spacing"));
        assert!(!generated.contains("pow_averaging_window"));
        assert!(!generated.contains("pow_damping_factor"));
        assert!(!generated.contains("pow_max_adjust_down_percent"));
        assert!(!generated.contains("testnet_min_difficulty_start_height"));
    }
}
