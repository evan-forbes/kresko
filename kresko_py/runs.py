"""Runs: name-based directories under `~/.kresko/runs/<exp>/<run-name>/`.

A run is the unit of result encapsulation. `start_run` resolves a free slug
(adding `-2`, `-3`, … on collision), copies the experiment source into the
run dir, and writes `manifest.json`. Stdout/stderr tee'ing is the caller's
job (the CLI's, in practice).
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import subprocess
from copy import deepcopy
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator

from kresko_py import paths

MANIFEST_FILENAME = "manifest.json"
NODES_DIRNAME = "nodes"
RESULT_FILENAME = "result.json"


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def default_run_slug(now: datetime | None = None) -> str:
    """Short timestamped slug like `r-20260507-141502` (UTC)."""
    moment = (now or datetime.now(timezone.utc)).strftime("%Y%m%d-%H%M%S")
    return f"r-{moment}"


def resolve_run_name(experiment: str, name: str | None = None) -> str:
    base = name or default_run_slug()
    paths.validate_slug(base, kind="run")
    runs_root = paths.experiment_runs_dir(experiment)
    if not (runs_root / base).exists():
        return base
    n = 2
    while True:
        candidate = f"{base}-{n}"
        if not (runs_root / candidate).exists():
            return candidate
        n += 1


def git_revision(cwd: Path) -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
        )
        return out.stdout.strip() if out.returncode == 0 else ""
    except FileNotFoundError:
        return ""


def start_run(
    experiment: str,
    *,
    name: str | None = None,
    argv: list[str] | None = None,
) -> Path:
    """Allocate a fresh run dir, copy the experiment source in, write manifest."""

    paths.ensure_home()
    paths.validate_slug(experiment, kind="experiment")

    src = paths.experiment_dir(experiment)
    if not src.exists():
        raise FileNotFoundError(f"experiment source {src} does not exist")

    run_name = resolve_run_name(experiment, name)
    run_path = paths.run_dir(experiment, run_name)
    run_path.mkdir(parents=True, exist_ok=False)

    _copy_experiment_source(src, run_path)
    (run_path / NODES_DIRNAME).mkdir(exist_ok=True)
    (run_path / "data").mkdir(exist_ok=True)

    manifest = {
        "experiment": experiment,
        "run_name": run_name,
        "argv": list(argv or []),
        "git_revision": git_revision(src),
        "host": os.uname().nodename if hasattr(os, "uname") else "",
        "started_at": utc_now(),
        "kresko_home": str(paths.kresko_home()),
    }
    write_manifest(run_path, manifest)
    return run_path


ENV_EXPERIMENT = "KRESKO_EXPERIMENT"
ENV_RUN_NAME = "KRESKO_RUN_NAME"
ENV_RUN_DIR = "KRESKO_RUN_DIR"


@contextlib.contextmanager
def open_run(
    experiment: str,
    *,
    name: str | None = None,
    argv: list[str] | None = None,
    chdir: bool = False,
) -> Iterator[Path]:
    """Allocate a run dir and expose it via env vars without going through the CLI.

    Sets `KRESKO_EXPERIMENT` / `KRESKO_RUN_NAME` / `KRESKO_RUN_DIR` so that
    `Experiment.current()` works inside the block, and restores the prior
    environment on exit. If `chdir=True`, also chdir's into the run dir
    (matches the CLI's behavior).
    """

    run_path = start_run(experiment, name=name, argv=argv)
    prior_env = {
        ENV_EXPERIMENT: os.environ.get(ENV_EXPERIMENT),
        ENV_RUN_NAME: os.environ.get(ENV_RUN_NAME),
        ENV_RUN_DIR: os.environ.get(ENV_RUN_DIR),
    }
    os.environ[ENV_EXPERIMENT] = experiment
    os.environ[ENV_RUN_NAME] = run_path.name
    os.environ[ENV_RUN_DIR] = str(run_path)
    prior_cwd = Path.cwd() if chdir else None
    try:
        if chdir:
            os.chdir(run_path)
        yield run_path
    finally:
        if prior_cwd is not None:
            os.chdir(prior_cwd)
        for key, value in prior_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def write_manifest(run_path: Path, manifest: dict[str, Any]) -> Path:
    path = run_path / MANIFEST_FILENAME
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def read_manifest(run_path: Path) -> dict[str, Any]:
    return json.loads((run_path / MANIFEST_FILENAME).read_text(encoding="utf-8"))


def write_result(
    run_path: Path,
    stage: str,
    ok: bool,
    *,
    failures: list[dict[str, Any]] | None = None,
    extra: dict[str, Any] | None = None,
) -> Path:
    path = run_path / RESULT_FILENAME
    payload = {
        "stage": stage,
        "ok": ok,
        "finished_at": utc_now(),
        "failures": failures or [],
        **(extra or {}),
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def write_node_snapshot(run_path: Path, asset: dict[str, Any]) -> Path:
    name = asset.get("name") or f"{asset.get('provider', 'unknown')}-{asset.get('provider_id', 'unknown')}"
    path = run_path / NODES_DIRNAME / f"{name}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(deepcopy(asset), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


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


def list_runs(experiment: str) -> list[Path]:
    root = paths.experiment_runs_dir(experiment)
    if not root.exists():
        return []
    return sorted([p for p in root.iterdir() if p.is_dir()])


def latest_result_path(experiment: str) -> Path | None:
    candidates: list[Path] = []
    for run_path in list_runs(experiment):
        result = run_path / RESULT_FILENAME
        if result.exists():
            candidates.append(result)
    if not candidates:
        return None
    return max(candidates, key=lambda path: path.stat().st_mtime)


def _copy_experiment_source(src: Path, dest: Path) -> None:
    skip = {"__pycache__", ".pytest_cache"}
    for entry in src.iterdir():
        if entry.name in skip:
            continue
        target = dest / entry.name
        if entry.is_dir():
            shutil.copytree(entry, target, ignore=shutil.ignore_patterns(*skip))
        else:
            shutil.copy2(entry, target)
