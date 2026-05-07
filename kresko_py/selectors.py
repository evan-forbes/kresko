"""Asset filtering: roles, names, glob patterns, run name, failed-from results."""

from __future__ import annotations

import fnmatch
import json
from pathlib import Path
from typing import Any, Iterable


def _split(values: str | Iterable[str] | None) -> list[str]:
    if values is None:
        return []
    if isinstance(values, str):
        return [part.strip() for part in values.split(",") if part.strip()]
    return [str(value).strip() for value in values if str(value).strip()]


def is_active(asset: dict[str, Any]) -> bool:
    if asset.get("status") in {"destroyed", "deleted", "failed"}:
        return False
    public_ip = asset.get("public_ip")
    return bool(public_ip) and public_ip != "TBD"


def active_assets(assets: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [asset for asset in assets if is_active(asset)]


def failed_node_names_from_result(result_path: str | Path) -> set[str]:
    path = Path(result_path)
    if not path.exists():
        return set()
    with path.open(encoding="utf-8") as fh:
        result = json.load(fh)
    failures = result.get("failures") or result.get("node_failures") or []
    out: set[str] = set()
    for failure in failures:
        name = failure.get("node") or failure.get("name")
        if name:
            out.add(name)
    return out


def select(
    assets: list[dict[str, Any]],
    *,
    roles: str | Iterable[str] | None = None,
    names: str | Iterable[str] | None = None,
    patterns: str | Iterable[str] | None = None,
    run_name: str | None = None,
    failed_from: str | Path | None = None,
) -> list[dict[str, Any]]:
    selected = active_assets(assets)
    role_set = set(_split(roles))
    name_set = set(_split(names))
    pattern_list = _split(patterns)

    if role_set:
        selected = [a for a in selected if a.get("role") in role_set]
    if name_set:
        selected = [a for a in selected if a.get("name") in name_set]
    if pattern_list:
        selected = [
            a
            for a in selected
            if any(fnmatch.fnmatch(a.get("name", ""), p) for p in pattern_list)
        ]
    if run_name:
        selected = [a for a in selected if a.get("run") == run_name]
    if failed_from:
        failed = failed_node_names_from_result(failed_from)
        selected = [a for a in selected if a.get("name") in failed]
    return selected
