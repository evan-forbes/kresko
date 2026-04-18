use std::{path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use zebra_chain::transparent;

use crate::config::LocalGenesisFundedKey;

#[derive(Clone)]
pub(crate) struct FundedKey {
    pub name: String,
    pub address: transparent::Address,
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
}

pub(crate) fn load_funded_key(explicit_path: Option<&str>) -> Result<(FundedKey, PathBuf)> {
    let path = resolve_funded_key_path(explicit_path)?;
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read funded key file {}", path.display()))?;

    let raw: LocalGenesisFundedKey =
        serde_json::from_str(&data).context("failed to parse funded key json")?;

    let key_bytes =
        hex::decode(&raw.secret_key_hex).context("funded key secret_key_hex is not valid hex")?;
    if key_bytes.len() != 32 {
        anyhow::bail!(
            "funded key secret_key_hex must decode to 32 bytes, got {}",
            key_bytes.len()
        );
    }

    let secret_key =
        SecretKey::from_slice(&key_bytes).context("funded key secret_key_hex is invalid")?;
    let secp = Secp256k1::new();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);

    if !raw.public_key_hex.is_empty() {
        let expected = hex::decode(&raw.public_key_hex)
            .context("funded key public_key_hex is not valid hex")?;
        if expected != public_key.serialize() {
            anyhow::bail!(
                "funded key file is inconsistent: public_key_hex does not match secret_key_hex"
            );
        }
    }

    let address =
        transparent::Address::from_str(&raw.address).context("funded key address is invalid")?;

    Ok((
        FundedKey {
            name: raw.name,
            address,
            secret_key,
            public_key,
        },
        path,
    ))
}

fn resolve_funded_key_path(explicit_path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("KRESKO_FUNDED_KEY_PATH") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    let config_path = PathBuf::from("/root/.config/funded_key.json");
    if config_path.exists() {
        return Ok(config_path);
    }

    if let Some(parsed_hostname) = detect_parsed_hostname() {
        let payload_path =
            PathBuf::from(format!("/root/payload/{parsed_hostname}/funded_key.json"));
        if payload_path.exists() {
            return Ok(payload_path);
        }
    }

    anyhow::bail!(
        "could not locate funded key file. pass --funded-key-path, or set KRESKO_FUNDED_KEY_PATH"
    )
}

fn detect_parsed_hostname() -> Option<String> {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })?;

    let parts: Vec<&str> = hostname.split('-').collect();
    if parts.len() >= 2 {
        Some(format!("{}-{}", parts[0], parts[1]))
    } else {
        Some(hostname)
    }
}
