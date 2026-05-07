use anyhow::{Context, Result};
use std::time::Duration;
use tokio::process::Command;

const SSH_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "ConnectTimeout=10",
    // Drop dead connections after ~3 minutes of silence so transfers can't hang forever.
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=6",
];

const SSH_OPTS_LONG_CONNECT: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "ConnectTimeout=60",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=10",
];

/// Execute a command on a remote host via SSH with a timeout.
pub async fn ssh_exec_timeout(
    host: &str,
    key: &str,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    ssh_exec_timeout_with_opts(host, key, command, timeout, SSH_OPTS).await
}

/// Execute a command on a remote host via SSH with a longer connection timeout.
pub async fn ssh_exec_long_connect_timeout(
    host: &str,
    key: &str,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    ssh_exec_timeout_with_opts(host, key, command, timeout, SSH_OPTS_LONG_CONNECT).await
}

async fn ssh_exec_timeout_with_opts(
    host: &str,
    key: &str,
    command: &str,
    timeout: Duration,
    opts: &[&str],
) -> Result<String> {
    let fut = Command::new("ssh")
        .args(opts)
        .args(["-i", key, &format!("root@{host}"), command])
        .output();

    let output = tokio::time::timeout(timeout, fut)
        .await
        .with_context(|| format!("SSH to {host} timed out"))?
        .with_context(|| format!("SSH to {host} failed"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
