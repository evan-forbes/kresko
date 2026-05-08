from __future__ import annotations

import shlex
from pathlib import Path
from typing import Any


def shell_join(parts: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in parts)


KRESKO_ENV_FILE = "/root/.kresko/env"


def tmux_start_command(session: str, command: str, log_path: str | None = None) -> str:
    if log_path:
        command = f"{command} > {shlex.quote(log_path)} 2>&1"
    # Wrap the payload in a login shell that auto-exports the kresko env file
    # written by node_init.sh. Without this, sessions started over the wire
    # (pyinfra, ad-hoc ssh) miss KRESKO_RPC_URL/PORT and tools default to a
    # bogus localhost port.
    wrapped = (
        f"set -a; [ -f {shlex.quote(KRESKO_ENV_FILE)} ] && . {shlex.quote(KRESKO_ENV_FILE)}; "
        f"set +a; {command}"
    )
    return shell_join(["tmux", "new-session", "-d", "-s", session, "bash", "-lc", wrapped])


def tmux_kill_command(session: str) -> str:
    return shell_join(["tmux", "kill-session", "-t", session])


def render_command_plan(nodes: list[dict[str, Any]], command: str) -> list[str]:
    return [f"{node['name']} ({node['public_ip']}): {command}" for node in nodes]


# tmux sessions kresko launches: zebra/app for the daemon, mine for kresko mine,
# txblast for the tx blaster. `kresko reset` should kill them all.
RESET_TMUX_SESSIONS = ("zebra", "app", "mine", "txblast")


def reset_command(
    *,
    tmux_sessions: tuple[str, ...] = RESET_TMUX_SESSIONS,
    zebra_state_dir: str = "/root/.cache/zebra",
    zebra_config_dir: str = "/root/.config",
    log_dir: str = "/root/logs",
) -> str:
    """Render the shell command that resets a node to a clean pre-deploy state.

    Kills known tmux sessions, stops any stray zebrad/kresko processes, then
    wipes state, configs, and logs. Idempotent: missing sessions/files are
    not errors.
    """
    parts = [
        # Kill tmux sessions if present.
        *[
            f"tmux kill-session -t {shlex.quote(name)} 2>/dev/null || true"
            for name in tmux_sessions
        ],
        # Drop the tmux server entirely so global env (KRESKO_RPC_*) does
        # not leak across deploys.
        "tmux kill-server 2>/dev/null || true",
        # Stop any leftover daemons that might keep the state dir busy.
        "pkill -f zebrad 2>/dev/null || true",
        "pkill -f 'kresko mine' 2>/dev/null || true",
        # Wipe Zebra state.
        f"rm -rf {shlex.quote(zebra_state_dir)}",
        # Wipe deployed configs (matches what node_init.sh writes).
        f"rm -f {shlex.quote(zebra_config_dir)}/zebrad.toml "
        f"{shlex.quote(zebra_config_dir)}/zebrad.bootstrap.toml "
        f"{shlex.quote(zebra_config_dir)}/funded_key.json",
        # Wipe the unified log tree plus the kresko helper dir.
        f"rm -rf {shlex.quote(log_dir)} /root/.kresko",
        # Legacy paths from earlier deploy layouts; harmless when missing.
        "rm -f /root/kresko-mine.log /root/kresko-mine-wait.sh "
        "/root/logs.bootstrap /root/payload.tar.gz",
    ]
    return " ; ".join(parts)


APT_LOCK_WAIT = (
    # Fresh Ubuntu cloud instances often run unattended-upgrades on first boot, which holds
    # the dpkg + apt-lists locks. Block until both are free before any
    # apt operations so pyinfra doesn't race with the cloud-init upgrade.
    "for _ in $(seq 1 90); do "
    "  if ! fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1 "
    "     && ! fuser /var/lib/dpkg/lock >/dev/null 2>&1 "
    "     && ! fuser /var/lib/apt/lists/lock >/dev/null 2>&1 "
    "     && ! pgrep -x unattended-upgr >/dev/null 2>&1 "
    "     && ! pgrep -x apt-get >/dev/null 2>&1 "
    "     && ! pgrep -x dpkg >/dev/null 2>&1; then "
    "    exit 0; "
    "  fi; "
    "  sleep 5; "
    "done; "
    "echo 'apt locks still held after 7.5 minutes' >&2; exit 1"
)


def pyinfra_deploy_base(payload_paths: list[str], remote_root: str = "/root/kresko") -> None:
    """Run common deploy mutations inside a pyinfra deploy file."""

    from pyinfra import host
    from pyinfra.operations import apt, files, server

    server.shell(
        name="Wait for apt/dpkg locks (cloud-init unattended-upgrades)",
        commands=[APT_LOCK_WAIT],
    )
    apt.packages(
        name="Install Kresko base packages",
        packages=["tmux", "curl", "tar", "rsync"],
        update=True,
    )
    server.shell(
        name="Prepare Kresko directories",
        commands=[f"mkdir -p {shlex.quote(remote_root)} /root/logs"],
    )
    for payload_path in payload_paths:
        source = Path(payload_path)
        if source.is_dir():
            files.sync(
                name=f"Sync {source}",
                src=str(source),
                dest=f"{remote_root}/{source.name}",
            )
        else:
            files.put(
                name=f"Upload {source}",
                src=str(source),
                dest=f"{remote_root}/{source.name}",
            )
    server.shell(
        name="Set hostname from Kresko metadata",
        commands=[f"hostnamectl set-hostname {shlex.quote(str(host.data.kresko_name))}"],
    )


def pyinfra_run_command(command: str) -> None:
    from pyinfra.operations import server

    server.shell(name="Run command", commands=[command])


def pyinfra_start_tmux(session: str, command: str, log_path: str | None = None) -> None:
    from pyinfra.operations import server

    server.shell(name=f"Start tmux session {session}", commands=[tmux_start_command(session, command, log_path)])


def pyinfra_kill_tmux(session: str) -> None:
    from pyinfra.operations import server

    server.shell(
        name=f"Kill tmux session {session}",
        commands=[f"{tmux_kill_command(session)} || true"],
    )


def pyinfra_collect(paths: list[str], destination: str) -> None:
    from pyinfra.operations import files, server

    server.shell(name="Prepare collection tarball", commands=["rm -f /tmp/kresko-collect.tar.gz"])
    quoted = " ".join(shlex.quote(path) for path in paths)
    server.shell(
        name="Archive requested artifacts",
        commands=[f"tar -czf /tmp/kresko-collect.tar.gz {quoted} || true"],
    )
    files.get(
        name="Fetch artifact archive",
        src="/tmp/kresko-collect.tar.gz",
        dest=f"{destination}/{{{{ host.data.kresko_name }}}}/kresko-collect.tar.gz",
    )
