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
from dataclasses import dataclass, field
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
)
from kresko_py.env import load_experiment_env
from kresko_py.inventory import write_pyinfra_inventory
from kresko_py.remote import render_command_plan, tmux_start_command
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
    cmd = ["pyinfra", str(inventory), str(deploy_file)]
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
            "key_path": "~/.ssh/id_ed25519",
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
                for asset in self.assets():
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
            plan = {"nodes": [a["name"] for a in nodes], "payload_paths": payload_paths}
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(stage, inventory, deploy_file, plan)
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
                    self.assets(),
                    client,
                    required_tag=self.experiment_tag_value,
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
    ) -> dict[str, Any]:
        result = self._pyinfra_runner(inventory, deploy_file, False)
        stdout_path = self.run_dir / f"pyinfra.{stage}.stdout.log"
        stderr_path = self.run_dir / f"pyinfra.{stage}.stderr.log"
        stdout_path.write_text(result.stdout or "", encoding="utf-8")
        stderr_path.write_text(result.stderr or "", encoding="utf-8")
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
