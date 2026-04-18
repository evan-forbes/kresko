use anyhow::Result;
use std::collections::HashMap;

use crate::cloud;
use crate::config::{Config, Instance, provider_configs};

pub async fn run(directory: &str, overwrite: bool) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let mut config = Config::load(dir)?;
    let provider_cfgs: Vec<_> = provider_configs(&config)
        .into_iter()
        .map(|mut provider_config| {
            if !overwrite {
                provider_config.miners.retain(needs_ip_refresh);
            }
            provider_config
        })
        .filter(|provider_config| !provider_config.miners.is_empty())
        .collect();

    if provider_cfgs.is_empty() {
        println!("No nodes need IP sync.");
        return Ok(());
    }

    let multi_provider = provider_cfgs.len() > 1;
    let mut saved_partial_updates = false;
    let mut synced_nodes = 0usize;

    for (idx, provider_config) in provider_cfgs.into_iter().enumerate() {
        let provider = provider_config.provider;
        if multi_provider {
            if idx > 0 {
                println!();
            }
            println!("Provider: {provider}");
        }

        let client = match cloud::new_client(provider_config.clone()) {
            Ok(client) => client,
            Err(error) if saved_partial_updates => {
                anyhow::bail!("{error}\nPartial IP updates were already saved to config.json.");
            }
            Err(error) => return Err(error),
        };

        let updated_instances = match client.sync_config_ips(overwrite).await {
            Ok(updated_instances) => updated_instances,
            Err(error) if saved_partial_updates => {
                anyhow::bail!("{error}\nPartial IP updates were already saved to config.json.");
            }
            Err(error) => return Err(error),
        };

        let changed = apply_provider_updates(&mut config, updated_instances);
        config.save(dir)?;
        saved_partial_updates = true;
        synced_nodes += changed;

        if changed == 0 {
            println!("No IP updates found.");
        } else {
            println!("Synced {changed} node(s) into config.json.");
        }
    }

    let active = config
        .miners
        .iter()
        .filter(|instance| instance.public_ip != "TBD" && !instance.public_ip.is_empty())
        .count();
    let pending = config.miners.len().saturating_sub(active);

    if pending == 0 {
        println!("IP sync complete. All instances in config have public IPs.");
    } else {
        println!(
            "IP sync complete. Synced {synced_nodes} node(s); {active} instance(s) have public IPs and {pending} remain unresolved."
        );
    }

    Ok(())
}

fn needs_ip_refresh(instance: &Instance) -> bool {
    instance.public_ip.is_empty()
        || instance.public_ip == "TBD"
        || instance.private_ip.is_empty()
        || instance.private_ip == "TBD"
}

fn apply_provider_updates(config: &mut Config, updated_instances: Vec<Instance>) -> usize {
    let before_by_name: HashMap<_, _> = config
        .miners
        .iter()
        .map(|instance| {
            (
                instance.name.clone(),
                (instance.public_ip.clone(), instance.private_ip.clone()),
            )
        })
        .collect();

    let mut updated_by_name = HashMap::new();
    for instance in updated_instances {
        updated_by_name.insert(instance.name.clone(), instance);
    }

    let mut changed = 0usize;
    for instance in &mut config.miners {
        if let Some(updated) = updated_by_name.remove(&instance.name) {
            let previous = before_by_name
                .get(&instance.name)
                .cloned()
                .unwrap_or_else(|| (String::new(), String::new()));
            let next = (updated.public_ip.clone(), updated.private_ip.clone());
            if previous != next {
                changed += 1;
                if !next.0.is_empty() && next.0 != "TBD" {
                    println!("  {} -> {}", instance.name, next.0);
                }
            }
            *instance = updated;
        }
    }

    changed
}
