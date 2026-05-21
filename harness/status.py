"""Query node block heights over JSON-RPC.

`kresko status` reads the asset store (`~/.kresko/assets/`), selects the
active nodes (optionally filtered by tag/provider/role/experiment/run), and
POSTs `getblockchaininfo`/`getblockcount` to each node's public IP. It is the
asset-store equivalent of the old Rust `kresko status`, which read a config
dir of miners.

Nodes don't record their RPC port in the asset store, so the port is supplied
by the caller (`--rpc-port`, defaulting to `KRESKO_RPC_PORT` or 8232). Local
genesis nodes use 18232; mainnet/public-testnet use 8232.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass
from typing import Any

import requests

DEFAULT_RPC_PORT = 8232
DEFAULT_TIMEOUT = 5.0
# Treat anything within rounding distance of fully verified as synced, matching
# the old Rust threshold so display is stable while Zebra reports 0.9999x.
SYNCED_THRESHOLD = 0.9999


@dataclass
class NodeStatus:
    name: str
    ip: str
    height: int | None = None
    verification_progress: float | None = None
    status: str = "unknown"
    error: str | None = None

    @property
    def reachable(self) -> bool:
        return self.height is not None

    def to_dict(self) -> dict[str, Any]:
        data = asdict(self)
        data["reachable"] = self.reachable
        if self.error is None:
            data.pop("error")
        return data


@dataclass
class StatusReport:
    nodes: list[NodeStatus]

    @property
    def total(self) -> int:
        return len(self.nodes)

    @property
    def reachable(self) -> int:
        return sum(1 for node in self.nodes if node.reachable)

    @property
    def unreachable(self) -> int:
        return self.total - self.reachable

    def to_dict(self) -> dict[str, Any]:
        return {
            "nodes": [node.to_dict() for node in self.nodes],
            "total": self.total,
            "reachable": self.reachable,
            "unreachable": self.unreachable,
        }


def query_status(
    assets: list[dict[str, Any]],
    *,
    rpc_port: int = DEFAULT_RPC_PORT,
    timeout: float = DEFAULT_TIMEOUT,
    max_workers: int = 16,
) -> StatusReport:
    """Query every asset's RPC height concurrently. Order matches `assets`."""
    if not assets:
        return StatusReport(nodes=[])

    def fetch(asset: dict[str, Any]) -> NodeStatus:
        return fetch_node_status(
            asset.get("name", ""),
            asset.get("public_ip", ""),
            rpc_port=rpc_port,
            timeout=timeout,
        )

    workers = max(1, min(max_workers, len(assets)))
    with ThreadPoolExecutor(max_workers=workers) as pool:
        nodes = list(pool.map(fetch, assets))
    return StatusReport(nodes=nodes)


def fetch_node_status(
    name: str,
    ip: str,
    *,
    rpc_port: int = DEFAULT_RPC_PORT,
    timeout: float = DEFAULT_TIMEOUT,
    session: requests.Session | None = None,
) -> NodeStatus:
    """Probe one node. `getblockchaininfo` carries both the height and the
    verification progress; fall back to the lighter `getblockcount` if it is
    busy during sync, so we still report a height when we can."""
    node = NodeStatus(name=name, ip=ip)
    if not ip or ip == "TBD":
        node.error = "no public IP"
        node.status = _status_label(node)
        return node

    own_session = session is None
    session = session or requests.Session()
    url = f"http://{ip}:{rpc_port}"
    try:
        try:
            info = _rpc_call(session, url, "getblockchaininfo", timeout)
            node.height = _as_int(info.get("blocks"))
            node.verification_progress = _as_float(info.get("verificationprogress"))
        except Exception:
            node.height = _as_int(_rpc_call(session, url, "getblockcount", timeout))
    except Exception as exc:
        node.error = _short_error(exc)
    node.status = _status_label(node)
    if own_session:
        session.close()
    return node


def summarize(report: StatusReport) -> dict[str, Any]:
    heights = sorted(node.height for node in report.nodes if node.height is not None)
    buckets: dict[int, int] = {}
    for height in heights:
        buckets[height] = buckets.get(height, 0) + 1
    return {
        "total": report.total,
        "reachable": report.reachable,
        "unreachable": report.unreachable,
        "lowest_height": heights[0] if heights else None,
        "highest_height": heights[-1] if heights else None,
        "median_height": heights[len(heights) // 2] if heights else None,
        "height_buckets": [
            {"height": height, "nodes": buckets[height]}
            for height in sorted(buckets, reverse=True)
        ],
    }


def render_report(report: StatusReport) -> str:
    if not report.nodes:
        return "No active nodes found."
    name_w = max(len("Name"), *(len(node.name) for node in report.nodes))
    ip_w = max(len("IP"), *(len(node.ip) for node in report.nodes))
    lines = [f"{'Name':<{name_w}}  {'IP':<{ip_w}}  {'Height':>9}  Status"]
    lines.append("-" * (name_w + ip_w + 9 + len("Status") + 6))
    for node in report.nodes:
        height = str(node.height) if node.height is not None else "N/A"
        lines.append(
            f"{node.name:<{name_w}}  {node.ip:<{ip_w}}  {height:>9}  {node.status}"
        )
    lines.append("")
    lines.append(
        f"{report.total} nodes: {report.reachable} reachable, "
        f"{report.unreachable} unreachable"
    )
    return "\n".join(lines)


def render_summary(summary: dict[str, Any]) -> str:
    lines = [
        f"{summary['total']} nodes: {summary['reachable']} reachable, "
        f"{summary['unreachable']} unreachable"
    ]
    if summary["highest_height"] is not None:
        lines.append(
            f"Heights: low={summary['lowest_height']}, "
            f"median={summary['median_height']}, high={summary['highest_height']}"
        )
    for bucket in summary["height_buckets"]:
        lines.append(f"  {bucket['nodes']} node(s) at height {bucket['height']}")
    return "\n".join(lines)


def _rpc_call(
    session: requests.Session, url: str, method: str, timeout: float
) -> Any:
    body = {"jsonrpc": "2.0", "id": 1, "method": method, "params": []}
    resp = session.post(url, json=body, timeout=timeout)
    resp.raise_for_status()
    data = resp.json()
    if data.get("error"):
        raise RuntimeError(str(data["error"]))
    return data.get("result")


def _status_label(node: NodeStatus) -> str:
    if node.height is None:
        return f"unreachable: {node.error}" if node.error else "unreachable"
    progress = node.verification_progress
    if progress is None:
        return "height ok; progress unknown"
    if progress >= SYNCED_THRESHOLD:
        return "synced"
    return f"syncing ({progress * 100:.1f}%)"


def _as_int(value: Any) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _as_float(value: Any) -> float | None:
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _short_error(exc: Exception) -> str:
    """Collapse verbose requests stack messages into a terse status label.

    The full urllib3 pool message is useless in a table; for status the
    distinction that matters is timeout vs. refused/unroutable vs. HTTP error.
    """
    if isinstance(exc, requests.exceptions.Timeout):
        return "timed out"
    if isinstance(exc, requests.exceptions.ConnectionError):
        return "connection failed"
    if isinstance(exc, requests.exceptions.HTTPError):
        code = getattr(exc.response, "status_code", None)
        return f"http error {code}" if code else "http error"
    text = str(exc) or exc.__class__.__name__
    return text if len(text) <= 120 else text[:117] + "..."
