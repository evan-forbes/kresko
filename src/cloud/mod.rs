pub mod digitalocean;
pub mod google_cloud;
pub mod google_cloud_quotas;
pub mod linode;

use anyhow::Result;

use crate::config::{Config, Instance, Provider};

/// Cloud client enum for provider dispatch.
pub enum CloudClient {
    DigitalOcean(digitalocean::DigitalOceanClient),
    GoogleCloud(google_cloud::GoogleCloudClient),
    Linode(linode::LinodeClient),
}

impl CloudClient {
    pub async fn up(&self, workers: usize) -> Result<Vec<Instance>> {
        match self {
            CloudClient::DigitalOcean(c) => c.up(workers).await,
            CloudClient::GoogleCloud(c) => c.up(workers).await,
            CloudClient::Linode(c) => c.up(workers).await,
        }
    }

    pub async fn sync_config_ips(&self, overwrite: bool) -> Result<Vec<Instance>> {
        match self {
            CloudClient::DigitalOcean(c) => c.sync_config_ips(overwrite).await,
            CloudClient::GoogleCloud(c) => c.sync_config_ips(overwrite).await,
            CloudClient::Linode(c) => c.sync_config_ips(overwrite).await,
        }
    }

    pub async fn down(&self, workers: usize, all: bool) -> Result<()> {
        match self {
            CloudClient::DigitalOcean(c) => c.down(workers, all).await,
            CloudClient::GoogleCloud(c) => c.down(workers, all).await,
            CloudClient::Linode(c) => c.down(workers, all).await,
        }
    }

    pub async fn matching_resource_names(&self, all: bool) -> Result<Vec<String>> {
        match self {
            CloudClient::DigitalOcean(c) => c.matching_resource_names(all).await,
            CloudClient::GoogleCloud(c) => c.matching_resource_names(all).await,
            CloudClient::Linode(c) => c.matching_resource_names(all).await,
        }
    }

    pub async fn list(&self) -> Result<()> {
        match self {
            CloudClient::DigitalOcean(c) => c.list().await,
            CloudClient::GoogleCloud(c) => c.list().await,
            CloudClient::Linode(c) => c.list().await,
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
        Provider::Linode => Ok(CloudClient::Linode(linode::LinodeClient::new(cfg)?)),
    }
}
