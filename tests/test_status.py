from __future__ import annotations

import json

import pytest
import requests

from kresko import assets, paths, status
from kresko.cli import main
from kresko.status import NodeStatus, StatusReport


class FakeResponse:
    def __init__(self, payload, status_code=200):
        self._payload = payload
        self.status_code = status_code

    def raise_for_status(self):
        if self.status_code >= 400:
            raise requests.HTTPError(str(self.status_code))

    def json(self):
        return self._payload


class FakeSession:
    """Routes RPC posts to a handler keyed by JSON-RPC method name."""

    def __init__(self, handler):
        self._handler = handler
        self.closed = False

    def post(self, url, json=None, timeout=None):
        return self._handler(json["method"])

    def close(self):
        self.closed = True


def test_fetch_uses_blockchaininfo_for_height_and_progress():
    def handler(method):
        assert method == "getblockchaininfo"
        return FakeResponse({"result": {"blocks": 1402233, "verificationprogress": 0.987}})

    node = status.fetch_node_status("miner-0", "203.0.113.1", session=FakeSession(handler))

    assert node.height == 1402233
    assert node.verification_progress == pytest.approx(0.987)
    assert node.status == "syncing (98.7%)"
    assert node.reachable is True


def test_fetch_marks_fully_verified_node_synced():
    session = FakeSession(lambda _m: FakeResponse({"result": {"blocks": 100, "verificationprogress": 1.0}}))
    node = status.fetch_node_status("miner-0", "203.0.113.1", session=session)
    assert node.status == "synced"


def test_fetch_falls_back_to_getblockcount_when_info_fails():
    def handler(method):
        if method == "getblockchaininfo":
            raise requests.ConnectionError("busy")
        return FakeResponse({"result": 4242})

    node = status.fetch_node_status("miner-0", "203.0.113.1", session=FakeSession(handler))

    assert node.height == 4242
    assert node.verification_progress is None
    assert node.status == "height ok; progress unknown"


def test_fetch_reports_unreachable_when_all_calls_fail():
    def handler(_method):
        raise requests.ConnectionError("connection refused")

    node = status.fetch_node_status("miner-0", "203.0.113.1", session=FakeSession(handler))

    assert node.height is None
    assert node.reachable is False
    assert node.status == "unreachable: connection failed"


def test_fetch_skips_nodes_without_ip():
    node = status.fetch_node_status("miner-0", "TBD")
    assert node.height is None
    assert node.status == "unreachable: no public IP"


def test_query_status_aggregates(monkeypatch):
    heights = {"203.0.113.1": 100, "203.0.113.2": None}

    def fake_fetch(name, ip, **kwargs):
        return NodeStatus(name=name, ip=ip, height=heights[ip])

    monkeypatch.setattr(status, "fetch_node_status", fake_fetch)

    report = status.query_status(
        [
            {"name": "miner-0", "public_ip": "203.0.113.1"},
            {"name": "miner-1", "public_ip": "203.0.113.2"},
        ]
    )

    assert report.total == 2
    assert report.reachable == 1
    assert report.unreachable == 1
    assert [n.name for n in report.nodes] == ["miner-0", "miner-1"]


def test_summarize_buckets_heights():
    report = StatusReport(
        nodes=[
            NodeStatus("a", "1", height=10),
            NodeStatus("b", "2", height=20),
            NodeStatus("c", "3", height=10),
            NodeStatus("d", "4", height=None),
        ]
    )
    summary = status.summarize(report)
    assert summary["lowest_height"] == 10
    assert summary["highest_height"] == 20
    assert summary["height_buckets"] == [
        {"height": 20, "nodes": 1},
        {"height": 10, "nodes": 2},
    ]


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def _make_asset(**overrides):
    base = {
        "provider": "vultr",
        "provider_id": "1",
        "name": "miner-0",
        "role": "miner",
        "experiment": "exp-a",
        "run": "exp-a-001",
        "public_ip": "203.0.113.1",
        "status": "active",
        "tags": ["kresko", "experiment-exp-a", "role-miner", "run-exp-a-001"],
    }
    base.update(overrides)
    assets.write_asset(base)


def test_cli_status_filters_and_queries(home, monkeypatch, capsys):
    _make_asset(provider_id="1", name="miner-0", role="miner")
    _make_asset(
        provider_id="2",
        name="rpc-0",
        role="rpc",
        public_ip="203.0.113.2",
        tags=["kresko", "experiment-exp-a", "role-rpc", "run-exp-a-001"],
    )

    queried: dict = {}

    def fake_query(items, *, rpc_port, timeout, **kwargs):
        queried["ips"] = [a["public_ip"] for a in items]
        queried["rpc_port"] = rpc_port
        return StatusReport(nodes=[NodeStatus("miner-0", "203.0.113.1", height=99, status="synced")])

    monkeypatch.setattr("kresko.cli.status.query_status", fake_query)

    rc = main(["status", "--role", "miner", "--rpc-port", "18232", "--json"])
    out = json.loads(capsys.readouterr().out)

    assert rc == 0
    # role filter dropped the rpc node before any RPC call
    assert queried["ips"] == ["203.0.113.1"]
    assert queried["rpc_port"] == 18232
    assert out["nodes"][0]["height"] == 99
    assert out["reachable"] == 1


def test_cli_status_skips_destroyed_nodes(home, monkeypatch, capsys):
    _make_asset(provider_id="1", name="miner-0", status="active")
    _make_asset(provider_id="2", name="miner-1", public_ip="203.0.113.9", status="destroyed")

    seen: dict = {}

    def fake_query(items, **kwargs):
        seen["names"] = [a["name"] for a in items]
        return StatusReport(nodes=[])

    monkeypatch.setattr("kresko.cli.status.query_status", fake_query)

    main(["status"])
    assert seen["names"] == ["miner-0"]
