use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::config::{Config, resolve_value, select_instances, shellexpand};
use crate::ssh;

const REMOTE_STATUS_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Debug, Clone, Serialize)]
pub struct TxblastNodeStatus {
    pub name: String,
    pub ip: String,
    pub status: String,
    pub ready: bool,
    pub phase: Option<String>,
    pub last_height: Option<u32>,
    pub ready_height: Option<u32>,
    pub ready_lanes: usize,
    pub target_ready_lanes: usize,
    pub reservoir_count: usize,
    pub treasury_backlog: usize,
    pub pending_total: usize,
    pub pending_fanout: usize,
    pub pending_reseed: usize,
    pub within_pending_limits: bool,
    pub last_timestamp: Option<String>,
    pub age_secs: Option<i64>,
    pub stall_reason: Option<String>,
    pub trace_dir: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TxblastStatusReport {
    pub nodes: Vec<TxblastNodeStatus>,
    pub total: usize,
    pub ready_nodes: usize,
    pub not_ready_nodes: usize,
    pub errored_nodes: usize,
    pub all_ready: bool,
    pub min_ready_height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TxblastLocalStatus {
    pub node: String,
    pub status: String,
    pub ready: bool,
    pub phase: Option<String>,
    pub last_height: Option<u32>,
    pub ready_height: Option<u32>,
    pub ready_lanes: usize,
    pub target_ready_lanes: usize,
    pub reservoir_count: usize,
    pub treasury_backlog: usize,
    pub pending_total: usize,
    pub pending_fanout: usize,
    pub pending_reseed: usize,
    pub within_pending_limits: bool,
    pub last_timestamp: Option<String>,
    pub age_secs: Option<i64>,
    pub stall_reason: Option<String>,
    pub trace_dir: String,
    pub registry_path: String,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryRecord {
    ts: String,
    node: String,
    height: Option<u32>,
    phase: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    reason: Option<String>,
    ready_lanes: usize,
    pending_lanes: usize,
    pending_fanout: usize,
    pending_reseed: usize,
    reservoir_count: usize,
    treasury_backlog: usize,
    max_in_flight: usize,
    target_ready_lanes: usize,
}

pub async fn run(
    instances: &str,
    json: bool,
    trace_dir: &str,
    stall_secs: i64,
    directory: &str,
) -> Result<()> {
    let report = query(instances, trace_dir, stall_secs, directory).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.nodes.is_empty() {
        println!("No matching txblast nodes found.");
        return Ok(());
    }

    println!(
        "{:<30} {:<18} {:<12} {:<8} {:<12} {:<11} {:<12}",
        "Name", "IP", "Status", "Height", "Phase", "ReadyLanes", "Stall"
    );
    println!("{}", "-".repeat(110));

    for node in &report.nodes {
        let height = node
            .last_height
            .map(|value| value.to_string())
            .unwrap_or_else(|| "N/A".to_owned());
        let phase = node.phase.as_deref().unwrap_or("unknown");
        let ready_lanes = format!("{}/{}", node.ready_lanes, node.target_ready_lanes);
        let stall = node.stall_reason.as_deref().unwrap_or("-");
        println!(
            "{:<30} {:<18} {:<12} {:<8} {:<12} {:<11} {:<12}",
            node.name, node.ip, node.status, height, phase, ready_lanes, stall
        );
    }

    println!();
    println!(
        "ready={}/{} all_ready={} min_ready_height={}",
        report.ready_nodes,
        report.total,
        report.all_ready,
        report
            .min_ready_height
            .map(|height| height.to_string())
            .unwrap_or_else(|| "N/A".to_owned())
    );

    Ok(())
}

pub fn run_local(json: bool, trace_dir: &str, stall_secs: i64) -> Result<()> {
    let status = query_local(trace_dir, stall_secs);

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    println!(
        "node={} status={} ready={} phase={} height={} ready_lanes={}/{} reservoirs={} pending={} stall_reason={}",
        status.node,
        status.status,
        status.ready,
        status.phase.as_deref().unwrap_or("unknown"),
        status
            .last_height
            .map(|value| value.to_string())
            .unwrap_or_else(|| "N/A".to_owned()),
        status.ready_lanes,
        status.target_ready_lanes,
        status.reservoir_count,
        status.pending_total,
        status.stall_reason.as_deref().unwrap_or("-"),
    );
    if let Some(error) = &status.error {
        println!("error={error}");
    }
    println!("trace_dir={}", status.trace_dir);
    println!("registry_path={}", status.registry_path);

    Ok(())
}

pub async fn query(
    instances: &str,
    trace_dir: &str,
    stall_secs: i64,
    directory: &str,
) -> Result<TxblastStatusReport> {
    let dir = Path::new(directory);
    let config = Config::load(dir)?;
    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);
    let targets = select_instances(&config.miners, instances);

    if targets.is_empty() {
        return Ok(TxblastStatusReport {
            nodes: Vec::new(),
            total: 0,
            ready_nodes: 0,
            not_ready_nodes: 0,
            errored_nodes: 0,
            all_ready: false,
            min_ready_height: None,
        });
    }

    let futures = targets.iter().map(|inst| {
        let name = inst.name.clone();
        let ip = inst.public_ip.clone();
        let key = key.clone();
        let trace_dir = trace_dir.to_owned();
        let local_command = format!(
            "source /root/payload/vars.sh && kresko txblast-status-local --json --trace-dir {} --stall-secs {}",
            shell_single_quote(&trace_dir), stall_secs,
        );
        let command = format!("bash -lc {}", shell_single_quote(&local_command));

        async move {
            match ssh::ssh_exec_timeout(
                &ip,
                &key,
                &command,
                REMOTE_STATUS_COMMAND_TIMEOUT,
            )
            .await
            {
                Ok(output) => match serde_json::from_str::<TxblastLocalStatus>(&output) {
                    Ok(local) => TxblastNodeStatus {
                        name,
                        ip,
                        status: local.status,
                        ready: local.ready,
                        phase: local.phase,
                        last_height: local.last_height,
                        ready_height: local.ready_height,
                        ready_lanes: local.ready_lanes,
                        target_ready_lanes: local.target_ready_lanes,
                        reservoir_count: local.reservoir_count,
                        treasury_backlog: local.treasury_backlog,
                        pending_total: local.pending_total,
                        pending_fanout: local.pending_fanout,
                        pending_reseed: local.pending_reseed,
                        within_pending_limits: local.within_pending_limits,
                        last_timestamp: local.last_timestamp,
                        age_secs: local.age_secs,
                        stall_reason: local.stall_reason,
                        trace_dir: local.trace_dir,
                        error: local.error,
                    },
                    Err(error) => TxblastNodeStatus {
                        name,
                        ip,
                        status: "error".to_owned(),
                        ready: false,
                        phase: None,
                        last_height: None,
                        ready_height: None,
                        ready_lanes: 0,
                        target_ready_lanes: 0,
                        reservoir_count: 0,
                        treasury_backlog: 0,
                        pending_total: 0,
                        pending_fanout: 0,
                        pending_reseed: 0,
                        within_pending_limits: false,
                        last_timestamp: None,
                        age_secs: None,
                        stall_reason: Some("invalid_status_json".to_owned()),
                        trace_dir,
                        error: Some(format!("failed to parse remote status JSON: {error}")),
                    },
                },
                Err(error) => TxblastNodeStatus {
                    name,
                    ip,
                    status: "error".to_owned(),
                    ready: false,
                    phase: None,
                    last_height: None,
                    ready_height: None,
                    ready_lanes: 0,
                    target_ready_lanes: 0,
                    reservoir_count: 0,
                    treasury_backlog: 0,
                    pending_total: 0,
                    pending_fanout: 0,
                    pending_reseed: 0,
                    within_pending_limits: false,
                    last_timestamp: None,
                    age_secs: None,
                    stall_reason: Some("remote_command_failed".to_owned()),
                    trace_dir,
                    error: Some(error.to_string()),
                },
            }
        }
    });

    let nodes = join_all(futures).await;
    let ready_nodes = nodes.iter().filter(|node| node.ready).count();
    let errored_nodes = nodes.iter().filter(|node| node.status == "error").count();
    let not_ready_nodes = nodes.len().saturating_sub(ready_nodes);
    let min_ready_height = nodes.iter().filter_map(|node| node.ready_height).min();

    Ok(TxblastStatusReport {
        total: nodes.len(),
        ready_nodes,
        not_ready_nodes,
        errored_nodes,
        all_ready: !nodes.is_empty() && ready_nodes == nodes.len(),
        min_ready_height,
        nodes,
    })
}

fn query_local(trace_dir: &str, stall_secs: i64) -> TxblastLocalStatus {
    let trace_dir = PathBuf::from(trace_dir);
    let registry_path = trace_dir.join("txblast_registry.jsonl");

    let default = || TxblastLocalStatus {
        node: node_name(),
        status: "error".to_owned(),
        ready: false,
        phase: None,
        last_height: None,
        ready_height: None,
        ready_lanes: 0,
        target_ready_lanes: 0,
        reservoir_count: 0,
        treasury_backlog: 0,
        pending_total: 0,
        pending_fanout: 0,
        pending_reseed: 0,
        within_pending_limits: false,
        last_timestamp: None,
        age_secs: None,
        stall_reason: None,
        trace_dir: trace_dir.display().to_string(),
        registry_path: registry_path.display().to_string(),
        error: None,
    };

    let record = match read_last_registry_record(&registry_path) {
        Ok(Some(record)) => record,
        Ok(None) => {
            let mut status = default();
            status.stall_reason = Some(if registry_path.is_file() {
                "no_registry_records".to_owned()
            } else {
                "trace_missing".to_owned()
            });
            status.error = Some(format!(
                "missing txblast registry trace at {}",
                registry_path.display()
            ));
            return status;
        }
        Err(error) => {
            let mut status = default();
            status.stall_reason = Some("trace_read_failed".to_owned());
            status.error = Some(error.to_string());
            return status;
        }
    };

    let pending_total = record.pending_lanes + record.pending_fanout + record.pending_reseed;
    let within_pending_limits = pending_total <= record.max_in_flight;
    let ready = is_ready_record(&record, within_pending_limits);
    let age_secs = parse_age_secs(&record.ts);
    let stall_reason =
        compute_stall_reason(&record, ready, within_pending_limits, age_secs, stall_secs);
    let effective_stall_secs = stall_secs_for_record(&record, stall_secs);
    let status = if ready {
        "ready"
    } else if record.phase == "recovering" {
        "recovering"
    } else if is_stalled(age_secs, effective_stall_secs) {
        "stalled"
    } else {
        "warming_up"
    };

    TxblastLocalStatus {
        node: record.node,
        status: status.to_owned(),
        ready,
        phase: Some(record.phase.clone()),
        last_height: record.height,
        ready_height: ready.then_some(record.height).flatten(),
        ready_lanes: record.ready_lanes,
        target_ready_lanes: record.target_ready_lanes,
        reservoir_count: record.reservoir_count,
        treasury_backlog: record.treasury_backlog,
        pending_total,
        pending_fanout: record.pending_fanout,
        pending_reseed: record.pending_reseed,
        within_pending_limits,
        last_timestamp: Some(record.ts),
        age_secs,
        stall_reason,
        trace_dir: trace_dir.display().to_string(),
        registry_path: registry_path.display().to_string(),
        error: None,
    }
}

fn read_last_registry_record(path: &Path) -> Result<Option<RegistryRecord>> {
    if !path.is_file() {
        return Ok(None);
    }

    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut last_line = None;

    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            last_line = Some(line);
        }
    }

    let Some(last_line) = last_line else {
        return Ok(None);
    };

    let record = serde_json::from_str(&last_line)
        .with_context(|| format!("failed to parse latest registry line in {}", path.display()))?;
    Ok(Some(record))
}

fn parse_age_secs(ts: &str) -> Option<i64> {
    let parsed = DateTime::parse_from_rfc3339(ts).ok()?;
    Some((Utc::now() - parsed.with_timezone(&Utc)).num_seconds())
}

fn is_stalled(age_secs: Option<i64>, stall_secs: i64) -> bool {
    age_secs.is_some_and(|age| age >= stall_secs)
}

fn is_proving_record(record: &RegistryRecord) -> bool {
    record.event == "build_start"
        || record
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("proving_"))
}

fn stall_secs_for_record(record: &RegistryRecord, stall_secs: i64) -> i64 {
    if is_proving_record(record) {
        stall_secs.max(600)
    } else {
        stall_secs
    }
}

fn live_note_count(record: &RegistryRecord) -> usize {
    record.ready_lanes + record.reservoir_count
}

fn is_ready_record(record: &RegistryRecord, within_pending_limits: bool) -> bool {
    if !within_pending_limits {
        return false;
    }

    match record.phase.as_str() {
        "bootstrap_scan" | "bootstrap_shield" => {
            live_note_count(record) >= record.target_ready_lanes
        }
        "steady_state" => live_note_count(record) > 0,
        _ => false,
    }
}

fn compute_stall_reason(
    record: &RegistryRecord,
    ready: bool,
    within_pending_limits: bool,
    age_secs: Option<i64>,
    stall_secs: i64,
) -> Option<String> {
    if ready {
        return None;
    }

    if !within_pending_limits {
        return Some("pending_limit_exceeded".to_owned());
    }

    let stale = is_stalled(age_secs, stall_secs_for_record(record, stall_secs));

    if let Some(reason) = record.reason.as_deref() {
        return Some(if stale {
            format!("{reason}_stalled")
        } else {
            reason.to_owned()
        });
    }

    match record.phase.as_str() {
        "bootstrap_scan" => {
            if record.event == "runtime_startup" {
                return Some(if stale {
                    "starting_up_stalled".to_owned()
                } else {
                    "starting_up".to_owned()
                });
            }

            if live_note_count(record) == 0 && record.treasury_backlog == 0 {
                if stale {
                    Some("awaiting_transparent_runtime_funds_stalled".to_owned())
                } else {
                    Some("awaiting_transparent_runtime_funds".to_owned())
                }
            } else if stale {
                Some("bootstrap_scan_stalled".to_owned())
            } else {
                Some("awaiting_bootstrap_shield".to_owned())
            }
        }
        "bootstrap_shield" => {
            if record.pending_lanes + record.pending_fanout + record.pending_reseed > 0 {
                if stale {
                    Some("awaiting_shielding_confirmation_stalled".to_owned())
                } else {
                    Some("awaiting_shielding_confirmation".to_owned())
                }
            } else if stale {
                Some("bootstrap_shield_stalled".to_owned())
            } else {
                Some("awaiting_ready_lanes".to_owned())
            }
        }
        "steady_state" => {
            let pending_total =
                record.pending_lanes + record.pending_fanout + record.pending_reseed;
            if live_note_count(record) == 0 {
                if pending_total > 0 {
                    if stale {
                        Some("awaiting_lane_confirmation_stalled".to_owned())
                    } else {
                        Some("awaiting_lane_confirmation".to_owned())
                    }
                } else if stale {
                    Some("lanes_exhausted_stalled".to_owned())
                } else {
                    Some("lanes_exhausted".to_owned())
                }
            } else if stale {
                Some("stale_status".to_owned())
            } else {
                Some("warming_up".to_owned())
            }
        }
        "recovering" => Some(if stale {
            "recovering_stalled".to_owned()
        } else {
            "recovering".to_owned()
        }),
        _ => {
            if stale {
                Some("stale_status".to_owned())
            } else {
                Some("unknown_phase".to_owned())
            }
        }
    }
}

fn node_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(phase: &str) -> RegistryRecord {
        RegistryRecord {
            ts: Utc::now().to_rfc3339(),
            node: "miner-0".to_owned(),
            height: Some(250),
            phase: phase.to_owned(),
            event: "test".to_owned(),
            reason: None,
            ready_lanes: 0,
            pending_lanes: 0,
            pending_fanout: 0,
            pending_reseed: 0,
            reservoir_count: 0,
            treasury_backlog: 0,
            max_in_flight: 256,
            target_ready_lanes: 200,
        }
    }

    #[test]
    fn steady_state_ready_requires_live_lanes_only() {
        let mut ready = record("steady_state");
        ready.ready_lanes = 1;

        let pending_total = ready.pending_lanes + ready.pending_fanout + ready.pending_reseed;
        let within_pending_limits = pending_total <= ready.max_in_flight;
        let is_ready = is_ready_record(&ready, within_pending_limits);

        assert!(is_ready);
        assert_eq!(
            compute_stall_reason(&ready, is_ready, within_pending_limits, Some(0), 120),
            None
        );
    }

    #[test]
    fn bootstrap_scan_without_funds_has_specific_reason() {
        let pending_total = 0usize;
        let within_pending_limits = pending_total <= 256;
        let status = compute_stall_reason(
            &record("bootstrap_scan"),
            false,
            within_pending_limits,
            Some(10),
            120,
        );

        assert_eq!(
            status.as_deref(),
            Some("awaiting_transparent_runtime_funds")
        );
    }

    #[test]
    fn bootstrap_scan_prefers_explicit_runtime_funding_reason() {
        let mut stalled = record("bootstrap_scan");
        stalled.reason = Some("awaiting_runtime_funding_visibility".to_owned());
        let pending_total = 0usize;
        let within_pending_limits = pending_total <= 256;
        let status = compute_stall_reason(&stalled, false, within_pending_limits, Some(10), 120);

        assert_eq!(
            status.as_deref(),
            Some("awaiting_runtime_funding_visibility")
        );
    }

    #[test]
    fn startup_registry_record_surfaces_starting_up() {
        let mut startup = record("bootstrap_scan");
        startup.event = "runtime_startup".to_owned();
        let pending_total = 0usize;
        let within_pending_limits = pending_total <= 256;
        let status = compute_stall_reason(&startup, false, within_pending_limits, Some(10), 120);

        assert_eq!(status.as_deref(), Some("starting_up"));
    }

    #[test]
    fn steady_state_prefers_explicit_proving_reason() {
        let mut steady = record("steady_state");
        steady.reason = Some("proving_lane_advance".to_owned());
        let status = compute_stall_reason(&steady, false, true, Some(10), 120);

        assert_eq!(status.as_deref(), Some("proving_lane_advance"));
    }

    #[test]
    fn proving_records_get_extended_stall_budget() {
        let mut bootstrap = record("bootstrap_shield");
        bootstrap.event = "build_start".to_owned();
        bootstrap.reason = Some("proving_bootstrap_shield".to_owned());

        assert_eq!(stall_secs_for_record(&bootstrap, 120), 600);
        assert_eq!(
            compute_stall_reason(&bootstrap, false, true, Some(300), 120).as_deref(),
            Some("proving_bootstrap_shield")
        );
        assert_eq!(
            compute_stall_reason(&bootstrap, false, true, Some(601), 120).as_deref(),
            Some("proving_bootstrap_shield_stalled")
        );
    }

    #[test]
    fn steady_state_with_no_live_lanes_reports_exhaustion() {
        let status = compute_stall_reason(&record("steady_state"), false, true, Some(10), 120);

        assert_eq!(status.as_deref(), Some("lanes_exhausted"));
    }

    #[test]
    fn recovering_phase_has_recovering_reason() {
        let status = compute_stall_reason(&record("recovering"), false, true, Some(10), 120);

        assert_eq!(status.as_deref(), Some("recovering"));
    }
}
