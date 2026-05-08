from __future__ import annotations

import json
import subprocess

import pytest

from harness import Experiment, NodeType, paths
from harness import assets as assets_store
from harness.experiment import ENV_EXPERIMENT, ENV_RUN_DIR, ENV_RUN_NAME, run_pyinfra
from harness.providers import DigitalOceanError, DigitalOceanProvider
from harness.runs import start_run


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


def miner_type() -> NodeType:
    return NodeType(
        provider="digitalocean",
        role="miner",
        region="nyc3",
        size="s-1vcpu-1gb",
        image="ubuntu-24-04-x64",
        payload_paths=["payload"],
        tags=["suite"],
    )


def fake_provider(fake: FakeDigitalOcean) -> dict[str, DigitalOceanProvider]:
    return {"digitalocean": DigitalOceanProvider(fake)}


def make_experiment(home, monkeypatch, name="api-exp", run="api-exp", **kwargs) -> Experiment:
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("# placeholder\n", encoding="utf-8")
    (src / "payload").mkdir(exist_ok=True)
    run_path = start_run(name, name=run)
    monkeypatch.setenv(ENV_EXPERIMENT, name)
    monkeypatch.setenv(ENV_RUN_NAME, run_path.name)
    monkeypatch.setenv(ENV_RUN_DIR, str(run_path))
    return Experiment.current(**kwargs)


def test_experiment_plan_creates_no_assets_but_writes_result(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=2)

    result = experiment.plan()

    assert result["ok"] is True
    assert [a["name"] for a in result["plan"]["create"]] == ["miner-0", "miner-1"]
    assert fake.created == []
    assert (experiment.run_dir / "result.json").exists()
    assert assets_store.list_assets() == []


def test_experiment_up_writes_assets_and_node_snapshots(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)

    result = experiment.up()

    assert result["ok"] is True
    asset = assets_store.read_asset("digitalocean", "1")
    assert asset["name"] == "miner-0"
    assert "experiment-api-exp" in asset["tags"]
    assert "role-miner" in asset["tags"]
    assert "run-api-exp" in asset["tags"]
    snapshot = experiment.run_dir / "nodes" / "miner-0.json"
    assert snapshot.exists()


def test_experiment_deploy_dry_run_writes_pyinfra_files(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()
    calls = []

    def runner(inventory, deploy_file, dry_run):
        calls.append((inventory, deploy_file, dry_run))
        return subprocess.CompletedProcess([], 0, "", "")

    experiment._pyinfra_runner = runner

    result = experiment.deploy(role="miner", dry_run=True)

    assert result["ok"] is True
    assert result["nodes"] == ["miner-0"]
    assert calls == []
    assert (experiment.run_dir / "inventory.py").exists()
    assert (experiment.run_dir / "deploy_payload.py").exists()


def test_experiment_run_tmux_uses_runner_hook(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()
    calls = []

    def runner(inventory, deploy_file, dry_run):
        calls.append((inventory, deploy_file, dry_run))
        return subprocess.CompletedProcess(["pyinfra"], 0, "ok", "")

    experiment._pyinfra_runner = runner

    result = experiment.run_tmux("smoke", "date", role="miner", log_path="/root/smoke.log")

    assert result["ok"] is True
    assert len(calls) == 1
    assert "tmux new-session" in result["command"]
    assert (experiment.run_dir / "pyinfra.run.stdout.log").read_text() == "ok"


def test_experiment_reset_dispatches_to_remote_command(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()

    captured: dict[str, str] = {}

    def runner(inventory, deploy_file, dry_run):
        captured["body"] = deploy_file.read_text()
        return subprocess.CompletedProcess([], 0, "", "")

    experiment._pyinfra_runner = runner
    result = experiment.reset(role="miner")

    assert result["ok"] is True
    assert result["stage"] == "reset"
    body = captured["body"]
    # The remote command must wipe state, configs, logs, and known tmux sessions.
    assert "rm -rf /root/.cache/zebra" in body
    assert "rm -rf /root/logs" in body
    assert "tmux kill-session -t zebra" in body
    assert "tmux kill-session -t mine" in body


def test_experiment_down_dry_run_validates_assets(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()
    fake.droplets_by_id["1"]["tags"] = [
        "kresko",
        "experiment-api-exp",
        "role-miner",
        "run-api-exp",
    ]

    result = experiment.down(dry_run=True)

    assert result["destroyed_provider_ids"] == ["1"]
    assert fake.deleted == []
    assert assets_store.read_asset("digitalocean", "1")["name"] == "miner-0"


def test_experiment_shell_tees_into_run_dir(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(home, monkeypatch, providers=fake_provider(fake))

    experiment.shell(["echo", "hello"])

    log = (experiment.run_dir / "echo.stdout.log").read_text()
    assert "$ echo hello" in log
    assert "hello" in log


def test_experiment_up_returns_partial_success_on_create_failure(home, monkeypatch):
    class CapacityFake(FakeDigitalOcean):
        def create_droplet(self, request):
            if request["name"] == "miner-1":
                raise DigitalOceanError("no capacity in nyc3")
            return super().create_droplet(request)

    fake = CapacityFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=2)

    result = experiment.up()

    assert result["ok"] is False
    assert result["requested"] == 2
    assert result["succeeded"] == 1
    assert len(result["failed"]) == 1
    assert result["failed"][0]["name"] == "miner-1"
    assert result["failed"][0]["kind"] == "create"
    # Result file records the failure.
    res = json.loads((experiment.run_dir / "result.json").read_text())
    assert res["ok"] is False
    assert res["failures"][0]["node"] == "miner-1"


def test_experiment_deploy_skips_failed_nodes(home, monkeypatch):
    class TimeoutFake(FakeDigitalOcean):
        def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
            if str(droplet_id) == "1":
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    fake = TimeoutFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=2)
    up = experiment.up()
    assert up["succeeded"] == 1

    def runner(inventory, deploy_file, dry_run):
        return subprocess.CompletedProcess([], 0, "", "")

    experiment._pyinfra_runner = runner
    deploy = experiment.deploy(role="miner", dry_run=True)

    # The failed node is gone from the deploy targets.
    assert deploy["nodes"] == ["miner-1"]


def test_experiment_up_retry_failed_clears_marker(home, monkeypatch):
    state = {"timeout_for": {"1"}}

    class FlakyFake(FakeDigitalOcean):
        def wait_for_ips(self, droplet_id, attempts=60, delay_secs=5.0):
            if str(droplet_id) in state["timeout_for"]:
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    fake = FlakyFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    first = experiment.up()
    assert first["ok"] is False
    assert assets_store.read_asset("digitalocean", "1")["status"] == "failed"

    # Cloud is healthy on the second attempt.
    state["timeout_for"] = set()
    second = experiment.up(retry_failed=True)
    assert second["ok"] is True
    asset = assets_store.read_asset("digitalocean", "1")
    assert asset["status"] == "active"
    assert "failure_reason" not in asset


def test_override_patches_size_image_region_for_role(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(home, monkeypatch, providers=fake_provider(fake))
    experiment.add(miner_type(), count=2)

    experiment.override("miner", size="s-8vcpu-16gb", image="ubuntu-25-04-x64", region="ams3")

    spec = experiment.spec()
    miner_group = next(g for g in spec.node_groups if g.role == "miner")
    assert miner_group.size == "s-8vcpu-16gb"
    assert miner_group.image == "ubuntu-25-04-x64"
    assert miner_group.region == "ams3"
    assert miner_group.count == 2


def test_override_patches_count(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(home, monkeypatch, providers=fake_provider(fake))
    experiment.add(miner_type(), count=4)

    experiment.override("miner", count=8)

    miner_group = next(g for g in experiment.spec().node_groups if g.role == "miner")
    assert miner_group.count == 8


def test_override_skips_other_roles(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(home, monkeypatch, providers=fake_provider(fake))
    experiment.add(miner_type(), count=1)
    rpc = NodeType(
        provider="digitalocean",
        role="rpc",
        region="nyc3",
        size="s-1vcpu-1gb",
        image="ubuntu-24-04-x64",
        payload_paths=["payload"],
    )
    experiment.add(rpc, count=2)

    experiment.override("miner", size="s-99vcpu")

    by_role = {g.role: g for g in experiment.spec().node_groups}
    assert by_role["miner"].size == "s-99vcpu"
    assert by_role["rpc"].size == "s-1vcpu-1gb"


def test_deploy_records_local_binary_provenance_from_payload_manifest(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()

    # Stage a payload-style build dir under the experiment payload path.
    payload_root = experiment.run_dir / "payload"
    build_dir = payload_root / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    (build_dir / "zebrad").write_bytes(b"fake-zebrad-bytes")
    (build_dir / "kresko").write_bytes(b"fake-kresko-bytes")
    # Manifest hashes match the staged files (provenance OK).
    import hashlib
    z_sha = hashlib.sha256(b"fake-zebrad-bytes").hexdigest()
    k_sha = hashlib.sha256(b"fake-kresko-bytes").hexdigest()
    (build_dir / "manifest.txt").write_text(
        f"zebrad_sha256={z_sha}\nkresko_sha256={k_sha}\n"
        f"zebrad_source=/build/zebra\nkresko_source=/build/kresko\n",
        encoding="utf-8",
    )

    experiment._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess([], 0, "", "")
    result = experiment.deploy(role="miner")

    binaries = result["binary_provenance_local"]["binaries"]
    assert binaries["zebrad"]["manifest_sha256"] == z_sha
    assert binaries["zebrad"]["staged_sha256"] == z_sha
    assert binaries["kresko"]["manifest_sha256"] == k_sha
    # Source paths from the manifest are surfaced so the deploy log says where
    # the bytes came from.
    assert binaries["zebrad"]["source"] == "/build/zebra"


def test_deploy_parses_remote_provenance_lines(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()

    fake_stdout = (
        "[miner-0] >>> Step: server.shell\n"
        "[miner-0] PROVENANCE: zebrad CHANGED (was=aaaa, now=bbbb)\n"
        "[miner-0] PROVENANCE: kresko unchanged (sha256=cccc)\n"
        "[miner-0] >>> done\n"
    )
    experiment._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess(
        [], 0, fake_stdout, ""
    )
    result = experiment.deploy(role="miner")

    remote = result["binary_provenance_remote"]
    assert any(b["binary"] == "zebrad" for b in remote["changed"])
    assert any(b["binary"] == "kresko" for b in remote["unchanged"])
    assert remote["installed"] == []


def test_deploy_provenance_buckets_first_install(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, providers=fake_provider(fake)
    )
    experiment.add(miner_type(), count=1)
    experiment.up()

    experiment._pyinfra_runner = lambda i, d, dr: subprocess.CompletedProcess(
        [],
        0,
        "[miner-0] PROVENANCE: zebrad installed (sha256=abcd, no previous binary)\n",
        "",
    )
    result = experiment.deploy(role="miner")

    assert result["binary_provenance_remote"]["installed"][0]["binary"] == "zebrad"


def test_run_pyinfra_passes_y_flag(monkeypatch):
    """pyinfra -y is required so deploy doesn't hang on a yes/no prompt
    in non-interactive runs (CI, kresko orchestration, etc.)."""
    captured: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        captured.append(list(cmd))
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    run_pyinfra(paths.kresko_home() / "inv.py", paths.kresko_home() / "deploy.py")

    assert captured, "subprocess.run was not called"
    assert captured[0][:2] == ["pyinfra", "-y"]


def test_run_dir_contains_copied_experiment_source(home):
    name = "copied-exp"
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("print('hi')\n", encoding="utf-8")
    (src / "payload").mkdir()
    (src / "payload" / "thing.txt").write_text("payload\n", encoding="utf-8")

    run_path = start_run(name, name="seed")

    assert (run_path / "run.py").read_text() == "print('hi')\n"
    assert (run_path / "payload" / "thing.txt").read_text() == "payload\n"
    assert (run_path / "manifest.json").exists()
    assert json.loads((run_path / "manifest.json").read_text())["run_name"] == "seed"
