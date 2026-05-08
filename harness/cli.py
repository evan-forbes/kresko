"""`kresko` CLI entrypoint and per-experiment CLI helper.

Top-level `kresko` subcommands::

    kresko run <experiment> [--run-name <slug>] -- [args...]
    kresko sync
    kresko assets list [--tag tag] [--provider name]
    kresko assets show <provider> <provider_id>
    kresko runs list <experiment>
    kresko runs show <experiment> <run-name>

`kresko run` allocates a new run dir under `~/.kresko/runs/<exp>/<run-name>/`,
copies `~/.kresko/experiments/<exp>/` into it, and executes the copied
`run.py` with `KRESKO_EXPERIMENT` / `KRESKO_RUN_NAME` / `KRESKO_RUN_DIR` set.

A literal `--` is required before any args forwarded to `run.py`. This
prevents `--name` (an experiment-level node filter) from being silently
swallowed as a run dir name.

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

from harness import assets, paths
from harness.env import load_experiment_env
from harness.experiment import ENV_EXPERIMENT, ENV_RUN_DIR, ENV_RUN_NAME, Experiment
from harness.runs import (
    MANIFEST_FILENAME,
    RESULT_FILENAME,
    list_runs,
    read_manifest,
    start_run,
)
from harness.sync import report_to_dict, sync_all

ExperimentAction = Callable[[Experiment, argparse.Namespace], dict[str, Any]]
ExperimentFactory = Callable[[], Experiment]
DEFAULT_ACTIONS = ("plan", "up", "deploy", "run", "reset", "collect", "down")


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    argv = list(argv)
    forwarded: list[str] = []
    if argv and argv[0] == "run" and "--" in argv:
        idx = argv.index("--")
        forwarded = argv[idx + 1 :]
        argv = argv[:idx]

    parser = argparse.ArgumentParser(prog="kresko", description="Kresko orchestration CLI")
    sub = parser.add_subparsers(dest="command", required=True)

    p_run = sub.add_parser("run", help="Run an experiment in a fresh run dir")
    p_run.add_argument("experiment", help="experiment name (matches ~/.kresko/experiments/<exp>/)")
    p_run.add_argument(
        "-n",
        "--run-name",
        dest="run_name",
        help="run dir slug (defaults to a short timestamped slug)",
    )
    p_run.add_argument(
        "--python",
        default=None,
        help=(
            "python interpreter used to launch run.py. Defaults to "
            "`uv run python` from the harness project root so pyinfra and "
            "other deps are present. Pass an explicit path to bypass uv."
        ),
    )

    p_sync = sub.add_parser("sync", help="Refresh ~/.kresko/assets/ from the cloud")
    p_sync.add_argument(
        "--provider",
        action="append",
        default=None,
        help="provider to sync (repeatable; defaults to all known providers)",
    )

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

    args, leftover = parser.parse_known_args(argv)
    if leftover:
        if args.command == "run":
            parser.error(
                f"unrecognized args after experiment: {' '.join(leftover)}\n"
                f"forward args to run.py with a literal `--`: "
                f"kresko run {args.experiment} -- {' '.join(leftover)}"
            )
        parser.error(f"unrecognized arguments: {' '.join(leftover)}")

    if args.command == "run":
        return cmd_run(args, forwarded)
    if args.command == "sync":
        return cmd_sync(args)
    if args.command == "assets":
        return cmd_assets(args)
    if args.command == "runs":
        return cmd_runs(args)
    parser.error(f"unknown command {args.command!r}")
    return 2


def cmd_run(args: argparse.Namespace, forwarded: list[str]) -> int:
    paths.ensure_home()
    load_experiment_env(
        experiment_root=paths.experiment_dir(args.experiment),
        repo_root=paths.kresko_home(),
    )

    extra = list(forwarded)
    run_path = start_run(args.experiment, name=args.run_name, argv=[args.experiment, *extra])
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
    cmd = _build_run_command(args.python, run_py, extra)
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


def _build_run_command(
    python_override: str | None, run_py: Path, extra: list[str]
) -> list[str]:
    """Pick the interpreter for `run.py`.

    Default: `uv run --project <kresko-repo> python <run_py>` so the spawned
    process always has pyinfra and the other repo deps, regardless of how
    the user invoked the CLI. Explicit `--python` skips uv entirely.
    """

    if python_override:
        return [python_override, str(run_py), *extra]
    project_root = _kresko_project_root()
    if project_root and _which("uv"):
        return [
            "uv",
            "run",
            "--project",
            str(project_root),
            "python",
            str(run_py),
            *extra,
        ]
    return [sys.executable, str(run_py), *extra]


def _kresko_project_root() -> Path | None:
    """Find the harness project root (the directory holding pyproject.toml)."""
    candidate = Path(__file__).resolve().parent.parent
    if (candidate / "pyproject.toml").exists():
        return candidate
    return None


def _which(program: str) -> str | None:
    from shutil import which as _shutil_which

    return _shutil_which(program)


def cmd_sync(args: argparse.Namespace) -> int:
    paths.ensure_home()
    load_experiment_env(
        experiment_root=paths.kresko_home(),
        repo_root=paths.kresko_home(),
    )
    reports = sync_all(providers=args.provider)
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
    _apply_provider_overrides(experiment, args, parser)

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
    elif action == "reset":
        result = experiment.reset(
            role=args.role,
            name=args.name,
            pattern=args.pattern,
            failed_from=args.failed_from,
            dry_run=args.dry_run,
        )
    elif action == "down":
        result = experiment.down(dry_run=args.dry_run, force_tag=args.force_tag)
    else:
        parser.error(f"unknown action {action!r}")
        return 2

    print(json.dumps(result, indent=2, sort_keys=True, default=str))
    return 0 if result.get("ok", False) else 1


def _apply_provider_overrides(
    experiment: Experiment,
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> None:
    """Apply --size/--image/--count/--region overrides to the built experiment.

    Each flag accepts `role=value` (per-role) or just `value` (all roles).
    Roles that don't exist in the experiment surface as a parser error so a
    typo doesn't silently no-op.
    """
    if not (args.size or args.image or args.count or args.region):
        return

    if not hasattr(experiment, "override"):
        # Stubs in tests may not implement override(); skip silently.
        return

    known_roles = {node.role for node, _ in getattr(experiment, "_node_specs", [])}

    def _parsed(values: list[str] | None, parse_value: Callable[[str], Any]) -> list[tuple[str | None, Any]]:
        out: list[tuple[str | None, Any]] = []
        for raw in values or []:
            if "=" in raw:
                role, value = raw.split("=", 1)
                role = role.strip() or None
            else:
                role, value = None, raw
            if role is not None and known_roles and role not in known_roles:
                parser.error(
                    f"unknown role {role!r} (experiment defines: {sorted(known_roles)})"
                )
            out.append((role, parse_value(value)))
        return out

    def _int(value: str) -> int:
        try:
            return int(value)
        except ValueError:
            parser.error(f"--count value must be an integer (got {value!r})")
            raise  # appease type checker; parser.error exits

    for role, size in _parsed(args.size, str):
        experiment.override(role, size=size)
    for role, image in _parsed(args.image, str):
        experiment.override(role, image=image)
    for role, region in _parsed(args.region, str):
        experiment.override(role, region=region)
    for role, count in _parsed(args.count, _int):
        experiment.override(role, count=count)


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
    parser.add_argument("--force-tag", help="for `down`: destroy all instances with this tag")
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

    # Provider-shape overrides. These let `kresko run <exp> -- up --size ...`
    # retune a run without editing run.py. build_experiment() must opt into
    # them by reading from `args` (see `apply_provider_overrides`).
    parser.add_argument(
        "--size",
        action="append",
        default=None,
        metavar="role=slug",
        help=(
            "override cloud size/plan for a role (e.g. miner=s-8vcpu-16gb). "
            "Repeat for multiple roles. Without role= applies to all."
        ),
    )
    parser.add_argument(
        "--image",
        action="append",
        default=None,
        metavar="role=image",
        help="override cloud image for a role. Repeat for multiple roles.",
    )
    parser.add_argument(
        "--count",
        action="append",
        default=None,
        metavar="role=N",
        help="override node count for a role (e.g. miner=8). Repeat for roles.",
    )
    parser.add_argument(
        "--region",
        action="append",
        default=None,
        metavar="role=slug",
        help="override cloud region for a role.",
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(main())
