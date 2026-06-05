from __future__ import annotations

import json

from kresko.selectors import select


def sample_assets():
    return [
        {"name": "miner-0", "role": "miner", "public_ip": "203.0.113.1", "status": "active", "fleet": "smoke"},
        {"name": "miner-1", "role": "miner", "public_ip": "203.0.113.2", "status": "active", "fleet": "smoke"},
        {"name": "rpc-0", "role": "rpc", "public_ip": "203.0.113.3", "status": "active", "fleet": "smoke"},
        {"name": "miner-2", "role": "miner", "public_ip": "", "status": "pending", "fleet": "smoke"},
        {"name": "miner-3", "role": "miner", "public_ip": "203.0.113.4", "status": "destroyed", "fleet": "smoke"},
        {"name": "miner-4", "role": "miner", "public_ip": "203.0.113.5", "status": "active", "fleet": "other"},
    ]


def test_selects_roles_names_and_globs():
    assets = sample_assets()

    assert [a["name"] for a in select(assets, roles="rpc")] == ["rpc-0"]
    assert [a["name"] for a in select(assets, names="miner-1")] == ["miner-1"]
    assert [a["name"] for a in select(assets, patterns="miner-*")] == [
        "miner-0",
        "miner-1",
        "miner-4",
    ]


def test_selects_by_fleet():
    assert [a["name"] for a in select(sample_assets(), fleet="other")] == ["miner-4"]


def test_select_skips_failed_status_assets():
    assets = sample_assets() + [
        {
            "name": "miner-5",
            "role": "miner",
            "public_ip": "203.0.113.99",
            "status": "failed",
            "fleet": "smoke",
        }
    ]
    selected = select(assets, roles="miner")
    assert "miner-5" not in [a["name"] for a in selected]


def test_selects_failed_nodes_from_previous_result(tmp_path):
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps({"failures": [{"node": "miner-1"}, {"node": "miner-3"}]}),
        encoding="utf-8",
    )

    selected = select(sample_assets(), failed_from=result_path)

    assert [a["name"] for a in selected] == ["miner-1"]
