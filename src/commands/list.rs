use anyhow::Result;

use crate::cloud;
use crate::config::{Config, provider_configs};

pub async fn run(directory: &str) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let provider_cfgs = provider_configs(&config);
    let multi_provider = provider_cfgs.len() > 1;

    for (idx, provider_config) in provider_cfgs.into_iter().enumerate() {
        if multi_provider {
            if idx > 0 {
                println!();
            }
            println!("Provider: {}", provider_config.provider);
        }

        let client = cloud::new_client(provider_config)?;
        client.list().await?;
    }

    Ok(())
}
