from __future__ import annotations

import pytest

from harness import assets, paths


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def make_asset(provider_id: str = "1", tags: list[str] | None = None) -> dict:
    return {
        "provider": "digitalocean",
        "provider_id": provider_id,
        "name": f"miner-{provider_id}",
        "role": "miner",
        "region": "nyc3",
        "size": "s-1vcpu-1gb",
        "public_ip": f"203.0.113.{provider_id}",
        "private_ip": "",
        "status": "active",
        "tags": tags
        or ["kresko", "experiment-smoke", "role-miner", "run-smoke"],
    }


def test_write_and_read_roundtrip(home):
    path = assets.write_asset(make_asset("42"))

    assert path.name == "digitalocean-42.json"
    loaded = assets.read_asset("digitalocean", "42")
    assert loaded["name"] == "miner-42"
    assert "kresko" in loaded["tags"]
    assert loaded["created_at"] == loaded["updated_at"]


def test_write_preserves_created_at(home):
    assets.write_asset(make_asset("42"))
    first = assets.read_asset("digitalocean", "42")

    asset = make_asset("42")
    asset["status"] = "rebooted"
    assets.write_asset(asset)

    second = assets.read_asset("digitalocean", "42")
    assert second["created_at"] == first["created_at"]
    assert second["status"] == "rebooted"


def test_normalize_requires_kresko_tag(home):
    with pytest.raises(ValueError):
        assets.write_asset(make_asset("42", tags=["experiment-smoke"]))


def test_list_assets_filters_by_tag(home):
    assets.write_asset(make_asset("1", tags=["kresko", "experiment-a", "role-miner"]))
    assets.write_asset(make_asset("2", tags=["kresko", "experiment-b", "role-miner"]))
    assets.write_asset(make_asset("3", tags=["kresko", "experiment-a", "role-rpc"]))

    miners_a = assets.list_assets(tags=["experiment-a", "role-miner"])
    assert [a["provider_id"] for a in miners_a] == ["1"]

    role_miner = assets.list_assets(tags=["role-miner"])
    assert sorted(a["provider_id"] for a in role_miner) == ["1", "2"]

    every = assets.list_assets()
    assert len(every) == 3


def test_delete_asset(home):
    assets.write_asset(make_asset("42"))
    assert assets.delete_asset("digitalocean", "42") is True
    assert assets.delete_asset("digitalocean", "42") is False
