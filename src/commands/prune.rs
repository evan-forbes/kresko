use anyhow::Result;

use crate::config::Config;

pub async fn run(directory: &str, dry_run: bool) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let mut config = Config::load(dir)?;

    let (failed, kept): (Vec<_>, Vec<_>) = config
        .miners
        .into_iter()
        .partition(|inst| inst.public_ip == "TBD");

    if failed.is_empty() {
        println!("No TBD instances to prune.");
        return Ok(());
    }

    println!(
        "{} {} instance(s) with public_ip=TBD:",
        if dry_run { "Would prune" } else { "Pruning" },
        failed.len()
    );
    for inst in &failed {
        println!(
            "  - {} ({}, {}, {})",
            inst.name, inst.provider, inst.region, inst.slug
        );
    }

    config.miners = kept;

    if dry_run {
        println!("Dry run: config not modified. Re-run without --dry-run to apply.");
    } else {
        config.save(dir)?;
        println!("Pruned. {} instance(s) remain.", config.miners.len());
    }

    Ok(())
}
