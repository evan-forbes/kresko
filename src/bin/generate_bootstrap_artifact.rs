use anyhow::{Context, Result};
use serde::Serialize;
use zebra_chain::{
    local_genesis::{LocalTestnetGenesisOptions, generate_local_testnet_with_funded_keys},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
};

#[derive(Debug, Serialize)]
struct FundedKeyJson {
    name: String,
    secret_key_hex: String,
    public_key_hex: String,
    address: String,
}

#[derive(Debug, Serialize)]
struct ManifestJson {
    artifact_id: String,
    seeded_block_count: u32,
    premine_block_count: u32,
    maturity_padding_block_count: u32,
    target_difficulty_limit: String,
    disable_pow: bool,
    genesis_hash: String,
    seeded_tip_hash: String,
    slow_start_interval: u32,
    pre_blossom_halving_interval: u32,
    activation_height: u32,
    treasury_address: String,
    treasury_public_key_hex: String,
    notes: String,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        anyhow::bail!(
            "usage: cargo run --bin generate_bootstrap_artifact -- <artifact-id> <output-dir>"
        );
    }

    let artifact_id = &args[1];
    let output_dir = std::path::Path::new(&args[2]);
    std::fs::create_dir_all(output_dir)?;

    let generated = generate_local_testnet_with_funded_keys(
        vec!["treasury".to_string()],
        LocalTestnetGenesisOptions {
            network_name: format!("KreskoBootstrap_{artifact_id}"),
            latest_network_upgrade: NetworkUpgrade::Nu6,
            disable_pow: false,
            target_spacing_secs: None,
            maturity_padding_blocks: 124,
        },
    )
    .map_err(|e| anyhow::anyhow!("failed to generate bootstrap chain artifact: {e}"))?;

    let network_params = generated
        .network
        .parameters()
        .context("generated local genesis did not produce testnet parameters")?;
    let activation_height = network_params
        .activation_heights()
        .iter()
        .find_map(|(height, upgrade)| (*upgrade == NetworkUpgrade::Nu6).then_some(height.0))
        .context("missing activation height for NU6")?;
    let genesis_hex = generated
        .genesis_hex()
        .map_err(|e| anyhow::anyhow!("failed to serialize generated genesis block: {e}"))?;

    let treasury = generated
        .funded_keys
        .first()
        .context("generated artifact did not create a treasury key")?;
    let treasury_key = FundedKeyJson {
        name: treasury.name.clone(),
        secret_key_hex: treasury.secret_key_hex.clone(),
        public_key_hex: treasury.public_key_hex.clone(),
        address: treasury.address.to_string(),
    };

    let mut premine_blocks_hex = String::new();
    for block in generated.blocks.iter().skip(1) {
        let mut bytes = Vec::new();
        block
            .zcash_serialize(&mut bytes)
            .context("failed to serialize seeded block")?;
        premine_blocks_hex.push_str(&to_hex(&bytes));
        premine_blocks_hex.push('\n');
    }

    let checkpoints_content = generated
        .checkpoints
        .iter()
        .map(|(height, hash)| format!("{} {}", height.0, hash))
        .collect::<Vec<_>>()
        .join("\n");
    let seeded_tip_hash = generated
        .checkpoints
        .last()
        .map(|(_, hash)| hash.to_string())
        .context("generated artifact has no checkpoints")?;
    let pre_blossom_halving_interval: u32 = network_params
        .pre_blossom_halving_interval()
        .try_into()
        .context("pre_blossom_halving_interval does not fit in u32")?;

    let manifest = ManifestJson {
        artifact_id: artifact_id.clone(),
        seeded_block_count: generated.blocks.len().saturating_sub(1) as u32,
        premine_block_count: generated.funded_keys.len() as u32,
        maturity_padding_block_count: 124,
        target_difficulty_limit: network_params.target_difficulty_limit().to_string(),
        disable_pow: network_params.disable_pow(),
        genesis_hash: network_params.genesis_hash().to_string(),
        seeded_tip_hash,
        slow_start_interval: network_params.slow_start_interval().0,
        pre_blossom_halving_interval,
        activation_height,
        treasury_address: treasury_key.address.clone(),
        treasury_public_key_hex: treasury_key.public_key_hex.clone(),
        notes: "Single-treasury cached PoW bootstrap chain for kresko experiments.".to_string(),
    };

    std::fs::write(output_dir.join("genesis.hex"), genesis_hex)?;
    std::fs::write(output_dir.join("premine_blocks.hex"), premine_blocks_hex)?;
    std::fs::write(output_dir.join("checkpoints.txt"), checkpoints_content)?;
    std::fs::write(
        output_dir.join("treasury_key.json"),
        serde_json::to_vec_pretty(&treasury_key)?,
    )?;
    std::fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    println!(
        "wrote bootstrap artifact {} to {} (seeded_blocks={}, genesis_hash={}, tip_hash={})",
        artifact_id,
        output_dir.display(),
        manifest.seeded_block_count,
        manifest.genesis_hash,
        manifest.seeded_tip_hash,
    );

    Ok(())
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
