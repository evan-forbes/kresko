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
];

/// Execute a command on a remote host via SSH.
pub async fn ssh_exec(host: &str, key: &str, command: &str) -> Result<String> {
    ssh_exec_timeout(host, key, command, Duration::from_secs(300)).await
}

/// Execute a command on a remote host via SSH with a timeout.
pub async fn ssh_exec_timeout(
    host: &str,
    key: &str,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    let fut = Command::new("ssh")
        .args(SSH_OPTS)
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

/// Execute a command on a remote host via SSH, capturing the exit code rather
/// than treating non-zero as an error. Returns Err only if the SSH transport
/// itself fails (e.g. host unreachable).
pub async fn ssh_exec_capture(host: &str, key: &str, command: &str) -> Result<(i32, String)> {
    let output = Command::new("ssh")
        .args(SSH_OPTS)
        .args(["-i", key, &format!("root@{host}"), command])
        .output()
        .await
        .with_context(|| format!("SSH to {host} failed"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((code, stdout))
}

/// Download a file via SCP (sftp-like).
pub async fn sftp_download(
    host: &str,
    key: &str,
    remote_path: &str,
    local_path: &str,
) -> Result<()> {
    let output = Command::new("scp")
        .args(SSH_OPTS)
        .args(["-i", key, &format!("root@{host}:{remote_path}"), local_path])
        .output()
        .await
        .with_context(|| format!("SFTP download from {host} failed"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SFTP download from {host} failed: {stderr}");
    }

    Ok(())
}
