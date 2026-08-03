from __future__ import annotations

import json
import os
import subprocess

import pytest

from kresko import DigitalOcean, Fleet, paths
from kresko import assets as assets_store
from kresko.fleet import run_pyinfra
from kresko.providers import DigitalOceanError, DigitalOceanProvider
from kresko.remote import DEFAULT_STATE_SNAPSHOT_URL


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


class FakeDigitalOcean:
    def __init__(self) -> None:
        self.created: list[dict] = []
        self.deleted: list[str] = []
        self.droplets_by_tag: dict[str, list[dict]] = {}
        self.droplets_by_id: dict[str, dict] = {}

    def list_droplets_by_tag(self, tag):
        return list(self.droplets_by_tag.get(tag, []))

    def lookup_ssh_key(self, selector):
        return 7

    def create_droplet(self, request):
        self.created.append(request)
        droplet_id = len(self.created)
        droplet = {
            "id": droplet_id,
            "name": request["name"],
            "status": "new",
            "region": {"slug": request["region"]},
            "size": {"slug": request["size"]},
            "image": {"slug": request["image"]},
            "tags": request["tags"],
            "networks": {"v4": []},
        }
        self.droplets_by_id[str(droplet_id)] = droplet
        for tag in request["tags"]:
            self.droplets_by_tag.setdefault(tag, []).append(droplet)
        return droplet

    def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
        droplet = dict(self.droplets_by_id[str(droplet_id)])
        droplet["status"] = "active"
        droplet["networks"] = {
            "v4": [{"type": "public", "ip_address": f"203.0.113.{droplet_id}"}]
        }
        return droplet

    def get_droplet(self, droplet_id):
        return self.droplets_by_id[str(droplet_id)]

    def delete_droplet(self, droplet_id):
        self.deleted.append(str(droplet_id))


def fake_provider(fake: FakeDigitalOcean) -> dict[str, DigitalOceanProvider]:
    return {"digitalocean": DigitalOceanProvider(fake)}


def fake_s3_runner(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    if len(cmd) > 2 and cmd[2] == "cp":
        return subprocess.CompletedProcess(cmd, 0, "", "")
    if len(cmd) > 2 and cmd[2] == "presign":
        return subprocess.CompletedProcess(cmd, 0, "https://example.test/payload.tar.gz\n", "")
    return subprocess.CompletedProcess(cmd, 1, "", "unexpected aws command")


def make_fleet(home, fake, name="ci-abc", **kwargs) -> Fleet:
    fleet = Fleet(
        name,
        ssh={"key_name": "kresko-key"},
        providers=fake_provider(fake),
        s3_runner=fake_s3_runner,
        **kwargs,
    )
    return fleet


def add_miners(fleet: Fleet, count: int = 1, **kwargs) -> Fleet:
    fleet.add(
        "miner",
        count=count,
        provider=DigitalOcean(region="nyc3", size="s-1vcpu-1gb", image="ubuntu-24-04-x64"),
        payload=["payload"],
        **kwargs,
    )
    return fleet


def make_payload(tmp_path, name: str = "payload"):
    payload = tmp_path / name
    payload.mkdir()
    (payload / "vars.sh").write_text('export KRESKO_FRESH_STATE="0"\n', encoding="utf-8")
    return payload


# --- construction ------------------------------------------------------------


def test_fleet_creates_state_dir(home):
    fleet = Fleet("ci-abc")
    assert fleet.dir == paths.fleet_dir("ci-abc")
    assert fleet.dir.is_dir()
    assert (fleet.dir / "nodes").is_dir()
    assert (fleet.dir / "data").is_dir()


def test_fleet_rejects_bad_name(home):
    with pytest.raises(ValueError):
        Fleet("Bad Name")


# --- plan / up ---------------------------------------------------------------


def test_plan_creates_no_assets_but_writes_result(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=2)

    result = fleet.plan()

    assert result["ok"] is True
    assert [a["name"] for a in result["plan"]["create"]] == ["miner-0", "miner-1"]
    assert (fleet.dir / "result.json").exists()
    assert assets_store.list_assets() == []


def test_up_writes_assets_and_node_snapshots(home):
    fake = FakeDigitalOcean()
    fleet = make_fleet(home, fake)
    add_miners(fleet, count=1)

    result = fleet.up()

    assert result["ok"] is True
    asset = assets_store.read_asset("digitalocean", "1")
    assert asset["name"] == "miner-0"
    assert asset["fleet"] == "ci-abc"
    assert "fleet-ci-abc" in asset["tags"]
    assert "role-miner" in asset["tags"]
    assert (fleet.dir / "nodes" / "miner-0.json").exists()


def test_up_is_idempotent_adopts_existing(home):
    fake = FakeDigitalOcean()
    fleet = make_fleet(home, fake)
    add_miners(fleet, count=2)

    first = fleet.up()
    assert first["ok"] is True
    assert len(fake.created) == 2

    # Second up against the same live fleet creates nothing — the nodes are
    # adopted by (fleet tag, name).
    second = fleet.up()
    assert second["ok"] is True
    assert len(fake.created) == 2
    assert second["plan"]["create"] == []
    assert sorted(a["name"] for a in second["plan"]["reuse"]) == ["miner-0", "miner-1"]


def test_up_returns_partial_success_on_create_failure(home):
    class CapacityFake(FakeDigitalOcean):
        def create_droplet(self, request):
            if request["name"] == "miner-1":
                raise DigitalOceanError("no capacity in nyc3")
            return super().create_droplet(request)

    fleet = make_fleet(home, CapacityFake())
    add_miners(fleet, count=2)

    result = fleet.up()

    assert result["ok"] is False
    assert result["requested"] == 2
    assert result["succeeded"] == 1
    assert [f["name"] for f in result["failed"]] == ["miner-1"]
    res = json.loads((fleet.dir / "result.json").read_text())
    assert res["ok"] is False
    assert res["fleet"] == "ci-abc"
    assert res["failures"][0]["node"] == "miner-1"


def test_plan_reports_not_ok_on_duplicate_live_nodes(home):
    # Two live instances share the desired name: a real up() raises in
    # reconcile, so plan() must not claim ok=True.
    def droplet(pid):
        return {
            "id": pid,
            "name": "miner-0",
            "status": "active",
            "region": {"slug": "nyc3"},
            "size": {"slug": "s-1vcpu-1gb"},
            "image": {"slug": "ubuntu-24-04-x64"},
            "tags": ["kresko", "fleet-ci-abc", "role-miner"],
            "networks": {"v4": [{"type": "public", "ip_address": f"203.0.113.{pid}"}]},
        }

    fake = FakeDigitalOcean()
    fake.droplets_by_tag["fleet-ci-abc"] = [droplet(1), droplet(2)]
    fleet = make_fleet(home, fake, name="ci-abc")
    add_miners(fleet, count=1)

    result = fleet.plan()

    assert result["ok"] is False
    assert result["duplicate"], "duplicate live nodes should be surfaced"


def test_load_env_reads_home_dotenv(home, monkeypatch, tmp_path):
    # No CLI wrapper in plain `python fleet.py` usage, so the Fleet itself must
    # load ~/.kresko/.env (KRESKO_HOME == tmp_path here) for provider creds.
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".env").write_text("KRESKO_TEST_HOME_TOKEN=from-home\n", encoding="utf-8")
    monkeypatch.delenv("KRESKO_TEST_HOME_TOKEN", raising=False)

    fleet = Fleet("ci-env")
    try:
        fleet._load_env()
        assert os.environ.get("KRESKO_TEST_HOME_TOKEN") == "from-home"
    finally:
        os.environ.pop("KRESKO_TEST_HOME_TOKEN", None)


# --- deploy ------------------------------------------------------------------


def test_deploy_dry_run_writes_pyinfra_files(home):
    fake = FakeDigitalOcean()
    fleet = make_fleet(home, fake)
    add_miners(fleet, count=1)
    fleet.up()

    calls = []

    def runner(inventory, deploy_file, dry_run):
        calls.append((inventory, deploy_file, dry_run))
        return subprocess.CompletedProcess([], 0, "", "")

    fleet._pyinfra_runner = runner

    result = fleet.deploy(role="miner", dry_run=True)

    assert result["ok"] is True
    assert result["nodes"] == ["miner-0"]
    assert result["delivery"] == "s3"
    assert calls == []
    assert (fleet.dir / "inventory.py").exists()
    assert (fleet.dir / "deploy_payload.py").exists()
    body = (fleet.dir / "deploy_payload.py").read_text()
    assert "pyinfra_deploy_s3" in body
    assert "pyinfra_deploy_base" not in body


def test_deploy_skips_failed_nodes(home):
    class TimeoutFake(FakeDigitalOcean):
        def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
            if str(droplet_id) == "1":
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    fleet = make_fleet(home, TimeoutFake())
    add_miners(fleet, count=2)
    up = fleet.up()
    assert up["succeeded"] == 1

    fleet._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess([], 0, "", "")
    deploy = fleet.deploy(role="miner", dry_run=True)

    # The failed node is gone from the deploy targets.
    assert deploy["nodes"] == ["miner-1"]


def test_deploy_state_snapshot_default_off(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    fleet.deploy(role="miner", dry_run=True)
    body = (fleet.dir / "deploy_payload.py").read_text()
    assert "pyinfra_state_snapshot" not in body


def test_deploy_state_snapshot_true_uses_default_url(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    result = fleet.deploy(role="miner", state_snapshot=True, dry_run=True)
    assert result["state_snapshot_url"] == DEFAULT_STATE_SNAPSHOT_URL
    body = (fleet.dir / "deploy_payload.py").read_text()
    assert "pyinfra_state_snapshot" in body
    assert DEFAULT_STATE_SNAPSHOT_URL in body


def test_deploy_state_snapshot_explicit_url(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    url = "http://snapshots.example/mainnet-latest.tar.gz"
    result = fleet.deploy(role="miner", state_snapshot=url, dry_run=True)
    assert result["state_snapshot_url"] == url
    assert url in (fleet.dir / "deploy_payload.py").read_text()


def test_deploy_records_local_binary_provenance(home, tmp_path):
    import hashlib

    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    # Stage a payload-style build dir and deploy it by explicit path.
    payload_root = tmp_path / "payload"
    build_dir = payload_root / "build"
    build_dir.mkdir(parents=True)
    (build_dir / "zebrad").write_bytes(b"fake-zebrad-bytes")
    (build_dir / "kresko").write_bytes(b"fake-kresko-bytes")
    z_sha = hashlib.sha256(b"fake-zebrad-bytes").hexdigest()
    k_sha = hashlib.sha256(b"fake-kresko-bytes").hexdigest()
    (build_dir / "manifest.txt").write_text(
        f"zebrad_sha256={z_sha}\nkresko_sha256={k_sha}\n"
        f"zebrad_source=/build/zebra\nkresko_source=/build/kresko\n",
        encoding="utf-8",
    )
    (payload_root / "vars.sh").write_text('export KRESKO_FRESH_STATE="0"\n', encoding="utf-8")

    fleet._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess([], 0, "", "")
    result = fleet.deploy(payload=str(payload_root), role="miner")

    binaries = result["binary_provenance_local"]["binaries"]
    assert binaries["zebrad"]["manifest_sha256"] == z_sha
    assert binaries["zebrad"]["staged_sha256"] == z_sha
    assert binaries["zebrad"]["source"] == "/build/zebra"
    assert result["delivery"] == "s3"
    assert result["payload_names"] == ["payload"]
    assert "payload_s3_key" in result
    assert "payload_archive_sha256" in result


def test_deploy_parses_remote_provenance_lines(home, tmp_path):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()
    payload = make_payload(tmp_path)

    fake_stdout = (
        "[miner-0] PROVENANCE: zebrad CHANGED (was=aaaa, now=bbbb)\n"
        "[miner-0] PROVENANCE: kresko unchanged (sha256=cccc)\n"
    )
    fleet._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess([], 0, fake_stdout, "")
    result = fleet.deploy(payload=str(payload), role="miner")

    remote = result["binary_provenance_remote"]
    assert any(b["binary"] == "zebrad" for b in remote["changed"])
    assert any(b["binary"] == "kresko" for b in remote["unchanged"])


# --- run (ephemeral + background) -------------------------------------------


def test_run_background_starts_tmux(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    fleet._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess(["pyinfra"], 0, "ok", "")
    result = fleet.run("date", background="smoke", role="miner", log_path="/root/smoke.log")

    assert result["ok"] is True
    assert result["background"] == "smoke"
    assert "tmux new-session" in result["command"]
    assert (fleet.dir / "pyinfra.run.stdout.log").read_text() == "ok"


def test_run_ephemeral_passes_command_verbatim(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    fleet._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess([], 0, "", "")
    result = fleet.run("kresko status", role="miner", dry_run=True)

    assert result["command"] == "kresko status"
    assert result["background"] is None


def test_reset_wipes_state_and_sessions(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    captured: dict[str, str] = {}

    def runner(inventory, deploy_file, dry_run):
        captured["body"] = deploy_file.read_text()
        return subprocess.CompletedProcess([], 0, "", "")

    fleet._pyinfra_runner = runner
    result = fleet.reset(role="miner")

    assert result["ok"] is True
    assert result["stage"] == "reset"
    body = captured["body"]
    assert "rm -rf /root/.cache/zebra" in body
    assert "rm -rf /root/.cache/zakura" in body
    assert "tmux kill-session -t zebra" in body
    assert "tmux kill-session -t mine" in body


# --- down --------------------------------------------------------------------


def test_down_dry_run_validates_assets(home):
    fake = FakeDigitalOcean()
    fleet = make_fleet(home, fake)
    add_miners(fleet, count=1)
    fleet.up()

    result = fleet.down(dry_run=True)

    assert result["destroyed_provider_ids"] == ["1"]
    assert fake.deleted == []
    assert assets_store.read_asset("digitalocean", "1")["name"] == "miner-0"


# --- status / archive / shell ------------------------------------------------


def test_status_reports_ok_flag(home, monkeypatch):
    from kresko import status as status_mod

    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=1)
    fleet.up()

    monkeypatch.setattr(
        status_mod,
        "fetch_node_status",
        lambda name, ip, **kw: status_mod.NodeStatus(name=name, ip=ip, height=100),
    )
    result = fleet.status()
    assert result["ok"] is True
    assert result["reachable"] == 1


def test_archive_writes_default_tarball(home):
    fleet = Fleet("ci-xyz")
    result = fleet.archive()
    assert result["ok"] is True
    assert (paths.fleets_dir() / "ci-xyz.tar.gz").exists()
    assert result["path"].endswith("ci-xyz.tar.gz")


def test_shell_tees_into_fleet_dir(home):
    fleet = Fleet("ci-shell")
    fleet.shell(["echo", "hello"])
    log = (fleet.dir / "echo.stdout.log").read_text()
    assert "$ echo hello" in log
    assert "hello" in log


# --- override ----------------------------------------------------------------


def test_override_patches_size_for_role(home):
    fleet = make_fleet(home, FakeDigitalOcean())
    add_miners(fleet, count=2)
    fleet.add("rpc", count=1, provider=DigitalOcean(region="nyc3", size="s-1vcpu-1gb"))

    fleet.override("miner", size="s-8vcpu-16gb", count=4)

    desired = fleet._desired()
    miners = [d for d in desired if d.role == "miner"]
    rpc = [d for d in desired if d.role == "rpc"]
    assert len(miners) == 4
    assert all(d.size == "s-8vcpu-16gb" for d in miners)
    assert len(rpc) == 1
    assert rpc[0].size == "s-1vcpu-1gb"


def test_run_pyinfra_passes_y_flag(monkeypatch):
    """pyinfra -y is required so deploy doesn't hang on a yes/no prompt."""
    captured: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        captured.append(list(cmd))
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)
    run_pyinfra(paths.kresko_home() / "inv.py", paths.kresko_home() / "deploy.py")

    assert captured and captured[0][:2] == ["pyinfra", "-y"]
