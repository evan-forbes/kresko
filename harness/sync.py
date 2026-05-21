"""Refresh `~/.kresko/assets/` from live cloud providers.

For each configured provider, list everything tagged `kresko`, upsert one JSON
per asset to `~/.kresko/assets/`, and prune local assets whose provider IDs
disappeared. Refuses to touch anything missing the `kresko` tag.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from harness import assets
from harness.assets import REQUIRED_TAG
from harness.providers import CloudProvider, ProviderError, get_provider, known_provider_names


@dataclass
class SyncReport:
    provider: str
    upserted: list[str]
    pruned: list[str]
    errors: list[str]


def sync_provider(provider: CloudProvider) -> SyncReport:
    upserted: list[str] = []
    errors: list[str] = []
    seen_ids: set[str] = set()

    for asset in provider.list_for_tag(REQUIRED_TAG):
        if REQUIRED_TAG not in (asset.get("tags") or []):
            errors.append(
                f"{provider.instance_noun} {asset.get('provider_id')} missing required tag, skipped"
            )
            continue
        seen_ids.add(asset["provider_id"])
        try:
            assets.write_asset(asset)
        except ValueError as exc:
            errors.append(f"{provider.instance_noun} {asset.get('provider_id')}: {exc}")
            continue
        upserted.append(asset["provider_id"])

    pruned: list[str] = []
    for existing in assets.list_assets(provider=provider.name):
        provider_id = existing.get("provider_id")
        if provider_id and provider_id not in seen_ids:
            assets.delete_asset(provider.name, provider_id)
            pruned.append(provider_id)

    return SyncReport(
        provider=provider.name,
        upserted=sorted(upserted),
        pruned=sorted(pruned),
        errors=errors,
    )


def sync_all(
    *,
    providers: list[str] | None = None,
    provider_map: dict[str, CloudProvider] | None = None,
) -> list[SyncReport]:
    """Run sync across known providers, returning per-provider reports."""

    enabled = providers or known_provider_names()
    reports: list[SyncReport] = []
    for name in enabled:
        try:
            provider = (provider_map or {}).get(name) or get_provider(name)
        except Exception as exc:
            reports.append(SyncReport(provider=name, upserted=[], pruned=[], errors=[str(exc)]))
            continue
        try:
            reports.append(sync_provider(provider))
        except ProviderError as exc:
            reports.append(SyncReport(provider=name, upserted=[], pruned=[], errors=[str(exc)]))
    return reports


def report_to_dict(report: SyncReport) -> dict[str, Any]:
    return {
        "provider": report.provider,
        "upserted": report.upserted,
        "pruned": report.pruned,
        "errors": report.errors,
    }
