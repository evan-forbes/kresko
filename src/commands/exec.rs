use anyhow::{Context, Result};
use base64::Engine;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{Config, Instance, resolve_value, select_instances, shellexpand};
use crate::ssh;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub total: usize,
    pub failed_nodes: Vec<String>,
}

/// What `kresko exec` should run on each node.
pub enum ExecTarget {
    /// Local script file to upload, then execute.
    LocalFile(PathBuf),
    /// Inline command body wrapped in `bash -c`.
    InlineCommand(String),
    /// Script already present at this absolute path on the remote.
    /// Used by `kresko run` for init scripts baked into the payload.
    RemoteFile(String),
}

pub async fn run(
    miners: &str,
    workers: usize,
    directory: &str,
    target: ExecTarget,
    on_failed: bool,
    with_output: bool,
) -> Result<ExecResult> {
    run_with_env(
        miners,
        workers,
        directory,
        target,
        on_failed,
        with_output,
        &[],
    )
    .await
}

/// Like [`run`], but prefixes each remote invocation with `KEY='VAL' ` pairs.
/// Shared env is applied to every node; for per-node overrides (e.g. tier),
/// the init script should read from a file written by genesis instead.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_env(
    miners: &str,
    workers: usize,
    directory: &str,
    target: ExecTarget,
    on_failed: bool,
    with_output: bool,
    shared_env: &[(String, String)],
) -> Result<ExecResult> {
    if workers == 0 {
        anyhow::bail!("workers must be greater than 0");
    }

    let dir = Path::new(directory);
    let config = Config::load(dir)?;
    let key = shellexpand(&resolve_value(
        None,
        "KRESKO_SSH_KEY_PATH",
        &config.ssh_key_path,
    ));

    let all: Vec<Instance> = select_instances(&config.miners, miners)
        .into_iter()
        .cloned()
        .collect();

    let targets: Vec<Instance> = if on_failed {
        let last = load_last_exec(dir).unwrap_or_default();
        all.into_iter()
            .filter(|inst| last.failed_nodes.contains(&inst.name))
            .collect()
    } else {
        all
    };

    if targets.is_empty() {
        println!("No targets.");
        return Ok(ExecResult::default());
    }

    let exec_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let (resolved_remote_path, staged_script) = match &target {
        ExecTarget::RemoteFile(path) => (path.clone(), None),
        ExecTarget::LocalFile(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read script {}", path.display()))?;
            (
                format!("/tmp/kresko-exec-{exec_id}.sh"),
                Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            )
        }
        ExecTarget::InlineCommand(cmd) => {
            let body = format!("#!/bin/bash\nset -e\n{cmd}\n");
            (
                format!("/tmp/kresko-exec-{exec_id}.sh"),
                Some(base64::engine::general_purpose::STANDARD.encode(body.as_bytes())),
            )
        }
    };

    let remote_log = format!("/tmp/kresko-exec-{exec_id}.log");

    println!(
        "exec: running {} on {} node(s)",
        match &target {
            ExecTarget::LocalFile(p) => format!("{}", p.display()),
            ExecTarget::InlineCommand(cmd) => format!("`{}`", cmd.replace('\n', "; ")),
            ExecTarget::RemoteFile(path) => path.clone(),
        },
        targets.len()
    );

    let mut failed = Vec::new();
    let mut captured: Vec<(String, String)> = Vec::new();

    let env_prefix = build_env_prefix(shared_env);

    for chunk in targets.chunks(workers) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|inst| {
                let ip = inst.public_ip.clone();
                let name = inst.name.clone();
                let key = key.clone();
                let remote_path = resolved_remote_path.clone();
                let remote_log = remote_log.clone();
                let staged_script = staged_script.clone();
                let env_prefix = env_prefix.clone();
                async move {
                    let cmd =
                        build_exec_command(&staged_script, &remote_path, &remote_log, &env_prefix);
                    let result = ssh::ssh_exec_capture(&ip, &key, &cmd).await;
                    let captured_output = if with_output {
                        download_remote_log(&ip, &key, &remote_log).await
                    } else {
                        None
                    };
                    (name, result, captured_output)
                }
            })
            .collect();

        for (name, result, output) in join_all(futs).await {
            match result {
                Ok((0, _)) => {
                    println!("  {name}: ok");
                    if let Some(out) = output {
                        captured.push((name, out));
                    }
                }
                Ok((code, _)) => {
                    println!("  {name}: FAILED (exit {code})");
                    failed.push(name.clone());
                    if let Some(out) = output {
                        captured.push((name, out));
                    }
                }
                Err(e) => {
                    eprintln!("  {name}: ssh error: {e}");
                    failed.push(name);
                }
            }
        }
    }

    if with_output {
        for (name, out) in &captured {
            println!("\n----- {name} -----\n{out}");
        }
    }

    let result = ExecResult {
        total: targets.len(),
        failed_nodes: failed,
    };
    save_last_exec(dir, &result)?;

    if !result.failed_nodes.is_empty() {
        println!(
            "\n{}/{} nodes failed: {:?}",
            result.failed_nodes.len(),
            result.total,
            result.failed_nodes
        );
    }
    Ok(result)
}

/// Convenience wrapper for callers (e.g. `kresko run`) that already know the
/// script lives at an absolute path on the remote.
#[allow(clippy::too_many_arguments)]
pub async fn run_remote_path(
    miners: &str,
    workers: usize,
    directory: &str,
    remote_path: &str,
    on_failed: bool,
    with_output: bool,
    shared_env: &[(String, String)],
) -> Result<ExecResult> {
    run_with_env(
        miners,
        workers,
        directory,
        ExecTarget::RemoteFile(remote_path.to_string()),
        on_failed,
        with_output,
        shared_env,
    )
    .await
}

fn build_exec_command(
    staged_script: &Option<String>,
    remote_path: &str,
    remote_log: &str,
    env_prefix: &str,
) -> String {
    if let Some(encoded) = staged_script {
        format!(
            "echo '{encoded}' | base64 -d > {remote_path} && \
             chmod +x {remote_path} && \
             {env_prefix}bash {remote_path} > {remote_log} 2>&1"
        )
    } else {
        format!("{env_prefix}bash {remote_path} > {remote_log} 2>&1")
    }
}

fn build_env_prefix(env: &[(String, String)]) -> String {
    if env.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for (k, v) in env {
        s.push_str(k);
        s.push('=');
        s.push_str(&shell_single_quote(v));
        s.push(' ');
    }
    s
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn download_remote_log(ip: &str, key: &str, remote_log: &str) -> Option<String> {
    match ssh::ssh_exec_capture(ip, key, &format!("cat {remote_log} 2>/dev/null || true")).await {
        Ok((_, stdout)) => Some(stdout),
        Err(_) => None,
    }
}

const LAST_EXEC_FILE: &str = ".kresko-last-exec.json";

fn load_last_exec(dir: &Path) -> Option<ExecResult> {
    let raw = std::fs::read_to_string(dir.join(LAST_EXEC_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_last_exec(dir: &Path, result: &ExecResult) -> Result<()> {
    let path = dir.join(LAST_EXEC_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(result)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
