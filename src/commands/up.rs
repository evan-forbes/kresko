use anyhow::Result;
use std::collections::HashMap;

use crate::cloud;
use crate::config::{Config, Instance, provider_configs};

pub async fn run(
    workers: usize,
    ssh_pub_key_path: Option<String>,
    ssh_key_name: Option<String>,
    directory: &str,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let mut config = Config::load(dir)?;

    if let Some(path) = ssh_pub_key_path {
        config.ssh_pub_key_path = path;
    }
    if let Some(name) = ssh_key_name {
        config.ssh_key_name = name;
    }

    let mut saved_partial_updates = false;
    for provider_config in provider_configs(&config) {
        let client = match cloud::new_client(provider_config.clone()) {
            Ok(client) => client,
            Err(error) if saved_partial_updates => {
                anyhow::bail!("{error}\nPartial IP updates were already saved to config.json.");
            }
            Err(error) => return Err(error),
        };

        let updated_instances = match client.up(workers).await {
            Ok(updated_instances) => updated_instances,
            Err(error) if saved_partial_updates => {
                anyhow::bail!("{error}\nPartial IP updates were already saved to config.json.");
            }
            Err(error) => return Err(error),
        };

        apply_provider_updates(&mut config, updated_instances);
        config.save(dir)?;
        saved_partial_updates = true;
    }

    let active = config
        .miners
        .iter()
        .filter(|instance| instance.public_ip != "TBD")
        .count();
    let pending = config.miners.len().saturating_sub(active);

    if pending == 0 {
        println!("All instances are up. Config saved.");
    } else {
        println!("{active} instance(s) are up and {pending} remain unavailable. Config saved.");
    }
    Ok(())
}

fn apply_provider_updates(config: &mut Config, updated_instances: Vec<Instance>) {
    let mut updated_by_name = HashMap::new();
    for instance in updated_instances {
        updated_by_name.insert(instance.name.clone(), instance);
    }

    for instance in &mut config.miners {
        if let Some(updated) = updated_by_name.remove(&instance.name) {
            *instance = updated;
        }
    }
}
