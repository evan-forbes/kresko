"""Provider-neutral instance reconciliation for experiment runs."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any

from harness import assets
from harness.providers import (
    CloudProvider,
    ProviderError,
    experiment_tag,
    role_tag,
    run_tag,
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
    spec: Any,
    *,
    experiment: str,
    run_name: str,
    providers: dict[str, CloudProvider],
    dry_run: bool = False,
    retry_failed: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    desired = _expand_desired(spec, experiment=experiment, run_name=run_name)
    _validate_unique_desired_names(desired)

    plan: dict[str, list[dict[str, Any]]] = {
        "create": [],
        "reuse": [],
        "duplicate": [],
        "failed": [],
    }
    creates: list[DesiredNode] = []
    target_by_name = {target.name: target for target in desired}
    exp_tag = experiment_tag(experiment)
    run_tag_value = run_tag(run_name)

    for provider_name, provider_targets in _group_desired(desired).items():
        provider = providers.get(provider_name)
        if provider is None:
            raise ProviderError(f"provider {provider_name!r} is not configured")

        existing = [
            asset
            for asset in provider.list_for_tag(exp_tag)
            if run_tag_value in set(asset.get("tags") or [])
        ]
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
        ssh_selector = (
            (spec.ssh or {}).get("key_name")
            or (spec.ssh or {}).get("ssh_key")
            or ""
        )
        if not ssh_selector:
            raise ProviderError("spec.ssh.key_name is required to create instances")
        for provider_name in sorted({target.provider for target in creates}):
            ssh_keys[provider_name] = providers[provider_name].lookup_ssh_key(
                str(ssh_selector)
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
        assets.write_asset(asset)
        refreshed_reuse.append(asset)
    plan["reuse"] = refreshed_reuse

    return plan


def _expand_desired(
    spec: Any, *, experiment: str, run_name: str
) -> list[DesiredNode]:
    base_tags = [assets.REQUIRED_TAG, experiment_tag(experiment), run_tag(run_name)]
    base_tags += list(getattr(spec, "tags", []) or [])
    out: list[DesiredNode] = []
    for group in spec.node_groups:
        prefix = group.name_prefix or group.role
        tags = sorted(set([*base_tags, *list(group.tags or []), role_tag(group.role)]))
        for index in range(group.count):
            out.append(
                DesiredNode(
                    provider=getattr(group, "provider", "digitalocean"),
                    name=f"{prefix}-{index}",
                    role=group.role,
                    region=group.region,
                    size=group.size,
                    image=group.image,
                    tags=tags,
                    ssh_user=group.ssh_user or (spec.ssh or {}).get("user", "root"),
                    provider_options=dict(getattr(group, "provider_options", {}) or {}),
                )
            )
    return out


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
        "experiment": tag_value(target.tags, "experiment-"),
        "run": tag_value(target.tags, "run-"),
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
