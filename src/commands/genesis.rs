use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::zebra_config;

pub fn run(
    zebrad_binary: &str,
    txblast_binary: Option<&str>,
    build_dir: &str,
    directory: &str,
) -> Result<()> {
    let dir = Path::new(directory);
    let config = Config::load(dir)?;

    let payload_dir = dir.join("payload");

    // Clean old payload
    if payload_dir.exists() {
        std::fs::remove_dir_all(&payload_dir)?;
    }
    std::fs::create_dir_all(&payload_dir)?;

    let template_path = dir.join("zebrad.toml");
    let template = if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .with_context(|| format!("failed to read template {}", template_path.display()))?
    } else {
        zebra_config::DEFAULT_ZEBRAD_TOML.to_string()
    };

    // Generate per-node configs
    println!("Generating per-node zebrad.toml configs...");
    for inst in &config.validators {
        let node_dir = payload_dir.join(inst.parsed_hostname());
        std::fs::create_dir_all(&node_dir)?;

        let node_config =
            zebra_config::generate_node_config(&template, inst, &config.validators)?;
        std::fs::write(node_dir.join("zebrad.toml"), &node_config)?;

        println!("  {} -> {}/zebrad.toml", inst.name, inst.parsed_hostname());
    }

    // Copy scripts
    let scripts_dir = dir.join("scripts");
    if scripts_dir.exists() {
        for entry in std::fs::read_dir(&scripts_dir)? {
            let entry = entry?;
            let dest = payload_dir.join(entry.file_name());
            std::fs::copy(entry.path(), &dest)?;
        }
    }

    // Copy binaries
    let bin_dir = payload_dir.join(build_dir);
    std::fs::create_dir_all(&bin_dir)?;

    let zebrad_path = Path::new(zebrad_binary);
    if !zebrad_path.exists() {
        anyhow::bail!("zebrad binary not found at {}", zebrad_binary);
    }
    std::fs::copy(zebrad_path, bin_dir.join("zebrad"))
        .with_context(|| format!("failed to copy zebrad from {}", zebrad_binary))?;
    println!("Copied zebrad binary from {zebrad_binary}");

    if let Some(txblast_path) = txblast_binary {
        let src = Path::new(txblast_path);
        if src.exists() {
            std::fs::copy(src, bin_dir.join("kresko"))?;
            println!("Copied txblast binary from {txblast_path}");
        }
    }

    // Write vars.sh with credentials
    let vars_content = format!(
        r#"#!/bin/bash
export CHAIN_ID="{}"
export AWS_ACCESS_KEY_ID="{}"
export AWS_SECRET_ACCESS_KEY="{}"
export AWS_DEFAULT_REGION="{}"
export AWS_S3_BUCKET="{}"
export AWS_S3_ENDPOINT="{}"
"#,
        config.chain_id,
        config.s3.access_key_id,
        config.s3.secret_access_key,
        config.s3.region,
        config.s3.bucket_name,
        config.s3.endpoint,
    );
    std::fs::write(payload_dir.join("vars.sh"), vars_content)?;

    println!("Genesis payload generated in {}", payload_dir.display());
    Ok(())
}
