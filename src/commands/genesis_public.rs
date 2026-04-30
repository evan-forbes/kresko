use anyhow::{Context, Result};
use std::path::Path;

use crate::commands::genesis::{append_zebra_trace_exports, copy_dir_recursive};
use crate::config::{Config, NetworkKind};
use crate::zebra_config;

pub fn run(
    zebrad_binary: &str,
    kresko_binary: Option<&str>,
    build_dir: &str,
    scripts_dir: &str,
    directory: &str,
) -> Result<()> {
    let dir = Path::new(directory);
    let mut config = Config::load(dir)?;
    config.require_public_network("genesis-public")?;
    config.local_genesis = None;
    config.save(dir)?;

    for inst in &config.miners {
        if inst.public_ip == "TBD" || inst.public_ip.is_empty() {
            anyhow::bail!(
                "node {} has no public IP; run `kresko up` or `kresko sync-ips` before `kresko genesis-public`",
                inst.name
            );
        }
    }
    if config.miners.is_empty() {
        anyhow::bail!("No miners configured. Run 'kresko add -t miner -c <N>' first.");
    }

    let template_path = dir.join("zebrad.toml");
    let template = if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .with_context(|| format!("failed to read template {}", template_path.display()))?
    } else {
        zebra_config::template_for(config.network_kind).to_string()
    };

    let payload_dir = dir.join("payload");
    if payload_dir.exists() {
        std::fs::remove_dir_all(&payload_dir)?;
    }
    std::fs::create_dir_all(&payload_dir)?;

    println!("Generating public-network per-node zebrad.toml configs...");
    for inst in &config.miners {
        let node_name = inst.parsed_hostname();
        let node_dir = payload_dir.join(&node_name);
        std::fs::create_dir_all(&node_dir)?;

        let node_config = zebra_config::generate_node_config(
            &template,
            config.network_kind,
            inst,
            &config.miners,
        )
        .with_context(|| format!("rendering zebrad.toml for node {node_name}"))?;
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;
        std::fs::write(node_dir.join("tier"), format!("{}\n", inst.tier))?;

        println!("  {} -> {node_name}/zebrad.toml", inst.name);
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
    let payload_scripts_dir = payload_dir.join("scripts");
    std::fs::create_dir_all(&payload_scripts_dir)?;
    std::fs::write(
        payload_scripts_dir.join("node_init.sh"),
        NODE_INIT_PUBLIC_SH,
    )?;
    // Backwards compat: keep the flat payload/node_init.sh lookup working too.
    std::fs::write(payload_dir.join("node_init.sh"), NODE_INIT_PUBLIC_SH)?;

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
export KRESKO_NETWORK_KIND="{}"
export KRESKO_ZEBRA_NETWORK="{}"
export KRESKO_MINING_MODE="observe"
export KRESKO_RPC_PORT="{}"
export KRESKO_P2P_PORT="{}"
export KRESKO_FRESH_STATE="{}"
export AWS_ACCESS_KEY_ID="{}"
export AWS_SECRET_ACCESS_KEY="{}"
export AWS_DEFAULT_REGION="{}"
export AWS_S3_BUCKET="{}"
export AWS_S3_ENDPOINT="{}"
"#,
        config.chain_id,
        config.network_kind,
        config.zebra_network_string(),
        config.rpc_port(),
        config.p2p_port(),
        fresh_state_default(config.network_kind),
        std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "kresko-data".into()),
        std::env::var("AWS_S3_ENDPOINT").unwrap_or_default(),
    );
    append_zebra_trace_exports(&mut vars_content);
    std::fs::write(payload_dir.join("vars.sh"), vars_content)?;

    println!(
        "Public {} payload generated in {} (rpc={}, p2p={}, fresh_state={})",
        config.network_kind,
        payload_dir.display(),
        config.rpc_port(),
        config.p2p_port(),
        fresh_state_default(config.network_kind),
    );
    Ok(())
}

fn fresh_state_default(network_kind: NetworkKind) -> &'static str {
    match network_kind {
        NetworkKind::Mainnet => "0",
        NetworkKind::PublicTestnet => "1",
        NetworkKind::LocalGenesis => "1",
    }
}

const NODE_INIT_PUBLIC_SH: &str = include_str!("../../scripts/node_init_public.sh");
