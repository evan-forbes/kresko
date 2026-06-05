from __future__ import annotations

import json

import pytest

from kresko import assets, paths
from kresko.fleet import TRACE_COLLECTION_PATHS
from kresko.cli import main


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def make_asset(home, **overrides):
    base = {
        "provider": "digitalocean",
        "provider_id": "1",
        "name": "miner-0",
        "role": "miner",
        "fleet": "smoke",
        "public_ip": "203.0.113.1",
        "status": "active",
        "tags": ["kresko", "fleet-smoke", "role-miner"],
    }
    base.update(overrides)
    assets.write_asset(base)


def test_assets_list_filters_by_tag(home, capsys):
    make_asset(home, provider_id="1")
    make_asset(
        home,
        provider_id="2",
        name="rpc-0",
        role="rpc",
        tags=["kresko", "fleet-smoke", "role-rpc"],
    )

    rc = main(["assets", "list", "--tag", "role-miner"])
    captured = capsys.readouterr()

    assert rc == 0
    out = json.loads(captured.out)
    assert [a["provider_id"] for a in out] == ["1"]


def test_assets_show_outputs_full_asset(home, capsys):
    make_asset(home)
    rc = main(["assets", "show", "digitalocean", "1"])
    captured = capsys.readouterr()

    assert rc == 0
    asset = json.loads(captured.out)
    assert asset["name"] == "miner-0"
    assert asset["public_ip"] == "203.0.113.1"


def test_sync_passes_provider_filter(monkeypatch, home, capsys):
    seen = {}

    def fake_sync_all(*, providers=None):
        from kresko.sync import SyncReport

        seen["providers"] = providers
        return [SyncReport(provider="vultr", upserted=[], pruned=[], errors=[])]

    monkeypatch.setattr("kresko.cli.sync_all", fake_sync_all)

    rc = main(["sync", "--provider", "vultr"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    assert seen["providers"] == ["vultr"]
    assert out[0]["provider"] == "vultr"


def test_ls_groups_nodes_by_fleet(home, capsys):
    make_asset(home, provider_id="1", name="miner-0", fleet="net-a", tags=["kresko", "fleet-net-a", "role-miner"])
    make_asset(home, provider_id="2", name="miner-1", fleet="net-a", public_ip="203.0.113.2", tags=["kresko", "fleet-net-a", "role-miner"])
    make_asset(home, provider_id="3", name="rpc-0", role="rpc", fleet="net-b", public_ip="203.0.113.3", tags=["kresko", "fleet-net-b", "role-rpc"])

    rc = main(["ls"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    by_fleet = {f["fleet"]: f for f in out}
    assert by_fleet["net-a"]["nodes"] == 2
    assert by_fleet["net-a"]["active"] == 2
    assert by_fleet["net-a"]["roles"] == ["miner"]
    assert by_fleet["net-b"]["roles"] == ["rpc"]


def test_ls_restricts_to_one_fleet(home, capsys):
    make_asset(home, provider_id="1", fleet="net-a", tags=["kresko", "fleet-net-a", "role-miner"])
    make_asset(home, provider_id="2", fleet="net-b", public_ip="203.0.113.2", tags=["kresko", "fleet-net-b", "role-miner"])

    rc = main(["ls", "net-b"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    assert [f["fleet"] for f in out] == ["net-b"]


def test_down_on_empty_fleet_is_ok(home, capsys):
    # No assets carry this fleet tag, so there is nothing to destroy: a clean
    # no-op (what a CI trap hits when the job never provisioned).
    rc = main(["down", "ci-missing", "--dry-run"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    assert out["destroyed_provider_ids"] == []
    assert out["errors"] == []


def test_archive_writes_tarball(home, capsys):
    rc = main(["archive", "ci-1"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    assert out["ok"] is True
    assert (paths.fleets_dir() / "ci-1.tar.gz").exists()


def test_download_traces_collects_standard_paths(monkeypatch, home, capsys):
    seen = {}

    class FakeFleet:
        def __init__(self, name):
            seen["fleet"] = name

        def download_traces(self, **kwargs):
            seen["kwargs"] = kwargs
            return {
                "ok": True,
                "stage": "collect",
                "paths": TRACE_COLLECTION_PATHS,
                "nodes": ["asia-0"],
            }

    monkeypatch.setattr("kresko.cli.Fleet", FakeFleet)

    rc = main(["download", "traces", "mainnet-zakura", "--name", "asia-0", "--dry-run"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    assert seen["fleet"] == "mainnet-zakura"
    assert seen["kwargs"]["name"] == ["asia-0"]
    assert seen["kwargs"]["dry_run"] is True
    assert out["paths"] == TRACE_COLLECTION_PATHS
