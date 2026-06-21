from __future__ import annotations

import subprocess

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


def test_inventory_omits_ssh_key_when_configured_key_is_loaded_in_agent(
    monkeypatch, tmp_path
):
    key_path = tmp_path / "id_ed25519"
    public_blob = "AAAAC3NzaC1lZDI1NTE5AAAAIKreskoLoadedAgentKey"
    key_path.write_text("encrypted-private-key-placeholder\n", encoding="utf-8")
    key_path.with_suffix(key_path.suffix + ".pub").write_text(
        f"ssh-ed25519 {public_blob} test@example\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("SSH_AUTH_SOCK", "/tmp/agent.sock")

    def fake_run(*_args, **_kwargs):
        return subprocess.CompletedProcess(
            ["ssh-add", "-L"],
            0,
            stdout=f"ssh-ed25519 {public_blob} test@example\n",
            stderr="",
        )

    monkeypatch.setattr(subprocess, "run", fake_run)

    groups = pyinfra_groups([_miner_asset()], {"user": "ubuntu", "key_path": str(key_path)})
    _host, data = groups["miner"][0]

    assert "ssh_key" not in data


def test_inventory_can_force_key_path_even_when_agent_has_key(monkeypatch, tmp_path):
    key_path = tmp_path / "id_ed25519"
    public_blob = "AAAAC3NzaC1lZDI1NTE5AAAAIKreskoLoadedAgentKey"
    key_path.write_text("encrypted-private-key-placeholder\n", encoding="utf-8")
    key_path.with_suffix(key_path.suffix + ".pub").write_text(
        f"ssh-ed25519 {public_blob} test@example\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("SSH_AUTH_SOCK", "/tmp/agent.sock")
    monkeypatch.setenv("KRESKO_SSH_FORCE_KEY_PATH", "1")

    groups = pyinfra_groups([_miner_asset()], {"user": "ubuntu", "key_path": str(key_path)})
    _host, data = groups["miner"][0]

    assert data["ssh_key"] == str(key_path)
