"""High-level Experiment API used by experiment scripts.

`Experiment.current()` returns an instance bound to the run dir set up by the
CLI. Inside an experiment script, code looks like::

    from kresko_py import DigitalOcean, Experiment, node_type

    miner = node_type(role="miner", provider=DigitalOcean(...), payload=["payload"])

    exp = Experiment.current()
    exp.add(miner, count=10)
    exp.up()
    exp.deploy()
    exp.run_tmux("app", "zebrad -c /root/kresko/zebrad.toml")
    exp.collect(["/root/logs", "/root/*.log"])

The Experiment is constructed *inside* an existing run dir (the CLI builds
the dir, sets `KRESKO_EXPERIMENT` / `KRESKO_RUN_NAME` / `KRESKO_RUN_DIR`,
then runs `run.py`). It writes assets to `~/.kresko/assets/` and snapshots
into `runs/<exp>/<run>/nodes/`.
"""

from __future__ import annotations

import os
import shlex
import subprocess
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Callable

from kresko_py import assets as assets_store
from kresko_py import paths
from kresko_py.digitalocean import (
    DigitalOceanClient,
    destroy_assets,
    destroy_tagged_droplets,
    experiment_tag,
    reconcile_droplets,
    run_tag,
)
from kresko_py.env import load_experiment_env
from kresko_py.inventory import write_pyinfra_inventory
from kresko_py.remote import RESET_TMUX_SESSIONS, render_command_plan, reset_command, tmux_start_command
from kresko_py.runs import (
    ENV_EXPERIMENT,
    ENV_RUN_DIR,
    ENV_RUN_NAME,
    node_failure,
    write_node_snapshot,
    write_result,
)
from kresko_py.selectors import select
from kresko_py.spec import ExperimentSpec, NodeGroup

PyinfraRunner = Callable[[Path, Path, bool], subprocess.CompletedProcess[str]]

__all__ = [
    "DigitalOcean",
    "DigitalOceanNodeType",
    "ENV_EXPERIMENT",
    "ENV_RUN_DIR",
    "ENV_RUN_NAME",
    "Experiment",
    "node_type",
    "run_pyinfra",
]


@dataclass(frozen=True)
class DigitalOcean:
    region: str
    size: str
    image: str = "ubuntu-24-04-x64"
    ssh_user: str = "root"
    tags: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class DigitalOceanNodeType:
    role: str
    region: str
    size: str
    image: str = "ubuntu-24-04-x64"
    ssh_user: str = "root"
    payload_paths: list[str] = field(default_factory=list)
    tags: list[str] = field(default_factory=list)
    name_prefix: str | None = None


def node_type(
    role: str,
    provider: DigitalOcean,
    payload: list[str] | None = None,
    tags: list[str] | None = None,
    name_prefix: str | None = None,
    ssh_user: str | None = None,
) -> DigitalOceanNodeType:
    return DigitalOceanNodeType(
        role=role,
        region=provider.region,
        size=provider.size,
        image=provider.image,
        ssh_user=ssh_user or provider.ssh_user,
        payload_paths=list(payload or []),
        tags=[*provider.tags, *(tags or [])],
        name_prefix=name_prefix,
    )


def run_pyinfra(
    inventory: Path, deploy_file: Path, dry_run: bool = False
) -> subprocess.CompletedProcess[str]:
    cmd = ["pyinfra", "-y", str(inventory), str(deploy_file)]
    if dry_run:
        cmd.append("--dry")
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


class Experiment:
    """Stateful runner bound to a single run directory.

    Don't instantiate directly inside `run.py`; call `Experiment.current()`,
    which reads the run-dir env vars set by the CLI.
    """

    def __init__(
        self,
        name: str,
        run_name: str,
        run_path: Path,
        *,
        provider: str = "digitalocean",
        tags: list[str] | None = None,
        ssh: dict[str, Any] | None = None,
        digitalocean_client: DigitalOceanClient | None = None,
        pyinfra_runner: PyinfraRunner | None = None,
    ) -> None:
        self.name = name
        self.run_name = run_name
        self.run_dir = Path(run_path).resolve()
        self.provider = provider
        self.tags = list(tags or [])
        self.ssh = {
            "user": "root",
            "key_path": "",
            "public_key_path": "~/.ssh/id_ed25519.pub",
            "key_name": "",
            **(ssh or {}),
        }
        self._node_specs: list[tuple[DigitalOceanNodeType, int]] = []
        self._digitalocean_client = digitalocean_client
        self._pyinfra_runner = pyinfra_runner or run_pyinfra

    @classmethod
    def current(
        cls,
        *,
        tags: list[str] | None = None,
        ssh: dict[str, Any] | None = None,
        provider: str = "digitalocean",
        digitalocean_client: DigitalOceanClient | None = None,
        pyinfra_runner: PyinfraRunner | None = None,
    ) -> "Experiment":
        name = os.environ.get(ENV_EXPERIMENT)
        run_name = os.environ.get(ENV_RUN_NAME)
        run_dir = os.environ.get(ENV_RUN_DIR)
        if not (name and run_name and run_dir):
            raise RuntimeError(
                "Experiment.current() requires KRESKO_EXPERIMENT, KRESKO_RUN_NAME, "
                "and KRESKO_RUN_DIR (set by `kresko run`)"
            )
        return cls(
            name=name,
            run_name=run_name,
            run_path=Path(run_dir),
            provider=provider,
            tags=tags,
            ssh=ssh,
            digitalocean_client=digitalocean_client,
            pyinfra_runner=pyinfra_runner,
        )

    def add(self, node: DigitalOceanNodeType, count: int = 1) -> None:
        if count < 0:
            raise ValueError("node count must be non-negative")
        self._node_specs.append((node, count))

    add_nodes = add

    def override(
        self,
        role: str | None = None,
        *,
        size: str | None = None,
        image: str | None = None,
        count: int | None = None,
        region: str | None = None,
    ) -> None:
        """Patch already-added node specs.

        `role=None` matches every spec; otherwise only specs whose role
        equals `role` are patched. Pass any of `size`/`image`/`region` to
        override that NodeType field, or `count` to override the spec's
        count. Useful for retuning a run from CLI flags without editing
        the experiment script.
        """
        new_specs: list[tuple[DigitalOceanNodeType, int]] = []
        for node, current_count in self._node_specs:
            if role is not None and node.role != role:
                new_specs.append((node, current_count))
                continue
            new_node = replace(
                node,
                size=size if size is not None else node.size,
                image=image if image is not None else node.image,
                region=region if region is not None else node.region,
            )
            new_count = count if count is not None else current_count
            if new_count < 0:
                raise ValueError("node count must be non-negative")
            new_specs.append((new_node, new_count))
        self._node_specs = new_specs

    def spec(self) -> ExperimentSpec:
        ssh = dict(self.ssh)
        for node, count in self._node_specs:
            if count > 0 and node.ssh_user:
                ssh.setdefault("user", node.ssh_user)
                if ssh.get("user") == "root":
                    ssh["user"] = node.ssh_user
                break
        return ExperimentSpec(
            name=self.name,
            provider=self.provider,
            tags=self.tags,
            ssh=ssh,
            node_groups=[
                NodeGroup(
                    role=node.role,
                    count=count,
                    region=node.region,
                    size=node.size,
                    image=node.image,
                    name_prefix=node.name_prefix,
                    tags=node.tags,
                    ssh_user=node.ssh_user,
                )
                for node, count in self._node_specs
            ],
            payload_paths=self.payload_paths(),
        )

    def payload_paths(self) -> list[str]:
        out: list[str] = []
        for node, _count in self._node_specs:
            for path in node.payload_paths:
                if path not in out:
                    out.append(path)
        return out

    @property
    def experiment_tag_value(self) -> str:
        return experiment_tag(self.name)

    def assets(self) -> list[dict[str, Any]]:
        return assets_store.list_assets(tags=[self.experiment_tag_value])

    def run_assets(self) -> list[dict[str, Any]]:
        return [
            asset
            for asset in self.assets()
            if asset.get("run") == self.run_name
        ]

    def shell(
        self,
        cmd: list[str] | str,
        *,
        cwd: str | Path | None = None,
        env: dict[str, str] | None = None,
        check: bool = True,
        log_name: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run a local shell command (e.g. the Rust kresko binary).

        Tees stdout/stderr into the run dir. Use `log_name` to override the
        log filename prefix; otherwise the program name is used.
        """

        if isinstance(cmd, str):
            args = shlex.split(cmd)
            display = cmd
        else:
            args = list(cmd)
            display = shlex.join(args)
        prefix = log_name or (Path(args[0]).name if args else "shell")
        stdout_path = self.run_dir / f"{prefix}.stdout.log"
        stderr_path = self.run_dir / f"{prefix}.stderr.log"
        with stdout_path.open("a", encoding="utf-8") as out, stderr_path.open(
            "a", encoding="utf-8"
        ) as err:
            out.write(f"$ {display}\n")
            out.flush()
            result = subprocess.run(
                args,
                cwd=str(cwd) if cwd else str(self.run_dir),
                env={**os.environ, **(env or {})},
                stdout=out,
                stderr=err,
                text=True,
                check=False,
            )
        if check and result.returncode != 0:
            raise RuntimeError(
                f"command failed ({result.returncode}): {display}; see {stderr_path}"
            )
        return result

    def plan(self) -> dict[str, Any]:
        return self.up(dry_run=True, stage="plan")

    def up(
        self,
        *,
        dry_run: bool = False,
        retry_failed: bool = False,
        stage: str = "up",
    ) -> dict[str, Any]:
        try:
            plan = reconcile_droplets(
                self.spec(),
                experiment=self.name,
                run_name=self.run_name,
                client=self._do_client(),
                dry_run=dry_run,
                retry_failed=retry_failed,
            )
            if not dry_run:
                for asset in self.run_assets():
                    write_node_snapshot(self.run_dir, asset)
            requested = sum(count for _, count in self._node_specs)
            failed = list(plan.get("failed", []))
            succeeded = max(0, requested - len(failed)) if not dry_run else 0
            ok = len(failed) == 0
            payload: dict[str, Any] = {
                "stage": stage,
                "ok": ok,
                "dry_run": dry_run,
                "requested": requested,
                "succeeded": succeeded,
                "failed": failed,
                "plan": _plan_to_jsonable(plan),
            }
            failures = [
                node_failure(
                    f["name"],
                    stage,
                    f.get("kind", "up"),
                    retryable=True,
                )
                for f in failed
            ]
            write_result(self.run_dir, stage, ok, failures=failures, extra=payload)
            return payload
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    def deploy(
        self,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        stage = "deploy"
        try:
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            payload_paths = self._absolute_payload_paths()
            inventory, deploy_file = self._write_pyinfra_deploy(
                "deploy_payload.py",
                "from kresko_py.remote import pyinfra_deploy_base\n"
                f"pyinfra_deploy_base({payload_paths!r})\n",
                nodes,
            )
            local_provenance = _read_local_binary_provenance(payload_paths)
            plan: dict[str, Any] = {
                "nodes": [a["name"] for a in nodes],
                "payload_paths": payload_paths,
                "binary_provenance_local": local_provenance,
            }
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(
                stage,
                inventory,
                deploy_file,
                plan,
                post_process=_attach_remote_provenance,
            )
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    def run_tmux(
        self,
        session: str,
        command: str,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        log_path: str | None = None,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        remote_command = tmux_start_command(session, command, log_path)
        return self.run_command(
            remote_command,
            role=role,
            name=name,
            pattern=pattern,
            failed_from=failed_from,
            dry_run=dry_run,
            stage="run",
        )

    def run_command(
        self,
        command: str,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        dry_run: bool = False,
        stage: str = "run",
    ) -> dict[str, Any]:
        try:
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            inventory, deploy_file = self._write_pyinfra_deploy(
                "run_command.py",
                "from kresko_py.remote import pyinfra_run_command\n"
                f"pyinfra_run_command({command!r})\n",
                nodes,
            )
            plan = {
                "nodes": [a["name"] for a in nodes],
                "command": command,
                "plan": render_command_plan(nodes, command),
            }
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(stage, inventory, deploy_file, plan, command)
        except Exception as exc:
            self._write_failure(stage, command, exc)
            raise

    def collect(
        self,
        paths_to_collect: list[str],
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        dest: str | Path | None = None,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        stage = "collect"
        try:
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            destination = str(dest or (self.run_dir / "data"))
            inventory, deploy_file = self._write_pyinfra_deploy(
                "collect.py",
                "from kresko_py.remote import pyinfra_collect\n"
                f"pyinfra_collect({paths_to_collect!r}, {destination!r})\n",
                nodes,
            )
            plan = {
                "nodes": [a["name"] for a in nodes],
                "paths": paths_to_collect,
                "dest": destination,
            }
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(stage, inventory, deploy_file, plan)
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    def reset(
        self,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        tmux_sessions: tuple[str, ...] = RESET_TMUX_SESSIONS,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        """Wipe Zebra state, configs, logs, and kresko tmux sessions on selected nodes.

        Use this to start the next deploy from a clean slate without
        re-provisioning droplets. The droplets themselves are untouched —
        run `down` for that.
        """
        command = reset_command(tmux_sessions=tmux_sessions)
        return self.run_command(
            command,
            role=role,
            name=name,
            pattern=pattern,
            failed_from=failed_from,
            dry_run=dry_run,
            stage="reset",
        )

    def down(
        self,
        *,
        dry_run: bool = False,
        force_tag: str | None = None,
    ) -> dict[str, Any]:
        stage = "down"
        try:
            client = self._do_client()
            if force_tag:
                destroyed = destroy_tagged_droplets(force_tag, client, dry_run=dry_run)
            else:
                destroyed = destroy_assets(
                    self.run_assets(),
                    client,
                    required_tags=[
                        self.experiment_tag_value,
                        run_tag(self.run_name),
                    ],
                    dry_run=dry_run,
                )
            return self._success_result(
                stage,
                dry_run,
                {"destroyed_provider_ids": destroyed},
            )
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    # internals -----------------------------------------------------------

    def _do_client(self) -> DigitalOceanClient:
        if self._digitalocean_client is None:
            load_experiment_env(self.run_dir)
            self._digitalocean_client = DigitalOceanClient()
        return self._digitalocean_client

    def _select(
        self,
        *,
        role: str | list[str] | None,
        name: str | list[str] | None,
        pattern: str | list[str] | None,
        failed_from: str | Path | None,
    ) -> list[dict[str, Any]]:
        return select(
            self.assets(),
            roles=role,
            names=name,
            patterns=pattern,
            run_name=self.run_name,
            failed_from=failed_from,
        )

    def _absolute_payload_paths(self) -> list[str]:
        out: list[str] = []
        for path in self.payload_paths():
            candidate = Path(path)
            out.append(
                str(candidate if candidate.is_absolute() else (self.run_dir / candidate).resolve())
            )
        return out

    def _write_pyinfra_deploy(
        self,
        filename: str,
        body: str,
        nodes: list[dict[str, Any]],
    ) -> tuple[Path, Path]:
        inventory = write_pyinfra_inventory(self.run_dir / "inventory.py", nodes, self.ssh)
        deploy_file = self.run_dir / filename
        deploy_file.write_text(body, encoding="utf-8")
        return inventory, deploy_file

    def _run_pyinfra_stage(
        self,
        stage: str,
        inventory: Path,
        deploy_file: Path,
        extra: dict[str, Any],
        command: str | None = None,
        post_process: Callable[[dict[str, Any], str, str], None] | None = None,
    ) -> dict[str, Any]:
        result = self._pyinfra_runner(inventory, deploy_file, False)
        stdout_path = self.run_dir / f"pyinfra.{stage}.stdout.log"
        stderr_path = self.run_dir / f"pyinfra.{stage}.stderr.log"
        stdout_text = result.stdout or ""
        stderr_text = result.stderr or ""
        stdout_path.write_text(stdout_text, encoding="utf-8")
        stderr_path.write_text(stderr_text, encoding="utf-8")
        failures: list[dict[str, Any]] = []
        if result.returncode != 0:
            failures.append(
                node_failure(
                    "all",
                    stage,
                    command or f"pyinfra {inventory} {deploy_file}",
                    exit_code=result.returncode,
                    stdout_path=str(stdout_path),
                    stderr_path=str(stderr_path),
                    retryable=True,
                )
            )
        payload = {
            **extra,
            "stage": stage,
            "ok": result.returncode == 0,
            "dry_run": False,
            "returncode": result.returncode,
            "stdout_path": str(stdout_path),
            "stderr_path": str(stderr_path),
        }
        if post_process is not None:
            post_process(payload, stdout_text, stderr_text)
        write_result(
            self.run_dir, stage, result.returncode == 0, failures=failures, extra=payload
        )
        return payload

    def _success_result(
        self, stage: str, dry_run: bool, extra: dict[str, Any]
    ) -> dict[str, Any]:
        payload = {"stage": stage, "ok": True, "dry_run": dry_run, **extra}
        write_result(self.run_dir, stage, True, extra=payload)
        return payload

    def _write_failure(self, stage: str, command: str, exc: Exception) -> None:
        write_result(
            self.run_dir,
            stage,
            False,
            failures=[node_failure("all", stage, command, retryable=True)],
            extra={"error": str(exc)},
        )


def _plan_to_jsonable(plan: dict[str, list[dict[str, Any]]]) -> dict[str, list[dict[str, Any]]]:
    return {key: list(value) for key, value in plan.items()}


def _sha256_file(path: Path) -> str:
    import hashlib

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(64 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _parse_payload_manifest(manifest_path: Path) -> dict[str, str]:
    """Read the flat key=value manifest written by `kresko genesis`."""
    fields: dict[str, str] = {}
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        if "=" not in line or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        fields[key.strip()] = value.strip()
    return fields


def _read_local_binary_provenance(payload_paths: list[str]) -> dict[str, Any]:
    """Locate `<payload>/build/{manifest.txt,zebrad,kresko}` and return what
    we know locally: the manifest's recorded hashes plus our own re-hash of
    the staged files. Both are returned so a tampered payload directory is
    visible (manifest_sha256 != staged_sha256).
    """
    out: dict[str, Any] = {}
    for payload in payload_paths:
        build_dir = Path(payload) / "build"
        manifest = build_dir / "manifest.txt"
        if not manifest.exists():
            continue
        manifest_fields = _parse_payload_manifest(manifest)
        binaries: dict[str, dict[str, str]] = {}
        for binary in ("zebrad", "kresko"):
            staged = build_dir / binary
            if not staged.exists():
                continue
            binaries[binary] = {
                "manifest_sha256": manifest_fields.get(f"{binary}_sha256", ""),
                "staged_sha256": _sha256_file(staged),
                "source": manifest_fields.get(f"{binary}_source", ""),
            }
        out["build_dir"] = str(build_dir)
        out["binaries"] = binaries
        return out
    return out


def _attach_remote_provenance(
    payload: dict[str, Any], stdout_text: str, _stderr_text: str
) -> None:
    """Post-processor for `Experiment.deploy` that surfaces node_init.sh's
    PROVENANCE lines in the result, so a redeploy makes binary swaps obvious.
    """
    payload["binary_provenance_remote"] = _parse_remote_provenance(stdout_text)


def _parse_remote_provenance(stdout_text: str) -> dict[str, list[dict[str, str]]]:
    """Group `PROVENANCE: <name> <state> ...` lines emitted by node_init.sh.

    Returns {"changed": [...], "unchanged": [...], "installed": [...]} so a
    redeploy makes it obvious which nodes saw a binary swap.
    """
    buckets: dict[str, list[dict[str, str]]] = {
        "changed": [],
        "unchanged": [],
        "installed": [],
    }
    for raw in stdout_text.splitlines():
        line = raw.strip()
        marker = "PROVENANCE:"
        idx = line.find(marker)
        if idx == -1:
            continue
        rest = line[idx + len(marker) :].strip()
        # Shapes:
        #   zebrad CHANGED (was=..., now=...)
        #   zebrad unchanged (sha256=...)
        #   zebrad installed (sha256=..., no previous binary)
        parts = rest.split(None, 2)
        if len(parts) < 2:
            continue
        binary, state = parts[0], parts[1]
        detail = parts[2] if len(parts) == 3 else ""
        bucket = (
            "changed"
            if state.lower() == "changed"
            else "installed"
            if state.lower() == "installed"
            else "unchanged"
        )
        buckets[bucket].append({"binary": binary, "detail": detail})
    return buckets
