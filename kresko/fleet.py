"""The Fleet API — the single noun the harness is built around.

A **fleet** is a named, tagged set of cloud nodes plus its accumulated state
under ``~/.kresko/fleets/<name>/``. A long-running network is a persistent
fleet; a CI job is an ephemeral fleet named after the commit and torn down at
the end. Same operations either way — the only difference is whether you call
``down()``.

Authoring is plain Python: import the class, call methods.

    from kresko import Fleet, Vultr

    fleet = Fleet("ci-abc123", ssh={"key_name": "kresko-key"})
    fleet.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
    fleet.up()                                  # idempotent: create missing, adopt live
    fleet.deploy("payload/")                    # S3-only payload delivery
    fleet.run("kresko mine ...", role="miner", background="mine")   # long-running (tmux)
    fleet.run("kresko status ...", role="miner")                    # ephemeral (waits)
    fleet.collect("/root/traces", role="miner")
    fleet.down()                                # omit for a long-running net
"""

from __future__ import annotations

import json
import os
import secrets
import shlex
import subprocess
import tarfile
import tempfile
from copy import deepcopy
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

from kresko import assets as assets_store
from kresko import paths
from kresko import s3
from kresko.env import load_experiment_env
from kresko.inventory import write_pyinfra_inventory
from kresko.providers import (
    CloudProvider,
    DigitalOcean,
    Vultr,
    fleet_tag,
    get_provider,
    known_provider_names,
    role_tag,
)
from kresko.reconcile import DesiredNode, reconcile_instances
from kresko.remote import (
    DEFAULT_STATE_SNAPSHOT_URL,
    RESET_TMUX_SESSIONS,
    render_command_plan,
    reset_command,
    tmux_start_command,
)
from kresko.selectors import select

PyinfraRunner = Callable[[Path, Path, bool], subprocess.CompletedProcess[str]]

RESULT_FILENAME = "result.json"
NODES_DIRNAME = "nodes"
TRACE_COLLECTION_PATHS = [
    "/root/logs",
    "/root/traces",
    "/root/.cache/kresko/txblast-traces",
]

__all__ = [
    "DigitalOcean",
    "Fleet",
    "TRACE_COLLECTION_PATHS",
    "Vultr",
    "run_pyinfra",
]


def run_pyinfra(
    inventory: Path, deploy_file: Path, dry_run: bool = False
) -> subprocess.CompletedProcess[str]:
    cmd = ["pyinfra", "-y", str(inventory), str(deploy_file)]
    if dry_run:
        cmd.append("--dry")
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


@dataclass(frozen=True)
class _NodeSpec:
    provider: str
    role: str
    count: int
    region: str
    size: str
    image: str
    ssh_user: str
    payload_paths: list[str] = field(default_factory=list)
    tags: list[str] = field(default_factory=list)
    name_prefix: str | None = None
    provider_options: dict[str, Any] = field(default_factory=dict)


def _provider_fields(provider: DigitalOcean | Vultr) -> dict[str, Any]:
    if isinstance(provider, DigitalOcean):
        return {
            "provider": "digitalocean",
            "region": provider.region,
            "size": provider.size,
            "image": provider.image,
            "ssh_user": provider.ssh_user,
            "tags": list(provider.tags),
            "provider_options": {},
        }
    if isinstance(provider, Vultr):
        return {
            "provider": "vultr",
            "region": provider.region,
            "size": provider.size,
            "image": provider.image,
            "ssh_user": provider.ssh_user,
            "tags": list(provider.tags),
            "provider_options": {
                "vpc_ids": list(provider.vpc_ids),
                "enable_ipv6": provider.enable_ipv6,
                "user_data": provider.user_data,
            },
        }
    raise TypeError(f"unsupported provider config {provider!r}")


def _as_path_list(payload: str | list[str] | None) -> list[str]:
    if payload is None:
        return []
    if isinstance(payload, str):
        return [payload]
    return list(payload)


def _resolve_state_snapshot(value: bool | str | None) -> str | None:
    """Resolve the ``state_snapshot`` deploy option to a concrete URL or None.

    ``False``/``None`` -> off. ``True`` -> ``$KRESKO_STATE_SNAPSHOT_URL`` or the
    default public mirror. A string is used verbatim.
    """
    if not value:
        return None
    if value is True:
        return os.environ.get("KRESKO_STATE_SNAPSHOT_URL") or DEFAULT_STATE_SNAPSHOT_URL
    return str(value)


class Fleet:
    """Stateful handle over a named, tagged set of cloud nodes."""

    def __init__(
        self,
        name: str,
        *,
        tags: list[str] | None = None,
        ssh: dict[str, Any] | None = None,
        providers: dict[str, CloudProvider] | None = None,
        pyinfra_runner: PyinfraRunner | None = None,
        s3_runner: s3.Runner | None = None,
    ) -> None:
        paths.validate_slug(name, kind="fleet")
        paths.ensure_home()
        self.name = name
        self.dir = paths.fleet_dir(name)
        self.dir.mkdir(parents=True, exist_ok=True)
        (self.dir / NODES_DIRNAME).mkdir(exist_ok=True)
        (self.dir / "data").mkdir(exist_ok=True)
        self.tags = list(tags or [])
        self.ssh = {
            "user": "root",
            "key_path": "",
            "public_key_path": "~/.ssh/id_ed25519.pub",
            "key_name": "",
            **(ssh or {}),
        }
        self._nodes: list[_NodeSpec] = []
        self._providers = dict(providers or {})
        self._pyinfra_runner = pyinfra_runner or run_pyinfra
        self._s3_runner = s3_runner
        # Optional co-located block explorer (see kresko.explorer). Populated by
        # add_explorer(); None means the explorer ops are clean no-ops.
        self._explorer: Any = None

    # node declaration ------------------------------------------------------

    def add(
        self,
        role: str,
        count: int = 1,
        *,
        provider: DigitalOcean | Vultr,
        payload: str | list[str] | None = None,
        tags: list[str] | None = None,
        name_prefix: str | None = None,
        ssh_user: str | None = None,
    ) -> "Fleet":
        """Declare `count` nodes of `role` on `provider`.

        One call replaces the old `node_type()` + `add()` pair. Returns self so
        adds can be chained.
        """
        if count < 0:
            raise ValueError("node count must be non-negative")
        fields = _provider_fields(provider)
        self._nodes.append(
            _NodeSpec(
                provider=fields["provider"],
                role=role,
                count=count,
                region=fields["region"],
                size=fields["size"],
                image=fields["image"],
                ssh_user=ssh_user or fields["ssh_user"],
                payload_paths=_as_path_list(payload),
                tags=[*fields["tags"], *(tags or [])],
                name_prefix=name_prefix,
                provider_options=fields["provider_options"],
            )
        )
        return self

    def override(
        self,
        role: str | None = None,
        *,
        size: str | None = None,
        image: str | None = None,
        count: int | None = None,
        region: str | None = None,
    ) -> None:
        """Patch already-added node specs (all roles if `role` is None)."""
        new_nodes: list[_NodeSpec] = []
        for node in self._nodes:
            if role is not None and node.role != role:
                new_nodes.append(node)
                continue
            new_count = count if count is not None else node.count
            if new_count < 0:
                raise ValueError("node count must be non-negative")
            new_nodes.append(
                replace(
                    node,
                    size=size if size is not None else node.size,
                    image=image if image is not None else node.image,
                    region=region if region is not None else node.region,
                    count=new_count,
                )
            )
        self._nodes = new_nodes

    @property
    def roles(self) -> list[str]:
        return sorted({node.role for node in self._nodes})

    # asset views -----------------------------------------------------------

    @property
    def fleet_tag_value(self) -> str:
        return fleet_tag(self.name)

    def fleet_assets(self) -> list[dict[str, Any]]:
        return assets_store.list_assets(tags=[self.fleet_tag_value])

    # local shell -----------------------------------------------------------

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

        Tees stdout/stderr into the fleet dir. Use `log_name` to override the
        log filename prefix; otherwise the program name is used.
        """
        if isinstance(cmd, str):
            args = shlex.split(cmd)
            display = cmd
        else:
            args = list(cmd)
            display = shlex.join(args)
        prefix = log_name or (Path(args[0]).name if args else "shell")
        stdout_path = self.dir / f"{prefix}.stdout.log"
        stderr_path = self.dir / f"{prefix}.stderr.log"
        with stdout_path.open("a", encoding="utf-8") as out, stderr_path.open(
            "a", encoding="utf-8"
        ) as err:
            out.write(f"$ {display}\n")
            out.flush()
            result = subprocess.run(
                args,
                cwd=str(cwd) if cwd else str(self.dir),
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

    # lifecycle -------------------------------------------------------------

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
            desired = self._desired()
            plan = reconcile_instances(
                desired,
                fleet=self.name,
                providers=self._providers_for({d.provider for d in desired}),
                ssh_key_selector=self.ssh.get("key_name") or self.ssh.get("ssh_key") or "",
                dry_run=dry_run,
                retry_failed=retry_failed,
            )
            if not dry_run:
                for asset in self.fleet_assets():
                    self._write_node_snapshot(asset)
            requested = sum(node.count for node in self._nodes)
            failed = list(plan.get("failed", []))
            # Duplicate live instances for one desired name are a hard error: a
            # real (non-dry-run) up() raises in reconcile before creating
            # anything. Surface that in plan()/dry-run too, so "ok": true never
            # promises an up() that would actually blow up.
            duplicates = list(plan.get("duplicate", []))
            succeeded = max(0, requested - len(failed)) if not dry_run else 0
            ok = not failed and not duplicates
            payload: dict[str, Any] = {
                "stage": stage,
                "ok": ok,
                "dry_run": dry_run,
                "requested": requested,
                "succeeded": succeeded,
                "failed": failed,
                "duplicate": duplicates,
                "plan": _plan_to_jsonable(plan),
            }
            failures = [
                _node_failure(f["name"], stage, f.get("kind", "up"), retryable=True)
                for f in failed
            ]
            failures += [
                _node_failure(d.get("name", ""), stage, "duplicate", retryable=False)
                for d in duplicates
            ]
            self._write_result(stage, ok, failures=failures, extra=payload)
            return payload
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    def deploy(
        self,
        payload: str | list[str] | None = None,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        state_snapshot: bool | str = False,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        """Ship a payload to selected nodes (S3-only delivery preserved).

        `payload` is the path(s) to ship; if omitted, the union of payloads
        declared on matching `add()` calls is used. `state_snapshot` opts the
        node into hydrating zebrad state from a snapshot (default off); see
        `_resolve_state_snapshot`.
        """
        stage = "deploy"
        try:
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            payload_paths = self._absolute_payload_paths(payload, role=role)
            snapshot_url = _resolve_state_snapshot(state_snapshot)
            local_provenance = _read_local_binary_provenance(payload_paths)
            payload_names = _payload_archive_names(payload_paths, require_exists=not dry_run)
            plan: dict[str, Any] = {
                "nodes": [a["name"] for a in nodes],
                "payload_paths": payload_paths,
                "payload_names": payload_names,
                "delivery": "s3",
                "binary_provenance_local": local_provenance,
            }
            if snapshot_url:
                plan["state_snapshot_url"] = snapshot_url
            if dry_run:
                body = _deploy_s3_body(
                    "DRY_RUN_PRESIGNED_URL",
                    payload_names,
                    archive_sha256="DRY_RUN_ARCHIVE_SHA256",
                    snapshot_url=snapshot_url,
                )
                self._write_pyinfra_deploy("deploy_payload.py", body, nodes)
                return self._success_result(stage, True, plan)
            with tempfile.TemporaryDirectory(prefix=f"kresko-{self.name}-payload-") as tmp:
                archive = Path(tmp) / "payload.tar.gz"
                payload_names = _build_payload_archive(payload_paths, archive)
                archive_sha256 = _sha256_file(archive)
                s3_key = _payload_s3_key(self.name)
                presigned_url = self._upload_payload_archive(archive, s3_key)
                plan.update(
                    {
                        "payload_names": payload_names,
                        "payload_archive_sha256": archive_sha256,
                        "payload_s3_key": s3_key,
                        "payload_s3_expires": _payload_s3_expires(),
                    }
                )
                body = _deploy_s3_body(
                    presigned_url,
                    payload_names,
                    archive_sha256=archive_sha256,
                    snapshot_url=snapshot_url,
                )
                inventory, deploy_file = self._write_pyinfra_deploy(
                    "deploy_payload.py", body, nodes
                )
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

    def _upload_payload_archive(self, archive: Path, key: str) -> str:
        if self._s3_runner is not None:
            return s3.upload_and_presign(
                archive,
                key,
                expires=_payload_s3_expires(),
                runner=self._s3_runner,
            )
        return s3.upload_and_presign(archive, key, expires=_payload_s3_expires())

    def run(
        self,
        command: str,
        *,
        background: str | None = None,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        log_path: str | None = None,
        dry_run: bool = False,
        stage: str = "run",
    ) -> dict[str, Any]:
        """Run a command on selected nodes.

        Ephemeral by default (waits, captures output). Pass
        `background="<session>"` to launch it in a detached tmux session for a
        long-running task. One method replaces the old `run_command` +
        `run_tmux` pair.
        """
        remote_command = (
            tmux_start_command(background, command, log_path) if background else command
        )
        try:
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            inventory, deploy_file = self._write_pyinfra_deploy(
                "run_command.py",
                "from kresko.remote import pyinfra_run_command\n"
                f"pyinfra_run_command({remote_command!r})\n",
                nodes,
            )
            plan = {
                "nodes": [a["name"] for a in nodes],
                "command": remote_command,
                "background": background,
                "plan": render_command_plan(nodes, remote_command),
            }
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(stage, inventory, deploy_file, plan, remote_command)
        except Exception as exc:
            self._write_failure(stage, remote_command, exc)
            raise

    def collect(
        self,
        paths_to_collect: str | list[str],
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
            targets = _as_path_list(paths_to_collect)
            nodes = self._select(role=role, name=name, pattern=pattern, failed_from=failed_from)
            destination = str(dest or (self.dir / "data"))
            inventory, deploy_file = self._write_pyinfra_deploy(
                "collect.py",
                "from kresko.remote import pyinfra_collect\n"
                f"pyinfra_collect({targets!r}, {destination!r})\n",
                nodes,
            )
            plan = {
                "nodes": [a["name"] for a in nodes],
                "paths": targets,
                "dest": destination,
            }
            if dry_run:
                return self._success_result(stage, True, plan)
            return self._run_pyinfra_stage(stage, inventory, deploy_file, plan)
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    def download_traces(
        self,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        failed_from: str | Path | None = None,
        dest: str | Path | None = None,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        """Collect standard fleet trace/log locations from selected nodes."""
        return self.collect(
            TRACE_COLLECTION_PATHS,
            role=role,
            name=name,
            pattern=pattern,
            failed_from=failed_from,
            dest=dest,
            dry_run=dry_run,
        )

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
        """Wipe Zebra state, configs, logs, traces, and tmux sessions on selected nodes.

        Starts the next deploy from a clean slate without re-provisioning. The
        cloud instances themselves are untouched — run `down` for that.
        """
        return self.run(
            reset_command(tmux_sessions=tmux_sessions),
            role=role,
            name=name,
            pattern=pattern,
            failed_from=failed_from,
            dry_run=dry_run,
            stage="reset",
        )

    def status(
        self,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        rpc_port: int | None = None,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """Query RPC block heights/health for the fleet's nodes.

        Returns the status report plus an `ok` flag: every selected node
        reachable. Useful as a CI gate (`sys.exit(0 if fleet.status()["ok"] ...`).
        """
        from kresko import status as status_mod

        nodes = self._select(role=role, name=name, pattern=pattern, failed_from=None)
        port = rpc_port or int(os.environ.get("KRESKO_RPC_PORT", status_mod.DEFAULT_RPC_PORT))
        report = status_mod.query_status(
            nodes, rpc_port=port, timeout=timeout or status_mod.DEFAULT_TIMEOUT
        )
        out = report.to_dict()
        out["ok"] = report.total > 0 and report.unreachable == 0
        return out

    def heights(
        self,
        *,
        role: str | list[str] | None = None,
        name: str | list[str] | None = None,
        pattern: str | list[str] | None = None,
        out: str | Path | None = None,
        start_height: int = 0,
        end_height: int | None = None,
        rpc_port: int | None = None,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """Walk each node's best chain into `data/heights.jsonl`.

        This is the canonical-chain input the fork and block-time analyses read,
        and it is separate from `download_traces()`: the traces come off the
        nodes' disks, this comes off their RPC, so it must be collected while the
        fleet is still up.
        """
        from kresko import heights as heights_mod
        from kresko import status as status_mod

        nodes = self._select(role=role, name=name, pattern=pattern, failed_from=None)
        port = rpc_port or int(os.environ.get("KRESKO_RPC_PORT", status_mod.DEFAULT_RPC_PORT))
        out_path = Path(out) if out else (self.dir / "data" / "heights.jsonl")
        return heights_mod.collect(
            nodes,
            out_path,
            start_height=start_height,
            end_height=end_height,
            rpc_port=port,
            timeout=timeout or status_mod.DEFAULT_TIMEOUT,
        )

    def archive(self, dest: str | Path | None = None) -> dict[str, Any]:
        """Tar the fleet dir into a reproducible bundle.

        Defaults to `~/.kresko/fleets/<name>.tar.gz`.
        """
        import tarfile

        dest_path = Path(dest) if dest else (paths.fleets_dir() / f"{self.name}.tar.gz")
        dest_path.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(dest_path, "w:gz") as tar:
            tar.add(self.dir, arcname=self.name)
        payload = {"stage": "archive", "ok": True, "path": str(dest_path)}
        return payload

    def down(
        self,
        *,
        dry_run: bool = False,
        force_tag: str | None = None,
    ) -> dict[str, Any]:
        stage = "down"
        try:
            destroyed: dict[str, list[str]] = {}
            errors: list[str] = []
            if force_tag:
                self._load_env()
                for provider_name in known_provider_names():
                    try:
                        provider = self._providers.get(provider_name) or get_provider(provider_name)
                        self._providers[provider_name] = provider
                    except Exception as exc:
                        errors.append(f"{provider_name}: {exc}")
                        continue
                    try:
                        destroyed[provider_name] = provider.delete_tagged(force_tag, dry_run=dry_run)
                    except Exception as exc:
                        errors.append(f"{provider_name}: {exc}")
            else:
                fleet_assets = self.fleet_assets()
                providers = self._providers_for(
                    {a.get("provider", "") for a in fleet_assets if a.get("provider")}
                )
                for asset in fleet_assets:
                    provider_name = asset.get("provider", "")
                    provider = providers.get(provider_name)
                    if provider is None:
                        errors.append(f"{provider_name}: provider is not configured")
                        continue
                    try:
                        provider_id = provider.delete(
                            asset,
                            required_tags=[self.fleet_tag_value],
                            dry_run=dry_run,
                        )
                    except Exception as exc:
                        errors.append(f"{provider_name}: {exc}")
                        continue
                    if provider_id:
                        destroyed.setdefault(provider_name, []).append(provider_id)
            payload = {
                "destroyed_provider_ids": [
                    provider_id for ids in destroyed.values() for provider_id in ids
                ],
                "destroyed": destroyed,
                "errors": errors,
            }
            if errors:
                payload = {"stage": stage, "ok": False, "dry_run": dry_run, **payload}
                self._write_result(
                    stage,
                    False,
                    failures=[_node_failure("all", stage, "down", retryable=True) for _ in errors],
                    extra=payload,
                )
                return payload
            return self._success_result(stage, dry_run, payload)
        except Exception as exc:
            self._write_failure(stage, stage, exc)
            raise

    # block explorer (demoted: still works via the normal deploy/run path) --

    def add_explorer(self, node: str = "miner-0", **kwargs: Any) -> Any:
        """Attach a co-located block explorer to this fleet (declarative)."""
        from kresko.explorer import ExplorerSpec

        self._explorer = ExplorerSpec.create(node=node, **kwargs)
        return self._explorer

    def deploy_explorer(self, *, dry_run: bool = False) -> dict[str, Any]:
        return self._explorer_op("deploy", dry_run=dry_run)

    def redeploy_explorer(self, *, dry_run: bool = False) -> dict[str, Any]:
        return self._explorer_op("redeploy", dry_run=dry_run)

    def plan_explorer(self) -> dict[str, Any]:
        return self._explorer_op("plan")

    def explorer_status(self) -> dict[str, Any]:
        return self._explorer_op("status")

    def explorer_logs(self) -> dict[str, Any]:
        return self._explorer_op("logs")

    def explorer_stop(self) -> dict[str, Any]:
        return self._explorer_op("stop")

    def _explorer_op(self, op: str, **kwargs: Any) -> dict[str, Any]:
        if self._explorer is None:
            return {
                "stage": f"explorer-{op}",
                "ok": True,
                "skipped": True,
                "reason": "no explorer configured (call fleet.add_explorer(...))",
            }
        from kresko.explorer import ExplorerDeployment

        deployment = ExplorerDeployment(self, self._explorer)
        return getattr(deployment, op)(**kwargs)

    # internals -------------------------------------------------------------

    def _desired(self) -> list[DesiredNode]:
        out: list[DesiredNode] = []
        for node in self._nodes:
            prefix = node.name_prefix or node.role
            tags = sorted(
                set(
                    [
                        assets_store.REQUIRED_TAG,
                        self.fleet_tag_value,
                        role_tag(node.role),
                        *self.tags,
                        *node.tags,
                    ]
                )
            )
            for index in range(node.count):
                out.append(
                    DesiredNode(
                        provider=node.provider,
                        name=f"{prefix}-{index}",
                        role=node.role,
                        region=node.region,
                        size=node.size,
                        image=node.image,
                        tags=tags,
                        ssh_user=node.ssh_user or self.ssh.get("user", "root"),
                        provider_options=dict(node.provider_options),
                    )
                )
        return out

    def _load_env(self) -> None:
        """Load provider credentials before talking to any cloud API.

        Precedence (shell wins, via dotenv override=False): explicit shell env,
        then a project `.env` discovered from the CWD (handy when driving a
        fleet from a repo checkout), then `~/.kresko/.env` — the documented home
        for tokens. Plain `python fleet.py` has no CLI wrapper to load this, so
        the Fleet must do it itself.
        """
        load_experiment_env(experiment_root=Path.cwd())
        load_experiment_env(experiment_root=self.dir, repo_root=paths.kresko_home())

    def _providers_for(self, names: set[str]) -> dict[str, CloudProvider]:
        self._load_env()
        out = dict(self._providers)
        for name in sorted(n for n in names if n):
            if name in out:
                continue
            out[name] = get_provider(name)
        self._providers.update(out)
        return {name: out[name] for name in names if name in out}

    def _select(
        self,
        *,
        role: str | list[str] | None,
        name: str | list[str] | None,
        pattern: str | list[str] | None,
        failed_from: str | Path | None,
    ) -> list[dict[str, Any]]:
        return select(
            self.fleet_assets(),
            roles=role,
            names=name,
            patterns=pattern,
            failed_from=failed_from,
        )

    def _node_payload_paths(self, role: str | list[str] | None) -> list[str]:
        if role is None:
            roles = None
        elif isinstance(role, str):
            roles = {role}
        else:
            roles = {str(r) for r in role}
        out: list[str] = []
        for node in self._nodes:
            if roles is not None and node.role not in roles:
                continue
            for path in node.payload_paths:
                if path not in out:
                    out.append(path)
        return out

    def _absolute_payload_paths(
        self, payload: str | list[str] | None, *, role: str | list[str] | None = None
    ) -> list[str]:
        declared = _as_path_list(payload) or self._node_payload_paths(role)
        out: list[str] = []
        for path in declared:
            candidate = Path(path)
            out.append(str(candidate if candidate.is_absolute() else candidate.resolve()))
        return out

    def _write_pyinfra_deploy(
        self,
        filename: str,
        body: str,
        nodes: list[dict[str, Any]],
    ) -> tuple[Path, Path]:
        inventory = write_pyinfra_inventory(self.dir / "inventory.py", nodes, self.ssh)
        deploy_file = self.dir / filename
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
        stdout_path = self.dir / f"pyinfra.{stage}.stdout.log"
        stderr_path = self.dir / f"pyinfra.{stage}.stderr.log"
        stdout_text = result.stdout or ""
        stderr_text = result.stderr or ""
        stdout_path.write_text(stdout_text, encoding="utf-8")
        stderr_path.write_text(stderr_text, encoding="utf-8")
        failures: list[dict[str, Any]] = []
        if result.returncode != 0:
            failures.append(
                _node_failure(
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
        self._write_result(stage, result.returncode == 0, failures=failures, extra=payload)
        return payload

    def _success_result(
        self, stage: str, dry_run: bool, extra: dict[str, Any]
    ) -> dict[str, Any]:
        payload = {"stage": stage, "ok": True, "dry_run": dry_run, **extra}
        self._write_result(stage, True, extra=payload)
        return payload

    def _write_failure(self, stage: str, command: str, exc: Exception) -> None:
        self._write_result(
            stage,
            False,
            failures=[_node_failure("all", stage, command, retryable=True)],
            extra={"error": str(exc)},
        )

    # fleet-dir state I/O (salvaged from the old runs module) ---------------

    def _write_result(
        self,
        stage: str,
        ok: bool,
        *,
        failures: list[dict[str, Any]] | None = None,
        extra: dict[str, Any] | None = None,
    ) -> Path:
        path = self.dir / RESULT_FILENAME
        payload = {
            "stage": stage,
            "ok": ok,
            "fleet": self.name,
            "finished_at": _utc_now(),
            "failures": failures or [],
            **(extra or {}),
        }
        path.write_text(json.dumps(payload, indent=2, sort_keys=True, default=str) + "\n", encoding="utf-8")
        return path

    # explorer.py still calls write_result(fleet.dir, ...); keep that working.
    def _write_node_snapshot(self, asset: dict[str, Any]) -> Path:
        return write_node_snapshot(self.dir, asset)


# module-level helpers reused by explorer.py ------------------------------------


def _utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def utc_now() -> str:
    return _utc_now()


def node_failure(
    node: str,
    stage: str,
    command: str,
    *,
    exit_code: int | None = None,
    stdout_path: str | None = None,
    stderr_path: str | None = None,
    retryable: bool = True,
) -> dict[str, Any]:
    return {
        "node": node,
        "stage": stage,
        "command": command,
        "exit_code": exit_code,
        "stdout_path": stdout_path,
        "stderr_path": stderr_path,
        "retryable": retryable,
    }


# Internal alias so methods above can call it without the public name.
_node_failure = node_failure


def write_result(
    fleet_dir: str | Path,
    stage: str,
    ok: bool,
    *,
    failures: list[dict[str, Any]] | None = None,
    extra: dict[str, Any] | None = None,
) -> Path:
    """Write `<fleet_dir>/result.json`. Used by explorer.py."""
    path = Path(fleet_dir) / RESULT_FILENAME
    payload = {
        "stage": stage,
        "ok": ok,
        "finished_at": _utc_now(),
        "failures": failures or [],
        **(extra or {}),
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True, default=str) + "\n", encoding="utf-8")
    return path


def write_node_snapshot(fleet_dir: str | Path, asset: dict[str, Any]) -> Path:
    name = asset.get("name") or f"{asset.get('provider', 'unknown')}-{asset.get('provider_id', 'unknown')}"
    path = Path(fleet_dir) / NODES_DIRNAME / f"{name}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(deepcopy(asset), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def git_revision(cwd: str | Path) -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            check=False,
        )
        return out.stdout.strip() if out.returncode == 0 else ""
    except FileNotFoundError:
        return ""


def _plan_to_jsonable(plan: dict[str, list[dict[str, Any]]]) -> dict[str, list[dict[str, Any]]]:
    return {key: list(value) for key, value in plan.items()}


def _sha256_file(path: Path) -> str:
    import hashlib

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(64 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _payload_archive_names(
    payload_paths: list[str], *, require_exists: bool = True
) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    for raw_path in payload_paths:
        path = Path(raw_path)
        if require_exists and not path.exists():
            raise FileNotFoundError(path)
        name = path.name
        if not name:
            raise ValueError(f"payload path has no archive name: {path}")
        if name in seen:
            raise ValueError(f"duplicate payload archive entry name: {name}")
        seen.add(name)
        names.append(name)
    if not names:
        raise ValueError("no payload paths selected for deploy")
    return names


def _build_payload_archive(payload_paths: list[str], archive: Path) -> list[str]:
    names = _payload_archive_names(payload_paths)
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "w:gz") as tar:
        for raw_path, name in zip(payload_paths, names, strict=True):
            tar.add(Path(raw_path), arcname=name)
    return names


def _payload_s3_key(fleet_name: str) -> str:
    prefix = os.environ.get("KRESKO_PAYLOAD_S3_PREFIX", "kresko").strip().strip("/")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    filename = f"payload-{stamp}-{secrets.token_hex(4)}.tar.gz"
    return f"{prefix}/{fleet_name}/{filename}" if prefix else f"{fleet_name}/{filename}"


def _payload_s3_expires() -> int:
    raw = os.environ.get("KRESKO_PAYLOAD_S3_EXPIRES", "21600")
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError("KRESKO_PAYLOAD_S3_EXPIRES must be an integer") from exc
    if value <= 0:
        raise ValueError("KRESKO_PAYLOAD_S3_EXPIRES must be positive")
    return value


def _deploy_s3_body(
    presigned_url: str,
    payload_names: list[str],
    *,
    archive_sha256: str,
    snapshot_url: str | None,
) -> str:
    body = (
        "from kresko.remote import pyinfra_deploy_s3\n"
        f"pyinfra_deploy_s3({presigned_url!r}, {payload_names!r}, "
        f"archive_sha256={archive_sha256!r})\n"
    )
    if snapshot_url:
        body += (
            "from kresko.remote import pyinfra_state_snapshot\n"
            f"pyinfra_state_snapshot({snapshot_url!r})\n"
        )
    return body


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
    """Locate `<payload>/build/{manifest.txt,zebrad,kresko}` and return what we
    know locally: the manifest's recorded hashes plus our own re-hash of the
    staged files, so a tampered payload directory is visible.
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
    """Surface node_init.sh's PROVENANCE lines so a redeploy makes binary swaps
    obvious."""
    payload["binary_provenance_remote"] = _parse_remote_provenance(stdout_text)


def _parse_remote_provenance(stdout_text: str) -> dict[str, list[dict[str, str]]]:
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
