from __future__ import annotations

import json

import pytest

from fleets import mainnet_zebra_snapshot
from kresko import assets, paths


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def test_valar_do_token_sets_digitalocean_token():
    env = {"VALAR_DO_TOKEN": "secret"}

    mainnet_zebra_snapshot.require_valar_do_token(env)

    assert env["DIGITALOCEAN_TOKEN"] == "secret"


def test_fleet_shape_uses_seven_bandwidth_sized_regions(home):
    env = {
        "VALAR_DO_TOKEN": "test-token",
        "KRESKO_FLEET": "mainnet-zebra-snapshot",
        "KRESKO_SSH_KEY_NAME": "kresko-mainnet",
        "KRESKO_SSH_KEY_PATH": "/home/evan/.ssh/kresko-mainnet",
    }

    fleet = mainnet_zebra_snapshot.make_fleet(env)
    desired = fleet._desired()

    assert [node.name for node in desired] == [
        "us-east-0",
        "us-west-0",
        "canada-0",
        "europe-west-0",
        "europe-central-0",
        "asia-south-0",
        "asia-pacific-0",
    ]
    assert [node.region for node in desired] == ["nyc3", "sfo3", "tor1", "lon1", "ams3", "blr1", "syd1"]
    assert [node.size for node in desired] == [
        "so1_5-4vcpu-32gb-intel",
        "so1_5-4vcpu-32gb-intel",
        "so1_5-4vcpu-32gb-intel",
        "so1_5-4vcpu-32gb",
        "so1_5-4vcpu-32gb-intel",
        "so1_5-4vcpu-32gb",
        "so1_5-4vcpu-32gb",
    ]
    assert all(node.image == "ubuntu-24-04-x64" for node in desired)
    assert all("snapshot" in node.tags for node in desired)


def test_custom_regions_must_define_exactly_seven(home):
    with pytest.raises(ValueError):
        mainnet_zebra_snapshot.make_fleet(
            {
                "VALAR_DO_TOKEN": "test-token",
                "KRESKO_ZEBRA_SNAPSHOT_REGIONS": "one:nyc3,two:sfo3",
            }
        )


def test_write_public_config_from_fleet_assets(home):
    fleet = mainnet_zebra_snapshot.make_fleet(
        {"VALAR_DO_TOKEN": "test-token", "KRESKO_FLEET": "mainnet-zebra-snapshot"}
    )
    for index, (name, region, size) in enumerate(mainnet_zebra_snapshot.DEFAULT_NODE_SPECS):
        assets.write_asset(
            {
                "provider": "digitalocean",
                "provider_id": str(index),
                "name": f"{name}-0",
                "role": "node",
                "fleet": "mainnet-zebra-snapshot",
                "region": region,
                "size": size,
                "image": "ubuntu-24-04-x64",
                "public_ip": f"203.0.113.{index}",
                "private_ip": "",
                "status": "active",
                "tags": ["kresko", "fleet-mainnet-zebra-snapshot", "role-node", "zebra", "mainnet"],
            }
        )

    path = mainnet_zebra_snapshot.write_public_config(fleet)
    config = json.loads(path.read_text(encoding="utf-8"))

    assert config["network_kind"] == "mainnet"
    assert len(config["miners"]) == 7
    assert {node["node_type"] for node in config["miners"]} == {"miner"}


def test_append_snapshot_env_is_idempotent(tmp_path):
    vars_path = tmp_path / "vars.sh"
    vars_path.write_text("#!/bin/bash\n", encoding="utf-8")

    mainnet_zebra_snapshot.append_snapshot_env(vars_path)
    mainnet_zebra_snapshot.append_snapshot_env(vars_path)

    text = vars_path.read_text(encoding="utf-8")
    assert text.count("# Zebra mainnet state snapshot") == 1
    assert mainnet_zebra_snapshot.DEFAULT_SNAPSHOT_ARCHIVE in text
    assert mainnet_zebra_snapshot.DEFAULT_SNAPSHOT_SHA256 in text
    for url in mainnet_zebra_snapshot.DEFAULT_SNAPSHOT_URLS:
        assert url in text


def test_tune_payload_zebrad_configs_enables_dual_p2p_and_zakura_bootstrap(tmp_path):
    payload = tmp_path / "payload"
    for name in ["us-east", "us-west"]:
        node_dir = payload / name
        node_dir.mkdir(parents=True)
        (node_dir / "zebrad.toml").write_text(
            """
[network]
peerset_initial_target_size = 25

[sync]
download_concurrency_limit = 50
""".lstrip(),
            encoding="utf-8",
        )

    tuned = mainnet_zebra_snapshot.tune_payload_zebrad_configs(
        payload,
        fleet_assets=[
            {"name": "us-east-0", "public_ip": "203.0.113.10"},
            {"name": "us-west-0", "public_ip": "203.0.113.11"},
        ],
    )

    assert tuned == [payload / "us-east" / "zebrad.toml", payload / "us-west" / "zebrad.toml"]
    text = (payload / "us-east" / "zebrad.toml").read_text(encoding="utf-8")
    assert "peerset_initial_target_size = 100" in text
    assert "v2_p2p = true" in text
    assert "legacy_p2p = true" in text
    assert "zakura_node_secret_key = " in text
    assert "[network.zakura]" in text
    assert 'listen_addr = "0.0.0.0:8234"' in text
    assert 'trace_dir = "/root/traces/zakura"' in text
    assert "message_rate_per_second = 4000" in text
    assert "bd3dc5d2a3d44c6bf90e364bf446231dbf9737e38a562ccf9e91ea631ea59b22@203.0.113.11:8234" in text
    assert "9ec67ad6834bc2ca0d659c240e042d3446c37cabcc092b527d459c87d938b4a4@203.0.113.10:8234" not in text
    assert "download_concurrency_limit = 100" in text


def test_tune_payload_zebrad_configs_makes_selected_nodes_zakura_only(tmp_path):
    payload = tmp_path / "payload"
    for name in ["asia-pacific", "europe-central", "us-east"]:
        node_dir = payload / name
        node_dir.mkdir(parents=True)
        (node_dir / "zebrad.toml").write_text(
            """
[network]
initial_mainnet_peers = [
    "dnsseed.z.cash:8233",
    "203.0.113.10:8233",
]
legacy_p2p = true
v2_p2p = false

[sync]
download_concurrency_limit = 50
""".lstrip(),
            encoding="utf-8",
        )

    mainnet_zebra_snapshot.tune_payload_zebrad_configs(
        payload,
        fleet_assets=[
            {"name": "asia-pacific-0", "public_ip": "203.0.113.20"},
            {"name": "europe-central-0", "public_ip": "203.0.113.30"},
            {"name": "us-east-0", "public_ip": "203.0.113.10"},
        ],
    )

    for node_dir in ["asia-pacific", "europe-central"]:
        text = (payload / node_dir / "zebrad.toml").read_text(encoding="utf-8")
        assert "initial_mainnet_peers = [\n]" in text
        assert '"dnsseed.z.cash:8233"' not in text
        assert "legacy_p2p = false" in text
        assert "v2_p2p = true" in text
        assert "[network.zakura.block_sync]" in text
        assert "replace_legacy_syncer = true" in text

    dual_stack_text = (payload / "us-east" / "zebrad.toml").read_text(encoding="utf-8")
    assert '"dnsseed.z.cash:8233"' in dual_stack_text
    assert "legacy_p2p = true" in dual_stack_text
