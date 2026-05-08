from __future__ import annotations

import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest


def _prepare_bootstrap_awk() -> str:
    script = Path("scripts/node_init.sh").read_text(encoding="utf-8")
    function_start = script.index("prepare_bootstrap_config()")
    awk_start = script.index("awk '\n", function_start) + len("awk '\n")
    awk_end = script.index("\n    ' /root/.config/zebrad.toml", awk_start)
    return script[awk_start:awk_end]


def test_prepare_bootstrap_config_replaces_full_peer_arrays():
    if shutil.which("awk") is None:
        pytest.skip("awk is required to test node_init.sh bootstrap config rendering")

    source = """
[network]
network = "Testnet"
listen_addr = "0.0.0.0:18233"
initial_mainnet_peers = [
    "dnsseed.str4d.xyz:8233",
    "dnsseed.z.cash:8233",
]
initial_testnet_peers = [
    "138.197.71.170:18233",
    "167.71.87.74:18233",
]
peerset_initial_target_size = 4

[state]
cache_dir = "/root/.cache/zebra"
"""

    rendered = subprocess.run(
        ["awk", _prepare_bootstrap_awk()],
        input=source,
        text=True,
        capture_output=True,
        check=True,
    ).stdout

    parsed = tomllib.loads(rendered)
    assert parsed["network"]["listen_addr"] == "127.0.0.1:0"
    assert parsed["network"]["initial_mainnet_peers"] == []
    assert parsed["network"]["initial_testnet_peers"] == []
    assert parsed["network"]["peerset_initial_target_size"] == 4
    assert parsed["state"]["cache_dir"] == "/root/.cache/zebra"
    assert "dnsseed.str4d.xyz" not in rendered
    assert "138.197.71.170" not in rendered
