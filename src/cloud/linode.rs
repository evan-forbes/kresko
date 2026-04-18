use anyhow::{Context, Result};
use futures::future::join_all;
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::config::{
    Config, Instance, LINODE_DEFAULT_IMAGE, require_env, resolve_value, shellexpand,
};

const LINODE_API: &str = "https://api.linode.com/v4";
const MAX_LINODES: usize = 100;
const KRESKO_GROUP: &str = "kresko";

pub struct LinodeClient {
    config: Config,
    http: Client,
    token: String,
    ssh_pub_key: String,
}

#[derive(Debug, Serialize)]
struct CreateLinodeRequest {
    label: String,
    region: String,
    #[serde(rename = "type")]
    linode_type: String,
    image: String,
    root_pass: String,
    authorized_keys: Vec<String>,
    private_ip: bool,
    booted: bool,
    group: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LinodeInstance {
    id: u64,
    #[serde(default)]
    label: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    ipv4: Vec<String>,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    created: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LinodesResponse {
    #[serde(default)]
    data: Vec<LinodeInstance>,
    #[serde(default)]
    page: usize,
    #[serde(default)]
    pages: usize,
}

#[derive(Debug, Deserialize)]
struct LinodeErrorBody {
    #[serde(default)]
    errors: Vec<LinodeError>,
}

#[derive(Debug, Deserialize)]
struct LinodeError {
    #[serde(default)]
    field: String,
    #[serde(default)]
    reason: String,
}

impl LinodeClient {
    pub fn new(config: Config) -> Result<Self> {
        let token = require_env("LINODE_TOKEN")?;
        let ssh_pub_key_path = shellexpand(&resolve_value(
            None,
            "KRESKO_SSH_PUB_KEY_PATH",
            &config.ssh_pub_key_path,
        ));
        if ssh_pub_key_path.is_empty() {
            anyhow::bail!("KRESKO_SSH_PUB_KEY_PATH not set");
        }

        let ssh_pub_key = std::fs::read_to_string(&ssh_pub_key_path)
            .with_context(|| format!("failed to read SSH public key from {ssh_pub_key_path}"))?;
        if ssh_pub_key.trim().is_empty() {
            anyhow::bail!("SSH public key at {} is empty", ssh_pub_key_path);
        }

        let http = Client::builder().timeout(Duration::from_secs(60)).build()?;

        Ok(Self {
            config,
            http,
            token,
            ssh_pub_key: ssh_pub_key.trim().to_string(),
        })
    }

    async fn list_instances(&self) -> Result<Vec<LinodeInstance>> {
        let mut instances = Vec::new();
        let mut page = 1usize;

        loop {
            let resp: LinodesResponse = self
                .http
                .get(format!(
                    "{LINODE_API}/linode/instances?page_size=100&page={page}"
                ))
                .bearer_auth(&self.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            instances.extend(resp.data);
            if resp.pages == 0 || page >= resp.pages {
                break;
            }
            page = resp.page + 1;
        }

        Ok(instances)
    }

    async fn create_instance(&self, instance: &Instance) -> Result<u64> {
        let req = CreateLinodeRequest {
            label: instance.name.clone(),
            region: instance.region.clone(),
            linode_type: instance.slug.clone(),
            image: LINODE_DEFAULT_IMAGE.to_string(),
            root_pass: random_root_password(),
            authorized_keys: vec![self.ssh_pub_key.clone()],
            private_ip: true,
            booted: true,
            group: KRESKO_GROUP.to_string(),
        };

        let response = self
            .http
            .post(format!("{LINODE_API}/linode/instances"))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .context("failed to call Linode create instance API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let detail = format_linode_error_detail(&body);
            anyhow::bail!(
                "failed to create Linode '{}' (region='{}', type='{}', image='{}'): HTTP {}{}{}",
                instance.name,
                instance.region,
                instance.slug,
                LINODE_DEFAULT_IMAGE,
                status.as_u16(),
                status
                    .canonical_reason()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default(),
                detail,
            );
        }

        let created: LinodeInstance = response
            .json()
            .await
            .context("failed to parse Linode create instance response")?;

        println!("Created Linode {} (id: {})", created.label, created.id);
        Ok(created.id)
    }

    async fn get_instance(&self, linode_id: u64) -> Result<LinodeInstance> {
        self.http
            .get(format!("{LINODE_API}/linode/instances/{linode_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("failed to parse Linode instance response")
    }

    async fn wait_for_ip(&self, linode_id: u64) -> Result<(String, String)> {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;

            let instance = self.get_instance(linode_id).await?;
            if instance.status.eq_ignore_ascii_case("running") {
                let (public_ip, private_ip) = linode_ips(&instance);
                if !public_ip.is_empty() {
                    return Ok((public_ip, private_ip));
                }
            }
        }

        anyhow::bail!("Timed out waiting for Linode {linode_id} to get an IP")
    }

    async fn destroy_instance(&self, linode_id: u64) -> Result<()> {
        self.http
            .delete(format!("{LINODE_API}/linode/instances/{linode_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()
            .context("failed to destroy Linode instance")?;

        Ok(())
    }
}

impl LinodeClient {
    pub async fn matching_resource_names(&self, all: bool) -> Result<Vec<String>> {
        let instances = self.list_instances().await?;
        let names = if all {
            instances
                .into_iter()
                .filter(|instance| {
                    instance.group == KRESKO_GROUP
                        || instance.tags.iter().any(|tag| tag == KRESKO_GROUP)
                })
                .map(|instance| instance.label)
                .collect()
        } else {
            let target_names: HashSet<_> = self
                .config
                .miners
                .iter()
                .map(|instance| instance.name.as_str())
                .collect();
            instances
                .into_iter()
                .filter(|instance| target_names.contains(instance.label.as_str()))
                .map(|instance| instance.label)
                .collect()
        };
        Ok(names)
    }

    pub async fn sync_config_ips(&self, overwrite: bool) -> Result<Vec<Instance>> {
        let instances = self.list_instances().await?;
        let mut instances_by_label: HashMap<String, Vec<&LinodeInstance>> = HashMap::new();
        for instance in &instances {
            instances_by_label
                .entry(instance.label.clone())
                .or_default()
                .push(instance);
        }

        let mut updated = self.config.miners.clone();
        for inst in &mut updated {
            if !should_refresh_ips(inst, overwrite) {
                continue;
            }

            let Some(matches) = instances_by_label.get(&inst.name) else {
                continue;
            };

            if matches.len() > 1 {
                let ids = matches
                    .iter()
                    .map(|linode| linode.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "multiple Linodes match node '{}': ids [{}]. Run 'kresko down' to clean duplicates before syncing IPs.",
                    inst.name,
                    ids,
                );
            }

            let (public_ip, private_ip) = linode_ips(matches[0]);
            if !public_ip.is_empty() {
                inst.public_ip = public_ip;
            }
            if !private_ip.is_empty() {
                inst.private_ip = private_ip;
            }
        }

        Ok(updated)
    }

    pub async fn up(&self, workers: usize) -> Result<Vec<Instance>> {
        if workers == 0 {
            anyhow::bail!("workers must be greater than 0");
        }

        let existing = self.list_instances().await?;
        let mut existing_by_label: HashMap<String, Vec<&LinodeInstance>> = HashMap::new();
        for instance in &existing {
            existing_by_label
                .entry(instance.label.clone())
                .or_default()
                .push(instance);
        }

        let mut updated = self.config.miners.clone();
        let mut wait_targets: Vec<(String, u64)> = Vec::new();
        let mut matched_existing: HashSet<String> = HashSet::new();

        for inst in &mut updated {
            if inst.public_ip != "TBD" {
                continue;
            }

            let Some(matches) = existing_by_label.get(&inst.name) else {
                continue;
            };

            if matches.len() > 1 {
                let ids = matches
                    .iter()
                    .map(|linode| linode.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "multiple Linodes match node '{}': ids [{}]. Run 'kresko down' to clean duplicates before 'kresko up'.",
                    inst.name,
                    ids,
                );
            }

            let linode = matches[0];
            matched_existing.insert(inst.name.clone());
            let (public_ip, private_ip) = linode_ips(linode);
            if !public_ip.is_empty() {
                inst.public_ip = public_ip;
                inst.private_ip = private_ip;
                println!(
                    "Reusing existing Linode {} (id: {}) -> {}",
                    inst.name, linode.id, inst.public_ip
                );
            } else {
                println!(
                    "Found existing Linode {} (id: {}) without IP yet; waiting...",
                    inst.name, linode.id
                );
                wait_targets.push((inst.name.clone(), linode.id));
            }
        }

        let pending: Vec<&Instance> = updated
            .iter()
            .filter(|instance| {
                instance.public_ip == "TBD" && !matched_existing.contains(&instance.name)
            })
            .collect();

        if pending.is_empty() && wait_targets.is_empty() {
            println!("All instances already have IPs assigned.");
            return Ok(updated);
        }

        if pending.len() > MAX_LINODES {
            anyhow::bail!(
                "Cannot create {} Linodes (max {})",
                pending.len(),
                MAX_LINODES
            );
        }

        if !pending.is_empty() {
            println!("Creating {} Linodes...", pending.len());

            let mut created_targets = Vec::with_capacity(pending.len());
            for chunk in pending.chunks(workers) {
                let create_futs: Vec<_> = chunk
                    .iter()
                    .map(|instance| async move {
                        let id = self.create_instance(instance).await?;
                        Ok::<_, anyhow::Error>((instance.name.clone(), id))
                    })
                    .collect();

                for result in join_all(create_futs).await {
                    match result {
                        Ok(target) => created_targets.push(target),
                        Err(error) => eprintln!("Warning: {error}"),
                    }
                }
            }
            wait_targets.extend(created_targets);
        }

        println!("Waiting for IPs...");
        let mut resolved = Vec::with_capacity(wait_targets.len());
        for chunk in wait_targets.chunks(workers) {
            let ip_futs: Vec<_> = chunk
                .iter()
                .map(|(name, id)| async move {
                    let (public_ip, private_ip) = self.wait_for_ip(*id).await?;
                    Ok::<_, anyhow::Error>((name.clone(), public_ip, private_ip))
                })
                .collect();

            for result in join_all(ip_futs).await {
                match result {
                    Ok(entry) => resolved.push(entry),
                    Err(error) => eprintln!("Warning: {error}"),
                }
            }
        }

        let mut resolved_by_name: HashMap<String, (String, String)> = HashMap::new();
        for (name, public_ip, private_ip) in resolved {
            resolved_by_name.insert(name, (public_ip, private_ip));
        }

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

        let unresolved = updated
            .iter()
            .filter(|inst| inst.public_ip == "TBD")
            .count();
        if unresolved > 0 {
            eprintln!(
                "Warning: {} Linode node(s) still have no public IP and remain unavailable.",
                unresolved
            );
        }

        Ok(updated)
    }

    pub async fn down(&self, workers: usize, all: bool) -> Result<()> {
        if workers == 0 {
            anyhow::bail!("workers must be greater than 0");
        }

        let instances = self.list_instances().await?;
        let targets: Vec<_> = if all {
            instances
                .into_iter()
                .filter(|instance| {
                    instance.group == KRESKO_GROUP
                        || instance.tags.iter().any(|tag| tag == KRESKO_GROUP)
                })
                .collect()
        } else {
            let target_names: HashSet<_> = self
                .config
                .miners
                .iter()
                .map(|instance| instance.name.as_str())
                .collect();
            instances
                .into_iter()
                .filter(|instance| target_names.contains(instance.label.as_str()))
                .collect()
        };

        if targets.is_empty() {
            if all {
                println!("No Linodes found in group '{}'", KRESKO_GROUP);
            } else {
                println!(
                    "No Linodes found for experiment '{}'",
                    self.config.experiment
                );
            }
            return Ok(());
        }

        println!("Destroying {} Linodes...", targets.len());

        for chunk in targets.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|instance| {
                    let id = instance.id;
                    let label = instance.label.clone();
                    async move {
                        self.destroy_instance(id).await?;
                        println!("  Destroyed {} (id: {})", label, id);
                        Ok::<_, anyhow::Error>(())
                    }
                })
                .collect();

            let results = join_all(futs).await;
            for result in results {
                if let Err(error) = result {
                    eprintln!("Warning: {error}");
                }
            }
        }

        Ok(())
    }

    pub async fn list(&self) -> Result<()> {
        let instances = self.list_instances().await?;
        let targets: Vec<_> = if self.config.miners.is_empty() {
            instances
                .into_iter()
                .filter(|instance| {
                    instance.group == KRESKO_GROUP
                        && instance.label.contains(&self.config.experiment)
                })
                .collect()
        } else {
            let target_names: HashSet<_> = self
                .config
                .miners
                .iter()
                .map(|instance| instance.name.as_str())
                .collect();
            instances
                .into_iter()
                .filter(|instance| target_names.contains(instance.label.as_str()))
                .collect()
        };

        if targets.is_empty() {
            println!(
                "No Linodes found for experiment '{}'",
                self.config.experiment
            );
            return Ok(());
        }

        println!(
            "{:<30} {:<12} {:<10} {:<18} {:<18} {:<25}",
            "Name", "Status", "Region", "Type", "Public IP", "Created"
        );
        println!("{}", "-".repeat(120));

        for instance in &targets {
            let (public_ip, _) = linode_ips(instance);
            println!(
                "{:<30} {:<12} {:<10} {:<18} {:<18} {:<25}",
                instance.label,
                instance.status,
                instance.region,
                instance.r#type,
                if public_ip.is_empty() {
                    "N/A"
                } else {
                    &public_ip
                },
                instance.created,
            );
        }

        Ok(())
    }
}

fn linode_ips(instance: &LinodeInstance) -> (String, String) {
    let mut public_ip = String::new();
    let mut private_ip = String::new();

    for ip in &instance.ipv4 {
        match ip.parse::<Ipv4Addr>() {
            Ok(addr) if addr.is_private() => {
                if private_ip.is_empty() {
                    private_ip = ip.clone();
                }
            }
            Ok(_) => {
                if public_ip.is_empty() {
                    public_ip = ip.clone();
                }
            }
            Err(_) => {
                if public_ip.is_empty() {
                    public_ip = ip.clone();
                }
            }
        }
    }

    (public_ip, private_ip)
}

fn random_root_password() -> String {
    let charset = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let mut password = String::from("Aa1!");

    while password.len() < 24 {
        let idx = rng.random_range(0..charset.len());
        password.push(charset[idx] as char);
    }

    password
}

fn should_refresh_ips(instance: &Instance, overwrite: bool) -> bool {
    overwrite
        || instance.public_ip.is_empty()
        || instance.public_ip == "TBD"
        || instance.private_ip.is_empty()
        || instance.private_ip == "TBD"
}

fn format_linode_error_detail(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }

    if let Ok(parsed) = serde_json::from_str::<LinodeErrorBody>(body) {
        let errors: Vec<String> = parsed
            .errors
            .into_iter()
            .map(|error| {
                if error.field.is_empty() {
                    error.reason
                } else {
                    format!("{}: {}", error.field, error.reason)
                }
            })
            .filter(|entry| !entry.trim().is_empty())
            .collect();
        if !errors.is_empty() {
            return format!(" [{}]", errors.join("; "));
        }
    }

    let trimmed = body.trim();
    let excerpt = if trimmed.len() > 400 {
        format!("{}...", &trimmed[..400])
    } else {
        trimmed.to_string()
    };
    format!(" [body={excerpt}]")
}
