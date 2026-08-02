from __future__ import annotations

import shlex
from pathlib import Path
from typing import Any


def shell_join(parts: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in parts)


KRESKO_ENV_FILE = "/root/.kresko/env"

# Default public state-snapshot mirror. Bringing a node up from a snapshot
# hydrates zebrad's state DB instead of syncing the whole chain over P2P. This
# is a public read mirror the node curls directly; it does NOT go through the
# S3 payload/presign path (that contract is for artifacts we own).
DEFAULT_STATE_SNAPSHOT_URL = "http://38.190.136.76:9997/"

# Where zebrad keeps its state cache on a node (matches scripts/node_init*.sh).
ZEBRA_STATE_CACHE_DIR = "/root/.cache/zebra"


def state_snapshot_command(url: str, *, cache_dir: str = ZEBRA_STATE_CACHE_DIR) -> str:
    """Render the node-side command that hydrates zebrad state from a snapshot.

    Idempotent: if the state cache is already populated, the download is
    skipped so re-deploys don't re-fetch. The node curls the public mirror
    directly and extracts the tarball into ``cache_dir`` before zebrad starts,
    so it resumes from the snapshot height instead of genesis.
    """
    if not url:
        raise ValueError("state_snapshot requires a non-empty URL")
    q_url = shlex.quote(url)
    q_dir = shlex.quote(cache_dir)
    archive = "/tmp/kresko-state-snapshot.tar.gz"
    q_archive = shlex.quote(archive)
    return (
        f"if [ -d {q_dir}/state ] && [ -n \"$(ls -A {q_dir}/state 2>/dev/null)\" ]; then "
        f"echo 'kresko: zebra state cache already present; skipping snapshot'; "
        f"else "
        f"echo 'kresko: hydrating zebra state from {url}'; "
        f"mkdir -p {q_dir}; "
        f"if curl -fSL --retry 3 --retry-connrefused --retry-delay 5 "
        f"--connect-timeout 10 --speed-time 120 --speed-limit 1024 "
        f"-o {q_archive} {q_url} && tar -xzf {q_archive} -C {q_dir}; then "
        f"rm -f {q_archive}; "
        f"echo 'kresko: snapshot extracted; zebrad will resume from the snapshot height'; "
        f"else "
        f"status=$?; "
        f"rm -f {q_archive}; "
        f"echo \"kresko: snapshot hydration failed with exit $status; falling back to P2P block sync\"; "
        f"fi; "
        f"fi"
    )


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
    trace_dir: str = "/root/traces",
) -> str:
    """Render the shell command that resets a node to a clean pre-deploy state.

    Kills known tmux sessions, stops any stray zebrad/kresko processes, then
    wipes state, configs, logs, and traces. Idempotent: missing sessions/files are
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
        "pkill -x zebrad 2>/dev/null || true",
        "pkill -f '[k]resko mine' 2>/dev/null || true",
        # Wipe Zebra state.
        f"rm -rf {shlex.quote(zebra_state_dir)}",
        # Wipe deployed configs (matches what node_init.sh writes).
        f"rm -f {shlex.quote(zebra_config_dir)}/zebrad.toml "
        f"{shlex.quote(zebra_config_dir)}/zebrad.bootstrap.toml "
        f"{shlex.quote(zebra_config_dir)}/funded_key.json",
        # Wipe per-run evidence plus the kresko helper dir. Callers must collect
        # anything they need before reset; retaining JSONL files here causes the
        # next run's trace oracle to evaluate stale process epochs.
        f"rm -rf {shlex.quote(log_dir)} {shlex.quote(trace_dir)} /root/.kresko",
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
        commands=[f"mkdir -p {shlex.quote(remote_root)} /root/logs /root/traces"],
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


def pyinfra_deploy_s3(
    presigned_url: str,
    payload_names: list[str],
    *,
    archive_sha256: str = "",
    remote_root: str = "/root/kresko",
) -> None:
    """Fetch a presigned payload archive and install it under ``remote_root``.

    The archive contains top-level entries named by ``payload_names``. We
    extract into ``remote_root`` staging space first, validate the payload, then
    swap entries into place. Nothing is ever extracted directly into ``/root``.
    """

    from pyinfra import host
    from pyinfra.operations import apt, server

    archive = "/tmp/kresko-payload.tar.gz"
    extract_root = f"{remote_root}/payload.unpack"
    quoted_url = shlex.quote(presigned_url)
    quoted_archive = shlex.quote(archive)
    quoted_extract_root = shlex.quote(extract_root)
    quoted_remote_root = shlex.quote(remote_root)
    names = [name for name in payload_names if name]
    if not names:
        raise ValueError("payload_names must not be empty")

    validate = []
    swaps = []
    for name in names:
        q_name = shlex.quote(name)
        validate.append(f"test -e {quoted_extract_root}/{q_name}")
        if name == "payload":
            validate.append(f"test -f {quoted_extract_root}/{q_name}/vars.sh")
        target = f"{quoted_remote_root}/{q_name}"
        candidate = f"{quoted_extract_root}/{q_name}"
        backup = f"{target}.old"
        swaps.extend(
            [
                f"rm -rf {backup}",
                f"if [ -e {target} ]; then mv {target} {backup}; fi",
                f"mv {candidate} {target}",
                f"chown -R root:root {target}",
                f"rm -rf {backup}",
            ]
        )

    verify_sha = []
    if archive_sha256:
        verify_sha.append(
            f"printf '%s  %s\\n' {shlex.quote(archive_sha256)} {quoted_archive} | sha256sum -c -"
        )

    script = " && ".join(
        [
            "set -euo pipefail",
            f"mkdir -p {quoted_remote_root} /root/logs /root/traces",
            f"rm -rf {quoted_extract_root}",
            f"mkdir -p {quoted_extract_root}",
            (
                "curl -fL --retry 3 --retry-connrefused --retry-delay 5 "
                "--connect-timeout 10 --speed-time 120 --speed-limit 1024 "
                f"-o {quoted_archive} {quoted_url}"
            ),
            *verify_sha,
            f"tar -xzf {quoted_archive} -C {quoted_extract_root}",
            *validate,
            *swaps,
            f"rm -rf {quoted_extract_root}",
            f"rm -f {quoted_archive} /root/payload.tar.gz",
        ]
    )

    server.shell(
        name="Wait for apt/dpkg locks (cloud-init unattended-upgrades)",
        commands=[APT_LOCK_WAIT],
    )
    apt.packages(
        name="Install Kresko base packages",
        packages=["tmux", "curl", "tar"],
        update=True,
    )
    server.shell(
        name="Fetch and install S3 payload archive",
        commands=[f"bash -lc {shlex.quote(script)}"],
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
    from pyinfra import host
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
        dest=collection_archive_path(destination, str(host.data.kresko_name)),
        create_local_dir=True,
    )


def collection_archive_path(destination: str, node_name: str) -> str:
    return str(Path(destination) / node_name / "kresko-collect.tar.gz")


def pyinfra_state_snapshot(url: str, cache_dir: str = ZEBRA_STATE_CACHE_DIR) -> None:
    from pyinfra.operations import server

    server.shell(
        name="Hydrate zebra state from snapshot",
        commands=[state_snapshot_command(url, cache_dir=cache_dir)],
    )
