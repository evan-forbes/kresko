from __future__ import annotations

import json

import pytest

from fleets import mainnet_zakura
from kresko import assets, paths


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def test_valar_do_token_sets_digitalocean_token():
    env = {"VALAR_DO_TOKEN": "secret"}

    mainnet_zakura.require_valar_do_token(env)

    assert env["DIGITALOCEAN_TOKEN"] == "secret"


def test_valar_do_token_overrides_stale_digitalocean_token():
    env = {"VALAR_DO_TOKEN": "valar", "DIGITALOCEAN_TOKEN": "digitalocean"}

    mainnet_zakura.require_valar_do_token(env)

    assert env["DIGITALOCEAN_TOKEN"] == "valar"


def test_missing_valar_do_token_raises_without_silent_fallback():
    env = {"DIGITALOCEAN_TOKEN": "stale"}

    with pytest.raises(SystemExit):
        mainnet_zakura.require_valar_do_token(env)

    # The stale token must never become the active credential.
    assert env["DIGITALOCEAN_TOKEN"] == "stale"


def test_fleet_shape_uses_three_stable_regional_node_names(home):
    env = {
        "VALAR_DO_TOKEN": "test-token",
        "KRESKO_FLEET": "mainnet-zakura",
        "KRESKO_SSH_KEY_NAME": "kresko-mainnet",
        "KRESKO_SSH_KEY_PATH": "/home/evan/.ssh/kresko-mainnet",
        "KRESKO_ZAKURA_REGIONS": "syd1,sfo3,lon1",
    }

    fleet = mainnet_zakura.make_fleet(env)
    desired = fleet._desired()

    assert [node.name for node in desired] == ["asia-0", "us-0", "europe-0"]
    assert [node.region for node in desired] == ["syd1", "sfo3", "lon1"]
    assert all(node.role == "node" for node in desired)
    assert all(node.provider == "digitalocean" for node in desired)
    assert all(node.size == "s-4vcpu-8gb" for node in desired)
    assert all(node.image == "ubuntu-24-04-x64" for node in desired)
    assert all("zakura" in node.tags for node in desired)
    assert all("mainnet" in node.tags for node in desired)


def test_write_public_config_from_fleet_assets(home):
    fleet = mainnet_zakura.make_fleet(
        {"VALAR_DO_TOKEN": "test-token", "KRESKO_FLEET": "mainnet-zakura"}
    )
    for provider_id, name, region, ip in [
        ("1", "asia-0", "sgp1", "203.0.113.1"),
        ("2", "us-0", "nyc3", "203.0.113.2"),
        ("3", "europe-0", "fra1", "203.0.113.3"),
    ]:
        assets.write_asset(
            {
                "provider": "digitalocean",
                "provider_id": provider_id,
                "name": name,
                "role": "node",
                "fleet": "mainnet-zakura",
                "region": region,
                "size": "s-4vcpu-8gb",
                "image": "ubuntu-24-04-x64",
                "public_ip": ip,
                "private_ip": "",
                "status": "active",
                "tags": ["kresko", "fleet-mainnet-zakura", "role-node", "zakura", "mainnet"],
            }
        )

    path = mainnet_zakura.write_public_config(fleet)
    config = json.loads(path.read_text(encoding="utf-8"))

    assert config["network_kind"] == "mainnet"
    assert config["local_genesis"] is None
    assert [node["name"] for node in config["miners"]] == ["asia-0", "europe-0", "us-0"]
    assert {node["node_type"] for node in config["miners"]} == {"miner"}
    assert {node["provider"] for node in config["miners"]} == {"digitalocean"}


def test_append_tracing_env_is_idempotent(tmp_path):
    vars_path = tmp_path / "vars.sh"
    vars_path.write_text("#!/bin/bash\n", encoding="utf-8")

    mainnet_zakura.append_tracing_env(vars_path)
    mainnet_zakura.append_tracing_env(vars_path)

    text = vars_path.read_text(encoding="utf-8")
    assert text.count("# Zakura tracing") == 1
    assert "ZEBRA_TRACING__FILTER" in text
    assert "ZEBRA_TRACING__LOG_FILE" in text


def test_patch_zakura_fast_sync_defaults_is_idempotent(tmp_path):
    root = tmp_path / "zakura"
    constants = root / "zebra-network" / "src" / "constants.rs"
    sync = root / "zebrad" / "src" / "components" / "sync.rs"
    constants.parent.mkdir(parents=True)
    sync.parent.mkdir(parents=True)
    constants.write_text(
        "pub const DEFAULT_PEERSET_INITIAL_TARGET_SIZE: usize = 25;\n",
        encoding="utf-8",
    )
    sync.write_text("download_concurrency_limit: 50,\n", encoding="utf-8")

    patched = mainnet_zakura.patch_zakura_fast_sync_defaults(root)
    patched_again = mainnet_zakura.patch_zakura_fast_sync_defaults(root)

    assert patched == [constants, sync]
    assert patched_again == []
    assert "usize = 100;" in constants.read_text(encoding="utf-8")
    assert "download_concurrency_limit: 100," in sync.read_text(encoding="utf-8")


def test_tune_payload_zebrad_configs_sets_fast_sync_values(tmp_path):
    node_dir = tmp_path / "payload" / "asia-0"
    node_dir.mkdir(parents=True)
    config_path = node_dir / "zebrad.toml"
    config_path.write_text(
        """[network]
network = "Mainnet"
peerset_initial_target_size = 25

[sync]
download_concurrency_limit = 50
checkpoint_verify_concurrency_limit = 1000
""",
        encoding="utf-8",
    )

    tuned = mainnet_zakura.tune_payload_zebrad_configs(tmp_path / "payload")

    assert tuned == [config_path]
    text = config_path.read_text(encoding="utf-8")
    assert "peerset_initial_target_size = 100" in text
    assert "download_concurrency_limit = 100" in text
    assert "checkpoint_verify_concurrency_limit = 1000" in text


def test_build_zakura_uses_ubuntu_xtask_package(monkeypatch, home, tmp_path):
    root = tmp_path / "zakura"
    binary = root / "target" / "ubuntu" / "zebra"
    constants = root / "zebra-network" / "src" / "constants.rs"
    sync = root / "zebrad" / "src" / "components" / "sync.rs"
    binary.parent.mkdir(parents=True)
    constants.parent.mkdir(parents=True)
    sync.parent.mkdir(parents=True)
    binary.write_bytes(b"ubuntu-zakura")
    constants.write_text(
        "pub const DEFAULT_PEERSET_INITIAL_TARGET_SIZE: usize = 25;\n",
        encoding="utf-8",
    )
    sync.write_text("download_concurrency_limit: 50,\n", encoding="utf-8")
    fleet = mainnet_zakura.make_fleet(
        {"VALAR_DO_TOKEN": "test-token", "KRESKO_FLEET": "mainnet-zakura"}
    )
    calls = []

    def fake_run(cmd, *, cwd=None):
        calls.append((cmd, cwd))

    monkeypatch.setattr(mainnet_zakura, "run", fake_run)
    monkeypatch.setattr(mainnet_zakura, "capture", lambda cmd: "abc123")

    result = mainnet_zakura.build_zakura(
        fleet,
        env={"ZAKURA_ROOT": str(root), "ZAKURA_REF": "release-tag"},
    )

    assert calls == [
        (["git", "-C", str(root), "checkout", "release-tag"], None),
        (["cargo", "xtask", "package", "ubuntu"], root),
    ]
    assert result["binary"] == str(binary)
    assert result["build_command"] == "cargo xtask package ubuntu"
    assert result["fast_block_sync_peer_target"] == 100
    assert result["fast_block_sync_download_concurrency_limit"] == 100
