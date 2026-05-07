"""Refresh `~/.kresko/assets/` from live cloud providers.

For each configured provider, list everything tagged `kresko`, upsert one JSON
per asset to `~/.kresko/assets/`, and prune local assets whose provider IDs
disappeared. Refuses to touch anything missing the `kresko` tag.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from kresko_py import assets, digitalocean
from kresko_py.assets import REQUIRED_TAG


@dataclass
class SyncReport:
    provider: str
    upserted: list[str]
    pruned: list[str]
    errors: list[str]


def sync_digitalocean(client: digitalocean.DigitalOceanClient | None = None) -> SyncReport:
    client = client or digitalocean.DigitalOceanClient()
    droplets = client.list_droplets_by_tag(REQUIRED_TAG)

    upserted: list[str] = []
    errors: list[str] = []
    seen_ids: set[str] = set()

    for droplet in droplets:
        if REQUIRED_TAG not in (droplet.get("tags") or []):
            errors.append(f"droplet {droplet.get('id')} missing required tag, skipped")
            continue
        asset = digitalocean.droplet_to_asset(droplet)
        seen_ids.add(asset["provider_id"])
        try:
            assets.write_asset(asset)
        except ValueError as exc:
            errors.append(f"droplet {droplet.get('id')}: {exc}")
            continue
        upserted.append(asset["provider_id"])

    pruned: list[str] = []
    for existing in assets.list_assets(provider="digitalocean"):
        provider_id = existing.get("provider_id")
        if provider_id and provider_id not in seen_ids:
            assets.delete_asset("digitalocean", provider_id)
            pruned.append(provider_id)

    return SyncReport(
        provider="digitalocean",
        upserted=sorted(upserted),
        pruned=sorted(pruned),
        errors=errors,
    )


def sync_all(*, providers: list[str] | None = None) -> list[SyncReport]:
    """Run sync across every configured provider, returning per-provider reports."""

    enabled = providers or ["digitalocean"]
    reports: list[SyncReport] = []
    for name in enabled:
        if name == "digitalocean":
            reports.append(sync_digitalocean())
        else:
            reports.append(SyncReport(provider=name, upserted=[], pruned=[], errors=["unsupported provider"]))
    return reports


def report_to_dict(report: SyncReport) -> dict[str, Any]:
    return {
        "provider": report.provider,
        "upserted": report.upserted,
        "pruned": report.pruned,
        "errors": report.errors,
    }
