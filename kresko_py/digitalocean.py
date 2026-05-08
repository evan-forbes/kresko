"""DigitalOcean provider: create/destroy droplets, write to assets/.

Tag contract: every droplet carries the `kresko` marker tag plus typed
prefix tags `experiment-<exp>`, `role-<role>`, `run-<name>`. The `kresko`
marker stays in place because DigitalOcean tags are flat strings, and we
need a safety token to refuse deletion of foreign droplets that happen to
share an `experiment-...` or `run-...` tag from some other tool.
"""

from __future__ import annotations

import os
import time
from typing import Any

import requests

from kresko_py import assets

DO_API = "https://api.digitalocean.com/v2"
PROVIDER = "digitalocean"

REQUIRED_TAG = "kresko"
EXPERIMENT_TAG_PREFIX = "experiment-"
ROLE_TAG_PREFIX = "role-"
RUN_TAG_PREFIX = "run-"
KRESKO_TYPED_TAG_PREFIXES = (EXPERIMENT_TAG_PREFIX, ROLE_TAG_PREFIX, RUN_TAG_PREFIX)


class DigitalOceanError(RuntimeError):
    pass


def experiment_tag(experiment: str) -> str:
    return f"{EXPERIMENT_TAG_PREFIX}{experiment}"


def role_tag(role: str) -> str:
    return f"{ROLE_TAG_PREFIX}{role}"


def run_tag(run_name: str) -> str:
    return f"{RUN_TAG_PREFIX}{run_name}"


def tag_value(tags: list[str], prefix: str) -> str:
    for tag in tags:
        if tag.startswith(prefix):
            return tag[len(prefix):]
    return ""


def validate_kresko_droplet(
    droplet: dict[str, Any],
    required_tag: str,
    expected_name: str | None = None,
) -> None:
    tags = set(droplet.get("tags") or [])
    droplet_id = droplet.get("id", "<unknown>")
    name = droplet.get("name", "<unknown>")
    if REQUIRED_TAG not in tags:
        raise DigitalOceanError(
            f"refusing to delete droplet {droplet_id} ({name}): missing required {REQUIRED_TAG!r} tag"
        )
    if required_tag not in tags:
        raise DigitalOceanError(
            f"refusing to delete droplet {droplet_id} ({name}): missing required tag {required_tag!r}"
        )
    if expected_name and name != expected_name:
        raise DigitalOceanError(
            f"refusing to delete droplet {droplet_id}: expected name {expected_name!r}, got {name!r}"
        )


def droplet_ips(droplet: dict[str, Any]) -> tuple[str, str]:
    public_ip = ""
    private_ip = ""
    for network in droplet.get("networks", {}).get("v4", []):
        if network.get("type") == "public":
            public_ip = network.get("ip_address", "")
        elif network.get("type") == "private":
            private_ip = network.get("ip_address", "")
    return public_ip, private_ip


def droplet_to_asset(droplet: dict[str, Any]) -> dict[str, Any]:
    public_ip, private_ip = droplet_ips(droplet)
    tags = sorted({str(t) for t in (droplet.get("tags") or []) if t})
    region = droplet.get("region")
    region_slug = region.get("slug", "") if isinstance(region, dict) else (region or "")
    image = droplet.get("image")
    image_slug = image.get("slug", "") if isinstance(image, dict) else (image or "")
    size = droplet.get("size")
    size_slug = size.get("slug", "") if isinstance(size, dict) else (size or droplet.get("size_slug", ""))

    return {
        "provider": PROVIDER,
        "provider_id": str(droplet.get("id", "")),
        "name": droplet.get("name", ""),
        "role": tag_value(tags, ROLE_TAG_PREFIX),
        "experiment": tag_value(tags, EXPERIMENT_TAG_PREFIX),
        "run": tag_value(tags, RUN_TAG_PREFIX),
        "region": region_slug,
        "size": size_slug,
        "image": image_slug,
        "public_ip": public_ip,
        "private_ip": private_ip,
        "status": droplet.get("status", "unknown"),
        "ssh_user": "root",
        "tags": tags,
    }


def create_droplet_request(
    name: str,
    region: str,
    size: str,
    image: str,
    tags: list[str],
    ssh_keys: list[int | str],
    monitoring: bool = True,
) -> dict[str, Any]:
    return {
        "name": name,
        "region": region,
        "size": size,
        "image": image,
        "ssh_keys": ssh_keys,
        "tags": sorted(set(tags)),
        "monitoring": monitoring,
    }


class DigitalOceanClient:
    def __init__(
        self,
        token: str | None = None,
        session: requests.Session | None = None,
        api_url: str = DO_API,
    ) -> None:
        self.token = token or os.environ.get("DIGITALOCEAN_TOKEN", "")
        if not self.token:
            raise DigitalOceanError("DIGITALOCEAN_TOKEN is not set")
        self.session = session or requests.Session()
        self.api_url = api_url.rstrip("/")

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        headers = kwargs.pop("headers", {})
        headers["Authorization"] = f"Bearer {self.token}"
        headers["Content-Type"] = "application/json"
        response = self.session.request(
            method, f"{self.api_url}{path}", headers=headers, timeout=60, **kwargs
        )
        if response.status_code >= 400:
            raise DigitalOceanError(f"DigitalOcean {method} {path} failed: {response.text}")
        if response.status_code == 204 or not response.content:
            return None
        return response.json()

    def list_ssh_keys(self) -> list[dict[str, Any]]:
        keys: list[dict[str, Any]] = []
        page = 1
        while True:
            body = self._request("GET", f"/account/keys?per_page=200&page={page}")
            batch = body.get("ssh_keys", [])
            keys.extend(batch)
            if len(batch) < 200:
                return keys
            page += 1

    def lookup_ssh_key(self, selector: str) -> int | str:
        for key in self.list_ssh_keys():
            if selector in {
                str(key.get("id", "")),
                key.get("name", ""),
                key.get("fingerprint", ""),
            }:
                return key["id"]
        raise DigitalOceanError(f"SSH key {selector!r} not found in DigitalOcean account")

    def list_droplets_by_tag(self, tag: str) -> list[dict[str, Any]]:
        droplets: list[dict[str, Any]] = []
        page = 1
        while True:
            body = self._request("GET", f"/droplets?tag_name={tag}&per_page=200&page={page}")
            batch = body.get("droplets", [])
            droplets.extend(batch)
            if len(batch) < 200:
                return droplets
            page += 1

    def create_droplet(self, request: dict[str, Any]) -> dict[str, Any]:
        body = self._request("POST", "/droplets", json=request)
        return body["droplet"]

    def get_droplet(self, droplet_id: int | str) -> dict[str, Any]:
        return self._request("GET", f"/droplets/{droplet_id}")["droplet"]

    def delete_droplet(self, droplet_id: int | str) -> None:
        self._request("DELETE", f"/droplets/{droplet_id}")

    def wait_for_ips(
        self, droplet_id: int | str, attempts: int = 60, delay_secs: float = 5.0
    ) -> dict[str, Any]:
        for _ in range(attempts):
            droplet = self.get_droplet(droplet_id)
            public_ip, _ = droplet_ips(droplet)
            if droplet.get("status") == "active" and public_ip:
                return droplet
            time.sleep(delay_secs)
        raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")


def reconcile_droplets(
    spec: Any,
    *,
    experiment: str,
    run_name: str,
    client: DigitalOceanClient,
    dry_run: bool = False,
    retry_failed: bool = False,
) -> dict[str, list[dict[str, Any]]]:
    """Create any missing droplets for the spec and write assets/.

    `spec` must expose `node_groups`, `tags`, and `ssh` (`key_name`/`ssh_key`).
    Existing droplets are looked up by `experiment_tag(experiment)`; any
    desired node whose name is missing gets created. Returns a plan dict
    with keys `create`/`reuse`/`duplicate`/`failed`. Per-droplet provisioning
    errors and wait-for-ip timeouts are captured in `failed` rather than
    raised — only top-level errors (auth, missing config) raise.
    """

    desired = _expand_desired(spec, experiment=experiment, run_name=run_name)
    exp_tag = experiment_tag(experiment)
    existing = [
        droplet
        for droplet in client.list_droplets_by_tag(exp_tag)
        if run_tag(run_name) in set(droplet.get("tags") or [])
    ]
    existing_by_name: dict[str, dict[str, Any]] = {
        d.get("name", ""): d for d in existing
    }

    plan: dict[str, list[dict[str, Any]]] = {
        "create": [],
        "reuse": [],
        "duplicate": [],
        "failed": [],
    }
    seen: set[str] = set()
    for droplet in existing:
        name = droplet.get("name", "")
        if name in seen:
            plan["duplicate"].append(droplet_to_asset(droplet))
        seen.add(name)

    target_by_name: dict[str, dict[str, Any]] = {target["name"]: target for target in desired}

    for target in desired:
        existing_droplet = existing_by_name.get(target["name"])
        if existing_droplet is None:
            plan["create"].append(target)
            continue
        existing_role = tag_value(existing_droplet.get("tags") or [], ROLE_TAG_PREFIX)
        if existing_role and existing_role != target["role"]:
            plan["failed"].append(
                {
                    "name": target["name"],
                    "role": target["role"],
                    "region": target["region"],
                    "size": target["size"],
                    "kind": "role_mismatch",
                    "message": (
                        f"existing droplet has role {existing_role!r}, "
                        f"expected {target['role']!r}"
                    ),
                }
            )
            continue
        plan["reuse"].append(droplet_to_asset(existing_droplet))

    # Validate the SSH key against DigitalOcean *before* the dry-run exit so
    # misconfigured KRESKO_SSH_KEY_NAME fails loudly during planning, not
    # mid-provision after some droplets are already up.
    ssh_key: int | str | None = None
    if plan["create"]:
        ssh_selector = (spec.ssh or {}).get("key_name") or (spec.ssh or {}).get("ssh_key") or ""
        if not ssh_selector:
            raise DigitalOceanError(
                "spec.ssh.key_name is required to create DigitalOcean droplets"
            )
        ssh_key = client.lookup_ssh_key(str(ssh_selector))

    if dry_run:
        return plan

    if plan["duplicate"]:
        names = ", ".join(sorted({a["name"] for a in plan["duplicate"]}))
        raise DigitalOceanError(f"duplicate DigitalOcean droplets found for: {names}")

    for target in plan["create"]:
        request = create_droplet_request(
            name=target["name"],
            region=target["region"],
            size=target["size"],
            image=target["image"],
            tags=target["tags"],
            ssh_keys=[ssh_key],
        )
        try:
            droplet = client.create_droplet(request)
        except Exception as exc:
            plan["failed"].append(
                _failure_record(target, kind="create", message=str(exc))
            )
            continue
        try:
            droplet = client.wait_for_ips(droplet["id"])
        except DigitalOceanError as exc:
            asset = droplet_to_asset(droplet)
            asset["role"] = target["role"]
            asset["experiment"] = experiment
            asset["run"] = run_name
            asset["status"] = "failed"
            asset["failure_reason"] = {
                "kind": "timeout",
                "message": str(exc),
                "region": target["region"],
                "size": target["size"],
            }
            assets.write_asset(asset)
            plan["failed"].append(
                _failure_record(target, kind="timeout", message=str(exc))
            )
            continue
        asset = droplet_to_asset(droplet)
        # Re-attach role/experiment/run from desired in case the cloud
        # reordered tags (we read them back from the same tag list anyway).
        asset["role"] = target["role"]
        asset["experiment"] = experiment
        asset["run"] = run_name
        assets.write_asset(asset)

    refreshed_reuse: list[dict[str, Any]] = []
    for asset in plan["reuse"]:
        prior = _read_local_asset(asset["provider"], asset["provider_id"])
        was_failed = (prior or {}).get("status") == "failed"
        if was_failed and not retry_failed:
            # Preserve local failed marker so selectors keep skipping it,
            # even if list_droplets_by_tag returned a fresher status.
            asset = {**asset, "status": "failed"}
            asset["failure_reason"] = (prior or {}).get("failure_reason", {})
            assets.write_asset(asset)
            refreshed_reuse.append(asset)
            continue
        if was_failed and retry_failed:
            target = target_by_name.get(asset.get("name", ""), {})
            try:
                droplet = client.wait_for_ips(asset["provider_id"])
            except DigitalOceanError as exc:
                asset = {**asset, "status": "failed"}
                asset["failure_reason"] = {
                    "kind": "timeout",
                    "message": str(exc),
                    "region": target.get("region", asset.get("region", "")),
                    "size": target.get("size", asset.get("size", "")),
                }
                assets.write_asset(asset)
                plan["failed"].append(
                    _failure_record(
                        target or asset,
                        kind="timeout",
                        message=str(exc),
                    )
                )
                refreshed_reuse.append(asset)
                continue
            asset = droplet_to_asset(droplet)
            if target:
                asset["role"] = target["role"]
                asset["experiment"] = experiment
                asset["run"] = run_name
            asset.pop("failure_reason", None)
        assets.write_asset(asset)
        refreshed_reuse.append(asset)
    plan["reuse"] = refreshed_reuse

    return plan


def _failure_record(target: dict[str, Any], *, kind: str, message: str) -> dict[str, Any]:
    return {
        "name": target.get("name", ""),
        "role": target.get("role", ""),
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


def _expand_desired(
    spec: Any, *, experiment: str, run_name: str
) -> list[dict[str, Any]]:
    base_tags = [REQUIRED_TAG, experiment_tag(experiment), run_tag(run_name)]
    base_tags += list(getattr(spec, "tags", []) or [])
    out: list[dict[str, Any]] = []
    for group in spec.node_groups:
        prefix = group.name_prefix or group.role
        tags = sorted(set([*base_tags, *list(group.tags or []), role_tag(group.role)]))
        for index in range(group.count):
            out.append(
                {
                    "name": f"{prefix}-{index}",
                    "role": group.role,
                    "region": group.region,
                    "size": group.size,
                    "image": group.image,
                    "tags": tags,
                    "ssh_user": group.ssh_user or (spec.ssh or {}).get("user", "root"),
                }
            )
    return out


def destroy_assets(
    asset_list: list[dict[str, Any]],
    client: DigitalOceanClient,
    *,
    required_tags: list[str],
    dry_run: bool = False,
) -> list[str]:
    """Destroy each asset's droplet after validating tags. Returns provider IDs.

    Every tag in `required_tags` must be present on the droplet (in addition
    to the mandatory `kresko` marker checked by `validate_kresko_droplet`).
    Pass both the experiment tag and the run tag so a stale asset record
    can't trick us into deleting a droplet from a different run.
    """

    if not required_tags:
        raise DigitalOceanError("destroy_assets requires at least one required_tag")

    destroyed: list[str] = []
    for asset in asset_list:
        provider_id = asset.get("provider_id")
        if not provider_id:
            continue
        droplet = client.get_droplet(provider_id)
        validate_kresko_droplet(droplet, required_tags[0], expected_name=asset.get("name"))
        droplet_tags = set(droplet.get("tags") or [])
        for tag in required_tags[1:]:
            if tag not in droplet_tags:
                raise DigitalOceanError(
                    f"refusing to delete droplet {droplet.get('id')} "
                    f"({droplet.get('name')!r}): missing required tag {tag!r}"
                )
        destroyed.append(str(provider_id))
        if not dry_run:
            client.delete_droplet(provider_id)
            assets.delete_asset(PROVIDER, provider_id)
    return destroyed


def destroy_tagged_droplets(
    tag: str, client: DigitalOceanClient, dry_run: bool = False
) -> list[str]:
    if tag == REQUIRED_TAG or not tag.startswith(KRESKO_TYPED_TAG_PREFIXES):
        raise DigitalOceanError(
            f"refusing force-tag deletion for {tag!r}; use a specific tag like "
            f"{EXPERIMENT_TAG_PREFIX}my-experiment, {ROLE_TAG_PREFIX}miner, or "
            f"{RUN_TAG_PREFIX}r-20260507-141502"
        )
    droplets = client.list_droplets_by_tag(tag)
    destroyed: list[str] = []
    for droplet in droplets:
        if not droplet.get("id"):
            continue
        validate_kresko_droplet(droplet, tag)
        destroyed.append(str(droplet["id"]))
    if not dry_run:
        for provider_id in destroyed:
            client.delete_droplet(provider_id)
            assets.delete_asset(PROVIDER, provider_id)
    return destroyed
