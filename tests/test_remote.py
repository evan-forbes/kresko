from __future__ import annotations

from kresko.remote import (
    APT_LOCK_WAIT,
    collection_archive_path,
    state_snapshot_command,
    tmux_kill_command,
    tmux_start_command,
)


def test_tmux_command_rendering_quotes_session_and_logs():
    command = tmux_start_command("txblast", "kresko txblast-local", "/root/kresko tx.log")

    assert "tmux new-session" in command
    assert "txblast" in command
    assert "'/root/kresko tx.log'" in command


def test_tmux_kill_command():
    assert tmux_kill_command("app") == "tmux kill-session -t app"


def test_reset_command_kills_known_sessions_and_wipes_state():
    from kresko.remote import RESET_TMUX_SESSIONS, reset_command

    cmd = reset_command()
    for session in RESET_TMUX_SESSIONS:
        assert f"tmux kill-session -t {session}" in cmd, f"missing kill for {session}"
    # The whole tmux server is killed so global env does not leak across
    # deploys.
    assert "tmux kill-server" in cmd
    # State, config, logs, traces, kresko env, and stale daemons must all be cleaned.
    assert "rm -rf /root/.cache/zebra" in cmd
    assert "/root/.config/zebrad.toml" in cmd
    assert "/root/.config/zebrad.bootstrap.toml" in cmd
    assert "rm -rf /root/logs /root/traces /root/.kresko" in cmd
    assert "pkill -x zebrad" in cmd
    assert "pkill -f '[k]resko mine'" in cmd
    # Idempotence: tmux/pkill failures must not fail the whole reset.
    assert "|| true" in cmd


def test_tmux_start_command_sources_kresko_env_file():
    """Sessions started via tmux_start_command must source /root/.kresko/env
    so KRESKO_RPC_URL/PORT are always populated, even when pyinfra opens a
    fresh tmux server with no `set-environment -g` state."""
    cmd = tmux_start_command("app", "zebrad -c /root/.config/zebrad.toml")
    # Wrapped in a bash login shell that auto-exports the env file written
    # by node_init.sh.
    assert "bash -lc" in cmd or "bash '-lc'" in cmd
    assert "/root/.kresko/env" in cmd
    assert "set -a" in cmd
    assert "set +a" in cmd
    # The original payload still runs after the env is sourced.
    assert "zebrad -c /root/.config/zebrad.toml" in cmd


def test_apt_lock_wait_checks_known_lock_files_and_processes():
    # Both dpkg lock files and the cloud-init/unattended-upgrades processes
    # must be checked, otherwise pyinfra's apt.packages races with first-boot
    # cloud-init.
    for needle in (
        "/var/lib/dpkg/lock-frontend",
        "/var/lib/dpkg/lock",
        "/var/lib/apt/lists/lock",
        "apt-get",
        "dpkg",
    ):
        assert needle in APT_LOCK_WAIT, f"missing apt-lock check for {needle}"


def test_state_snapshot_command_falls_back_to_p2p_when_snapshot_fails():
    cmd = state_snapshot_command("http://38.190.136.76:9997/")

    assert "--retry-connrefused" in cmd
    assert "--connect-timeout 10" in cmd
    assert "--speed-time 120" in cmd
    assert "&& tar -xzf" in cmd
    assert "snapshot hydration failed with exit $status; falling back to P2P block sync" in cmd
    assert "rm -f /tmp/kresko-state-snapshot.tar.gz" in cmd


def test_collection_archive_path_is_unique_per_node(tmp_path):
    producer = collection_archive_path(str(tmp_path), "producer-0")
    candidate = collection_archive_path(str(tmp_path), "candidate-0")

    assert producer == str(tmp_path / "producer-0" / "kresko-collect.tar.gz")
    assert candidate == str(tmp_path / "candidate-0" / "kresko-collect.tar.gz")
    assert producer != candidate
