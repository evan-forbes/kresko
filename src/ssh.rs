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

/// Upload a file via SCP.
pub async fn scp_upload(host: &str, key: &str, local_path: &str, remote_path: &str) -> Result<()> {
    let output = Command::new("scp")
        .args(SSH_OPTS)
        .args(["-i", key, local_path, &format!("root@{host}:{remote_path}")])
        .output()
        .await
        .with_context(|| format!("SCP to {host} failed"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SCP to {host} failed: {stderr}");
    }

    Ok(())
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
