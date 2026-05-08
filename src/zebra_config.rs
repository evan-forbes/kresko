use anyhow::{Context, Result};
use toml::map::Map;

use crate::config::Instance;
use crate::config::{DaaConfig, NetworkKind};

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
            set_path(&mut config, &["mempool", "debug_enable_at_height"], 0.into());
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
            set_path(&mut config, &["tracing", "use_color"], false.into());
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
            set_path(&mut config, &["tracing", "use_color"], false.into());
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
            set_path(&mut config, &["tracing", "use_color"], false.into());
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
    testnet_params.insert(
        "lockbox_disbursements".to_string(),
        toml::Value::Array(Vec::new()),
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
    // NU6.1 is the one-time ZIP-271 lockbox disbursement event from mainnet; activating it on a
    // local testnet would require synthesising disbursement outputs in the activation-block
    // coinbase (and a deferred-pool balance to cover them), which kresko does not produce.
    // Skipping it lets NU7 activate directly without tripping the lockbox-disbursements rule.
    for upgrade in [
        "Overwinter",
        "Sapling",
        "Blossom",
        "Heartwood",
        "Canopy",
        "NU5",
        "NU6",
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
        LocalTestnetParameters, apply_local_testnet_parameters, ensure_miner_address_is_set,
        generate_node_config, set_miner_address, template_for, testnet_toml_parameters,
        verify_local_testnet_parameters,
    };
    use crate::config::{DaaConfig, Instance, NetworkKind, NodeType, Provider};

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
        assert!(generated.contains("NU6 = 1"));
        assert!(!generated.contains("NU6.1"));
        assert!(generated.contains("NU7 = 1"));
        verify_local_testnet_parameters(&generated, &params)
            .expect("rendered config should preserve local testnet parameters");
    }
}
