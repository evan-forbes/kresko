pub mod digitalocean;
pub mod google_cloud;

use anyhow::Result;

use crate::config::{Config, Instance, Provider};

/// Cloud client enum for provider dispatch.
pub enum CloudClient {
    DigitalOcean(digitalocean::DigitalOceanClient),
    GoogleCloud(google_cloud::GoogleCloudClient),
}

impl CloudClient {
    pub async fn up(&self, workers: usize) -> Result<Vec<Instance>> {
        match self {
            CloudClient::DigitalOcean(c) => c.up(workers).await,
            CloudClient::GoogleCloud(c) => c.up(workers).await,
        }
    }

    pub async fn down(&self, workers: usize, all: bool) -> Result<()> {
        match self {
            CloudClient::DigitalOcean(c) => c.down(workers, all).await,
            CloudClient::GoogleCloud(c) => c.down(workers, all).await,
        }
    }

    pub async fn list(&self) -> Result<()> {
        match self {
            CloudClient::DigitalOcean(c) => c.list().await,
            CloudClient::GoogleCloud(c) => c.list().await,
        }
    }
}

/// Create the appropriate cloud client based on config.
pub fn new_client(cfg: Config) -> Result<CloudClient> {
    match cfg.provider {
        Provider::DigitalOcean => Ok(CloudClient::DigitalOcean(
            digitalocean::DigitalOceanClient::new(cfg)?,
        )),
        Provider::GoogleCloud => Ok(CloudClient::GoogleCloud(
            google_cloud::GoogleCloudClient::new(cfg)?,
        )),
    }
}
