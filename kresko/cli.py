"""`kresko-fleet` CLI: a console over the global asset store.

Invoked as `kresko-fleet` (a console script) or `python -m kresko`. The Rust
binary owns the bare `kresko` command (genesis/mine/txblast/...); this is a
separate Python tool for fleet ops.

Orchestration lives in your fleet scripts (plain Python: import `Fleet`, call
methods). This CLI is the inspection + safety layer — deliberately able to
operate without re-running any script, which is what makes emergency teardown
and CI cleanup reliable::

    kresko-fleet ls                 # list fleets and their nodes
    kresko-fleet status <fleet>     # RPC heights / health (reads the asset store)
    kresko-fleet sync [--provider ...]  # refresh ~/.kresko/assets/ from the clouds
    kresko-fleet assets list|show ...   # raw asset inspection
    kresko-fleet down <fleet>       # destroy a fleet by tag — no script needed
    kresko-fleet archive <fleet>    # tar the fleet dir into a bundle

`down` works two ways on purpose: `fleet.down()` from a script when you hold the
object, and `kresko-fleet down <name>` from the asset store when you don't (the
script is gone, the CI job is being cleaned up in a trap/finally).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from typing import Any

from kresko import assets, paths, selectors, status
from kresko.env import load_experiment_env
from kresko.fleet import Fleet
from kresko.providers import fleet_tag
from kresko.sync import report_to_dict, sync_all


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    parser = argparse.ArgumentParser(prog="kresko-fleet", description="Kresko fleet console")
    sub = parser.add_subparsers(dest="command", required=True)

    p_ls = sub.add_parser("ls", help="List fleets and their nodes (from the asset store)")
    p_ls.add_argument("fleet", nargs="?", help="restrict to one fleet")
    p_ls.add_argument("--provider", help="filter by provider name")

    p_sync = sub.add_parser("sync", help="Refresh ~/.kresko/assets/ from the cloud")
    p_sync.add_argument(
        "--provider",
        action="append",
        default=None,
        help="provider to sync (repeatable; defaults to all known providers)",
    )

    p_status = sub.add_parser(
        "status", help="Query node block heights over RPC (reads the asset store)"
    )
    p_status.add_argument("fleet", nargs="?", help="fleet name (filters by fleet tag)")
    p_status.add_argument("--tag", action="append", default=[], help="filter by tag (repeat for AND)")
    p_status.add_argument("--provider", help="filter by provider name")
    p_status.add_argument("--role", action="append", default=None, help="filter by role (repeat)")
    p_status.add_argument("--name", action="append", default=None, help="filter by node name (repeat)")
    p_status.add_argument(
        "--pattern", action="append", default=None, help="fnmatch pattern over node names (repeat)"
    )
    p_status.add_argument(
        "--rpc-port",
        type=int,
        default=int(os.environ.get("KRESKO_RPC_PORT", status.DEFAULT_RPC_PORT)),
        help=(
            "RPC port to query (default: $KRESKO_RPC_PORT or "
            f"{status.DEFAULT_RPC_PORT}; local-genesis nodes use 18232)"
        ),
    )
    p_status.add_argument(
        "--timeout", type=float, default=status.DEFAULT_TIMEOUT, help="per-node RPC timeout in seconds"
    )
    p_status.add_argument("--summary", action="store_true", help="print aggregate height stats only")
    p_status.add_argument("--json", action="store_true", help="emit JSON instead of a table")

    p_assets = sub.add_parser("assets", help="Inspect the asset store")
    p_assets_sub = p_assets.add_subparsers(dest="assets_command", required=True)
    p_assets_list = p_assets_sub.add_parser("list", help="List assets, optionally filtered")
    p_assets_list.add_argument("--tag", action="append", default=[], help="filter by tag (repeat for AND)")
    p_assets_list.add_argument("--provider", help="filter by provider name")
    p_assets_show = p_assets_sub.add_parser("show", help="Print one asset")
    p_assets_show.add_argument("provider")
    p_assets_show.add_argument("provider_id")

    p_down = sub.add_parser("down", help="Destroy a fleet by its tag (no script needed)")
    p_down.add_argument("fleet", help="fleet name")
    p_down.add_argument("--dry-run", action="store_true", help="validate + report without deleting")
    p_down.add_argument(
        "--force-tag",
        help="destroy all instances carrying this tag (must be fleet-<...> or role-<...>)",
    )

    p_archive = sub.add_parser("archive", help="Tar a fleet dir into a reproducible bundle")
    p_archive.add_argument("fleet", help="fleet name")
    p_archive.add_argument("--dest", help="output path (default: ~/.kresko/fleets/<name>.tar.gz)")

    p_download = sub.add_parser("download", help="Download artifacts from fleet nodes")
    p_download_sub = p_download.add_subparsers(dest="download_command", required=True)
    p_download_traces = p_download_sub.add_parser(
        "traces", help="Download standard logs and trace directories"
    )
    p_download_traces.add_argument("fleet", help="fleet name")
    p_download_traces.add_argument("--role", action="append", default=None, help="filter by role")
    p_download_traces.add_argument("--name", action="append", default=None, help="filter by node name")
    p_download_traces.add_argument(
        "--pattern", action="append", default=None, help="fnmatch pattern over node names"
    )
    p_download_traces.add_argument("--dest", help="local destination (default: fleet data dir)")
    p_download_traces.add_argument("--dry-run", action="store_true", help="write pyinfra plan only")

    args = parser.parse_args(argv)

    if args.command == "ls":
        return cmd_ls(args)
    if args.command == "sync":
        return cmd_sync(args)
    if args.command == "status":
        return cmd_status(args)
    if args.command == "assets":
        return cmd_assets(args)
    if args.command == "down":
        return cmd_down(args)
    if args.command == "archive":
        return cmd_archive(args)
    if args.command == "download":
        return cmd_download(args)
    parser.error(f"unknown command {args.command!r}")
    return 2


def _load_creds() -> None:
    """Load ~/.kresko/.env (and any repo .env) so providers can authenticate."""
    load_experiment_env(experiment_root=paths.kresko_home(), repo_root=paths.kresko_home())


def cmd_ls(args: argparse.Namespace) -> int:
    paths.ensure_home()
    items = assets.list_assets(provider=args.provider)
    fleets: dict[str, list[dict[str, Any]]] = {}
    for asset in items:
        name = asset.get("fleet") or "(unowned)"
        fleets.setdefault(name, []).append(asset)
    out = []
    for name in sorted(fleets):
        if args.fleet and name != args.fleet:
            continue
        nodes = fleets[name]
        out.append(
            {
                "fleet": name,
                "nodes": len(nodes),
                "active": sum(1 for n in nodes if selectors.is_active(n)),
                "roles": sorted({n.get("role", "") for n in nodes if n.get("role")}),
                "providers": sorted({n.get("provider", "") for n in nodes if n.get("provider")}),
            }
        )
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0


def cmd_sync(args: argparse.Namespace) -> int:
    paths.ensure_home()
    _load_creds()
    reports = sync_all(providers=args.provider)
    out = [report_to_dict(report) for report in reports]
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0 if all(not r.errors for r in reports) else 1


def cmd_status(args: argparse.Namespace) -> int:
    paths.ensure_home()
    tags = list(args.tag)
    if args.fleet:
        tags.append(fleet_tag(args.fleet))
    items = assets.list_assets(tags=tags, provider=args.provider)
    items = selectors.select(
        items,
        roles=args.role,
        names=args.name,
        patterns=args.pattern,
    )

    report = status.query_status(items, rpc_port=args.rpc_port, timeout=args.timeout)

    if args.json:
        payload = status.summarize(report) if args.summary else report.to_dict()
        print(json.dumps(payload, indent=2, sort_keys=True))
    elif args.summary:
        print(status.render_summary(status.summarize(report)))
    else:
        print(status.render_report(report))
    return 0


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


def cmd_down(args: argparse.Namespace) -> int:
    paths.ensure_home()
    _load_creds()
    fleet = Fleet(args.fleet)
    result = fleet.down(dry_run=args.dry_run, force_tag=args.force_tag)
    print(json.dumps(result, indent=2, sort_keys=True, default=str))
    return 0 if result.get("ok", False) else 1


def cmd_archive(args: argparse.Namespace) -> int:
    paths.ensure_home()
    fleet = Fleet(args.fleet)
    result = fleet.archive(dest=args.dest)
    print(json.dumps(result, indent=2, sort_keys=True, default=str))
    return 0 if result.get("ok", False) else 1


def cmd_download(args: argparse.Namespace) -> int:
    paths.ensure_home()
    if args.download_command == "traces":
        fleet = Fleet(args.fleet)
        result = fleet.download_traces(
            role=args.role,
            name=args.name,
            pattern=args.pattern,
            dest=args.dest,
            dry_run=args.dry_run,
        )
        print(json.dumps(result, indent=2, sort_keys=True, default=str))
        return 0 if result.get("ok", False) else 1
    return 2


def _summarize_assets(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "provider": a.get("provider"),
            "provider_id": a.get("provider_id"),
            "name": a.get("name"),
            "role": a.get("role"),
            "fleet": a.get("fleet"),
            "public_ip": a.get("public_ip"),
            "status": a.get("status"),
            "tags": a.get("tags", []),
        }
        for a in items
    ]


if __name__ == "__main__":
    raise SystemExit(main())
