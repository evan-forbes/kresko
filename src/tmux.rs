use anyhow::Result;
use base64::Engine;
use futures::future::join_all;
use std::time::Duration;

use crate::config::Instance;
use crate::ssh;

/// Run a script in a detached tmux session on multiple remote instances.
pub async fn run_script_in_tmux(
    instances: &[Instance],
    ssh_key: &str,
    script: &str,
    session_name: &str,
    timeout: Duration,
) -> Vec<(String, Result<()>)> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script);
    let remote_script_path = format!("/root/kresko-{session_name}.sh");
    let log_file = format!("/root/kresko-{session_name}.log");

    let futs: Vec<_> = instances
        .iter()
        .map(|inst| {
            let ip = inst.public_ip.clone();
            let name = inst.name.clone();
            let key = ssh_key.to_string();
            let encoded = encoded.clone();
            let remote_script_path = remote_script_path.clone();
            let log_file = log_file.clone();
            let session = session_name.to_string();

            async move {
                let cmd = format!(
                    "echo '{encoded}' | base64 -d > {remote_script_path} && \
                     chmod +x {remote_script_path} && \
                     tmux new-session -d -s {session} 'bash {remote_script_path} 2>&1 | tee -a {log_file}'"
                );
                let result = ssh::ssh_exec_timeout(&ip, &key, &cmd, timeout).await;
                (name, result.map(|_| ()))
            }
        })
        .collect();

    join_all(futs).await
}

/// Stop a tmux session on remote instances.
pub async fn stop_tmux_session(
    instances: &[Instance],
    ssh_key: &str,
    session_name: &str,
    timeout: Duration,
) -> Vec<(String, Result<()>)> {
    let futs: Vec<_> = instances
        .iter()
        .map(|inst| {
            let ip = inst.public_ip.clone();
            let name = inst.name.clone();
            let key = ssh_key.to_string();
            let session = session_name.to_string();

            async move {
                // Send Ctrl+C first
                let _ = ssh::ssh_exec_timeout(
                    &ip,
                    &key,
                    &format!("tmux send-keys -t {session} C-c 2>/dev/null || true"),
                    Duration::from_secs(5),
                )
                .await;

                // Wait briefly then kill if still running
                tokio::time::sleep(Duration::from_secs(2)).await;

                let result = ssh::ssh_exec_timeout(
                    &ip,
                    &key,
                    &format!("tmux kill-session -t {session} 2>/dev/null || true"),
                    timeout,
                )
                .await;
                (name, result.map(|_| ()))
            }
        })
        .collect();

    join_all(futs).await
}
