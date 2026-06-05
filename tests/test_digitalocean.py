from __future__ import annotations

import pytest

from kresko import assets, paths
from kresko.providers import (
    REQUIRED_TAG,
    DigitalOceanClient,
    DigitalOceanError,
    DigitalOceanProvider,
    ProviderError,
    create_droplet_request,
    droplet_to_asset,
    fleet_tag,
    role_tag,
    tag_value,
)
from kresko.reconcile import DesiredNode, reconcile_instances


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def miners(
    count: int,
    *,
    fleet: str = "smoke",
    role: str = "miner",
    region: str = "nyc3",
    size: str = "s-1vcpu-1gb",
    image: str = "ubuntu-24-04-x64",
) -> list[DesiredNode]:
    tags = sorted({REQUIRED_TAG, fleet_tag(fleet), role_tag(role)})
    return [
        DesiredNode(
            provider="digitalocean",
            name=f"{role}-{i}",
            role=role,
            region=region,
            size=size,
            image=image,
            tags=tags,
            ssh_user="root",
            provider_options={},
        )
        for i in range(count)
    ]


class FakeResponse:
    def __init__(self, status_code=200, payload=None):
        self.status_code = status_code
        self._payload = payload or {}
        self.text = "error"
        self.content = b"{}" if payload is not None else b""

    def json(self):
        return self._payload


class FakeSession:
    def __init__(self, handler=None):
        self.calls = []
        self._handler = handler or (lambda method, url, kwargs: FakeResponse(payload={}))

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        return self._handler(method, url, kwargs)


def test_create_droplet_request_shape():
    request = create_droplet_request(
        name="miner-0",
        region="nyc3",
        size="s-1vcpu-1gb",
        image="ubuntu-24-04-x64",
        tags=["kresko", "fleet-smoke", "role-miner"],
        ssh_keys=[7],
    )

    assert request["name"] == "miner-0"
    assert request["ssh_keys"] == [7]
    assert request["tags"] == sorted(request["tags"])
    assert request["monitoring"] is True


def test_droplet_to_asset_extracts_role_and_fleet_from_tags():
    droplet = {
        "id": 99,
        "name": "miner-0",
        "status": "active",
        "region": {"slug": "nyc3"},
        "size": {"slug": "s-1vcpu-1gb"},
        "image": {"slug": "ubuntu-24-04-x64"},
        "networks": {
            "v4": [
                {"type": "public", "ip_address": "203.0.113.1"},
                {"type": "private", "ip_address": "10.0.0.1"},
            ]
        },
        "tags": ["kresko", "fleet-nu7-testnet", "role-miner"],
    }

    asset = droplet_to_asset(droplet)

    assert asset["provider"] == "digitalocean"
    assert asset["provider_id"] == "99"
    assert asset["role"] == "miner"
    assert asset["fleet"] == "nu7-testnet"
    assert asset["public_ip"] == "203.0.113.1"
    assert asset["private_ip"] == "10.0.0.1"
    assert asset["region"] == "nyc3"


def test_tag_value_returns_first_match():
    tags = ["kresko", "fleet-smoke", "role-miner", "extra"]
    assert tag_value(tags, "fleet-") == "smoke"
    assert tag_value(tags, "role-") == "miner"
    assert tag_value(tags, "missing-") == ""


class FakeClient:
    def __init__(self):
        self.created: list[dict] = []
        self.deleted: list[str] = []
        self.droplets_by_tag: dict[str, list[dict]] = {}
        self.droplets_by_id: dict[str, dict] = {}

    def list_droplets_by_tag(self, tag):
        return list(self.droplets_by_tag.get(tag, []))

    def lookup_ssh_key(self, selector):
        return 7

    def create_droplet(self, request):
        self.created.append(request)
        droplet_id = len(self.created)
        droplet = {
            "id": droplet_id,
            "name": request["name"],
            "status": "new",
            "region": {"slug": request["region"]},
            "size": {"slug": request["size"]},
            "image": {"slug": request["image"]},
            "tags": request["tags"],
            "networks": {"v4": []},
        }
        self.droplets_by_id[str(droplet_id)] = droplet
        return droplet

    def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
        droplet = dict(self.droplets_by_id[str(droplet_id)])
        droplet["status"] = "active"
        droplet["networks"] = {
            "v4": [{"type": "public", "ip_address": f"203.0.113.{droplet_id}"}]
        }
        return droplet

    def get_droplet(self, droplet_id):
        return self.droplets_by_id[str(droplet_id)]

    def delete_droplet(self, droplet_id):
        self.deleted.append(str(droplet_id))


def _existing_droplet(provider_id, *, role="miner", fleet="smoke", ip="203.0.113.42"):
    return {
        "id": provider_id,
        "name": "miner-0",
        "status": "active",
        "region": {"slug": "nyc3"},
        "size": {"slug": "s-1vcpu-1gb"},
        "image": {"slug": "ubuntu-24-04-x64"},
        "tags": ["kresko", fleet_tag(fleet), role_tag(role)],
        "networks": {"v4": [{"type": "public", "ip_address": ip}]},
    }


def test_reconcile_droplets_dry_run_returns_plan(home):
    client = FakeClient()

    plan = reconcile_instances(
        miners(2),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
        dry_run=True,
    )

    assert [a["name"] for a in plan["create"]] == ["miner-0", "miner-1"]
    assert plan["create"][0]["tags"] == sorted({"kresko", "fleet-smoke", "role-miner"})
    assert client.created == []


def test_reconcile_droplets_creates_and_writes_assets(home):
    client = FakeClient()

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert [a["name"] for a in plan["create"]] == ["miner-0"]
    assert len(client.created) == 1
    asset = assets.read_asset("digitalocean", "1")
    assert asset["name"] == "miner-0"
    assert asset["fleet"] == "smoke"
    assert "fleet-smoke" in asset["tags"]
    assert "role-miner" in asset["tags"]


def test_reconcile_droplets_adopts_existing_by_name(home):
    client = FakeClient()
    client.droplets_by_tag["fleet-smoke"] = [_existing_droplet(42)]

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert plan["create"] == []
    assert [a["name"] for a in plan["reuse"]] == ["miner-0"]
    assert client.created == []
    adopted = assets.read_asset("digitalocean", "42")
    assert adopted["public_ip"] == "203.0.113.42"
    assert adopted["fleet"] == "smoke"


def test_reconcile_droplets_no_ssh_key_needed_when_all_adopted(home):
    """Idempotent re-run against a fully live fleet needs no SSH key (nothing
    is created), so adopting a running net never trips the key requirement."""
    client = FakeClient()
    client.droplets_by_tag["fleet-smoke"] = [_existing_droplet(42)]

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="",
    )

    assert [a["name"] for a in plan["reuse"]] == ["miner-0"]


def test_reconcile_droplets_dry_run_validates_ssh_key(home):
    """Dry-run/plan must surface a missing SSH key, not silently succeed and
    blow up only when the user runs `up`."""

    class MissingKeyClient(FakeClient):
        def lookup_ssh_key(self, selector):
            raise DigitalOceanError(f"SSH key {selector!r} not found")

    with pytest.raises(DigitalOceanError):
        reconcile_instances(
            miners(1),
            fleet="smoke",
            providers={"digitalocean": DigitalOceanProvider(MissingKeyClient())},
            ssh_key_selector="missing-key",
            dry_run=True,
        )


def test_reconcile_droplets_refuses_role_mismatch_on_reuse(home):
    client = FakeClient()
    # Existing droplet matches the desired name but has a different role
    # (e.g. config drift). Reuse must refuse.
    client.droplets_by_tag["fleet-smoke"] = [_existing_droplet(42, role="rpc")]

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert plan["reuse"] == []
    assert [f["kind"] for f in plan["failed"]] == ["role_mismatch"]


def test_destroy_assets_validates_required_tag(home):
    asset = {
        "provider": "digitalocean",
        "provider_id": "42",
        "name": "miner-0",
        "tags": ["kresko", "fleet-smoke", "role-miner"],
    }
    assets.write_asset(asset)

    client = FakeClient()
    client.droplets_by_id["42"] = {
        "id": 42,
        "name": "miner-0",
        "tags": ["kresko", "fleet-smoke", "role-miner"],
    }

    destroyed = DigitalOceanProvider(client).delete(asset, required_tags=["fleet-smoke"])

    assert destroyed == "42"
    assert client.deleted == ["42"]


def test_destroy_assets_refuses_when_kresko_tag_missing(home):
    asset = {
        "provider": "digitalocean",
        "provider_id": "42",
        "name": "miner-0",
        "tags": ["kresko"],
    }
    client = FakeClient()
    client.droplets_by_id["42"] = {"id": 42, "name": "miner-0", "tags": ["kresko"]}

    with pytest.raises(ProviderError):
        DigitalOceanProvider(client).delete(asset, required_tags=["fleet-smoke"])
    assert client.deleted == []


def test_destroy_assets_refuses_on_fleet_mismatch(home):
    """A stale asset record carrying the wrong fleet tag must not be able to
    kill another fleet's droplets — `down` requires the matching fleet tag."""
    asset = {
        "provider": "digitalocean",
        "provider_id": "42",
        "name": "miner-0",
        "tags": ["kresko", "fleet-other", "role-miner"],
    }
    client = FakeClient()
    client.droplets_by_id["42"] = {
        "id": 42,
        "name": "miner-0",
        "tags": ["kresko", "fleet-other", "role-miner"],
    }

    with pytest.raises(ProviderError):
        DigitalOceanProvider(client).delete(asset, required_tags=["fleet-mine"])
    assert client.deleted == []


def test_destroy_tagged_droplets_refuses_kresko_tag():
    class _Client:
        def list_droplets_by_tag(self, tag):
            return []

        def delete_droplet(self, droplet_id):
            pass

    with pytest.raises(ProviderError):
        DigitalOceanProvider(_Client()).delete_tagged("kresko")


def test_reconcile_droplets_marks_create_failures(home):
    class CapacityClient(FakeClient):
        def create_droplet(self, request):
            if request["name"] == "miner-1":
                raise DigitalOceanError("no capacity in nyc3")
            return super().create_droplet(request)

    client = CapacityClient()
    plan = reconcile_instances(
        miners(2),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert [a["name"] for a in plan["create"]] == ["miner-0", "miner-1"]
    assert len(plan["failed"]) == 1
    failure = plan["failed"][0]
    assert failure["name"] == "miner-1"
    assert failure["kind"] == "create"
    assert failure["region"] == "nyc3"
    assert "capacity" in failure["message"]
    # Successful node still has its asset; failed-to-create node does not.
    assert assets.read_asset("digitalocean", "1")["name"] == "miner-0"
    with pytest.raises(FileNotFoundError):
        assets.read_asset("digitalocean", "2")


def test_reconcile_droplets_marks_wait_timeouts_as_failed(home):
    class TimeoutClient(FakeClient):
        def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
            if str(droplet_id) == "1":
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    client = TimeoutClient()
    plan = reconcile_instances(
        miners(2),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert len(plan["failed"]) == 1
    failure = plan["failed"][0]
    assert failure["kind"] == "timeout"
    assert failure["name"] == "miner-0"
    asset = assets.read_asset("digitalocean", "1")
    assert asset["status"] == "failed"
    assert asset["failure_reason"]["kind"] == "timeout"
    assert asset["failure_reason"]["region"] == "nyc3"
    # The other node provisioned normally.
    healthy = assets.read_asset("digitalocean", "2")
    assert healthy["status"] == "active"
    assert healthy["public_ip"] == "203.0.113.2"


def test_reconcile_droplets_preserves_failed_marker_without_retry(home):
    client = FakeClient()
    client.droplets_by_tag["fleet-smoke"] = [_existing_droplet(9, ip="203.0.113.9")]
    # Pre-existing local asset says failed; reuse path should keep that status
    # so selectors keep skipping it.
    assets.write_asset(
        {
            "provider": "digitalocean",
            "provider_id": "9",
            "name": "miner-0",
            "tags": ["kresko", "fleet-smoke", "role-miner"],
            "status": "failed",
            "failure_reason": {"kind": "timeout", "region": "nyc3", "size": "s-1vcpu-1gb"},
        }
    )

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
    )

    assert plan["failed"] == []
    asset = assets.read_asset("digitalocean", "9")
    assert asset["status"] == "failed"


def test_reconcile_droplets_retry_failed_clears_marker_when_active(home):
    droplet = _existing_droplet(9, ip="203.0.113.9")
    client = FakeClient()
    client.droplets_by_tag["fleet-smoke"] = [droplet]
    client.droplets_by_id["9"] = droplet
    assets.write_asset(
        {
            "provider": "digitalocean",
            "provider_id": "9",
            "name": "miner-0",
            "tags": ["kresko", "fleet-smoke", "role-miner"],
            "status": "failed",
            "failure_reason": {"kind": "timeout"},
        }
    )

    plan = reconcile_instances(
        miners(1),
        fleet="smoke",
        providers={"digitalocean": DigitalOceanProvider(client)},
        ssh_key_selector="kresko-key",
        retry_failed=True,
    )

    assert plan["failed"] == []
    asset = assets.read_asset("digitalocean", "9")
    assert asset["status"] == "active"
    assert "failure_reason" not in asset


def test_client_constructs_authorized_requests():
    def handler(method, url, kwargs):
        if url.endswith("/account/keys?per_page=200&page=1"):
            return FakeResponse(payload={"ssh_keys": [{"id": 7, "name": "kresko-key"}]})
        return FakeResponse(payload={})

    session = FakeSession(handler=handler)
    client = DigitalOceanClient(token="token", session=session)

    assert client.lookup_ssh_key("kresko-key") == 7
    method, url, kwargs = session.calls[0]
    assert method == "GET"
    assert kwargs["headers"]["Authorization"] == "Bearer token"
