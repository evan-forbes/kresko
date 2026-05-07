from __future__ import annotations

import shlex
from pathlib import Path
from typing import Any


def shell_join(parts: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in parts)


def tmux_start_command(session: str, command: str, log_path: str | None = None) -> str:
    if log_path:
        command = f"{command} > {shlex.quote(log_path)} 2>&1"
    return shell_join(["tmux", "new-session", "-d", "-s", session, command])


def tmux_kill_command(session: str) -> str:
    return shell_join(["tmux", "kill-session", "-t", session])


def render_command_plan(nodes: list[dict[str, Any]], command: str) -> list[str]:
    return [f"{node['name']} ({node['public_ip']}): {command}" for node in nodes]


def pyinfra_deploy_base(payload_paths: list[str], remote_root: str = "/root/kresko") -> None:
    """Run common deploy mutations inside a pyinfra deploy file."""

    from pyinfra.operations import apt, files, server

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
        commands=["hostnamectl set-hostname {{ host.data.kresko_name }}"],
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
