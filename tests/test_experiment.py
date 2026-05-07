from __future__ import annotations

import json
import subprocess

import pytest

from kresko_py import DigitalOceanNodeType, Experiment, paths
from kresko_py import assets as assets_store
from kresko_py.experiment import ENV_EXPERIMENT, ENV_RUN_DIR, ENV_RUN_NAME
from kresko_py.runs import start_run


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

    def wait_for_ips(self, droplet_id):
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


def miner_type() -> DigitalOceanNodeType:
    return DigitalOceanNodeType(
        role="miner",
        region="nyc3",
        size="s-1vcpu-1gb",
        image="ubuntu-24-04-x64",
        payload_paths=["payload"],
        tags=["suite"],
    )


def make_experiment(home, monkeypatch, name="api-exp", run="api-exp", **kwargs) -> Experiment:
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("# placeholder\n", encoding="utf-8")
    (src / "payload").mkdir(exist_ok=True)
    run_path = start_run(name)
    monkeypatch.setenv(ENV_EXPERIMENT, name)
    monkeypatch.setenv(ENV_RUN_NAME, run_path.name)
    monkeypatch.setenv(ENV_RUN_DIR, str(run_path))
    return Experiment.current(**kwargs)


def test_experiment_plan_creates_no_assets_but_writes_result(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
    )
    experiment.add(miner_type(), count=1)

    result = experiment.up()

    assert result["ok"] is True
    asset = assets_store.read_asset("digitalocean", "1")
    assert asset["name"] == "miner-0"
    assert "kresko-exp-api-exp" in asset["tags"]
    assert "kresko-role-miner" in asset["tags"]
    assert "kresko-run-api-exp" in asset["tags"]
    snapshot = experiment.run_dir / "nodes" / "miner-0.json"
    assert snapshot.exists()


def test_experiment_deploy_dry_run_writes_pyinfra_files(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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


def test_experiment_down_dry_run_validates_assets(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
    )
    experiment.add(miner_type(), count=1)
    experiment.up()
    fake.droplets_by_id["1"]["tags"] = [
        "kresko",
        "kresko-exp-api-exp",
        "kresko-role-miner",
        "kresko-run-api-exp",
    ]

    result = experiment.down(dry_run=True)

    assert result["destroyed_provider_ids"] == ["1"]
    assert fake.deleted == []
    assert assets_store.read_asset("digitalocean", "1")["name"] == "miner-0"


def test_experiment_shell_tees_into_run_dir(home, monkeypatch):
    fake = FakeDigitalOcean()
    experiment = make_experiment(home, monkeypatch, digitalocean_client=fake)

    experiment.shell(["echo", "hello"])

    log = (experiment.run_dir / "echo.stdout.log").read_text()
    assert "$ echo hello" in log
    assert "hello" in log


def test_experiment_up_returns_partial_success_on_create_failure(home, monkeypatch):
    from kresko_py.digitalocean import DigitalOceanError

    class CapacityFake(FakeDigitalOcean):
        def create_droplet(self, request):
            if request["name"] == "miner-1":
                raise DigitalOceanError("no capacity in nyc3")
            return super().create_droplet(request)

    fake = CapacityFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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
    from kresko_py.digitalocean import DigitalOceanError

    class TimeoutFake(FakeDigitalOcean):
        def wait_for_ips(self, droplet_id):
            if str(droplet_id) == "1":
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    fake = TimeoutFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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
    from kresko_py.digitalocean import DigitalOceanError

    state = {"timeout_for": {"1"}}

    class FlakyFake(FakeDigitalOcean):
        def wait_for_ips(self, droplet_id):
            if str(droplet_id) in state["timeout_for"]:
                raise DigitalOceanError(f"timed out waiting for droplet {droplet_id} IP")
            return super().wait_for_ips(droplet_id)

    fake = FlakyFake()
    experiment = make_experiment(
        home, monkeypatch, ssh={"key_name": "kresko-key"}, digitalocean_client=fake
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


def test_run_dir_contains_copied_experiment_source(home):
    name = "copied-exp"
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("print('hi')\n", encoding="utf-8")
    (src / "payload").mkdir()
    (src / "payload" / "thing.txt").write_text("payload\n", encoding="utf-8")

    run_path = start_run(name)

    assert (run_path / "run.py").read_text() == "print('hi')\n"
    assert (run_path / "payload" / "thing.txt").read_text() == "payload\n"
    assert (run_path / "manifest.json").exists()
    assert json.loads((run_path / "manifest.json").read_text())["run_name"] == name
