from __future__ import annotations

import base64

import pytest

from kresko.providers import (
    VultrClient,
    VultrError,
    VultrProvider,
    create_vultr_instance_request,
    parse_vultr_image_selector,
    vultr_to_asset,
)
from kresko.reconcile import DesiredNode


class FakeResponse:
    def __init__(self, status_code=200, payload=None):
        self.status_code = status_code
        self._payload = payload or {}
        self.text = "error"
        self.content = b"{}" if payload is not None else b""

    def json(self):
        return self._payload


class FakeSession:
    def __init__(self, handler):
        self.calls = []
        self._handler = handler

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        return self._handler(method, url, kwargs)


def desired_node(**overrides) -> DesiredNode:
    base = {
        "provider": "vultr",
        "name": "vultr-miner-0",
        "role": "miner",
        "region": "ord",
        "size": "vc2-1c-1gb",
        "image": "os:1743",
        "tags": ["kresko", "fleet-smoke", "role-miner"],
        "ssh_user": "root",
        "provider_options": {},
    }
    base.update(overrides)
    return DesiredNode(**base)


def test_parse_vultr_image_selector_requires_explicit_prefix():
    assert parse_vultr_image_selector("os:1743") == {"os_id": 1743}
    assert parse_vultr_image_selector("image:abc") == {"image_id": "abc"}
    assert parse_vultr_image_selector("snapshot:snap") == {"snapshot_id": "snap"}
    assert parse_vultr_image_selector("app:42") == {"app_id": 42}
    assert parse_vultr_image_selector("iso:iso") == {"iso_id": "iso"}
    with pytest.raises(VultrError):
        parse_vultr_image_selector("ubuntu-24-04-x64")


def test_create_vultr_instance_request_shape_and_user_data_encoding():
    request = create_vultr_instance_request(
        desired_node(
            provider_options={
                "vpc_ids": ["vpc-1"],
                "enable_ipv6": True,
                "user_data": "#cloud-config\n",
            }
        ),
        "ssh-key-id",
    )

    assert request["label"] == "vultr-miner-0"
    assert request["region"] == "ord"
    assert request["plan"] == "vc2-1c-1gb"
    assert request["os_id"] == 1743
    assert request["sshkey_id"] == ["ssh-key-id"]
    assert request["vpc2_ids"] == ["vpc-1"]
    assert request["enable_ipv6"] is True
    assert base64.b64decode(request["user_data"]).decode("utf-8") == "#cloud-config\n"


def test_vultr_to_asset_maps_tags_and_ips():
    asset = vultr_to_asset(
        {
            "id": "abc",
            "label": "vultr-miner-0",
            "region": "ord",
            "plan": "vc2-1c-1gb",
            "os_id": 1743,
            "main_ip": "203.0.113.10",
            "internal_ip": "",
            "status": "active",
            "tags": ["kresko", "fleet-smoke", "role-miner"],
        }
    )

    assert asset["provider"] == "vultr"
    assert asset["provider_id"] == "abc"
    assert asset["name"] == "vultr-miner-0"
    assert asset["role"] == "miner"
    assert asset["fleet"] == "smoke"
    assert asset["public_ip"] == "203.0.113.10"
    assert asset["private_ip"] == ""


def test_vultr_client_uses_cursor_pagination_and_client_side_tag_filtering():
    def handler(method, url, kwargs):
        if url.endswith("/instances") and kwargs["params"] == {"per_page": 100}:
            return FakeResponse(
                payload={
                    "instances": [
                        {"id": "1", "label": "a", "tags": ["kresko"]},
                        {"id": "2", "label": "b", "tags": ["other"]},
                    ],
                    "meta": {"links": {"next": "next-page"}},
                }
            )
        if url.endswith("/instances") and kwargs["params"] == {
            "per_page": 100,
            "cursor": "next-page",
        }:
            return FakeResponse(
                payload={
                    "instances": [{"id": "3", "label": "c", "tags": ["kresko"]}],
                    "meta": {"links": {"next": ""}},
                }
            )
        raise AssertionError(url)

    session = FakeSession(handler)
    client = VultrClient(token="token", session=session, post_ready_delay_secs=0)

    instances = client.list_instances_by_tag("kresko")

    assert [i["id"] for i in instances] == ["1", "3"]
    assert session.calls[0][2]["headers"]["Authorization"] == "Bearer token"


def test_vultr_client_wait_ready_requires_running_power_status():
    calls = {"get": 0}

    def handler(method, url, kwargs):
        assert method == "GET"
        calls["get"] += 1
        power = "stopped" if calls["get"] == 1 else "running"
        return FakeResponse(
            payload={
                "instance": {
                    "id": "abc",
                    "label": "vultr-miner-0",
                    "main_ip": "203.0.113.10",
                    "status": "active",
                    "power_status": power,
                    "tags": ["kresko"],
                }
            }
        )

    client = VultrClient(
        token="token",
        session=FakeSession(handler),
        post_ready_delay_secs=0,
    )

    instance = client.wait_ready("abc", attempts=2, delay_secs=0)

    assert instance["power_status"] == "running"
    assert calls["get"] == 2


def test_vultr_provider_requires_running_power_status_for_ready():
    class FakeClient:
        def __init__(self):
            self.created = []

        def list_instances_by_tag(self, tag):
            return []

        def lookup_ssh_key(self, selector):
            return "ssh-key-id"

        def create_instance(self, request):
            self.created.append(request)
            return {
                "id": "abc",
                "label": request["label"],
                "region": request["region"],
                "plan": request["plan"],
                "os_id": request["os_id"],
                "status": "pending",
                "tags": request["tags"],
            }

        def wait_ready(self, provider_id, attempts=60, delay_secs=5.0):
            return {
                "id": provider_id,
                "label": "vultr-miner-0",
                "region": "ord",
                "plan": "vc2-1c-1gb",
                "os_id": 1743,
                "main_ip": "203.0.113.10",
                "internal_ip": "",
                "status": "active",
                "power_status": "running",
                "tags": ["kresko", "fleet-smoke", "role-miner"],
            }

    provider = VultrProvider(FakeClient())
    created = provider.create(desired_node(), "ssh-key-id")
    ready = provider.wait_ready(created["provider_id"])

    assert created["provider_id"] == "abc"
    assert ready["status"] == "active"
    assert ready["public_ip"] == "203.0.113.10"
