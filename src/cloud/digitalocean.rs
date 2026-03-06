use anyhow::{Context, Result};
use futures::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::config::{
    Config, DO_DEFAULT_IMAGE, Instance, experiment_tag, require_env, resolve_value, shellexpand,
};

const DO_API: &str = "https://api.digitalocean.com/v2";
const MAX_DROPLETS: usize = 100;

pub struct DigitalOceanClient {
    config: Config,
    http: Client,
    token: String,
}

#[derive(Debug, Serialize)]
struct CreateDropletRequest {
    name: String,
    region: String,
    size: String,
    image: String,
    ssh_keys: Vec<serde_json::Value>,
    tags: Vec<String>,
    monitoring: bool,
}

#[derive(Debug, Deserialize)]
struct DropletResponse {
    droplet: Droplet,
}

#[derive(Debug, Deserialize)]
struct DropletsResponse {
    droplets: Vec<Droplet>,
}

#[derive(Debug, Deserialize)]
struct Droplet {
    id: u64,
    name: String,
    status: String,
    region: DropletRegion,
    networks: DropletNetworks,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct DropletRegion {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct DropletNetworks {
    v4: Vec<NetworkV4>,
}

#[derive(Debug, Deserialize)]
struct NetworkV4 {
    ip_address: String,
    #[serde(rename = "type")]
    net_type: String,
}

#[derive(Debug, Deserialize)]
struct SshKeysResponse {
    ssh_keys: Vec<SshKey>,
}

#[derive(Debug, Deserialize)]
struct SshKey {
    id: u64,
    name: String,
    fingerprint: String,
    #[serde(default)]
    public_key: String,
}

#[derive(Debug, Deserialize)]
struct DoErrorBody {
    #[serde(default)]
    id: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    request_id: String,
}

impl DigitalOceanClient {
    pub fn new(config: Config) -> Result<Self> {
        let token = require_env("DIGITALOCEAN_TOKEN")?;

        let http = Client::builder().timeout(Duration::from_secs(60)).build()?;

        Ok(Self {
            config,
            http,
            token,
        })
    }

    async fn list_ssh_keys(&self) -> Result<Vec<SshKey>> {
        let mut keys = Vec::new();
        let mut page = 1usize;

        loop {
            let resp: SshKeysResponse = self
                .http
                .get(format!("{DO_API}/account/keys?per_page=200&page={page}"))
                .bearer_auth(&self.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let count = resp.ssh_keys.len();
            keys.extend(resp.ssh_keys);
            if count < 200 {
                break;
            }
            page += 1;
        }

        Ok(keys)
    }

    async fn lookup_ssh_key(&self) -> Result<serde_json::Value> {
        let key_name = resolve_value(None, "KRESKO_SSH_KEY_NAME", &self.config.ssh_key_name);
        let ssh_keys = self.list_ssh_keys().await?;

        if !key_name.is_empty() {
            for key in &ssh_keys {
                if key.name == key_name
                    || key.fingerprint == key_name
                    || key.id.to_string() == key_name
                {
                    return Ok(serde_json::json!(key.id));
                }
            }
        }

        // Fallback to matching by public key material if configured.
        let ssh_pub_key_path = resolve_value(
            None,
            "KRESKO_SSH_PUB_KEY_PATH",
            &self.config.ssh_pub_key_path,
        );
        let ssh_pub_key_path = shellexpand(&ssh_pub_key_path);
        if !ssh_pub_key_path.is_empty() {
            if let Ok(local_pub_key) = std::fs::read_to_string(&ssh_pub_key_path) {
                if let Some(local_norm) = normalize_ssh_public_key(&local_pub_key) {
                    for key in &ssh_keys {
                        if let Some(remote_norm) = normalize_ssh_public_key(&key.public_key) {
                            if local_norm == remote_norm {
                                return Ok(serde_json::json!(key.id));
                            }
                        }
                    }
                }
            }
        }

        if !key_name.is_empty() {
            anyhow::bail!(
                "SSH key '{}' not found in DigitalOcean account (also failed to match by public key at '{}')",
                key_name,
                ssh_pub_key_path
            );
        }

        anyhow::bail!(
            "No matching SSH key found in DigitalOcean account. Set KRESKO_SSH_KEY_NAME or KRESKO_SSH_PUB_KEY_PATH."
        );
    }

    async fn create_droplet(&self, instance: &Instance, ssh_key: serde_json::Value) -> Result<u64> {
        let req = CreateDropletRequest {
            name: instance.name.clone(),
            region: instance.region.clone(),
            size: instance.slug.clone(),
            image: DO_DEFAULT_IMAGE.to_string(),
            ssh_keys: vec![ssh_key],
            tags: instance.tags.clone(),
            monitoring: true,
        };

        let response = self
            .http
            .post(format!("{DO_API}/droplets"))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .context("failed to call DigitalOcean create droplet API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let detail = format_do_error_detail(&body);
            anyhow::bail!(
                "failed to create droplet '{}' (region='{}', size='{}', image='{}'): HTTP {}{}{}",
                instance.name,
                instance.region,
                instance.slug,
                DO_DEFAULT_IMAGE,
                status.as_u16(),
                status
                    .canonical_reason()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                detail
            );
        }

        let resp: DropletResponse = response
            .json()
            .await
            .context("failed to parse DigitalOcean create droplet response")?;

        println!(
            "Created droplet {} (id: {})",
            instance.name, resp.droplet.id
        );
        Ok(resp.droplet.id)
    }

    async fn wait_for_ip(&self, droplet_id: u64) -> Result<(String, String)> {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;

            let resp: DropletResponse = self
                .http
                .get(format!("{DO_API}/droplets/{droplet_id}"))
                .bearer_auth(&self.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            if resp.droplet.status == "active" {
                let mut public_ip = String::new();
                let mut private_ip = String::new();

                for net in &resp.droplet.networks.v4 {
                    match net.net_type.as_str() {
                        "public" => public_ip = net.ip_address.clone(),
                        "private" => private_ip = net.ip_address.clone(),
                        _ => {}
                    }
                }

                if !public_ip.is_empty() {
                    return Ok((public_ip, private_ip));
                }
            }
        }

        anyhow::bail!("Timed out waiting for droplet {droplet_id} to get an IP");
    }

    async fn destroy_droplet(&self, droplet_id: u64) -> Result<()> {
        self.http
            .delete(format!("{DO_API}/droplets/{droplet_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()
            .context("failed to destroy droplet")?;

        Ok(())
    }

    async fn list_droplets_by_tag(&self, tag: &str) -> Result<Vec<Droplet>> {
        let mut droplets = Vec::new();
        let mut page = 1usize;

        loop {
            let resp: DropletsResponse = self
                .http
                .get(format!(
                    "{DO_API}/droplets?tag_name={tag}&per_page=200&page={page}"
                ))
                .bearer_auth(&self.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let count = resp.droplets.len();
            droplets.extend(resp.droplets);
            if count < 200 {
                break;
            }
            page += 1;
        }

        Ok(droplets)
    }
}

impl DigitalOceanClient {
    pub async fn up(&self, workers: usize) -> Result<Vec<Instance>> {
        if workers == 0 {
            anyhow::bail!("workers must be greater than 0");
        }

        let tag = experiment_tag(&self.config.experiment);
        let existing = self.list_droplets_by_tag(&tag).await?;
        let mut existing_by_name: HashMap<String, Vec<&Droplet>> = HashMap::new();
        for droplet in &existing {
            existing_by_name
                .entry(droplet.name.clone())
                .or_default()
                .push(droplet);
        }

        let mut updated = self.config.miners.clone();
        let mut wait_targets: Vec<(String, u64)> = Vec::new();
        let mut matched_existing: HashSet<String> = HashSet::new();

        // Reconcile TBD instances with existing droplets first.
        for inst in &mut updated {
            if inst.public_ip != "TBD" {
                continue;
            }

            let Some(matches) = existing_by_name.get(&inst.name) else {
                continue;
            };

            if matches.len() > 1 {
                let ids = matches
                    .iter()
                    .map(|d| d.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "multiple droplets match node '{}': ids [{}]. Run 'kresko down' to clean duplicates before 'kresko up'.",
                    inst.name,
                    ids
                );
            }

            let droplet = matches[0];
            matched_existing.insert(inst.name.clone());
            let (public_ip, private_ip) = droplet_ips(droplet);
            if !public_ip.is_empty() {
                inst.public_ip = public_ip;
                inst.private_ip = private_ip;
                println!(
                    "Reusing existing droplet {} (id: {}) -> {}",
                    inst.name, droplet.id, inst.public_ip
                );
            } else {
                println!(
                    "Found existing droplet {} (id: {}) without IP yet; waiting...",
                    inst.name, droplet.id
                );
                wait_targets.push((inst.name.clone(), droplet.id));
            }
        }

        // Any instance still TBD after reconciliation needs a new droplet.
        let pending: Vec<&Instance> = updated
            .iter()
            .filter(|i| i.public_ip == "TBD" && !matched_existing.contains(&i.name))
            .collect();

        if pending.is_empty() && wait_targets.is_empty() {
            println!("All instances already have IPs assigned.");
            return Ok(updated);
        }

        if pending.len() > MAX_DROPLETS {
            anyhow::bail!(
                "Cannot create {} droplets (max {})",
                pending.len(),
                MAX_DROPLETS
            );
        }

        if !pending.is_empty() {
            let ssh_key = self.lookup_ssh_key().await?;

            println!("Creating {} droplets...", pending.len());

            let mut created_targets: Vec<(String, u64)> = Vec::with_capacity(pending.len());
            for chunk in pending.chunks(workers) {
                let create_futs: Vec<_> = chunk
                    .iter()
                    .map(|inst| {
                        let ssh_key = ssh_key.clone();
                        async move {
                            let id = self.create_droplet(inst, ssh_key).await?;
                            Ok::<_, anyhow::Error>((inst.name.clone(), id))
                        }
                    })
                    .collect();

                let mut chunk_targets: Vec<(String, u64)> = join_all(create_futs)
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>>>()?;
                created_targets.append(&mut chunk_targets);
            }
            wait_targets.extend(created_targets);
        }

        // Wait for IPs for droplets we just created, and existing droplets that
        // matched by name but had no public IP yet.
        println!("Waiting for IPs...");
        let mut resolved: Vec<(String, String, String)> = Vec::with_capacity(wait_targets.len());
        for chunk in wait_targets.chunks(workers) {
            let ip_futs: Vec<_> = chunk
                .iter()
                .map(|(name, id)| async move {
                    let (public_ip, private_ip) = self.wait_for_ip(*id).await?;
                    Ok::<_, anyhow::Error>((name.clone(), public_ip, private_ip))
                })
                .collect();

            let mut chunk_resolved: Vec<(String, String, String)> = join_all(ip_futs)
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            resolved.append(&mut chunk_resolved);
        }

        let mut resolved_by_name: HashMap<String, (String, String)> = HashMap::new();
        for (name, public_ip, private_ip) in resolved {
            resolved_by_name.insert(name, (public_ip, private_ip));
        }

        // Update instances with resolved IPs by node name.
        for inst in &mut updated {
            if inst.public_ip == "TBD" {
                let Some((public_ip, private_ip)) = resolved_by_name.get(&inst.name) else {
                    continue;
                };
                inst.public_ip = public_ip.clone();
                inst.private_ip = private_ip.clone();
                println!("  {} -> {}", inst.name, inst.public_ip);
            }
        }

        Ok(updated)
    }

    pub async fn down(&self, workers: usize, all: bool) -> Result<()> {
        if workers == 0 {
            anyhow::bail!("workers must be greater than 0");
        }

        let tag = if all {
            "kresko".to_string()
        } else {
            experiment_tag(&self.config.experiment)
        };
        let droplets = self.list_droplets_by_tag(&tag).await?;

        if droplets.is_empty() {
            if all {
                println!("No droplets found with tag 'kresko'");
            } else {
                println!(
                    "No droplets found for experiment '{}'",
                    self.config.experiment
                );
            }
            return Ok(());
        }

        println!("Destroying {} droplets...", droplets.len());

        for chunk in droplets.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|d| {
                    let id = d.id;
                    let name = d.name.clone();
                    async move {
                        self.destroy_droplet(id).await?;
                        println!("  Destroyed {name} (id: {id})");
                        Ok::<_, anyhow::Error>(())
                    }
                })
                .collect();

            let results = join_all(futs).await;
            for r in results {
                if let Err(e) = r {
                    eprintln!("Warning: {e}");
                }
            }
        }

        Ok(())
    }

    pub async fn list(&self) -> Result<()> {
        let tag = experiment_tag(&self.config.experiment);
        let droplets = self.list_droplets_by_tag(&tag).await?;

        if droplets.is_empty() {
            println!(
                "No droplets found for experiment '{}'",
                self.config.experiment
            );
            return Ok(());
        }

        println!(
            "{:<30} {:<12} {:<10} {:<18} {:<25}",
            "Name", "Status", "Region", "Public IP", "Created"
        );
        println!("{}", "-".repeat(95));

        for d in &droplets {
            let public_ip = d
                .networks
                .v4
                .iter()
                .find(|n| n.net_type == "public")
                .map(|n| n.ip_address.as_str())
                .unwrap_or("N/A");

            println!(
                "{:<30} {:<12} {:<10} {:<18} {:<25}",
                d.name, d.status, d.region.slug, public_ip, d.created_at
            );
        }

        Ok(())
    }
}

fn format_do_error_detail(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }

    if let Ok(parsed) = serde_json::from_str::<DoErrorBody>(body) {
        let mut parts = Vec::new();
        if !parsed.id.is_empty() {
            parts.push(format!("id={}", parsed.id));
        }
        if !parsed.message.is_empty() {
            parts.push(format!("message={}", parsed.message));
        }
        if !parsed.request_id.is_empty() {
            parts.push(format!("request_id={}", parsed.request_id));
        }
        if !parts.is_empty() {
            return format!(" [{}]", parts.join(", "));
        }
    }

    let trimmed = body.trim();
    let excerpt = if trimmed.len() > 400 {
        format!("{}...", &trimmed[..400])
    } else {
        trimmed.to_string()
    };
    format!(" [body={}]", excerpt)
}

fn normalize_ssh_public_key(raw: &str) -> Option<String> {
    let mut parts = raw.split_whitespace();
    let key_type = parts.next()?;
    let key = parts.next()?;
    Some(format!("{key_type} {key}"))
}

fn droplet_ips(d: &Droplet) -> (String, String) {
    let mut public_ip = String::new();
    let mut private_ip = String::new();

    for net in &d.networks.v4 {
        match net.net_type.as_str() {
            "public" => public_ip = net.ip_address.clone(),
            "private" => private_ip = net.ip_address.clone(),
            _ => {}
        }
    }

    (public_ip, private_ip)
}
