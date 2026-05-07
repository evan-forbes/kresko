"""`kresko` CLI entrypoint and per-experiment CLI helper.

Top-level `kresko` subcommands::

    kresko run <experiment> [--name <slug>] -- [args...]
    kresko sync
    kresko assets list [--tag tag] [--provider name]
    kresko assets show <provider> <provider_id>
    kresko runs list <experiment>
    kresko runs show <experiment> <run-name>

`kresko run` allocates a new run dir under `~/.kresko/runs/<exp>/<name>/`,
copies `~/.kresko/experiments/<exp>/` into it, and executes the copied
`run.py` with `KRESKO_EXPERIMENT` / `KRESKO_RUN_NAME` / `KRESKO_RUN_DIR` set.

Per-experiment scripts use `run_experiment()` for the standard lifecycle
verbs (plan/up/deploy/down/collect) plus any experiment-specific actions::

    def build_experiment() -> Experiment: ...
    def smoke(exp, args): return exp.run_tmux(...)
    if __name__ == "__main__":
        sys.exit(run_experiment(build_experiment, {"smoke": smoke}))
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
from pathlib import Path
from typing import IO, Any, Callable

from kresko_py import assets, paths
from kresko_py.env import load_experiment_env
from kresko_py.experiment import ENV_EXPERIMENT, ENV_RUN_DIR, ENV_RUN_NAME, Experiment
from kresko_py.runs import (
    MANIFEST_FILENAME,
    RESULT_FILENAME,
    list_runs,
    read_manifest,
    start_run,
)
from kresko_py.sync import report_to_dict, sync_all

ExperimentAction = Callable[[Experiment, argparse.Namespace], dict[str, Any]]
ExperimentFactory = Callable[[], Experiment]
DEFAULT_ACTIONS = ("plan", "up", "deploy", "run", "collect", "down")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="kresko", description="Kresko orchestration CLI")
    sub = parser.add_subparsers(dest="command", required=True)

    p_run = sub.add_parser("run", help="Run an experiment in a fresh run dir")
    p_run.add_argument("experiment", help="experiment name (matches ~/.kresko/experiments/<exp>/)")
    p_run.add_argument("--name", help="run name slug (defaults to experiment name)")
    p_run.add_argument(
        "--python",
        default=sys.executable,
        help="python interpreter used to launch run.py (default: current)",
    )
    p_run.add_argument("args", nargs=argparse.REMAINDER, help="extra args passed to run.py")

    sub.add_parser("sync", help="Refresh ~/.kresko/assets/ from the cloud")

    p_assets = sub.add_parser("assets", help="Inspect the asset store")
    p_assets_sub = p_assets.add_subparsers(dest="assets_command", required=True)
    p_assets_list = p_assets_sub.add_parser("list", help="List assets, optionally filtered")
    p_assets_list.add_argument("--tag", action="append", default=[], help="filter by tag (repeat for AND)")
    p_assets_list.add_argument("--provider", help="filter by provider name")
    p_assets_show = p_assets_sub.add_parser("show", help="Print one asset")
    p_assets_show.add_argument("provider")
    p_assets_show.add_argument("provider_id")

    p_runs = sub.add_parser("runs", help="Inspect runs")
    p_runs_sub = p_runs.add_subparsers(dest="runs_command", required=True)
    p_runs_list = p_runs_sub.add_parser("list", help="List runs of an experiment")
    p_runs_list.add_argument("experiment")
    p_runs_show = p_runs_sub.add_parser("show", help="Show a run's manifest and result")
    p_runs_show.add_argument("experiment")
    p_runs_show.add_argument("run_name")

    args = parser.parse_args(argv)

    if args.command == "run":
        return cmd_run(args)
    if args.command == "sync":
        return cmd_sync(args)
    if args.command == "assets":
        return cmd_assets(args)
    if args.command == "runs":
        return cmd_runs(args)
    parser.error(f"unknown command {args.command!r}")
    return 2


def cmd_run(args: argparse.Namespace) -> int:
    paths.ensure_home()
    load_experiment_env(paths.kresko_home())

    extra = list(args.args or [])
    if extra and extra[0] == "--":
        extra = extra[1:]

    run_path = start_run(args.experiment, name=args.name, argv=[args.experiment, *extra])
    run_py = run_path / "run.py"
    if not run_py.exists():
        print(f"error: {run_py} does not exist (experiment must include run.py)", file=sys.stderr)
        return 2

    env = {
        **os.environ,
        ENV_EXPERIMENT: args.experiment,
        ENV_RUN_NAME: run_path.name,
        ENV_RUN_DIR: str(run_path),
    }
    print(f"run dir: {run_path}", file=sys.stderr)
    cmd = [args.python, str(run_py), *extra]
    stdout_path = run_path / "stdout.log"
    stderr_path = run_path / "stderr.log"
    with stdout_path.open("a", encoding="utf-8") as out, stderr_path.open("a", encoding="utf-8") as err:
        proc = subprocess.Popen(
            cmd,
            cwd=str(run_path),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert proc.stdout is not None and proc.stderr is not None
        out_thread = threading.Thread(target=_tee, args=(proc.stdout, sys.stdout, out))
        err_thread = threading.Thread(target=_tee, args=(proc.stderr, sys.stderr, err))
        out_thread.start()
        err_thread.start()
        proc.wait()
        out_thread.join()
        err_thread.join()
        return proc.returncode


def _tee(src: IO[str], *dests: IO[str]) -> None:
    for line in iter(src.readline, ""):
        for dest in dests:
            dest.write(line)
            dest.flush()


def cmd_sync(args: argparse.Namespace) -> int:
    paths.ensure_home()
    load_experiment_env(paths.kresko_home())
    reports = sync_all()
    out = [report_to_dict(report) for report in reports]
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0 if all(not r.errors for r in reports) else 1


def cmd_assets(args: argparse.Namespace) -> int:
    paths.ensure_home()
    if args.assets_command == "list":
        items = assets.list_assets(tags=args.tag, provider=args.provider)
        print(json.dumps(_summarize_assets(items), indent=2, sort_keys=True))
        return 0
    if args.assets_command == "show":
        try:
            asset = assets.read_asset(args.provider, args.provider_id)
        except FileNotFoundError:
            print(f"asset {args.provider}-{args.provider_id} not found", file=sys.stderr)
            return 1
        print(json.dumps(asset, indent=2, sort_keys=True))
        return 0
    return 2


def cmd_runs(args: argparse.Namespace) -> int:
    paths.ensure_home()
    if args.runs_command == "list":
        runs = list_runs(args.experiment)
        out: list[dict[str, Any]] = []
        for path in runs:
            manifest = _read_safely(path / MANIFEST_FILENAME)
            result = _read_safely(path / RESULT_FILENAME)
            out.append(
                {
                    "run_name": path.name,
                    "started_at": manifest.get("started_at", ""),
                    "stage": result.get("stage", ""),
                    "ok": result.get("ok"),
                }
            )
        print(json.dumps(out, indent=2, sort_keys=True))
        return 0
    if args.runs_command == "show":
        run_path = paths.run_dir(args.experiment, args.run_name)
        if not run_path.exists():
            print(f"run {args.experiment}/{args.run_name} not found", file=sys.stderr)
            return 1
        out = {
            "run_dir": str(run_path),
            "manifest": _read_safely(run_path / MANIFEST_FILENAME),
            "result": _read_safely(run_path / RESULT_FILENAME),
        }
        print(json.dumps(out, indent=2, sort_keys=True))
        return 0
    return 2


def _summarize_assets(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "provider": a.get("provider"),
            "provider_id": a.get("provider_id"),
            "name": a.get("name"),
            "role": a.get("role"),
            "experiment": a.get("experiment"),
            "run": a.get("run"),
            "public_ip": a.get("public_ip"),
            "status": a.get("status"),
            "tags": a.get("tags", []),
        }
        for a in items
    ]


def _read_safely(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


def run_experiment(
    build_experiment: ExperimentFactory,
    extra_actions: dict[str, ExperimentAction] | None = None,
    *,
    argv: list[str] | None = None,
) -> int:
    """Default CLI shim for an experiment's `run.py`.

    Parses standard subcommands and dispatches to `Experiment` lifecycle
    methods. Pass `extra_actions={"verb": fn}` to register experiment-
    specific verbs; their handler signature is `fn(exp, args) -> dict`.
    Returns the process exit code (0 on `result["ok"]`, 1 otherwise).
    """

    extra_actions = extra_actions or {}
    parser = _build_experiment_parser(extra_actions.keys())
    args = parser.parse_args(argv)

    experiment = build_experiment()

    action = args.action
    if action in extra_actions:
        result = extra_actions[action](experiment, args)
    elif action == "plan":
        result = experiment.plan()
    elif action == "up":
        result = experiment.up(
            dry_run=args.dry_run, retry_failed=args.retry_failed
        )
    elif action == "deploy":
        result = experiment.deploy(
            role=args.role,
            name=args.name,
            pattern=args.pattern,
            failed_from=args.failed_from,
            dry_run=args.dry_run,
        )
    elif action == "run":
        if not args.command:
            parser.error("run requires --command")
        result = experiment.run_command(
            args.command,
            role=args.role,
            name=args.name,
            pattern=args.pattern,
            failed_from=args.failed_from,
            dry_run=args.dry_run,
        )
    elif action == "collect":
        if not args.path:
            parser.error("collect requires at least one --path")
        result = experiment.collect(
            list(args.path),
            role=args.role,
            name=args.name,
            pattern=args.pattern,
            failed_from=args.failed_from,
            dest=args.dest,
            dry_run=args.dry_run,
        )
    elif action == "down":
        result = experiment.down(dry_run=args.dry_run, force_tag=args.force_tag)
    else:
        parser.error(f"unknown action {action!r}")
        return 2

    print(json.dumps(result, indent=2, sort_keys=True, default=str))
    return 0 if result.get("ok", False) else 1


def _build_experiment_parser(extra_verbs: Any) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Kresko experiment runner")
    choices = list(DEFAULT_ACTIONS) + sorted(extra_verbs)
    parser.add_argument("action", choices=choices)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--retry-failed",
        action="store_true",
        help="re-poll droplets currently marked status=failed (up only)",
    )
    parser.add_argument("--role", action="append", default=None, help="filter by role (repeat)")
    parser.add_argument("--name", action="append", default=None, help="filter by node name (repeat)")
    parser.add_argument(
        "--pattern", action="append", default=None, help="fnmatch pattern over node names (repeat)"
    )
    parser.add_argument(
        "--failed-from",
        help="path to a result.json; deploy only the nodes that failed there",
    )
    parser.add_argument("--force-tag", help="for `down`: destroy all droplets with this tag")
    parser.add_argument(
        "--command", help="for `run`: shell command to execute on selected nodes"
    )
    parser.add_argument(
        "--path",
        action="append",
        default=None,
        help="for `collect`: remote path to pull (repeat)",
    )
    parser.add_argument("--dest", help="for `collect`: local destination directory")
    return parser


if __name__ == "__main__":
    raise SystemExit(main())
