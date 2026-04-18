use anyhow::{Context, Result, anyhow};
use aws_sdk_s3::Client;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

use crate::config::S3Config;

const DEFAULT_UPLOAD_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_DELAY_SECS: u64 = 3;

/// Create an S3 client from our config.
pub async fn new_client(cfg: &S3Config) -> Result<Client> {
    let creds = aws_sdk_s3::config::Credentials::new(
        &cfg.access_key_id,
        &cfg.secret_access_key,
        None,
        None,
        "kresko",
    );

    let mut builder = aws_sdk_s3::Config::builder()
        .region(aws_sdk_s3::config::Region::new(cfg.region.clone()))
        .credentials_provider(creds)
        .behavior_version_latest();

    if !cfg.endpoint.is_empty() {
        builder = builder.endpoint_url(&cfg.endpoint).force_path_style(true);
    }

    Ok(Client::from_conf(builder.build()))
}

/// Upload a file to S3.
pub async fn upload_file(
    client: &Client,
    cfg: &S3Config,
    bucket: &str,
    key: &str,
    file_path: &Path,
) -> Result<()> {
    let attempts = env_usize("KRESKO_S3_UPLOAD_ATTEMPTS", DEFAULT_UPLOAD_ATTEMPTS).max(1);
    let retry_delay_secs = env_u64(
        "KRESKO_S3_UPLOAD_RETRY_DELAY_SECS",
        DEFAULT_RETRY_DELAY_SECS,
    )
    .max(1);
    let aws_cli_fallback = env_bool("KRESKO_S3_UPLOAD_AWS_CLI_FALLBACK", true);
    let mut last_error = None;

    for attempt in 1..=attempts {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(file_path)
            .await
            .with_context(|| format!("failed to read {}", file_path.display()))?;

        match client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await
        {
            Ok(_) => {
                println!(
                    "Uploaded {} to s3://{}/{}",
                    file_path.display(),
                    bucket,
                    key
                );
                return Ok(());
            }
            Err(err) => {
                let err_msg = format!("{err:#}");
                eprintln!(
                    "S3 upload attempt {attempt}/{attempts} failed for s3://{bucket}/{key}: {err_msg}"
                );
                last_error = Some(err_msg);

                if attempt < attempts {
                    let delay_secs = retry_delay_secs.saturating_mul(attempt as u64);
                    eprintln!("Retrying S3 upload in {delay_secs}s...");
                    sleep(Duration::from_secs(delay_secs)).await;
                }
            }
        }
    }

    if aws_cli_fallback {
        eprintln!("Falling back to aws CLI upload for s3://{bucket}/{key}...");
        match aws_cli_upload(cfg, bucket, key, file_path).await {
            Ok(()) => {
                println!(
                    "Uploaded {} to s3://{}/{} via aws CLI fallback",
                    file_path.display(),
                    bucket,
                    key
                );
                return Ok(());
            }
            Err(err) => {
                let sdk_msg =
                    last_error.unwrap_or_else(|| "unknown SDK upload failure".to_string());
                return Err(anyhow!(
                    "failed to upload {key} to S3 after {attempts} SDK attempt(s): {sdk_msg}; aws CLI fallback failed: {err:#}"
                ));
            }
        }
    }

    Err(anyhow!(
        "failed to upload {key} to S3 after {attempts} SDK attempt(s): {}",
        last_error.unwrap_or_else(|| "unknown SDK upload failure".to_string())
    ))
}

/// Create a presigned GET URL for an object.
pub async fn presign_get_url(
    client: &Client,
    bucket: &str,
    key: &str,
    expires_in: Duration,
) -> Result<String> {
    let presign_config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)
        .context("invalid presign expiration")?;

    let req = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .presigned(presign_config)
        .await
        .with_context(|| format!("failed to presign s3://{bucket}/{key}"))?;

    Ok(req.uri().to_string())
}

async fn aws_cli_upload(cfg: &S3Config, bucket: &str, key: &str, file_path: &Path) -> Result<()> {
    let destination = format!("s3://{bucket}/{key}");
    let mut cmd = Command::new("aws");
    cmd.arg("s3")
        .arg("cp")
        .arg(file_path)
        .arg(&destination)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .env("AWS_DEFAULT_REGION", &cfg.region)
        .env("AWS_REGION", &cfg.region)
        .env("AWS_PAGER", "");

    if !cfg.endpoint.is_empty() {
        cmd.arg("--endpoint-url").arg(&cfg.endpoint);
    }

    let output = cmd
        .output()
        .await
        .context("failed to spawn aws CLI for S3 upload")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("aws exited with status {}", output.status)
        };
        return Err(anyhow!("aws s3 cp failed: {detail}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        println!("{stdout}");
    }

    Ok(())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}
