use anyhow::Result;
use std::time::{Duration, Instant};

use crate::cloud;
use crate::config::{Config, Provider, provider_configs};

pub async fn run(
    all: bool,
    workers: usize,
    wait: bool,
    timeout_secs: u64,
    directory: &str,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = if all {
        Config::load(dir).unwrap_or_default()
    } else {
        Config::load(dir)?
    };

    let mut clients = Vec::new();

    if all {
        println!("Destroying ALL kresko instances...");

        let mut any_provider = false;
        let mut errors = Vec::new();

        for provider in [
            Provider::DigitalOcean,
            Provider::GoogleCloud,
            Provider::Linode,
        ] {
            if !provider_has_credentials(provider) {
                continue;
            }

            any_provider = true;
            let mut provider_config = config.clone();
            provider_config.provider = provider;

            match cloud::new_client(provider_config) {
                Ok(client) => {
                    if let Err(error) = client.down(workers, true).await {
                        errors.push(format!("{provider}: {error}"));
                    } else {
                        clients.push((provider, client));
                    }
                }
                Err(error) => errors.push(format!("{provider}: {error}")),
            }
        }

        if !any_provider {
            anyhow::bail!(
                "No cloud provider credentials found. Set DIGITALOCEAN_TOKEN, LINODE_TOKEN, and/or GOOGLE_CLOUD_PROJECT + GOOGLE_CLOUD_KEY_JSON_PATH."
            );
        }

        if !errors.is_empty() {
            anyhow::bail!(
                "failed to destroy all instances:\n- {}",
                errors.join("\n- ")
            );
        }
    } else {
        println!(
            "Destroying instances for experiment '{}'...",
            config.experiment
        );

        for provider_config in provider_configs(&config) {
            let provider = provider_config.provider;
            let client = cloud::new_client(provider_config)?;
            client.down(workers, false).await?;
            clients.push((provider, client));
        }
    }

    if wait {
        wait_for_cleanup(&clients, all, Duration::from_secs(timeout_secs)).await?;
        println!("All instances destroyed and provider state is clear.");
    } else {
        println!("Destroy requested. Provider-side deletion is still in progress.");
    }

    Ok(())
}

pub async fn run_force(
    workers: usize,
    wait: bool,
    timeout_secs: u64,
    directory: &str,
) -> Result<()> {
    run(true, workers, wait, timeout_secs, directory).await
}

async fn wait_for_cleanup(
    clients: &[(Provider, cloud::CloudClient)],
    all: bool,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();

    loop {
        let mut remaining = Vec::new();
        for (provider, client) in clients {
            let names = client.matching_resource_names(all).await?;
            if !names.is_empty() {
                remaining.push(format!("{provider}: {}", names.join(", ")));
            }
        }

        if remaining.is_empty() {
            return Ok(());
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "timed out waiting for provider cleanup:\n- {}",
                remaining.join("\n- ")
            );
        }

        println!("Waiting for provider cleanup...");
        for entry in &remaining {
            println!("  {entry}");
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn provider_has_credentials(provider: Provider) -> bool {
    match provider {
        Provider::DigitalOcean => !std::env::var("DIGITALOCEAN_TOKEN")
            .unwrap_or_default()
            .is_empty(),
        Provider::GoogleCloud => {
            !std::env::var("GOOGLE_CLOUD_PROJECT")
                .unwrap_or_default()
                .is_empty()
                && !std::env::var("GOOGLE_CLOUD_KEY_JSON_PATH")
                    .unwrap_or_default()
                    .is_empty()
        }
        Provider::Linode => !std::env::var("LINODE_TOKEN").unwrap_or_default().is_empty(),
    }
}
