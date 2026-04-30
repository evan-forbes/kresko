use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    let status = Command::new("scp")
        .args(SSH_OPTS)
        .args(["-i", key, &format!("root@{host}:{remote_path}"), local_path])
        .status()
        .await
        .with_context(|| format!("SFTP download from {host} failed"))?;

    if !status.success() {
        anyhow::bail!("SFTP download from {host} failed with status {status}");
    }

    Ok(())
}

/// Execute a command on a remote host via SSH and stream stdout into a local file.
/// On timeout the SSH child is killed and the partial local file is removed.
pub async fn ssh_exec_to_file(
    host: &str,
    key: &str,
    command: &str,
    local_path: &str,
    timeout: Duration,
) -> Result<u64> {
    let mut child = Command::new("ssh")
        .args(SSH_OPTS)
        .args(["-i", key, &format!("root@{host}"), command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("SSH to {host} failed"))?;

    let mut stdout = child
        .stdout
        .take()
        .with_context(|| format!("SSH to {host} did not expose stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .with_context(|| format!("SSH to {host} did not expose stderr"))?;

    let mut file = File::create(local_path)
        .await
        .with_context(|| format!("failed to create {local_path}"))?;

    let stdout_task = tokio::spawn(async move {
        let copied = tokio::io::copy(&mut stdout, &mut file).await?;
        file.flush().await?;
        Ok::<u64, std::io::Error>(copied)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(res) => res.with_context(|| format!("SSH to {host} failed"))?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let _ = tokio::fs::remove_file(local_path).await;
            anyhow::bail!(
                "SSH command on {host} timed out after {}s",
                timeout.as_secs()
            );
        }
    };
    let copied = stdout_task
        .await
        .with_context(|| format!("SSH stdout task for {host} failed"))??;
    let stderr = stderr_task
        .await
        .with_context(|| format!("SSH stderr task for {host} failed"))??;

    if !status.success() {
        let _ = tokio::fs::remove_file(local_path).await;
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        if stderr.is_empty() {
            anyhow::bail!("SSH command on {host} failed with status {status}");
        }
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(copied)
}
