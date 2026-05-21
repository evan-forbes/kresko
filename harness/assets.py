"""Asset store: one JSON file per live cloud asset.

Filename is `<provider>-<provider-id>.json`, so renames in the cloud do not
break local lookups. The `kresko` tag is mandatory; `sync` and destroy paths
both refuse to act on anything missing it.
"""

from __future__ import annotations

import json
import os
import tempfile
from copy import deepcopy
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

from harness.paths import asset_path, assets_dir

REQUIRED_TAG = "kresko"
ASSET_SCHEMA_VERSION = 1


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def normalize_asset(asset: dict[str, Any]) -> dict[str, Any]:
    if not asset.get("provider"):
        raise ValueError("asset is missing 'provider'")
    if not asset.get("provider_id"):
        raise ValueError("asset is missing 'provider_id'")
    tags = sorted({str(tag) for tag in asset.get("tags", []) if tag})
    if REQUIRED_TAG not in tags:
        raise ValueError(
            f"asset {asset.get('provider')}-{asset.get('provider_id')} is missing required {REQUIRED_TAG!r} tag"
        )
    out = deepcopy(asset)
    out["schema_version"] = ASSET_SCHEMA_VERSION
    out["tags"] = tags
    out.setdefault("public_ip", "")
    out.setdefault("private_ip", "")
    out.setdefault("status", "unknown")
    out.setdefault("name", "")
    out.setdefault("role", "")
    out.setdefault("region", "")
    out.setdefault("size", "")
    out.setdefault("image", "")
    out.setdefault("ssh_user", "root")
    return out


def write_asset(asset: dict[str, Any]) -> Path:
    asset = normalize_asset(asset)
    path = asset_path(asset["provider"], asset["provider_id"])
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = deepcopy(asset)
    payload["updated_at"] = utc_now()
    payload.setdefault("created_at", payload["updated_at"])
    if path.exists():
        try:
            existing = read_json(path)
            if existing.get("created_at"):
                payload["created_at"] = existing["created_at"]
        except (OSError, json.JSONDecodeError):
            pass
    fd, tmp_name = tempfile.mkstemp(prefix=".asset.", suffix=".json", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_name, path)
    except Exception:
        try:
            os.unlink(tmp_name)
        except FileNotFoundError:
            pass
        raise
    return path


def read_json(path: str | Path) -> dict[str, Any]:
    with Path(path).open(encoding="utf-8") as fh:
        return json.load(fh)


def read_asset(provider: str, provider_id: str) -> dict[str, Any]:
    return read_json(asset_path(provider, provider_id))


def delete_asset(provider: str, provider_id: str) -> bool:
    path = asset_path(provider, provider_id)
    try:
        path.unlink()
        return True
    except FileNotFoundError:
        return False


def list_assets(
    *,
    tags: Iterable[str] | None = None,
    provider: str | None = None,
) -> list[dict[str, Any]]:
    """Return every asset matching all of the given tags (AND, not OR)."""

    root = assets_dir()
    if not root.exists():
        return []
    needed = {tag for tag in (tags or []) if tag}
    out: list[dict[str, Any]] = []
    for path in sorted(root.glob("*.json")):
        try:
            asset = read_json(path)
        except json.JSONDecodeError:
            continue
        if provider and asset.get("provider") != provider:
            continue
        asset_tags = set(asset.get("tags") or [])
        if needed and not needed.issubset(asset_tags):
            continue
        out.append(asset)
    return out
