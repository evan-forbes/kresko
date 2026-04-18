use anyhow::Result;
use rand::prelude::IndexedRandom;
use std::collections::{HashMap, HashSet};

use crate::{
    cloud::{digitalocean::DigitalOceanClient, google_cloud_quotas},
    config::*,
};

pub async fn run(
    node_type: &str,
    count: usize,
    provider_flag: Option<&str>,
    region: &str,
    directory: &str,
    low_resource: bool,
) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let mut config = Config::load(dir)?;

    let node_type: NodeType = node_type.parse()?;
    let provider: Provider = match provider_flag {
        Some(p) => p.parse()?,
        None => config.provider,
    };

    // Candidate slugs in preference order. For DO we try the basic slug
    // first, then fall back to premium AMD / Intel variants. Other
    // providers have only one candidate today.
    let candidate_slugs: Vec<&'static str> = match (provider, node_type, low_resource) {
        (Provider::DigitalOcean, NodeType::Miner, true) => DO_LOW_MINER_SLUG_FALLBACKS.to_vec(),
        (Provider::DigitalOcean, NodeType::Miner, false) => DO_FULL_MINER_SLUG_FALLBACKS.to_vec(),
        (Provider::GoogleCloud, NodeType::Miner, true) => vec![GCP_LOW_RESOURCE_MACHINE],
        (Provider::GoogleCloud, NodeType::Miner, false) => vec![GCP_DEFAULT_MACHINE],
        (Provider::Linode, NodeType::Miner, true) => vec![LINODE_LOW_RESOURCE_MINER_TYPE],
        (Provider::Linode, NodeType::Miner, false) => vec![LINODE_DEFAULT_MINER_TYPE],
    };
    let tier = if low_resource { "low" } else { "full" };

    // Fetch live DO region->sizes map so we can (a) restrict region
    // selection to datacenters that carry at least one candidate slug
    // and (b) pick the specific slug per region.
    let do_region_map: Option<HashMap<String, HashSet<String>>> =
        if provider == Provider::DigitalOcean {
            match DigitalOceanClient::list_region_size_map().await {
                Ok(map) => Some(map),
                Err(error) => {
                    eprintln!("Warning: failed to query DigitalOcean region/size map: {error}");
                    eprintln!(
                        "Warning: falling back to static region list ({}) and primary slug only.",
                        DO_REGIONS.join(", ")
                    );
                    None
                }
            }
        } else {
            None
        };

    // Regions available for the requested size tier. For DO with a live
    // map, this is every datacenter carrying at least one candidate slug.
    let regions: Vec<String> = match (provider, &do_region_map) {
        (Provider::DigitalOcean, Some(map)) => {
            let mut r: Vec<String> = map
                .iter()
                .filter(|(_, sizes)| candidate_slugs.iter().any(|c| sizes.contains(*c)))
                .map(|(slug, _)| slug.clone())
                .collect();
            r.sort();
            if region == "all" || region == "random" {
                println!(
                    "DigitalOcean: {} region(s) carry one of {:?}: {}",
                    r.len(),
                    candidate_slugs,
                    r.join(", ")
                );
            }
            r
        }
        (Provider::DigitalOcean, None) => DO_REGIONS.iter().map(|s| s.to_string()).collect(),
        (Provider::GoogleCloud, _) => GCP_REGIONS.iter().map(|s| s.to_string()).collect(),
        (Provider::Linode, _) => LINODE_REGIONS.iter().map(|s| s.to_string()).collect(),
    };

    let existing_count = config
        .miners
        .iter()
        .filter(|instance| instance.node_type == node_type)
        .count();

    let mut next_idx = existing_count;
    if region == "all" {
        if regions.is_empty() {
            anyhow::bail!(
                "No regions available for {provider} carrying any of {:?}",
                candidate_slugs
            );
        }
        for selected_region in &regions {
            for _ in 0..count {
                push_instance(
                    &mut config,
                    provider,
                    node_type,
                    tier,
                    &candidate_slugs,
                    do_region_map.as_ref(),
                    selected_region,
                    &mut next_idx,
                );
            }
        }
    } else {
        for _ in 0..count {
            let selected_region = if region == "random" {
                if regions.is_empty() {
                    anyhow::bail!(
                        "Region 'random' is not supported for {provider}: no regions carry any of {:?}.",
                        candidate_slugs
                    );
                }
                let mut rng = rand::rng();
                regions.choose(&mut rng).unwrap().clone()
            } else {
                if !regions.iter().any(|r| r == region) {
                    anyhow::bail!(
                        "Region '{region}' not available for {provider} with {:?}. Available: {}",
                        candidate_slugs,
                        regions.join(", ")
                    );
                }
                region.to_string()
            };

            push_instance(
                &mut config,
                provider,
                node_type,
                tier,
                &candidate_slugs,
                do_region_map.as_ref(),
                &selected_region,
                &mut next_idx,
            );
        }
    }

    google_cloud_quotas::validate_assignment(&config.miners)?;

    config.save(dir)?;
    println!("Total miners: {}", config.miners.len());

    Ok(())
}

/// Pick the best candidate slug for `region` and append an instance to
/// `config.miners`. If no candidate is carried in the region, prints a
/// warning and leaves the config unchanged.
#[allow(clippy::too_many_arguments)]
fn push_instance(
    config: &mut Config,
    provider: Provider,
    node_type: NodeType,
    tier: &str,
    candidate_slugs: &[&'static str],
    do_region_map: Option<&HashMap<String, HashSet<String>>>,
    selected_region: &str,
    next_idx: &mut usize,
) {
    let slug = match pick_slug(selected_region, candidate_slugs, do_region_map) {
        Some(slug) => slug,
        None => {
            eprintln!(
                "Warning: skipping region {selected_region} — none of {:?} are carried there.",
                candidate_slugs
            );
            return;
        }
    };

    let name = format!(
        "{node_type}-{next_idx}-{}-{}",
        config.experiment, selected_region
    );

    let instance = Instance::new_base(
        node_type,
        provider,
        slug,
        selected_region,
        &name,
        &config.experiment,
        tier,
    );

    println!(
        "Added {} ({}, {}, {}, tier={})",
        instance.name, provider, selected_region, instance.slug, instance.tier
    );
    config.miners.push(instance);
    *next_idx += 1;
}

/// Return the first candidate slug carried in `region`, or the primary
/// candidate when we have no live size data to consult.
fn pick_slug<'a>(
    region: &str,
    candidates: &[&'a str],
    do_region_map: Option<&HashMap<String, HashSet<String>>>,
) -> Option<&'a str> {
    match do_region_map {
        Some(map) => {
            let sizes = map.get(region)?;
            candidates.iter().find(|c| sizes.contains(**c)).copied()
        }
        None => candidates.first().copied(),
    }
}
