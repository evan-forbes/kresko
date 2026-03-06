use anyhow::Result;
use rand::prelude::IndexedRandom;

use crate::config::*;

pub fn run(
    node_type: &str,
    count: usize,
    provider_flag: Option<&str>,
    region: &str,
    directory: &str,
) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let mut config = Config::load(dir)?;

    let node_type: NodeType = node_type.parse()?;
    let provider: Provider = match provider_flag {
        Some(p) => p.parse()?,
        None => config.provider,
    };

    let regions = match provider {
        Provider::DigitalOcean => DO_REGIONS,
        Provider::GoogleCloud => GCP_REGIONS,
    };

    let default_slug = match (provider, node_type) {
        (Provider::DigitalOcean, NodeType::Miner) => DO_DEFAULT_MINER_SLUG,
        (Provider::GoogleCloud, NodeType::Miner) => GCP_DEFAULT_MACHINE,
    };

    let existing_count = config
        .miners
        .iter()
        .filter(|i| i.node_type == node_type)
        .count();

    for i in 0..count {
        let idx = existing_count + i;
        let selected_region = if region == "random" {
            let mut rng = rand::rng();
            regions.choose(&mut rng).unwrap()
        } else {
            // Validate region
            if !regions.contains(&region) {
                anyhow::bail!(
                    "Region '{region}' not available for {provider}. Available: {}",
                    regions.join(", ")
                );
            }
            &region
        };

        let name = format!(
            "{node_type}-{idx}-{}-{}",
            config.experiment, selected_region
        );

        let instance = Instance::new_base(
            node_type,
            provider,
            default_slug,
            selected_region,
            &name,
            &config.experiment,
        );

        println!(
            "Added {} ({}, {})",
            instance.name, provider, selected_region
        );
        config.miners.push(instance);
    }

    config.save(dir)?;
    println!("Total miners: {}", config.miners.len());

    Ok(())
}
