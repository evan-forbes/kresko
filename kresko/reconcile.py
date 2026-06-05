"""Provider-neutral instance reconciliation for a fleet.

Idempotent: a node already live in the cloud and carrying the fleet tag plus
the matching name is *adopted* (reused) instead of recreated; only missing
nodes are created. This is what lets `Fleet.up()` run repeatedly against a
long-running network without churning instances, and what lets a one-shot
re-tag bring pre-existing nodes under fleet management.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any

from kresko import assets
from kresko.providers import (
    CloudProvider,
    ProviderError,
    fleet_tag,
    tag_value,
)


@dataclass(frozen=True)
class DesiredNode:
    provider: str
    name: str
    role: str
    region: str
    size: str
    image: str
    tags: list[str]
    ssh_user: str
    provider_options: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def reconcile_instances(
    desired: list[DesiredNode],
    *,
    fleet: str,
    providers: dict[str, CloudProvider],
    ssh_key_selector: str = "",
    dry_run: bool = False,
    retry_failed: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    _validate_unique_desired_names(desired)

    plan: dict[str, list[dict[str, Any]]] = {
        "create": [],
        "reuse": [],
        "duplicate": [],
        "failed": [],
    }
    creates: list[DesiredNode] = []
    target_by_name = {target.name: target for target in desired}
    fleet_tag_value = fleet_tag(fleet)

    for provider_name, provider_targets in _group_desired(desired).items():
        provider = providers.get(provider_name)
        if provider is None:
            raise ProviderError(f"provider {provider_name!r} is not configured")

        existing = provider.list_for_tag(fleet_tag_value)
        existing_by_name: dict[str, dict[str, Any]] = {}
        seen: set[str] = set()
        for asset in existing:
            name = asset.get("name", "")
            if name in seen:
                plan["duplicate"].append(asset)
            else:
                existing_by_name[name] = asset
            seen.add(name)

        for target in provider_targets:
            existing_asset = existing_by_name.get(target.name)
            if existing_asset is None:
                creates.append(target)
                plan["create"].append(target.to_dict())
                continue
            existing_role = tag_value(existing_asset.get("tags") or [], "role-")
            if existing_role and existing_role != target.role:
                plan["failed"].append(
                    {
                        "name": target.name,
                        "role": target.role,
                        "provider": target.provider,
                        "region": target.region,
                        "size": target.size,
                        "kind": "role_mismatch",
                        "message": (
                            f"existing {provider.instance_noun} has role {existing_role!r}, "
                            f"expected {target.role!r}"
                        ),
                    }
                )
                continue
            plan["reuse"].append(existing_asset)

    ssh_keys: dict[str, str | int] = {}
    if creates:
        if not ssh_key_selector:
            raise ProviderError(
                "an SSH key name is required to create instances "
                "(pass Fleet(ssh={'key_name': ...}))"
            )
        for provider_name in sorted({target.provider for target in creates}):
            ssh_keys[provider_name] = providers[provider_name].lookup_ssh_key(
                str(ssh_key_selector)
            )

    if dry_run:
        return plan

    if plan["duplicate"]:
        names = ", ".join(sorted({a["name"] for a in plan["duplicate"]}))
        raise ProviderError(f"duplicate cloud instances found for: {names}")

    for target in creates:
        provider = providers[target.provider]
        try:
            created = provider.create(target, ssh_keys[target.provider])
        except Exception as exc:
            plan["failed"].append(_failure_record(target, kind="create", message=str(exc)))
            continue
        try:
            ready = provider.wait_ready(created["provider_id"])
        except Exception as exc:
            failed_asset = _target_asset(created, target)
            failed_asset["status"] = "failed"
            failed_asset["failure_reason"] = {
                "kind": "timeout",
                "message": str(exc),
                "region": target.region,
                "size": target.size,
            }
            assets.write_asset(failed_asset)
            plan["failed"].append(_failure_record(target, kind="timeout", message=str(exc)))
            continue
        assets.write_asset(_target_asset(ready, target))

    refreshed_reuse: list[dict[str, Any]] = []
    for asset in plan["reuse"]:
        prior = _read_local_asset(asset["provider"], asset["provider_id"])
        was_failed = (prior or {}).get("status") == "failed"
        if was_failed and not retry_failed:
            asset = {**asset, "status": "failed"}
            asset["failure_reason"] = (prior or {}).get("failure_reason", {})
            assets.write_asset(asset)
            refreshed_reuse.append(asset)
            continue
        if was_failed and retry_failed:
            target = target_by_name.get(asset.get("name", ""))
            provider = providers[asset["provider"]]
            try:
                ready = provider.wait_ready(asset["provider_id"])
            except Exception as exc:
                failed_asset = {**asset, "status": "failed"}
                failed_asset["failure_reason"] = {
                    "kind": "timeout",
                    "message": str(exc),
                    "region": (target.region if target else asset.get("region", "")),
                    "size": (target.size if target else asset.get("size", "")),
                }
                assets.write_asset(failed_asset)
                plan["failed"].append(
                    _failure_record(target or asset, kind="timeout", message=str(exc))
                )
                refreshed_reuse.append(failed_asset)
                continue
            asset = _target_asset(ready, target) if target else ready
            asset.pop("failure_reason", None)
        else:
            # Healthy adopted node: re-stamp the local mirror so its fleet/role
            # fields and tags reflect the desired spec. This is what makes a
            # one-shot re-tag of a pre-existing node converge under `up`.
            target = target_by_name.get(asset.get("name", ""))
            if target is not None:
                asset = _target_asset(asset, target)
        assets.write_asset(asset)
        refreshed_reuse.append(asset)
    plan["reuse"] = refreshed_reuse

    return plan


def _group_desired(desired: list[DesiredNode]) -> dict[str, list[DesiredNode]]:
    grouped: dict[str, list[DesiredNode]] = {}
    for target in desired:
        grouped.setdefault(target.provider, []).append(target)
    return grouped


def _validate_unique_desired_names(desired: list[DesiredNode]) -> None:
    seen: dict[str, str] = {}
    for target in desired:
        prior = seen.get(target.name)
        if prior:
            raise ProviderError(
                f"duplicate desired node name {target.name!r}; split same-role "
                "mixed-provider nodes with distinct name_prefix values"
            )
        seen[target.name] = target.provider


def _target_asset(asset: dict[str, Any], target: DesiredNode) -> dict[str, Any]:
    return {
        **asset,
        "role": target.role,
        "fleet": tag_value(target.tags, "fleet-"),
        "ssh_user": target.ssh_user,
        "tags": target.tags,
    }


def _failure_record(
    target: DesiredNode | dict[str, Any], *, kind: str, message: str
) -> dict[str, Any]:
    if isinstance(target, DesiredNode):
        return {
            "name": target.name,
            "role": target.role,
            "provider": target.provider,
            "region": target.region,
            "size": target.size,
            "kind": kind,
            "message": message,
        }
    return {
        "name": target.get("name", ""),
        "role": target.get("role", ""),
        "provider": target.get("provider", ""),
        "region": target.get("region", ""),
        "size": target.get("size", ""),
        "kind": kind,
        "message": message,
    }


def _read_local_asset(provider: str, provider_id: str) -> dict[str, Any] | None:
    try:
        return assets.read_asset(provider, provider_id)
    except FileNotFoundError:
        return None
