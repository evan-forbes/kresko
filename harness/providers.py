"""Cloud provider adapters used by the Python experiment harness."""

from __future__ import annotations

import base64
import os
import time
from dataclasses import dataclass, field
from typing import Any, Protocol

import requests

from harness import assets

DO_API = "https://api.digitalocean.com/v2"
VULTR_API = "https://api.vultr.com/v2"

DIGITALOCEAN = "digitalocean"
VULTR = "vultr"
KNOWN_PROVIDERS = (DIGITALOCEAN, VULTR)

REQUIRED_TAG = assets.REQUIRED_TAG
EXPERIMENT_TAG_PREFIX = "experiment-"
ROLE_TAG_PREFIX = "role-"
RUN_TAG_PREFIX = "run-"
KRESKO_TYPED_TAG_PREFIXES = (EXPERIMENT_TAG_PREFIX, ROLE_TAG_PREFIX, RUN_TAG_PREFIX)


class ProviderError(RuntimeError):
    pass


class DigitalOceanError(ProviderError):
    pass


class VultrError(ProviderError):
    pass


class CloudProvider(Protocol):
    name: str
    instance_noun: str

    def list_for_tag(self, tag: str) -> list[dict[str, Any]]:
        ...

    def lookup_ssh_key(self, selector: str) -> str | int:
        ...

    def create(self, node: Any, ssh_key: str | int) -> dict[str, Any]:
        ...

    def wait_ready(
        self, provider_id: str, *, attempts: int = 60, delay_secs: float = 5.0
    ) -> dict[str, Any]:
        ...

    def delete(
        self,
        asset: dict[str, Any],
        *,
        required_tags: list[str],
        dry_run: bool = False,
    ) -> str | None:
        ...

    def delete_tagged(self, tag: str, *, dry_run: bool = False) -> list[str]:
        ...


@dataclass(frozen=True)
class DigitalOcean:
    region: str
    size: str
    image: str = "ubuntu-24-04-x64"
    ssh_user: str = "root"
    tags: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class Vultr:
    region: str
    size: str
    image: str
    ssh_user: str = "root"
    tags: list[str] = field(default_factory=list)
    vpc_ids: list[str] = field(default_factory=list)
    enable_ipv6: bool = False
    user_data: str | None = None


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


def require_force_tag(tag: str) -> None:
    if tag == REQUIRED_TAG or not tag.startswith(KRESKO_TYPED_TAG_PREFIXES):
        raise ProviderError(
            f"refusing force-tag deletion for {tag!r}; use a specific tag like "
            f"{EXPERIMENT_TAG_PREFIX}my-experiment, {ROLE_TAG_PREFIX}miner, or "
            f"{RUN_TAG_PREFIX}r-20260507-141502"
        )


def known_provider_names() -> list[str]:
    return list(KNOWN_PROVIDERS)


def get_provider(name: str) -> CloudProvider:
    if name == DIGITALOCEAN:
        return DigitalOceanProvider()
    if name == VULTR:
        return VultrProvider()
    raise ProviderError(f"unsupported provider {name!r}")


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
    size_slug = (
        size.get("slug", "")
        if isinstance(size, dict)
        else (size or droplet.get("size_slug", ""))
    )

    return {
        "provider": DIGITALOCEAN,
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


def validate_kresko_instance(
    provider: str,
    instance_noun: str,
    instance: dict[str, Any],
    required_tags: list[str],
    expected_name: str | None = None,
) -> None:
    tags = set(instance.get("tags") or [])
    provider_id = instance.get("id") or instance.get("provider_id") or "<unknown>"
    name = instance.get("name") or instance.get("label") or "<unknown>"
    for tag in [REQUIRED_TAG, *required_tags]:
        if tag not in tags:
            raise ProviderError(
                f"refusing to delete {provider} {instance_noun} {provider_id} "
                f"({name}): missing required tag {tag!r}"
            )
    if expected_name and name != expected_name:
        raise ProviderError(
            f"refusing to delete {provider} {instance_noun} {provider_id}: "
            f"expected name {expected_name!r}, got {name!r}"
        )


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


class DigitalOceanProvider:
    name = DIGITALOCEAN
    instance_noun = "droplet"

    def __init__(self, client: DigitalOceanClient | Any | None = None) -> None:
        self.client = client or DigitalOceanClient()

    def list_for_tag(self, tag: str) -> list[dict[str, Any]]:
        return [droplet_to_asset(droplet) for droplet in self.client.list_droplets_by_tag(tag)]

    def lookup_ssh_key(self, selector: str) -> str | int:
        return self.client.lookup_ssh_key(selector)

    def create(self, node: Any, ssh_key: str | int) -> dict[str, Any]:
        request = create_droplet_request(
            name=node.name,
            region=node.region,
            size=node.size,
            image=node.image,
            tags=node.tags,
            ssh_keys=[ssh_key],
        )
        return droplet_to_asset(self.client.create_droplet(request))

    def wait_ready(
        self, provider_id: str, *, attempts: int = 60, delay_secs: float = 5.0
    ) -> dict[str, Any]:
        return droplet_to_asset(
            self.client.wait_for_ips(provider_id, attempts=attempts, delay_secs=delay_secs)
        )

    def delete(
        self,
        asset: dict[str, Any],
        *,
        required_tags: list[str],
        dry_run: bool = False,
    ) -> str | None:
        provider_id = asset.get("provider_id")
        if not provider_id:
            return None
        droplet = self.client.get_droplet(provider_id)
        validate_kresko_instance(
            self.name,
            self.instance_noun,
            droplet,
            required_tags,
            expected_name=asset.get("name"),
        )
        if not dry_run:
            self.client.delete_droplet(provider_id)
            assets.delete_asset(self.name, provider_id)
        return str(provider_id)

    def delete_tagged(self, tag: str, *, dry_run: bool = False) -> list[str]:
        require_force_tag(tag)
        droplets = self.client.list_droplets_by_tag(tag)
        destroyed: list[str] = []
        for droplet in droplets:
            if not droplet.get("id"):
                continue
            validate_kresko_instance(self.name, self.instance_noun, droplet, [tag])
            destroyed.append(str(droplet["id"]))
        if not dry_run:
            for provider_id in destroyed:
                self.client.delete_droplet(provider_id)
                assets.delete_asset(self.name, provider_id)
        return destroyed


def parse_vultr_image_selector(selector: str) -> dict[str, Any]:
    if ":" not in selector:
        raise VultrError(
            "Vultr image must use an explicit selector: "
            "os:<id>, image:<uuid>, snapshot:<id>, app:<id>, or iso:<id>"
        )
    kind, value = selector.split(":", 1)
    if not value:
        raise VultrError(f"empty Vultr image selector {selector!r}")
    if kind == "os":
        return {"os_id": int(value)}
    if kind == "app":
        return {"app_id": int(value)}
    if kind == "image":
        return {"image_id": value}
    if kind == "snapshot":
        return {"snapshot_id": value}
    if kind == "iso":
        return {"iso_id": value}
    raise VultrError(
        f"unsupported Vultr image selector {selector!r}; use os:, image:, snapshot:, app:, or iso:"
    )


def create_vultr_instance_request(node: Any, ssh_key: str | int) -> dict[str, Any]:
    options = dict(getattr(node, "provider_options", {}) or {})
    request: dict[str, Any] = {
        "label": node.name,
        "region": node.region,
        "plan": node.size,
        "tags": sorted(set(node.tags)),
        "sshkey_id": [str(ssh_key)],
        **parse_vultr_image_selector(node.image),
    }
    vpc_ids = list(options.get("vpc_ids") or [])
    if vpc_ids:
        request["vpc2_ids"] = vpc_ids
    if options.get("enable_ipv6"):
        request["enable_ipv6"] = True
    user_data = options.get("user_data")
    if user_data:
        request["user_data"] = base64.b64encode(
            str(user_data).encode("utf-8")
        ).decode("ascii")
    return request


def vultr_to_asset(instance: dict[str, Any]) -> dict[str, Any]:
    tags = sorted({str(t) for t in (instance.get("tags") or []) if t})
    image = (
        instance.get("image_id")
        or instance.get("os_id")
        or instance.get("snapshot_id")
        or instance.get("app_id")
        or instance.get("iso_id")
        or ""
    )
    return {
        "provider": VULTR,
        "provider_id": str(instance.get("id", "")),
        "name": instance.get("label") or instance.get("name", ""),
        "role": tag_value(tags, ROLE_TAG_PREFIX),
        "experiment": tag_value(tags, EXPERIMENT_TAG_PREFIX),
        "run": tag_value(tags, RUN_TAG_PREFIX),
        "region": instance.get("region", ""),
        "size": instance.get("plan", ""),
        "image": str(image),
        "public_ip": instance.get("main_ip", "") or "",
        "private_ip": instance.get("internal_ip", "") or "",
        "status": instance.get("status", "unknown"),
        "ssh_user": "root",
        "tags": tags,
    }


class VultrClient:
    def __init__(
        self,
        token: str | None = None,
        session: requests.Session | None = None,
        api_url: str = VULTR_API,
        post_ready_delay_secs: float = 10.0,
    ) -> None:
        self.token = token or os.environ.get("VULTR_API_KEY", "")
        if not self.token:
            raise VultrError("VULTR_API_KEY is not set")
        self.session = session or requests.Session()
        self.api_url = api_url.rstrip("/")
        self.post_ready_delay_secs = post_ready_delay_secs

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        headers = kwargs.pop("headers", {})
        headers["Authorization"] = f"Bearer {self.token}"
        headers["Content-Type"] = "application/json"
        response = self.session.request(
            method, f"{self.api_url}{path}", headers=headers, timeout=60, **kwargs
        )
        if response.status_code >= 400:
            raise VultrError(f"Vultr {method} {path} failed: {response.text}")
        if response.status_code == 204 or not response.content:
            return None
        return response.json()

    def _cursor_pages(self, path: str, key: str) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        cursor = ""
        while True:
            params: dict[str, Any] = {"per_page": 100}
            if cursor:
                params["cursor"] = cursor
            body = self._request("GET", path, params=params)
            batch = body.get(key, [])
            out.extend(batch)
            cursor = (
                (body.get("meta") or {})
                .get("links", {})
                .get("next", "")
            )
            if not cursor:
                return out

    def list_ssh_keys(self) -> list[dict[str, Any]]:
        return self._cursor_pages("/ssh-keys", "ssh_keys")

    def lookup_ssh_key(self, selector: str) -> str:
        for key in self.list_ssh_keys():
            if selector in {
                str(key.get("id", "")),
                key.get("name", ""),
                key.get("fingerprint", ""),
            }:
                return str(key["id"])
        raise VultrError(f"SSH key {selector!r} not found in Vultr account")

    def list_instances_by_tag(self, tag: str) -> list[dict[str, Any]]:
        return [
            instance
            for instance in self._cursor_pages("/instances", "instances")
            if tag in set(instance.get("tags") or [])
        ]

    def create_instance(self, request: dict[str, Any]) -> dict[str, Any]:
        body = self._request("POST", "/instances", json=request)
        return body["instance"]

    def get_instance(self, instance_id: str) -> dict[str, Any]:
        return self._request("GET", f"/instances/{instance_id}")["instance"]

    def delete_instance(self, instance_id: str) -> None:
        self._request("DELETE", f"/instances/{instance_id}")

    def wait_ready(
        self, instance_id: str, attempts: int = 60, delay_secs: float = 5.0
    ) -> dict[str, Any]:
        for _ in range(attempts):
            instance = self.get_instance(instance_id)
            power_status = instance.get("power_status")
            power_ok = power_status in (None, "", "running")
            if instance.get("status") == "active" and power_ok and instance.get("main_ip"):
                if self.post_ready_delay_secs > 0:
                    time.sleep(self.post_ready_delay_secs)
                return instance
            time.sleep(delay_secs)
        raise VultrError(f"timed out waiting for Vultr instance {instance_id} IP")


class VultrProvider:
    name = VULTR
    instance_noun = "instance"

    def __init__(self, client: VultrClient | Any | None = None) -> None:
        self.client = client or VultrClient()

    def list_for_tag(self, tag: str) -> list[dict[str, Any]]:
        return [vultr_to_asset(instance) for instance in self.client.list_instances_by_tag(tag)]

    def lookup_ssh_key(self, selector: str) -> str:
        return str(self.client.lookup_ssh_key(selector))

    def create(self, node: Any, ssh_key: str | int) -> dict[str, Any]:
        return vultr_to_asset(
            self.client.create_instance(create_vultr_instance_request(node, ssh_key))
        )

    def wait_ready(
        self, provider_id: str, *, attempts: int = 60, delay_secs: float = 5.0
    ) -> dict[str, Any]:
        return vultr_to_asset(
            self.client.wait_ready(provider_id, attempts=attempts, delay_secs=delay_secs)
        )

    def delete(
        self,
        asset: dict[str, Any],
        *,
        required_tags: list[str],
        dry_run: bool = False,
    ) -> str | None:
        provider_id = asset.get("provider_id")
        if not provider_id:
            return None
        instance = self.client.get_instance(provider_id)
        validate_kresko_instance(
            self.name,
            self.instance_noun,
            instance,
            required_tags,
            expected_name=asset.get("name"),
        )
        if not dry_run:
            self.client.delete_instance(provider_id)
            assets.delete_asset(self.name, provider_id)
        return str(provider_id)

    def delete_tagged(self, tag: str, *, dry_run: bool = False) -> list[str]:
        require_force_tag(tag)
        instances = self.client.list_instances_by_tag(tag)
        destroyed: list[str] = []
        for instance in instances:
            if not instance.get("id"):
                continue
            validate_kresko_instance(self.name, self.instance_noun, instance, [tag])
            destroyed.append(str(instance["id"]))
        if not dry_run:
            for provider_id in destroyed:
                self.client.delete_instance(provider_id)
                assets.delete_asset(self.name, provider_id)
        return destroyed
