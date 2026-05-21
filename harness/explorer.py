"""Co-located Zcash block-explorer deployment.

Declarative: an experiment opts in from `build_experiment()` with one line —

    experiment.add_explorer(node="miner-0")

— and the explorer then "pops up" during the experiment's launch flow when
`exp.deploy_explorer()` runs. Operationally, the same machinery is exposed as
`explorer-*` verbs (see `explorer_actions`) for redeploy / status / logs / stop.

The explorer is the devdotbo/zcash-explorer Phoenix app, run via `docker
compose` on an existing node, reaching that node's local Zebra RPC through
`host.docker.internal`. The source tree is delivered to the node via an S3
presigned URL the node `curl`s — never scp/rsync (see `harness.s3` and the
operator's S3-only distribution rule). The small, secret `.env` is written
over the SSH session's stdin so credentials never touch S3.

This module is intentionally free of experiment-specific assumptions; the
target node, network, ports, and source path are all carried by
`ExplorerSpec` (with env-var fallbacks that preserve the original behavior).
"""

from __future__ import annotations

import json
import os
import secrets
import shlex
import subprocess
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

from harness import s3
from harness.env import load_experiment_env
from harness.runs import node_failure, write_result

if TYPE_CHECKING:  # pragma: no cover - typing only, avoids an import cycle.
    from harness.experiment import Experiment

__all__ = [
    "ExplorerSpec",
    "ExplorerDeployment",
    "CommandRunner",
    "build_source_archive",
    "explorer_actions",
    "render_env",
]

DEFAULT_SOURCE = "/home/evan/src/zcash/devdotbo/zcash-explorer"
DEFAULT_REMOTE_ROOT = "/root/zcash-explorer"
METADATA_FILENAME = "explorer.json"
REMOTE_ARCHIVE = "/tmp/kresko-zcash-explorer.tar.gz"
REMOTE_FUNDED_KEY = "/root/.config/funded_key.json"
REMOTE_ZEBRAD_CONFIG = "/root/.config/zebrad.toml"

# Paths inside the source tree we never ship: VCS, secrets, and build outputs
# that the node rebuilds itself (Docker `mix release` / `npm install`).
SOURCE_EXCLUDES = {".git", ".env", "_build", "deps", "node_modules", ".elixir_ls"}

# Per-network defaults. These mirror the checked-in zcash-explorer
# docker-compose.yml, which maps testnet 20001:4000 and mainnet 20000:4000 and
# hardwires the Zebra RPC port per service.
NETWORK_DEFAULTS: dict[str, dict[str, Any]] = {
    "testnet": {
        "compose_service": "explorer-testnet",
        "public_port": 20001,
        "container_port": 4000,
        "rpc_port": 18232,
    },
    "mainnet": {
        "compose_service": "explorer-mainnet",
        "public_port": 20000,
        "container_port": 4000,
        "rpc_port": 8232,
    },
}


@dataclass(frozen=True)
class ExplorerSpec:
    """Everything needed to bring the explorer up on one existing node."""

    source: Path
    node: str
    role: str
    network: str
    remote_root: str
    public_port: int
    container_port: int
    rpc_port: int
    compose_service: str
    lightwalletd_enabled: bool
    s3_prefix: str
    s3_expires: int
    faucet_enabled: bool
    faucet_source_address: str | None
    faucet_amount: str
    faucet_daily_ip_limit: int
    faucet_window_seconds: int
    faucet_min_confirmations: int

    @classmethod
    def create(
        cls,
        *,
        node: str | None = None,
        source: str | Path | None = None,
        network: str | None = None,
        role: str = "miner",
        remote_root: str | None = None,
        public_port: int | None = None,
        container_port: int | None = None,
        rpc_port: int | None = None,
        compose_service: str | None = None,
        lightwalletd_enabled: bool = False,
        s3_prefix: str | None = None,
        s3_expires: int | None = None,
        faucet_enabled: bool | None = None,
        faucet_source_address: str | None = None,
        faucet_amount: str | None = None,
        faucet_daily_ip_limit: int | None = None,
        faucet_window_seconds: int | None = None,
        faucet_min_confirmations: int | None = None,
        env: dict[str, str] | None = None,
    ) -> "ExplorerSpec":
        """Build a spec from explicit kwargs, falling back to env, then defaults.

        Precedence per field: explicit argument > `KRESKO_EXPLORER_*` env var >
        per-network default. This keeps the Python API clean
        (`add_explorer(node="miner-0")`) while letting an operator retune a run
        from the environment without editing code.
        """
        env = env if env is not None else os.environ
        network = (network or env.get("KRESKO_EXPLORER_NETWORK") or "testnet").strip().lower()
        if network not in NETWORK_DEFAULTS:
            raise ValueError(
                f"unknown explorer network {network!r} (use one of {sorted(NETWORK_DEFAULTS)})"
            )
        defaults = NETWORK_DEFAULTS[network]
        source_path = Path(
            source or env.get("KRESKO_EXPLORER_SOURCE") or DEFAULT_SOURCE
        ).expanduser()

        def _int(value: int | None, env_key: str, fallback: int) -> int:
            if value is not None:
                return int(value)
            return int(env.get(env_key, fallback))

        def _bool(value: bool | None, env_key: str, fallback: bool = False) -> bool:
            if value is not None:
                return bool(value)
            raw = env.get(env_key)
            if raw is None:
                return fallback
            return raw.strip().lower() in {"1", "true", "yes", "on"}

        enabled = _bool(faucet_enabled, "KRESKO_EXPLORER_FAUCET_ENABLED")
        if enabled and network != "testnet":
            raise ValueError("explorer faucet can only be enabled on testnet")

        return cls(
            source=source_path,
            node=node or env.get("KRESKO_EXPLORER_NODE") or "miner-0",
            role=role or "miner",
            network=network,
            remote_root=str(
                remote_root or env.get("KRESKO_EXPLORER_REMOTE_ROOT") or DEFAULT_REMOTE_ROOT
            ),
            public_port=_int(public_port, "KRESKO_EXPLORER_PORT", defaults["public_port"]),
            container_port=_int(
                container_port, "KRESKO_EXPLORER_CONTAINER_PORT", defaults["container_port"]
            ),
            rpc_port=_int(rpc_port, "KRESKO_EXPLORER_RPC_PORT", defaults["rpc_port"]),
            compose_service=compose_service
            or env.get("KRESKO_EXPLORER_SERVICE")
            or defaults["compose_service"],
            lightwalletd_enabled=bool(lightwalletd_enabled),
            s3_prefix=s3_prefix or env.get("KRESKO_EXPLORER_S3_PREFIX") or "explorer",
            s3_expires=_int(s3_expires, "KRESKO_EXPLORER_S3_EXPIRES", 3600),
            faucet_enabled=enabled,
            faucet_source_address=(
                faucet_source_address or env.get("KRESKO_EXPLORER_FAUCET_SOURCE_ADDRESS")
            ),
            faucet_amount=faucet_amount or env.get("KRESKO_EXPLORER_FAUCET_AMOUNT") or "0.1",
            faucet_daily_ip_limit=_int(
                faucet_daily_ip_limit, "KRESKO_EXPLORER_FAUCET_DAILY_IP_LIMIT", 10
            ),
            faucet_window_seconds=_int(
                faucet_window_seconds, "KRESKO_EXPLORER_FAUCET_WINDOW_SECONDS", 86_400
            ),
            faucet_min_confirmations=_int(
                faucet_min_confirmations, "KRESKO_EXPLORER_FAUCET_MIN_CONFIRMATIONS", 1
            ),
        )

    @property
    def env_file(self) -> str:
        return f"{self.remote_root}/.env"

    def validate(self) -> None:
        if not self.source.exists():
            raise FileNotFoundError(f"explorer source does not exist: {self.source}")
        for required in ("docker-compose.yml", "Dockerfile", "mix.exs"):
            path = self.source / required
            if not path.exists():
                raise FileNotFoundError(f"explorer source is missing {required}: {path}")
        expected_public = NETWORK_DEFAULTS[self.network]["public_port"]
        if self.public_port != expected_public or self.container_port != 4000:
            raise RuntimeError(
                f"the checked-in explorer docker-compose.yml maps {expected_public}:4000 for "
                f"{self.compose_service}; keep public_port={expected_public} / "
                "container_port=4000, or add a compose override first"
            )


# --- pure command/payload builders (unit-testable, no I/O) -------------------


def render_env(
    spec: ExplorerSpec, public_ip: str, faucet_source_address: str | None = None
) -> str:
    """Render the `.env` the explorer container reads.

    The compose file selects `${TESTNET_*}` or `${MAINNET_*}` per service, so
    the network-specific secret/hostname keys must match `spec.network`.
    """
    secret = secrets.token_urlsafe(64)
    prefix = "TESTNET" if spec.network == "testnet" else "MAINNET"
    values = {
        "ZCASHD_HOSTNAME": "host.docker.internal",
        "ZCASHD_PORT": str(spec.rpc_port),
        "ZCASHD_USERNAME": "zcashrpc",
        "ZCASHD_PASSWORD": "changeme",
        "ZCASH_NETWORK": spec.network,
        "LIGHTWALLETD_ENABLED": "true" if spec.lightwalletd_enabled else "false",
        "EXPLORER_SCHEME": "http",
        "EXPLORER_HOSTNAME": public_ip,
        "EXPLORER_PORT": str(spec.public_port),
        "PORT": str(spec.container_port),
        f"{prefix}_EXPLORER_HOSTNAME": public_ip,
        f"{prefix}_SECRET_KEY_BASE": secret,
        "VK_CPUS": "0.3",
        "VK_MEM": "1024M",
        "VK_RUNNER_IMAGE": "nighthawkapps/vkrunner",
        "FAUCET_ENABLED": "false",
    }
    if spec.faucet_enabled:
        source_address = faucet_source_address or spec.faucet_source_address
        if spec.network != "testnet":
            raise ValueError("refusing to render faucet env for non-testnet explorer")
        if not source_address:
            raise ValueError("faucet is enabled but no source address was provided")
        values.update(
            {
                "FAUCET_ENABLED": "true",
                "FAUCET_SOURCE_ADDRESS": source_address,
                "FAUCET_AMOUNT": spec.faucet_amount,
                "FAUCET_DAILY_IP_LIMIT": str(spec.faucet_daily_ip_limit),
                "FAUCET_WINDOW_SECONDS": str(spec.faucet_window_seconds),
                "FAUCET_MIN_CONFIRMATIONS": str(spec.faucet_min_confirmations),
            }
        )
    return "".join(f"{key}={shlex.quote(value)}\n" for key, value in values.items())


def build_source_archive(source: str | Path, archive_path: str | Path) -> None:
    """Tar.gz the explorer source tree into `archive_path`, minus build outputs."""
    source = Path(source)
    with tarfile.open(archive_path, "w:gz") as archive:
        for path in sorted(source.rglob("*")):
            relative = path.relative_to(source)
            if any(part in SOURCE_EXCLUDES for part in relative.parts):
                continue
            archive.add(path, arcname=str(relative), recursive=False)


def remote_prepare_command(spec: ExplorerSpec) -> str:
    return "\n".join(
        [
            "set -euo pipefail",
            "export DEBIAN_FRONTEND=noninteractive",
            f"mkdir -p {shlex.quote(spec.remote_root)}",
            (
                "if ! command -v docker >/dev/null 2>&1 "
                "|| ! docker compose version >/dev/null 2>&1 "
                "|| ! command -v curl >/dev/null 2>&1; then "
                "apt-get update && apt-get install -y docker.io docker-compose-v2 curl; "
                "fi"
            ),
            "systemctl enable --now docker >/dev/null 2>&1 || service docker start >/dev/null 2>&1 || true",
            "docker compose version",
        ]
    )


def remote_fetch_command(spec: ExplorerSpec, presigned_url: str) -> str:
    """Fetch the source archive from the presigned URL and unpack it, preserving .env."""
    return "\n".join(
        [
            "set -euo pipefail",
            f"mkdir -p {shlex.quote(spec.remote_root)}",
            f"curl -fsSL {shlex.quote(presigned_url)} -o {shlex.quote(REMOTE_ARCHIVE)}",
            (
                f"find {shlex.quote(spec.remote_root)} -mindepth 1 -maxdepth 1 "
                "! -name .env -exec rm -rf {} +"
            ),
            f"tar -xzf {shlex.quote(REMOTE_ARCHIVE)} -C {shlex.quote(spec.remote_root)}",
            f"rm -f {shlex.quote(REMOTE_ARCHIVE)}",
        ]
    )


def remote_rpc_check_command(spec: ExplorerSpec) -> str:
    """Wait (up to ~3 min) for the node's local Zebra RPC to answer.

    Co-locating during launch means zebrad may still be starting, so this
    retries rather than failing fast.
    """
    payload = '{"jsonrpc":"1.0","id":"explorer","method":"getblockchaininfo","params":[]}'
    url = f"http://127.0.0.1:{spec.rpc_port}/"
    return "\n".join(
        [
            "set -euo pipefail",
            "for _ in $(seq 1 60); do",
            (
                "  if curl -fsS --max-time 3 --data-binary "
                + shlex.quote(payload)
                + " -H "
                + shlex.quote("content-type: text/plain;")
                + f" {shlex.quote(url)} >/tmp/kresko-explorer-rpc.json 2>/dev/null; then"
            ),
            "    cat /tmp/kresko-explorer-rpc.json",
            "    exit 0",
            "  fi",
            "  sleep 3",
            "done",
            f"echo 'zebra RPC at 127.0.0.1:{spec.rpc_port} did not respond within ~180s' >&2",
            "exit 1",
        ]
    )


def remote_faucet_rpc_check_command(spec: ExplorerSpec, source_address: str) -> str:
    """Verify the target RPC exposes the wallet methods the faucet requires."""
    url = f"http://127.0.0.1:{spec.rpc_port}/"
    validate_payload = json.dumps(
        {
            "jsonrpc": "1.0",
            "id": "faucet-validate",
            "method": "validateaddress",
            "params": [source_address],
        }
    )
    sendmany_help_payload = json.dumps(
        {
            "jsonrpc": "1.0",
            "id": "faucet-z-sendmany",
            "method": "help",
            "params": ["z_sendmany"],
        }
    )
    return "\n".join(
        [
            "set -euo pipefail",
            "check_rpc() {",
            "  label=\"$1\"",
            "  payload=\"$2\"",
            (
                "  response=$(curl -fsS --max-time 5 --data-binary \"$payload\" "
                "-H 'content-type: text/plain;' "
                f"{shlex.quote(url)})"
            ),
            "  if ! printf '%s' \"$response\" | jq -e '.error == null' >/dev/null; then",
            "    echo \"faucet RPC check failed for ${label}: ${response}\" >&2",
            "    exit 1",
            "  fi",
            "}",
            f"check_rpc validateaddress {shlex.quote(validate_payload)}",
            f"check_rpc z_sendmany {shlex.quote(sendmany_help_payload)}",
        ]
    )


def remote_faucet_source_address_command() -> str:
    """Echo the target node's public funded/miner address for faucet use."""
    return "\n".join(
        [
            "set -euo pipefail",
            "address=",
            f"if [ -f {shlex.quote(REMOTE_FUNDED_KEY)} ]; then",
            (
                "  address=$(jq -r '.address // empty' "
                f"{shlex.quote(REMOTE_FUNDED_KEY)} 2>/dev/null || true)"
            ),
            "fi",
            f"if [ -z \"$address\" ] && [ -f {shlex.quote(REMOTE_ZEBRAD_CONFIG)} ]; then",
            (
                "  address=$(kresko config get-miner-address "
                f"{shlex.quote(REMOTE_ZEBRAD_CONFIG)} 2>/dev/null || true)"
            ),
            "fi",
            "case \"${address,,}\" in",
            "  ''|auto|__auto__|__auto_miner_address__)",
            "    echo 'could not discover a concrete faucet source address' >&2",
            "    exit 1",
            "    ;;",
            "esac",
            "printf '%s\\n' \"$address\"",
        ]
    )


def remote_compose_up_command(spec: ExplorerSpec) -> str:
    return "\n".join(
        [
            "set -euo pipefail",
            f"cd {shlex.quote(spec.remote_root)}",
            f"docker compose up -d --build {shlex.quote(spec.compose_service)}",
        ]
    )


def remote_compose_ps_command(spec: ExplorerSpec) -> str:
    return f"cd {shlex.quote(spec.remote_root)} && docker compose ps {shlex.quote(spec.compose_service)}"


def remote_container_rpc_check_command(spec: ExplorerSpec) -> str:
    return (
        f"cd {shlex.quote(spec.remote_root)} && "
        f"docker compose exec -T {shlex.quote(spec.compose_service)} sh -lc "
        + shlex.quote(f"nc -z -w 2 host.docker.internal {spec.rpc_port}")
    )


def remote_http_check_command(spec: ExplorerSpec, public_ip: str) -> str:
    """Poll the public URL; echo the final HTTP status as the last line."""
    url = f"http://{public_ip}:{spec.public_port}/"
    return "\n".join(
        [
            "code=000",
            "for _ in $(seq 1 12); do",
            f"  code=$(curl -sS -o /dev/null -w '%{{http_code}}' --max-time 5 {shlex.quote(url)} || true)",
            '  case "$code" in 200|302) break;; esac',
            "  sleep 5",
            "done",
            'echo "$code"',
        ]
    )


def remote_logs_command(spec: ExplorerSpec, tail: int = 200) -> str:
    return (
        f"cd {shlex.quote(spec.remote_root)} && "
        f"docker compose logs --tail={int(tail)} {shlex.quote(spec.compose_service)}"
    )


def remote_stop_command(spec: ExplorerSpec) -> str:
    return f"cd {shlex.quote(spec.remote_root)} && docker compose stop {shlex.quote(spec.compose_service)}"


# --- process runner ----------------------------------------------------------


class CommandRunner:
    """Runs local subprocesses, teeing each into `<run_dir>/<log_name>.std*.log`.

    Returns a step dict shaped like the rest of the harness
    (`{name, ok, returncode, stdout_path, stderr_path}`) so failures slot into
    `result.json` the same way. Injected in tests to avoid real ssh/aws calls.
    """

    def __init__(self, run_dir: str | Path) -> None:
        self.run_dir = Path(run_dir)

    def run(
        self,
        command: list[str],
        log_name: str,
        *,
        input_text: str | None = None,
    ) -> dict[str, Any]:
        stdout_path = self.run_dir / f"{log_name}.stdout.log"
        stderr_path = self.run_dir / f"{log_name}.stderr.log"
        display = shlex.join(command)
        with stdout_path.open("w", encoding="utf-8") as out, stderr_path.open(
            "w", encoding="utf-8"
        ) as err:
            out.write(f"$ {display}\n")
            out.flush()
            result = subprocess.run(
                command,
                input=input_text,
                stdout=out,
                stderr=err,
                text=True,
                check=False,
            )
        return {
            "name": log_name,
            "ok": result.returncode == 0,
            "returncode": result.returncode,
            "stdout_path": str(stdout_path),
            "stderr_path": str(stderr_path),
        }

    def read_stdout_tail(self, log_name: str) -> str:
        path = self.run_dir / f"{log_name}.stdout.log"
        if not path.exists():
            return ""
        for line in reversed(path.read_text(encoding="utf-8").splitlines()):
            if line.strip() and not line.startswith("$ "):
                return line.strip()
        return ""


def failures_from_steps(node: str, stage: str, steps: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        node_failure(
            node,
            stage,
            step["name"],
            exit_code=step.get("returncode"),
            stdout_path=step.get("stdout_path"),
            stderr_path=step.get("stderr_path"),
            retryable=True,
        )
        for step in steps
        if not step.get("ok", False)
    ]


# --- orchestration -----------------------------------------------------------


class ExplorerDeployment:
    """Drives the explorer lifecycle against one node in the current run."""

    def __init__(
        self,
        exp: "Experiment",
        spec: ExplorerSpec,
        *,
        runner: CommandRunner | None = None,
        s3_runner: Callable[[list[str]], "subprocess.CompletedProcess[str]"] | None = None,
    ) -> None:
        self.exp = exp
        self.spec = spec
        self.runner = runner or CommandRunner(exp.run_dir)
        self.s3_runner = s3_runner

    # public lifecycle ------------------------------------------------------

    def plan(self) -> dict[str, Any]:
        asset = self._target_asset()
        meta = self._write_metadata(asset, "planned")
        return self._result(
            "explorer-plan",
            True,
            asset,
            [],
            url=meta["url"],
            metadata_path=str(self.exp.run_dir / METADATA_FILENAME),
        )

    def deploy(self, *, dry_run: bool = False) -> dict[str, Any]:
        return self._bringup("explorer-deploy", rpc_check=True, dry_run=dry_run)

    def redeploy(self, *, dry_run: bool = False) -> dict[str, Any]:
        # Redeploy assumes the network is already up, so it skips the RPC wait.
        return self._bringup("explorer-redeploy", rpc_check=False, dry_run=dry_run)

    def status(self) -> dict[str, Any]:
        asset = self._target_asset()
        outcome = self._status_steps(asset)
        meta = self._write_metadata(asset, "running" if outcome["ok"] else "error")
        return self._result(
            "explorer-status",
            outcome["ok"],
            asset,
            outcome["steps"],
            url=meta["url"],
            http_status=outcome["http_status"],
        )

    def logs(self) -> dict[str, Any]:
        asset = self._target_asset()
        step = self.runner.run(
            self._ssh(asset, remote_logs_command(self.spec)), "explorer-logs"
        )
        meta = self._write_metadata(asset, "unknown")
        return self._result("explorer-logs", step["ok"], asset, [step], url=meta["url"])

    def stop(self) -> dict[str, Any]:
        asset = self._target_asset()
        step = self.runner.run(
            self._ssh(asset, remote_stop_command(self.spec)), "explorer-stop"
        )
        meta = self._write_metadata(asset, "stopped" if step["ok"] else "error")
        return self._result("explorer-stop", step["ok"], asset, [step], url=meta["url"])

    # internals -------------------------------------------------------------

    def _bringup(self, stage: str, *, rpc_check: bool, dry_run: bool) -> dict[str, Any]:
        asset = self._target_asset()
        self.spec.validate()
        if dry_run:
            meta = self._write_metadata(asset, "planned")
            return self._result(stage, True, asset, [], dry_run=True, url=meta["url"])

        load_experiment_env(self.exp.run_dir)
        self._write_metadata(asset, "deploying")
        steps: list[dict[str, Any]] = []
        faucet_source_address: str | None = None

        steps.append(self.runner.run(self._ssh(asset, remote_prepare_command(self.spec)), "explorer-prepare"))
        if not steps[-1]["ok"]:
            return self._failure(stage, asset, steps)

        if self.spec.faucet_enabled:
            faucet = self._discover_faucet_source_address(asset)
            steps.append(faucet["step"])
            if not faucet["ok"]:
                return self._failure(stage, asset, steps)
            faucet_source_address = faucet["address"]
            steps.append(
                self.runner.run(
                    self._ssh(
                        asset,
                        remote_faucet_rpc_check_command(self.spec, faucet_source_address),
                    ),
                    "explorer-faucet-rpc-check",
                )
            )
            if not steps[-1]["ok"]:
                return self._failure(stage, asset, steps)

        source = self._deliver_source(asset)
        steps.extend(source["steps"])
        if not source["ok"]:
            return self._failure(stage, asset, steps)

        steps.append(self._write_remote_env(asset, faucet_source_address=faucet_source_address))
        if not steps[-1]["ok"]:
            return self._failure(stage, asset, steps)

        if rpc_check:
            steps.append(
                self.runner.run(self._ssh(asset, remote_rpc_check_command(self.spec)), "explorer-rpc-check")
            )
            if not steps[-1]["ok"]:
                return self._failure(stage, asset, steps)

        steps.append(
            self.runner.run(self._ssh(asset, remote_compose_up_command(self.spec)), "explorer-compose-up")
        )
        if not steps[-1]["ok"]:
            return self._failure(stage, asset, steps)

        outcome = self._status_steps(asset)
        steps.extend(outcome["steps"])
        meta = self._write_metadata(asset, "running" if outcome["ok"] else "error")
        return self._result(
            stage,
            outcome["ok"],
            asset,
            steps,
            url=meta["url"],
            http_status=outcome["http_status"],
            source_s3_key=source.get("key"),
            faucet_source_address=faucet_source_address,
        )

    def _deliver_source(self, asset: dict[str, Any]) -> dict[str, Any]:
        """Build the source tarball, push it to S3, and have the node curl it."""
        key = self._s3_key()
        with tempfile.TemporaryDirectory(prefix="kresko-explorer-source-") as tmp:
            archive = Path(tmp) / "zcash-explorer.tar.gz"
            build_source_archive(self.spec.source, archive)
            try:
                url = self._upload_and_presign(archive, key)
            except s3.S3Error as exc:
                step = {
                    "name": "explorer-s3-upload",
                    "ok": False,
                    "returncode": 1,
                    "stdout_path": None,
                    "stderr_path": None,
                    "error": str(exc),
                }
                return {"ok": False, "steps": [step], "key": key}
        fetch = self.runner.run(
            self._ssh(asset, remote_fetch_command(self.spec, url)), "explorer-fetch-source"
        )
        return {"ok": fetch["ok"], "steps": [fetch], "key": key}

    def _upload_and_presign(self, archive: Path, key: str) -> str:
        if self.s3_runner is not None:
            return s3.upload_and_presign(
                archive, key, expires=self.spec.s3_expires, runner=self.s3_runner
            )
        return s3.upload_and_presign(archive, key, expires=self.spec.s3_expires)

    def _s3_key(self) -> str:
        return (
            f"{self.spec.s3_prefix}/{self.exp.name}/{self.exp.run_name}/"
            f"zcash-explorer-{secrets.token_hex(4)}.tar.gz"
        )

    def _discover_faucet_source_address(self, asset: dict[str, Any]) -> dict[str, Any]:
        step = self.runner.run(
            self._ssh(asset, remote_faucet_source_address_command()),
            "explorer-faucet-source",
        )
        address = self.runner.read_stdout_tail("explorer-faucet-source")
        if step["ok"] and not address:
            step = {**step, "ok": False, "returncode": 1, "error": "empty faucet source address"}
        return {"ok": step["ok"], "step": step, "address": address}

    def _write_remote_env(
        self, asset: dict[str, Any], *, faucet_source_address: str | None = None
    ) -> dict[str, Any]:
        env_text = render_env(self.spec, asset["public_ip"], faucet_source_address)
        remote_command = (
            f"mkdir -p {shlex.quote(self.spec.remote_root)} && "
            f"cat > {shlex.quote(self.spec.env_file)}"
        )
        return self.runner.run(
            self._ssh(asset, remote_command), "explorer-write-env", input_text=env_text
        )

    def _status_steps(self, asset: dict[str, Any]) -> dict[str, Any]:
        steps = [
            self.runner.run(self._ssh(asset, remote_compose_ps_command(self.spec)), "explorer-compose-ps"),
            self.runner.run(
                self._ssh(asset, remote_container_rpc_check_command(self.spec)),
                "explorer-container-rpc-check",
            ),
            self.runner.run(
                self._ssh(asset, remote_http_check_command(self.spec, asset["public_ip"])),
                "explorer-http-check",
            ),
        ]
        http_status = self.runner.read_stdout_tail("explorer-http-check")
        return {
            "ok": all(step["ok"] for step in steps) and http_status in {"200", "302"},
            "steps": steps,
            "http_status": http_status,
        }

    def _target_asset(self) -> dict[str, Any]:
        candidates = sorted(
            [
                asset
                for asset in self.exp.run_assets()
                if asset.get("role") == self.spec.role
                and asset.get("status") != "failed"
                and asset.get("public_ip")
            ],
            key=lambda asset: asset.get("name", ""),
        )
        if self.spec.node:
            for asset in candidates:
                if asset.get("name") == self.spec.node:
                    return asset
            names = ", ".join(asset.get("name", "<unnamed>") for asset in candidates) or "<none>"
            raise RuntimeError(
                f"explorer target node {self.spec.node!r} not found among active "
                f"{self.spec.role} nodes in run {self.exp.run_name!r}: {names}"
            )
        if not candidates:
            raise RuntimeError(
                f"no active {self.spec.role} nodes with a public IP in run {self.exp.run_name!r}"
            )
        return candidates[0]

    def _ssh(self, asset: dict[str, Any], remote_command: str) -> list[str]:
        return [
            *self._ssh_transport(),
            self._ssh_target(asset),
            "bash -lc " + shlex.quote(remote_command),
        ]

    def _ssh_transport(self) -> list[str]:
        command = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new"]
        key_path = self.exp.ssh.get("key_path")
        if key_path:
            command.extend(["-i", str(Path(key_path).expanduser())])
        return command

    def _ssh_target(self, asset: dict[str, Any]) -> str:
        user = asset.get("ssh_user") or self.exp.ssh.get("user", "root")
        return f"{user}@{asset['public_ip']}"

    def _write_metadata(self, asset: dict[str, Any], status: str) -> dict[str, Any]:
        public_ip = asset["public_ip"]
        metadata = {
            "experiment": self.exp.name,
            "run": self.exp.run_name,
            "node": asset["name"],
            "public_ip": public_ip,
            "public_port": self.spec.public_port,
            "container_port": self.spec.container_port,
            "network": self.spec.network,
            "compose_service": self.spec.compose_service,
            "remote_root": self.spec.remote_root,
            "rpc_port": self.spec.rpc_port,
            "faucet_enabled": self.spec.faucet_enabled,
            "faucet_source_address": self.spec.faucet_source_address,
            "faucet_amount": self.spec.faucet_amount if self.spec.faucet_enabled else None,
            "run_dir": str(self.exp.run_dir),
            "source": str(self.spec.source),
            "status": status,
            "url": f"http://{public_ip}:{self.spec.public_port}",
        }
        (self.exp.run_dir / METADATA_FILENAME).write_text(
            json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        return metadata

    def _result(
        self,
        stage: str,
        ok: bool,
        asset: dict[str, Any],
        steps: list[dict[str, Any]],
        **extra: Any,
    ) -> dict[str, Any]:
        payload = {
            "stage": stage,
            "ok": ok,
            "node": asset.get("name"),
            "public_ip": asset.get("public_ip"),
            "steps": steps,
            **extra,
        }
        failures = [] if ok else failures_from_steps(asset.get("name", "all"), stage, steps)
        write_result(self.exp.run_dir, stage, ok, failures=failures, extra=payload)
        return payload

    def _failure(
        self, stage: str, asset: dict[str, Any], steps: list[dict[str, Any]]
    ) -> dict[str, Any]:
        self._write_metadata(asset, "error")
        return self._result(
            stage,
            False,
            asset,
            steps,
            url=f"http://{asset.get('public_ip')}:{self.spec.public_port}",
        )


def explorer_actions() -> dict[str, Callable[["Experiment", Any], dict[str, Any]]]:
    """`extra_actions` map exposing the explorer lifecycle as `explorer-*` verbs.

    These delegate to the `Experiment` methods, which no-op cleanly when the
    experiment didn't `add_explorer(...)`.
    """
    return {
        "explorer-plan": lambda exp, args: exp.plan_explorer(),
        "explorer-deploy": lambda exp, args: exp.deploy_explorer(
            dry_run=getattr(args, "dry_run", False)
        ),
        "explorer-redeploy": lambda exp, args: exp.redeploy_explorer(
            dry_run=getattr(args, "dry_run", False)
        ),
        "explorer-status": lambda exp, args: exp.explorer_status(),
        "explorer-logs": lambda exp, args: exp.explorer_logs(),
        "explorer-stop": lambda exp, args: exp.explorer_stop(),
    }
