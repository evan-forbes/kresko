use anyhow::Result;
use tokio::try_join;

use crate::commands::{download, download_heights};
use crate::config::{Config, select_instances};

pub async fn run(
    nodes: &str,
    workers: usize,
    trace_tables: &str,
    directory: &str,
    data_subdir: Option<&str>,
) -> Result<()> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let active_count = select_instances(&config.miners, nodes).len();

    if active_count == 0 {
        println!("No matching active nodes found for collection.");
        return Ok(());
    }

    download::run_logs(nodes, workers, false, directory, data_subdir).await?;
    try_join!(
        download_heights::run(nodes, workers, None, false, directory, data_subdir),
        download::run_traces(nodes, workers, trace_tables, directory, data_subdir),
    )?;

    Ok(())
}
