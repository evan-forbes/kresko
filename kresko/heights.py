"""Collect the per-node canonical chain into `heights.jsonl`.

`kresko heights` walks every selected node's best chain over JSON-RPC and
writes one record per (node, height):

    {"node": ..., "ip": ..., "height": ..., "hash": ..., "time": ..., "size": ...}

This is the join key for the propagation analysis. `analyze_forks.py` takes the
majority hash at each height as the canonical chain and calls everything else a
competing block; `analyze_block_times.py` differences `time` on the node with
the most rows to get the observed inter-block time.

Two things to know about the fields:

- `hash` is the block hash as `getblockhash` returns it, which is the same
  display order the `peer_message` trace records in `mid`. The analyzers join
  on it directly, so it must not be byte-reversed here.
- `time` is the block header's own timestamp, set by whichever node mined it.
  On a private devnet those are close to useless for *propagation* timing --
  they are miner-supplied and only loosely disciplined -- so use them for
  inter-block spacing and take propagation timing from the trace `wall_ts`.

Nodes disagree about their tip near the front of the chain, which is the entire
point of the measurement, so each node is walked independently and no attempt
is made to reconcile them here.
"""

from __future__ import annotations

import json
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator

import requests

from kresko.status import DEFAULT_RPC_PORT, DEFAULT_TIMEOUT, _rpc_call

# One RPC round trip per block per node, so a 40k-block chain across 80 nodes is
# 3.2M calls. Batching by height range is not available, but a session per node
# keeps the connection warm, and nodes are walked concurrently.
DEFAULT_MAX_WORKERS = 16


@dataclass
class NodeHeights:
    """The result of walking one node."""

    name: str
    ip: str
    rows: list[dict[str, Any]]
    error: str | None = None

    @property
    def ok(self) -> bool:
        return self.error is None


def collect(
    assets: list[dict[str, Any]],
    out_path: str | Path,
    *,
    start_height: int = 0,
    end_height: int | None = None,
    rpc_port: int = DEFAULT_RPC_PORT,
    timeout: float = DEFAULT_TIMEOUT,
    max_workers: int = DEFAULT_MAX_WORKERS,
) -> dict[str, Any]:
    """Walk every node and write `heights.jsonl` at `out_path`.

    `end_height` defaults to each node's own tip, which is what makes the file
    show the disagreement between nodes rather than hiding it behind a common
    ceiling.

    Returns a summary suitable for a readiness gate: `ok` is true only when
    every selected node was walked without error and produced at least one row.
    """
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    def walk(asset: dict[str, Any]) -> NodeHeights:
        return fetch_node_heights(
            asset.get("name", ""),
            asset.get("public_ip", ""),
            start_height=start_height,
            end_height=end_height,
            rpc_port=rpc_port,
            timeout=timeout,
        )

    if not assets:
        return {"stage": "heights", "ok": False, "nodes": 0, "rows": 0, "path": str(out_path)}

    workers = max(1, min(max_workers, len(assets)))
    with ThreadPoolExecutor(max_workers=workers) as pool:
        results = list(pool.map(walk, assets))

    rows = 0
    with out_path.open("w", encoding="utf-8") as handle:
        for result in results:
            for row in result.rows:
                handle.write(json.dumps(row, separators=(",", ":")))
                handle.write("\n")
                rows += 1

    failures = [
        {"node": result.name, "error": result.error} for result in results if not result.ok
    ]
    empty = [result.name for result in results if result.ok and not result.rows]

    return {
        "stage": "heights",
        "ok": not failures and not empty and rows > 0,
        "path": str(out_path),
        "nodes": len(results),
        "rows": rows,
        "per_node": [
            {
                "node": result.name,
                "rows": len(result.rows),
                "first_height": result.rows[0]["height"] if result.rows else None,
                "last_height": result.rows[-1]["height"] if result.rows else None,
            }
            for result in results
        ],
        "failures": failures,
        "empty": empty,
    }


def fetch_node_heights(
    name: str,
    ip: str,
    *,
    start_height: int = 0,
    end_height: int | None = None,
    rpc_port: int = DEFAULT_RPC_PORT,
    timeout: float = DEFAULT_TIMEOUT,
    session: requests.Session | None = None,
) -> NodeHeights:
    """Walk one node's best chain from `start_height` to its tip."""
    if not ip or ip == "TBD":
        return NodeHeights(name=name, ip=ip, rows=[], error="no public IP")

    own_session = session is None
    session = session or requests.Session()
    url = f"http://{ip}:{rpc_port}"
    try:
        tip = int(_rpc_call(session, url, "getblockcount", timeout))
        last = tip if end_height is None else min(tip, end_height)
        rows = list(
            _walk(session, url, name, ip, start_height, last, timeout)
        )
        return NodeHeights(name=name, ip=ip, rows=rows)
    except Exception as exc:  # noqa: BLE001 - one bad node must not lose the rest
        return NodeHeights(name=name, ip=ip, rows=[], error=f"{type(exc).__name__}: {exc}")
    finally:
        if own_session:
            session.close()


def _walk(
    session: requests.Session,
    url: str,
    name: str,
    ip: str,
    start_height: int,
    end_height: int,
    timeout: float,
) -> Iterator[dict[str, Any]]:
    """Yield one row per height, skipping heights the node cannot serve.

    A node that is still syncing, or that reorganised mid-walk, can fail a
    single height while the rest of the range is fine. Dropping that height is
    better than dropping the node: the analysis takes a majority vote per
    height, so one missing row costs one vote, while a missing node costs a
    whole chain.
    """
    for height in range(start_height, end_height + 1):
        try:
            block_hash = _rpc_call(session, url, "getblockhash", timeout, [height])
            # Verbosity 1 returns the header fields plus `size` without the
            # transaction bodies, which is all the analysis reads and keeps the
            # response small enough to walk tens of thousands of blocks.
            block = _rpc_call(session, url, "getblock", timeout, [block_hash, 1])
        except Exception:  # noqa: BLE001 - see the docstring
            continue

        yield {
            "node": name,
            "ip": ip,
            "height": height,
            "hash": block_hash,
            "time": block.get("time"),
            "size": block.get("size"),
        }
