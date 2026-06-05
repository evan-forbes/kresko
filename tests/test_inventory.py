from __future__ import annotations

from kresko.inventory import pyinfra_groups


def _miner_asset() -> dict:
    return {
        "name": "miner-0",
        "role": "miner",
        "provider": "digitalocean",
        "provider_id": "42",
        "public_ip": "203.0.113.42",
        "private_ip": "10.0.0.42",
        "region": "nyc3",
        "size": "s-1vcpu-1gb",
        "status": "active",
        "fleet": "smoke",
    }


def test_inventory_groups_include_role_and_provider_metadata():
    ssh = {"user": "ubuntu", "key_path": "~/.ssh/test"}

    groups = pyinfra_groups([_miner_asset()], ssh)

    assert "all" in groups
    assert "miner" in groups
    assert "digitalocean" in groups
    host, data = groups["miner"][0]
    assert host == "203.0.113.42"
    assert data["ssh_user"] == "ubuntu"
    assert data["kresko_name"] == "miner-0"
    assert data["kresko_provider_id"] == "42"
    assert data["kresko_fleet"] == "smoke"
    assert data["ssh_key"] == "~/.ssh/test"


def test_inventory_omits_ssh_key_when_path_blank():
    """Empty key_path means defer to ssh-agent — pyinfra picks up the loaded
    key without us pinning a specific file."""
    groups = pyinfra_groups([_miner_asset()], {"user": "ubuntu", "key_path": ""})
    _host, data = groups["miner"][0]
    assert "ssh_key" not in data
