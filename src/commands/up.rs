use anyhow::Result;

use crate::cloud;
use crate::config::Config;

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

    // Override config with flags if provided
    if let Some(path) = ssh_pub_key_path {
        config.ssh_pub_key_path = path;
    }
    if let Some(name) = ssh_key_name {
        config.ssh_key_name = name;
    }

    let client = cloud::new_client(config.clone())?;
    let updated_instances = client.up(workers).await?;

    config.miners = updated_instances;
    config.save(dir)?;

    println!("All instances are up. Config saved.");
    Ok(())
}
