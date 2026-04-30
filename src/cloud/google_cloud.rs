use anyhow::{Context, Result};
use base64::Engine;
use futures::future::join_all;
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::config::{
    Config, GCP_REGIONS, Instance, gcp_disk_size_gb, require_env, resolve_value, shellexpand,
};

const COMPUTE_API: &str = "https://compute.googleapis.com/compute/v1";

pub struct GoogleCloudClient {
    config: Config,
    http: Client,
    project: String,
    key_path: String,
    ssh_pub_key: String,
    cached_token: Mutex<Option<(String, Instant)>>,
}

#[derive(Debug, Deserialize)]
struct Operation {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct InstanceResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default, rename = "networkInterfaces")]
    network_interfaces: Vec<NetworkInterface>,
}

#[derive(Debug, Deserialize)]
struct NetworkInterface {
    #[serde(default, rename = "networkIP")]
    network_ip: String,
    #[serde(default, rename = "accessConfigs")]
    access_configs: Vec<AccessConfig>,
}

#[derive(Debug, Deserialize)]
struct AccessConfig {
    #[serde(default, rename = "natIP")]
    nat_ip: String,
}

#[derive(Debug, Deserialize)]
struct InstanceListResponse {
    #[serde(default)]
    items: Vec<InstanceResponse>,
}

#[derive(Debug, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

impl GoogleCloudClient {
    pub fn new(config: Config) -> Result<Self> {
        let project = require_env("GOOGLE_CLOUD_PROJECT")?;

        let key_path = shellexpand(&require_env("GOOGLE_CLOUD_KEY_JSON_PATH")?);

        let ssh_pub_key_path = shellexpand(&resolve_value(
            None,
            "KRESKO_SSH_PUB_KEY_PATH",
            &config.ssh_pub_key_path,
        ));
        if ssh_pub_key_path.is_empty() {
            anyhow::bail!("KRESKO_SSH_PUB_KEY_PATH not set");
        }
        let pub_key = std::fs::read_to_string(&ssh_pub_key_path)
            .with_context(|| format!("failed to read SSH public key from {}", ssh_pub_key_path))?;
        if pub_key.trim().is_empty() {
            anyhow::bail!("SSH public key at {} is empty", ssh_pub_key_path);
        }

        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        Ok(Self {
            config,
            http,
            project,
            key_path,
            ssh_pub_key: pub_key.trim().to_string(),
            cached_token: Mutex::new(None),
        })
    }

    async fn get_access_token(&self) -> Result<String> {
        // Return cached token if still valid (refresh 100s before expiry)
        {
            let cache = self.cached_token.lock().await;
            if let Some((token, fetched_at)) = cache.as_ref() {
                if fetched_at.elapsed() < Duration::from_secs(3500) {
                    return Ok(token.clone());
                }
            }
        }

        let token = self.fetch_access_token().await?;

        {
            let mut cache = self.cached_token.lock().await;
            *cache = Some((token.clone(), Instant::now()));
        }

        Ok(token)
    }

    async fn fetch_access_token(&self) -> Result<String> {
        let key_json = std::fs::read_to_string(&self.key_path)
            .with_context(|| format!("failed to read GCP key from {}", self.key_path))?;

        let sa_key: ServiceAccountKey =
            serde_json::from_str(&key_json).context("failed to parse GCP service account key")?;

        // Build JWT
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"RS256","typ":"JWT"}"#);

        let claims = serde_json::json!({
            "iss": sa_key.client_email,
            "scope": "https://www.googleapis.com/auth/compute",
            "aud": sa_key.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let claims_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());

        let signing_input = format!("{header}.{claims_b64}");

        // Shell out to openssl for RS256 signing (avoids pulling in a full RSA crate)
        let tmp_key = format!("/tmp/kresko_gcp_key_{}_{}.pem", std::process::id(), now);
        tokio::fs::write(&tmp_key, &sa_key.private_key).await?;

        let mut child = tokio::process::Command::new("openssl")
            .args(["dgst", "-sha256", "-sign", &tmp_key, "-binary"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(signing_input.as_bytes()).await?;
        }

        let output = child.wait_with_output().await?;
        let _ = tokio::fs::remove_file(tmp_key).await;

        if !output.status.success() {
            anyhow::bail!(
                "openssl signing failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&output.stdout);
        let jwt = format!("{header}.{claims_b64}.{signature}");

        // Exchange JWT for access token
        let resp: serde_json::Value = self
            .http
            .post(&sa_key.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        resp["access_token"]
            .as_str()
            .map(|s| s.to_string())
            .context("no access_token in GCP token response")
    }

    fn zone_for_region(region: &str) -> String {
        format!("{region}-b")
    }

    async fn create_instance(&self, instance: &Instance, token: &str) -> Result<String> {
        let zone = Self::zone_for_region(&instance.region);

        let body = serde_json::json!({
            "name": instance.name,
            "machineType": format!("zones/{zone}/machineTypes/{}", instance.slug),
            "labels": {
                "kresko": "true",
                "experiment": &self.config.experiment,
            },
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "initializeParams": {
                    "sourceImage": "projects/ubuntu-os-cloud/global/images/family/ubuntu-2204-lts",
                    "diskSizeGb": gcp_disk_size_gb(self.config.network_kind),
                    "diskType": format!("zones/{zone}/diskTypes/pd-ssd"),
                }
            }],
            "networkInterfaces": [{
                "network": "global/networks/default",
                "accessConfigs": [{
                    "type": "ONE_TO_ONE_NAT",
                    "name": "External NAT",
                }]
            }],
            "metadata": {
                "items": [{
                    "key": "ssh-keys",
                    "value": format!("root:{}", self.ssh_pub_key),
                }]
            },
            "tags": {
                "items": ["kresko"]
            }
        });

        let response = self
            .http
            .post(format!(
                "{COMPUTE_API}/projects/{}/zones/{zone}/instances",
                self.project
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("failed to call GCP create instance API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "failed to create GCP instance '{}' (zone='{}', machine='{}'): HTTP {}{}{}",
                instance.name,
                zone,
                instance.slug,
                status.as_u16(),
                status
                    .canonical_reason()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default(),
                format_gcp_error_detail(&body),
            );
        }

        let resp: Operation = response.json().await?;

        println!("Created GCP instance {} (op: {})", instance.name, resp.name);
        Ok(resp.name)
    }

    async fn wait_for_instance_ip(
        &self,
        name: &str,
        zone: &str,
        token: &str,
    ) -> Result<(String, String)> {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;

            let resp: InstanceResponse = self
                .http
                .get(format!(
                    "{COMPUTE_API}/projects/{}/zones/{zone}/instances/{name}",
                    self.project
                ))
                .bearer_auth(token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            if resp.status == "RUNNING" {
                if let Some(iface) = resp.network_interfaces.first() {
                    let private_ip = iface.network_ip.clone();
                    let public_ip = iface
                        .access_configs
                        .first()
                        .map(|a| a.nat_ip.clone())
                        .unwrap_or_default();

                    if !public_ip.is_empty() {
                        return Ok((public_ip, private_ip));
                    }
                }
            }
        }

        anyhow::bail!("Timed out waiting for GCP instance {name} IP");
    }

    async fn delete_instance(&self, name: &str, zone: &str, token: &str) -> Result<()> {
        self.http
            .delete(format!(
                "{COMPUTE_API}/projects/{}/zones/{zone}/instances/{name}",
                self.project
            ))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()
            .context("failed to delete GCP instance")?;

        Ok(())
    }

    async fn ensure_firewall_rule(&self, token: &str) -> Result<()> {
        // Check if rule exists
        let resp = self
            .http
            .get(format!(
                "{COMPUTE_API}/projects/{}/global/firewalls/kresko-allow-all-ports",
                self.project
            ))
            .bearer_auth(token)
            .send()
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        // Create firewall rule
        let body = serde_json::json!({
            "name": "kresko-allow-all-ports",
            "network": format!("projects/{}/global/networks/default", self.project),
            "direction": "INGRESS",
            "allowed": [
                {"IPProtocol": "tcp"},
                {"IPProtocol": "udp"},
            ],
            "sourceRanges": ["0.0.0.0/0"],
            "targetTags": ["kresko"],
        });

        self.http
            .post(format!(
                "{COMPUTE_API}/projects/{}/global/firewalls",
                self.project
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .error_for_status()
            .context("failed to create firewall rule")?;

        println!("Created firewall rule kresko-allow-all-ports");
        Ok(())
    }
}

impl GoogleCloudClient {
    pub async fn matching_resource_names(&self, all: bool) -> Result<Vec<String>> {
        let token = self.get_access_token().await?;
        let filter = if all {
            "labels.kresko%3Dtrue".to_string()
        } else {
            format!("labels.experiment%3D{}", self.config.experiment)
        };
        Ok(self
            .list_instances_all_zones(&token, &filter)
            .await
            .into_iter()
            .map(|(instance, _)| instance.name)
            .collect())
    }

    pub async fn sync_config_ips(&self, overwrite: bool) -> Result<Vec<Instance>> {
        let token = self.get_access_token().await?;
        let found = self.list_config_instances(&token).await;
        let mut instances_by_name: HashMap<String, Vec<(&InstanceResponse, &str)>> = HashMap::new();
        for (instance, zone) in &found {
            instances_by_name
                .entry(instance.name.clone())
                .or_default()
                .push((instance, zone.as_str()));
        }

        let mut updated = self.config.miners.clone();
        for inst in &mut updated {
            if !should_refresh_ips(inst, overwrite) {
                continue;
            }

            let Some(matches) = instances_by_name.get(&inst.name) else {
                continue;
            };

            if matches.len() > 1 {
                let zones = matches
                    .iter()
                    .map(|(_, zone)| zone.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "multiple GCP instances match node '{}': zones [{}]. Clean up duplicates before syncing IPs.",
                    inst.name,
                    zones
                );
            }

            let (response, _) = matches[0];
            let (public_ip, private_ip) = gcp_instance_ips(response);
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

        let token = self.get_access_token().await?;

        self.ensure_firewall_rule(&token).await?;

        let pending: Vec<&Instance> = self
            .config
            .miners
            .iter()
            .filter(|i| i.public_ip == "TBD")
            .collect();

        if pending.is_empty() {
            println!("All instances already have IPs assigned.");
            return Ok(self.config.miners.clone());
        }

        println!("Creating {} GCP instances...", pending.len());

        let mut wait_targets = Vec::new();
        for chunk in pending.chunks(workers) {
            let create_futs: Vec<_> = chunk
                .iter()
                .map(|inst| {
                    let zone = Self::zone_for_region(&inst.region);
                    let token = token.clone();
                    async move {
                        self.create_instance(inst, &token).await?;
                        Ok::<_, anyhow::Error>((inst.name.clone(), zone))
                    }
                })
                .collect();

            for result in join_all(create_futs).await {
                match result {
                    Ok(target) => wait_targets.push(target),
                    Err(error) => eprintln!("Warning: {error}"),
                }
            }
        }

        // Wait for IPs
        println!("Waiting for IPs...");
        let mut resolved = Vec::with_capacity(pending.len());
        for chunk in wait_targets.chunks(workers) {
            let ip_futs: Vec<_> = chunk
                .iter()
                .map(|(name, zone)| {
                    let name = name.clone();
                    let zone = zone.clone();
                    let token = token.clone();
                    async move {
                        let (public_ip, private_ip) =
                            self.wait_for_instance_ip(&name, &zone, &token).await?;
                        Ok::<_, anyhow::Error>((name, public_ip, private_ip))
                    }
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

        let mut updated = self.config.miners.clone();
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
                "Warning: {} GCP node(s) still have no public IP and remain unavailable.",
                unresolved
            );
        }

        Ok(updated)
    }

    async fn list_instances_all_zones(
        &self,
        token: &str,
        filter: &str,
    ) -> Vec<(InstanceResponse, String)> {
        let futs: Vec<_> = GCP_REGIONS
            .iter()
            .map(|region| {
                let zone = Self::zone_for_region(region);
                let url = format!(
                    "{COMPUTE_API}/projects/{}/zones/{zone}/instances?filter={filter}",
                    self.project
                );
                let token = token.to_string();
                let http = self.http.clone();
                async move {
                    let resp = http.get(&url).bearer_auth(&token).send().await.ok()?;
                    if resp.status().is_success() {
                        let list: InstanceListResponse = resp.json().await.ok()?;
                        Some(
                            list.items
                                .into_iter()
                                .map(|inst| (inst, zone.clone()))
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        None
                    }
                }
            })
            .collect();

        join_all(futs)
            .await
            .into_iter()
            .flatten()
            .flatten()
            .collect()
    }

    async fn list_config_instances(&self, token: &str) -> Vec<(InstanceResponse, String)> {
        let target_names: HashSet<_> = self
            .config
            .miners
            .iter()
            .map(|instance| instance.name.as_str())
            .collect();

        self.list_instances_all_zones(
            token,
            &format!("labels.experiment%3D{}", self.config.experiment),
        )
        .await
        .into_iter()
        .filter(|(instance, _)| target_names.contains(instance.name.as_str()))
        .collect()
    }

    pub async fn down(&self, workers: usize, all: bool) -> Result<()> {
        if workers == 0 {
            anyhow::bail!("workers must be greater than 0");
        }

        let token = self.get_access_token().await?;

        let filter = if all {
            "labels.kresko%3Dtrue".to_string()
        } else {
            format!("labels.experiment%3D{}", self.config.experiment)
        };
        let found = self.list_instances_all_zones(&token, &filter).await;
        let all_instances: Vec<_> = found
            .iter()
            .map(|(i, z)| (i.name.clone(), z.clone()))
            .collect();

        if all_instances.is_empty() {
            if all {
                println!("No GCP instances found with label kresko=true");
            } else {
                println!(
                    "No GCP instances found for experiment '{}'",
                    self.config.experiment
                );
            }
            return Ok(());
        }

        println!("Destroying {} GCP instances...", all_instances.len());

        for chunk in all_instances.chunks(workers) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|(name, zone)| {
                    let name = name.clone();
                    let zone = zone.clone();
                    let token = token.clone();
                    async move {
                        self.delete_instance(&name, &zone, &token).await?;
                        println!("  Destroyed {name}");
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
        let token = self.get_access_token().await?;

        let instances = self
            .list_instances_all_zones(&token, "labels.kresko%3Dtrue")
            .await;

        println!(
            "{:<30} {:<12} {:<15} {:<18}",
            "Name", "Status", "Zone", "Public IP"
        );
        println!("{}", "-".repeat(75));

        for (inst, zone) in &instances {
            let public_ip = inst
                .network_interfaces
                .first()
                .and_then(|i| i.access_configs.first())
                .map(|a| a.nat_ip.as_str())
                .unwrap_or("N/A");

            println!(
                "{:<30} {:<12} {:<15} {:<18}",
                inst.name, inst.status, zone, public_ip
            );
        }

        Ok(())
    }
}

fn format_gcp_error_detail(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(error) = value.get("error") {
            if let Some(message) = error.get("message").and_then(|value| value.as_str()) {
                return format!(" [message={message}]");
            }
            if let Some(errors) = error.get("errors").and_then(|value| value.as_array()) {
                let joined = errors
                    .iter()
                    .filter_map(|entry| {
                        let reason = entry.get("reason").and_then(|value| value.as_str());
                        let message = entry.get("message").and_then(|value| value.as_str());
                        match (reason, message) {
                            (Some(reason), Some(message)) => Some(format!("{reason}: {message}")),
                            (Some(reason), None) => Some(reason.to_string()),
                            (None, Some(message)) => Some(message.to_string()),
                            (None, None) => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !joined.is_empty() {
                    return format!(" [{}]", joined.join("; "));
                }
            }
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

fn gcp_instance_ips(instance: &InstanceResponse) -> (String, String) {
    let Some(iface) = instance.network_interfaces.first() else {
        return (String::new(), String::new());
    };

    let public_ip = iface
        .access_configs
        .first()
        .map(|config| config.nat_ip.clone())
        .unwrap_or_default();
    let private_ip = iface.network_ip.clone();
    (public_ip, private_ip)
}

fn should_refresh_ips(instance: &Instance, overwrite: bool) -> bool {
    overwrite
        || instance.public_ip.is_empty()
        || instance.public_ip == "TBD"
        || instance.private_ip.is_empty()
        || instance.private_ip == "TBD"
}
