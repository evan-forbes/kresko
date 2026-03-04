use anyhow::Result;
use futures::future::join_all;
use std::path::Path;

use crate::config::Config;
use crate::s3;

const UPLOAD_CONCURRENCY: usize = 8;

pub async fn run(directory: &str) -> Result<()> {
    let dir = Path::new(directory);
    let config = Config::load(dir)?;

    let data_dir = dir.join("data");
    if !data_dir.exists() {
        anyhow::bail!("No data directory found. Run 'kresko download' first.");
    }

    let s3_client = s3::new_client(&config.s3).await?;
    let files = walkdir(&data_dir)?;

    println!("Uploading {} files...", files.len());

    for chunk in files.chunks(UPLOAD_CONCURRENCY) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|entry| {
                let relative = entry.strip_prefix(&data_dir).unwrap_or(entry);
                let s3_key = format!("{}/data/{}", config.experiment, relative.display());
                let s3_client = &s3_client;
                let bucket = &config.s3.bucket_name;
                async move { s3::upload_file(s3_client, bucket, &s3_key, entry).await }
            })
            .collect();

        let results = join_all(futs).await;
        for r in results {
            r?;
        }
    }

    println!(
        "Data uploaded to s3://{}/{}/data/",
        config.s3.bucket_name, config.experiment
    );
    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path)?);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}
