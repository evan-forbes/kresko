use anyhow::Result;
use std::time::Duration;

use crate::commands::fund_runtime_keys;
use crate::config::{Config, TxType, resolve_value, select_instances, shellexpand};
use crate::tmux;
use crate::txblast::OrchardBlastRuntimeConfig;

pub async fn run(
    instances: &str,
    tx_type: TxType,
    rate: u64,
    amount: f64,
    orchard_max_in_flight: Option<usize>,
    orchard_target_ready_lanes: Option<usize>,
    orchard_lane_low_watermark: Option<usize>,
    orchard_fanout_max_in_flight: Option<usize>,
    orchard_progress_interval_secs: Option<u64>,
    trace_enable: bool,
    trace_dir: Option<&str>,
    directory: &str,
) -> Result<()> {
    let dir = std::path::Path::new(directory);
    let config = Config::load(dir)?;
    let orchard_runtime = OrchardBlastRuntimeConfig::from_parts(
        config.orchard_txblast.clone(),
        orchard_max_in_flight,
        orchard_target_ready_lanes,
        orchard_lane_low_watermark,
        orchard_fanout_max_in_flight,
        orchard_progress_interval_secs,
    )?;

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);

    let targets = select_instances(&config.miners, instances);

    if targets.is_empty() {
        println!("No matching instances found.");
        return Ok(());
    }

    println!(
        "Starting txblast on {} nodes (type={tx_type}, rate={rate}/s, amount={amount})...",
        targets.len()
    );

    let expected_runtime_funding_txid = match tx_type {
        TxType::Shielded | TxType::Both => {
            fund_runtime_keys::expected_funding_txid(directory).await?
        }
        TxType::Transparent => None,
    };

    let mut tail_lines = vec![format!(
        "    --orchard-progress-interval-secs {}",
        orchard_runtime.progress_interval.as_secs()
    )];
    if trace_enable || trace_dir.is_some() {
        let trace_dir = trace_dir.unwrap_or("/root/.cache/kresko/txblast-traces");
        tail_lines.push("    --trace-enable".to_owned());
        tail_lines.push(format!("    --trace-dir {}", shell_single_quote(trace_dir)));
    }
    if let Some(txid) = expected_runtime_funding_txid.as_deref() {
        tail_lines.push(format!(
            "    --expected-runtime-funding-txid {}",
            shell_single_quote(txid)
        ));
    }
    let tail_args = tail_lines.join(" \\\n");

    let script = format!(
        r#"#!/bin/bash
kresko txblast-local \
    --rpc-endpoint http://localhost:18232 \
    --tx-type {tx_type} \
    --rate {rate} \
    --amount {amount} \
    --orchard-lanes-per-miner {} \
    --orchard-lane-value-zats {} \
    --orchard-fanout-source-value-zats {} \
    --orchard-fanout-outputs {} \
    --orchard-max-in-flight {} \
    --orchard-target-ready-lanes {} \
    --orchard-lane-low-watermark {} \
    --orchard-fanout-max-in-flight {} \
{tail_args}
"#,
        orchard_runtime.lane_premine.lanes_per_miner,
        orchard_runtime.lane_premine.lane_value_zats,
        orchard_runtime.lane_premine.fanout_source_value_zats,
        orchard_runtime.lane_premine.fanout_outputs,
        orchard_runtime.max_in_flight,
        orchard_runtime.target_ready_lanes,
        orchard_runtime.lane_low_watermark,
        orchard_runtime.fanout_max_in_flight,
        tail_args = tail_args,
    );

    let owned_targets: Vec<_> = targets.into_iter().cloned().collect();
    let results = tmux::run_script_in_tmux(
        &owned_targets,
        &key,
        &script,
        "txblast",
        Duration::from_secs(30),
    )
    .await;

    for (name, result) in &results {
        match result {
            Ok(()) => println!("  {name}: txblast started"),
            Err(e) => eprintln!("  {name}: failed: {e}"),
        }
    }

    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
