use anyhow::Result;

use crate::cloud;
use crate::config::{Config, Provider};

pub async fn run(all: bool, workers: usize, directory: &str) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = if all {
        Config::load(dir).unwrap_or_default()
    } else {
        Config::load(dir)?
    };

    let do_token = std::env::var("DIGITALOCEAN_TOKEN").unwrap_or_default();
    let gcp_project = std::env::var("GOOGLE_CLOUD_PROJECT").unwrap_or_default();

    if all {
        println!("Destroying ALL kresko instances...");

        let mut any_provider = false;
        let mut errors = Vec::new();

        if !do_token.is_empty() {
            any_provider = true;
            let mut do_cfg = config.clone();
            do_cfg.provider = Provider::DigitalOcean;
            match cloud::digitalocean::DigitalOceanClient::new(do_cfg) {
                Ok(client) => {
                    if let Err(e) = client.down(workers, true).await {
                        errors.push(format!("digitalocean: {e}"));
                    }
                }
                Err(e) => errors.push(format!("digitalocean: {e}")),
            }
        }

        if !gcp_project.is_empty() {
            any_provider = true;
            let mut gcp_cfg = config.clone();
            gcp_cfg.provider = Provider::GoogleCloud;
            match cloud::google_cloud::GoogleCloudClient::new(gcp_cfg) {
                Ok(client) => {
                    if let Err(e) = client.down(workers, true).await {
                        errors.push(format!("googlecloud: {e}"));
                    }
                }
                Err(e) => errors.push(format!("googlecloud: {e}")),
            }
        }

        if !any_provider {
            anyhow::bail!(
                "No cloud provider credentials found. Set DIGITALOCEAN_TOKEN and/or GOOGLE_CLOUD_PROJECT + GOOGLE_CLOUD_KEY_JSON_PATH."
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

        let client = cloud::new_client(config)?;
        client.down(workers, false).await?;
    }

    println!("All instances destroyed.");
    Ok(())
}
