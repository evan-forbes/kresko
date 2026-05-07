from __future__ import annotations

from kresko_py.inventory import pyinfra_groups


def test_inventory_groups_include_role_and_provider_metadata():
    assets = [
        {
            "name": "miner-0",
            "role": "miner",
            "provider": "digitalocean",
            "provider_id": "42",
            "public_ip": "203.0.113.42",
            "private_ip": "10.0.0.42",
            "region": "nyc3",
            "size": "s-1vcpu-1gb",
            "status": "active",
            "experiment": "smoke",
            "run": "smoke",
        }
    ]
    ssh = {"user": "ubuntu", "key_path": "~/.ssh/test"}

    groups = pyinfra_groups(assets, ssh)

    assert "all" in groups
    assert "miner" in groups
    assert "digitalocean" in groups
    host, data = groups["miner"][0]
    assert host == "203.0.113.42"
    assert data["ssh_user"] == "ubuntu"
    assert data["kresko_name"] == "miner-0"
    assert data["kresko_provider_id"] == "42"
    assert data["kresko_experiment"] == "smoke"
    assert data["kresko_run"] == "smoke"
    assert data["ssh_key"] == "~/.ssh/test"
