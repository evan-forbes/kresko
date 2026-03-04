use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use std::path::Path;
use std::time::Duration;

use crate::config::S3Config;

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
pub async fn upload_file(client: &Client, bucket: &str, key: &str, file_path: &Path) -> Result<()> {
    let body = aws_sdk_s3::primitives::ByteStream::from_path(file_path)
        .await
        .with_context(|| format!("failed to read {}", file_path.display()))?;

    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .send()
        .await
        .with_context(|| format!("failed to upload {key} to S3"))?;

    println!(
        "Uploaded {} to s3://{}/{}",
        file_path.display(),
        bucket,
        key
    );
    Ok(())
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

