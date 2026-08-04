from __future__ import annotations

import json

import requests

from kresko import heights


class FakeResponse:
    def __init__(self, payload, status_code=200):
        self._payload = payload
        self.status_code = status_code

    def raise_for_status(self):
        if self.status_code >= 400:
            raise requests.HTTPError(str(self.status_code))

    def json(self):
        return self._payload


class FakeChainSession:
    """Serves `getblockcount`/`getblockhash`/`getblock` from a fixture chain.

    `chain` maps height -> (hash, time, size). A height listed in `broken`
    fails, standing in for a node that reorganised or was still syncing when
    the walk reached it.
    """

    def __init__(self, chain, broken=()):
        self._chain = chain
        self._broken = set(broken)
        self.closed = False
        self.calls = []

    def post(self, url, json=None, timeout=None):
        method = json["method"]
        params = json["params"]
        self.calls.append((method, tuple(params)))

        if method == "getblockcount":
            return FakeResponse({"result": max(self._chain)})

        if method == "getblockhash":
            height = params[0]
            if height in self._broken:
                return FakeResponse({"error": {"code": -8, "message": "out of range"}})
            return FakeResponse({"result": self._chain[height][0]})

        if method == "getblock":
            block_hash, verbosity = params
            for height, (candidate, time, size) in self._chain.items():
                if candidate == block_hash:
                    assert verbosity == 1, "bodies are never needed, and are large"
                    return FakeResponse(
                        {"result": {"height": height, "time": time, "size": size}}
                    )
            return FakeResponse({"error": {"code": -5, "message": "not found"}})

        raise AssertionError(f"unexpected method {method}")

    def close(self):
        self.closed = True


def chain(count, *, first_hash_byte=0xAA, base_time=1_700_000_000, spacing=75):
    return {
        height: (
            f"{first_hash_byte:02x}" + f"{height:062x}",
            base_time + height * spacing,
            1000 + height,
        )
        for height in range(count)
    }


def test_walk_emits_one_row_per_height_with_the_analyzer_schema():
    session = FakeChainSession(chain(4))

    result = heights.fetch_node_heights("miner-0", "203.0.113.1", session=session)

    assert result.ok
    assert [row["height"] for row in result.rows] == [0, 1, 2, 3]

    row = result.rows[2]
    # These six keys are the file's contract: `analyze_forks.py` reads
    # height/hash, `analyze_block_times.py` reads node/time, and the write-up
    # reads size.
    assert set(row) == {"node", "ip", "height", "hash", "time", "size"}
    assert row["node"] == "miner-0"
    assert row["ip"] == "203.0.113.1"
    assert row["hash"] == "aa" + f"{2:062x}"
    assert row["time"] == 1_700_000_000 + 2 * 75
    assert row["size"] == 1002


def test_a_height_the_node_cannot_serve_is_skipped_not_fatal():
    # One failed height must cost one majority vote, not the whole node: a
    # dropped node would remove an entire chain from the canonical-hash tally.
    session = FakeChainSession(chain(5), broken={2})

    result = heights.fetch_node_heights("miner-1", "203.0.113.2", session=session)

    assert result.ok
    assert [row["height"] for row in result.rows] == [0, 1, 3, 4]


def test_each_node_walks_to_its_own_tip():
    # Nodes disagreeing about the tip is the measurement, not an error, so the
    # walk must not clamp every node to a common ceiling.
    short = FakeChainSession(chain(3))
    tall = FakeChainSession(chain(6))

    assert len(heights.fetch_node_heights("a", "203.0.113.1", session=short).rows) == 3
    assert len(heights.fetch_node_heights("b", "203.0.113.2", session=tall).rows) == 6


def test_start_and_end_height_bound_the_walk():
    session = FakeChainSession(chain(10))

    result = heights.fetch_node_heights(
        "miner-0", "203.0.113.1", start_height=3, end_height=5, session=session
    )

    assert [row["height"] for row in result.rows] == [3, 4, 5]


def test_end_height_above_the_tip_stops_at_the_tip():
    session = FakeChainSession(chain(3))

    result = heights.fetch_node_heights(
        "miner-0", "203.0.113.1", end_height=100, session=session
    )

    assert [row["height"] for row in result.rows] == [0, 1, 2]


def test_a_node_without_a_public_ip_is_reported_not_raised():
    result = heights.fetch_node_heights("pending", "TBD")

    assert not result.ok
    assert result.error == "no public IP"
    assert result.rows == []


class FakeSessionRouter:
    """One fake session shared by every node, dispatching on the request URL.

    Patched over `requests.Session` so `collect` runs its real code path --
    thread pool, per-node walk, file write -- against fixture chains.
    """

    def __init__(self, chains_by_ip):
        self._sessions = {
            ip: FakeChainSession(chain_map) for ip, chain_map in chains_by_ip.items()
        }
        self.closed = False

    def post(self, url, json=None, timeout=None):
        ip = url.removeprefix("http://").split(":")[0]
        session = self._sessions.get(ip)
        if session is None:
            raise requests.ConnectionError(f"no route to {ip}")
        return session.post(url, json=json, timeout=timeout)

    def close(self):
        self.closed = True


def test_collect_writes_jsonl_and_gates_on_every_node_succeeding(tmp_path, monkeypatch):
    router = FakeSessionRouter(
        {
            "203.0.113.1": chain(3, first_hash_byte=0xAA),
            # A one-block fork: the same heights, a different hash.
            "203.0.113.2": chain(3, first_hash_byte=0xBB),
        }
    )
    monkeypatch.setattr(heights.requests, "Session", lambda: router)

    out = tmp_path / "data" / "heights.jsonl"
    summary = heights.collect(
        [
            {"name": "miner-0", "public_ip": "203.0.113.1"},
            {"name": "miner-1", "public_ip": "203.0.113.2"},
        ],
        out,
    )

    assert summary["ok"]
    assert summary["nodes"] == 2
    assert summary["rows"] == 6
    assert summary["failures"] == []

    rows = [json.loads(line) for line in out.read_text().splitlines()]
    assert len(rows) == 6
    assert {row["node"] for row in rows} == {"miner-0", "miner-1"}
    # Both nodes report height 2 with different hashes: exactly the shape the
    # fork analysis resolves by majority vote.
    at_two = {row["node"]: row["hash"] for row in rows if row["height"] == 2}
    assert len(set(at_two.values())) == 2


def test_collect_is_not_ok_when_a_node_is_unreachable(tmp_path, monkeypatch):
    router = FakeSessionRouter({"203.0.113.1": chain(2)})
    monkeypatch.setattr(heights.requests, "Session", lambda: router)

    summary = heights.collect(
        [
            {"name": "miner-0", "public_ip": "203.0.113.1"},
            {"name": "miner-1", "public_ip": "203.0.113.9"},
        ],
        tmp_path / "heights.jsonl",
    )

    assert not summary["ok"]
    assert [failure["node"] for failure in summary["failures"]] == ["miner-1"]
    # The reachable node's rows are still written, so a partial collection is
    # inspectable rather than discarded.
    assert summary["rows"] == 2


def test_collect_with_no_nodes_is_not_ok(tmp_path):
    summary = heights.collect([], tmp_path / "heights.jsonl")

    assert not summary["ok"]
    assert summary["rows"] == 0
