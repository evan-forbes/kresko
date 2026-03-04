use anyhow::Result;

use crate::cloud;
use crate::config::Config;

pub async fn run(directory: &str) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;

    let client = cloud::new_client(config)?;
    client.list().await?;

    Ok(())
}
