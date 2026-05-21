from __future__ import annotations

import json
import subprocess
import tarfile

import pytest

from harness import Experiment, explorer, paths
from harness.experiment import ENV_EXPERIMENT, ENV_RUN_DIR, ENV_RUN_NAME
from harness.explorer import ExplorerDeployment, ExplorerSpec
from harness.runs import start_run


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def make_experiment(monkeypatch, name="exp", run="exp") -> Experiment:
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("# placeholder\n", encoding="utf-8")
    run_path = start_run(name, name=run)
    monkeypatch.setenv(ENV_EXPERIMENT, name)
    monkeypatch.setenv(ENV_RUN_NAME, run_path.name)
    monkeypatch.setenv(ENV_RUN_DIR, str(run_path))
    return Experiment.current(ssh={"user": "root", "key_path": ""})


def make_source(tmp_path):
    """A minimal explorer source tree that passes ExplorerSpec.validate()."""
    source = tmp_path / "zcash-explorer"
    source.mkdir()
    for name in ("docker-compose.yml", "Dockerfile", "mix.exs"):
        (source / name).write_text("x\n", encoding="utf-8")
    return source


class FakeRunner:
    """Stand-in for explorer.CommandRunner that records steps instead of shelling out."""

    def __init__(self, *, http_status="200", stdout_tails=None):
        self.http_status = http_status
        self.stdout_tails = stdout_tails or {}
        self.steps: list[dict] = []

    def run(self, command, log_name, *, input_text=None):
        self.steps.append(
            {"command": list(command), "log_name": log_name, "input_text": input_text}
        )
        return {
            "name": log_name,
            "ok": True,
            "returncode": 0,
            "stdout_path": None,
            "stderr_path": None,
        }

    def read_stdout_tail(self, log_name):
        if log_name in self.stdout_tails:
            return self.stdout_tails[log_name]
        return self.http_status if log_name == "explorer-http-check" else ""

    def joined(self):
        return ["  ".join(step["command"]) for step in self.steps]


def fake_s3_runner():
    calls: list[list[str]] = []

    def runner(cmd):
        calls.append(list(cmd))
        if cmd[2] == "presign":
            return subprocess.CompletedProcess(cmd, 0, "https://s3.example/key?sig=abc", "")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    runner.calls = calls  # type: ignore[attr-defined]
    return runner


# --- ExplorerSpec.create -----------------------------------------------------


def test_spec_testnet_defaults():
    spec = ExplorerSpec.create(node="miner-0", env={})
    assert spec.network == "testnet"
    assert spec.compose_service == "explorer-testnet"
    assert spec.public_port == 20001
    assert spec.container_port == 4000
    assert spec.rpc_port == 18232
    assert spec.role == "miner"
    assert str(spec.source).endswith("zcash-explorer")


def test_spec_mainnet_defaults():
    spec = ExplorerSpec.create(node="miner-0", network="mainnet", env={})
    assert spec.compose_service == "explorer-mainnet"
    assert spec.public_port == 20000
    assert spec.rpc_port == 8232


def test_spec_unknown_network_raises():
    with pytest.raises(ValueError, match="unknown explorer network"):
        ExplorerSpec.create(network="regtest", env={})


def test_spec_env_overrides_defaults():
    spec = ExplorerSpec.create(
        env={
            "KRESKO_EXPLORER_NODE": "miner-2",
            "KRESKO_EXPLORER_SOURCE": "/srv/explorer",
            "KRESKO_EXPLORER_PORT": "21001",
        }
    )
    assert spec.node == "miner-2"
    assert str(spec.source) == "/srv/explorer"
    assert spec.public_port == 21001


def test_spec_faucet_env_overrides_defaults():
    spec = ExplorerSpec.create(
        env={
            "KRESKO_EXPLORER_FAUCET_ENABLED": "true",
            "KRESKO_EXPLORER_FAUCET_SOURCE_ADDRESS": "tmSource",
            "KRESKO_EXPLORER_FAUCET_AMOUNT": "0.25",
            "KRESKO_EXPLORER_FAUCET_DAILY_IP_LIMIT": "7",
            "KRESKO_EXPLORER_FAUCET_WINDOW_SECONDS": "3600",
            "KRESKO_EXPLORER_FAUCET_MIN_CONFIRMATIONS": "2",
        }
    )
    assert spec.faucet_enabled is True
    assert spec.faucet_source_address == "tmSource"
    assert spec.faucet_amount == "0.25"
    assert spec.faucet_daily_ip_limit == 7
    assert spec.faucet_window_seconds == 3600
    assert spec.faucet_min_confirmations == 2


def test_spec_rejects_mainnet_faucet():
    with pytest.raises(ValueError, match="faucet can only be enabled on testnet"):
        ExplorerSpec.create(network="mainnet", faucet_enabled=True, env={})


def test_spec_explicit_kwarg_beats_env():
    spec = ExplorerSpec.create(node="miner-3", env={"KRESKO_EXPLORER_NODE": "miner-9"})
    assert spec.node == "miner-3"


# --- pure builders -----------------------------------------------------------


def test_render_env_testnet():
    spec = ExplorerSpec.create(node="miner-0", env={})
    text = explorer.render_env(spec, "203.0.113.7")
    assert "ZCASH_NETWORK=testnet" in text
    assert "ZCASHD_PORT=18232" in text
    assert "EXPLORER_PORT=20001" in text
    assert "EXPLORER_HOSTNAME=203.0.113.7" in text
    assert "LIGHTWALLETD_ENABLED=false" in text
    assert "FAUCET_ENABLED=false" in text
    assert "FAUCET_SOURCE_ADDRESS=" not in text
    assert "TESTNET_SECRET_KEY_BASE=" in text
    assert "MAINNET_SECRET_KEY_BASE=" not in text


def test_render_env_includes_enabled_faucet_vars():
    spec = ExplorerSpec.create(
        node="miner-0",
        faucet_enabled=True,
        faucet_amount="0.25",
        faucet_daily_ip_limit=7,
        faucet_window_seconds=3600,
        faucet_min_confirmations=2,
        env={},
    )
    text = explorer.render_env(spec, "203.0.113.7", "tmSource")
    assert "FAUCET_ENABLED=true" in text
    assert "FAUCET_SOURCE_ADDRESS=tmSource" in text
    assert "FAUCET_AMOUNT=0.25" in text
    assert "FAUCET_DAILY_IP_LIMIT=7" in text
    assert "FAUCET_WINDOW_SECONDS=3600" in text
    assert "FAUCET_MIN_CONFIRMATIONS=2" in text


def test_render_env_enabled_faucet_requires_source_address():
    spec = ExplorerSpec.create(node="miner-0", faucet_enabled=True, env={})
    with pytest.raises(ValueError, match="no source address"):
        explorer.render_env(spec, "203.0.113.7")


def test_render_env_mainnet():
    spec = ExplorerSpec.create(node="miner-0", network="mainnet", env={})
    text = explorer.render_env(spec, "203.0.113.7")
    assert "ZCASH_NETWORK=mainnet" in text
    assert "ZCASHD_PORT=8232" in text
    assert "MAINNET_SECRET_KEY_BASE=" in text


def test_build_source_archive_excludes_build_outputs(tmp_path):
    source = make_source(tmp_path)
    (source / "lib").mkdir()
    (source / "lib" / "app.ex").write_text("code\n", encoding="utf-8")
    (source / ".git").mkdir()
    (source / ".git" / "HEAD").write_text("ref\n", encoding="utf-8")
    (source / "_build").mkdir()
    (source / "_build" / "junk.beam").write_text("junk\n", encoding="utf-8")
    (source / "deps").mkdir()
    (source / "deps" / "x.ex").write_text("dep\n", encoding="utf-8")

    archive = tmp_path / "src.tar.gz"
    explorer.build_source_archive(source, archive)

    with tarfile.open(archive) as tar:
        names = set(tar.getnames())
    assert "lib/app.ex" in names
    assert "mix.exs" in names
    assert not any(n.startswith(".git") for n in names)
    assert not any(n.startswith("_build") for n in names)
    assert not any(n.startswith("deps") for n in names)


def test_remote_fetch_command_curls_presigned_url():
    spec = ExplorerSpec.create(node="miner-0", env={})
    cmd = explorer.remote_fetch_command(spec, "https://s3.example/key?sig=abc")
    assert "curl -fsSL" in cmd
    assert "s3.example/key" in cmd
    assert "tar -xzf" in cmd
    # The existing .env on the node must survive a source refresh.
    assert "! -name .env" in cmd


def test_remote_rpc_check_uses_network_port():
    spec = ExplorerSpec.create(node="miner-0", env={})
    cmd = explorer.remote_rpc_check_command(spec)
    assert "127.0.0.1:18232" in cmd
    assert "getblockchaininfo" in cmd


def test_remote_faucet_source_address_reads_funded_key_then_config():
    cmd = explorer.remote_faucet_source_address_command()
    assert "/root/.config/funded_key.json" in cmd
    assert "jq -r '.address // empty'" in cmd
    assert "/root/.config/zebrad.toml" in cmd
    assert "kresko config get-miner-address" in cmd


def test_remote_faucet_rpc_check_requires_wallet_methods():
    spec = ExplorerSpec.create(node="miner-0", faucet_enabled=True, env={})
    cmd = explorer.remote_faucet_rpc_check_command(spec, "tmSource")
    assert "127.0.0.1:18232" in cmd
    assert "validateaddress" in cmd
    assert "z_sendmany" in cmd
    assert "faucet RPC check failed" in cmd


def test_remote_compose_up_targets_service():
    spec = ExplorerSpec.create(node="miner-0", env={})
    cmd = explorer.remote_compose_up_command(spec)
    assert "docker compose up -d --build explorer-testnet" in cmd


def test_validate_rejects_wrong_port(tmp_path):
    source = make_source(tmp_path)
    spec = ExplorerSpec.create(node="miner-0", source=source, public_port=1234, env={})
    with pytest.raises(RuntimeError, match="docker-compose.yml maps"):
        spec.validate()


def test_validate_rejects_missing_source(tmp_path):
    spec = ExplorerSpec.create(node="miner-0", source=tmp_path / "nope", env={})
    with pytest.raises(FileNotFoundError):
        spec.validate()


# --- target selection --------------------------------------------------------


def _assets():
    return [
        {"name": "miner-0", "role": "miner", "status": "active", "public_ip": "203.0.113.1"},
        {"name": "miner-1", "role": "miner", "status": "active", "public_ip": "203.0.113.2"},
        {"name": "miner-2", "role": "miner", "status": "failed", "public_ip": "203.0.113.3"},
    ]


def test_target_asset_by_name(home, monkeypatch):
    exp = make_experiment(monkeypatch)
    exp.add_explorer(node="miner-1")
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())
    deployment = ExplorerDeployment(exp, exp._explorer, runner=FakeRunner())
    assert deployment._target_asset()["name"] == "miner-1"


def test_target_asset_missing_raises(home, monkeypatch):
    exp = make_experiment(monkeypatch)
    exp.add_explorer(node="miner-9")
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())
    deployment = ExplorerDeployment(exp, exp._explorer, runner=FakeRunner())
    with pytest.raises(RuntimeError, match="miner-9"):
        deployment._target_asset()


def test_target_asset_first_when_node_unset(home, monkeypatch):
    exp = make_experiment(monkeypatch)
    spec = ExplorerSpec.create(node="", env={})  # falls back to "miner-0" via create
    spec = explorer.ExplorerSpec(**{**spec.__dict__, "node": ""})  # force unset
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())
    deployment = ExplorerDeployment(exp, spec, runner=FakeRunner())
    assert deployment._target_asset()["name"] == "miner-0"


# --- Experiment integration --------------------------------------------------


def test_deploy_explorer_noop_when_unconfigured(home, monkeypatch):
    exp = make_experiment(monkeypatch)
    result = exp.deploy_explorer()
    assert result["ok"] is True
    assert result["skipped"] is True


def test_full_deploy_uses_s3_and_curl(home, monkeypatch, tmp_path):
    exp = make_experiment(monkeypatch)
    source = make_source(tmp_path)
    exp.add_explorer(node="miner-0", source=source)
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())

    runner = FakeRunner(http_status="200")
    s3_runner = fake_s3_runner()
    deployment = ExplorerDeployment(exp, exp._explorer, runner=runner, s3_runner=s3_runner)

    result = deployment.deploy()

    assert result["ok"] is True
    assert result["url"] == "http://203.0.113.1:20001"

    # Source was delivered via S3 (cp + presign), never scp.
    assert [c[2] for c in s3_runner.calls] == ["cp", "presign"]
    all_commands = "\n".join(runner.joined())
    assert "scp" not in all_commands
    # The node fetches the presigned URL with curl.
    fetch = next(s for s in runner.steps if s["log_name"] == "explorer-fetch-source")
    assert "s3.example/key" in "  ".join(fetch["command"])

    # The .env is written over stdin (not scp), and carries the testnet secret.
    write_env = next(s for s in runner.steps if s["log_name"] == "explorer-write-env")
    assert write_env["input_text"] is not None
    assert "TESTNET_SECRET_KEY_BASE=" in write_env["input_text"]
    assert "ZCASHD_PORT=18232" in write_env["input_text"]

    # Metadata + result are persisted in the run dir.
    meta = json.loads((exp.run_dir / "explorer.json").read_text())
    assert meta["status"] == "running"
    assert meta["url"] == "http://203.0.113.1:20001"
    assert json.loads((exp.run_dir / "result.json").read_text())["ok"] is True


def test_full_deploy_writes_faucet_env_from_remote_source_address(home, monkeypatch, tmp_path):
    exp = make_experiment(monkeypatch)
    source = make_source(tmp_path)
    exp.add_explorer(node="miner-0", source=source, faucet_enabled=True)
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())

    runner = FakeRunner(
        http_status="200", stdout_tails={"explorer-faucet-source": "tmDiscovered"}
    )
    deployment = ExplorerDeployment(exp, exp._explorer, runner=runner, s3_runner=fake_s3_runner())

    result = deployment.deploy()

    assert result["ok"] is True
    assert result["faucet_source_address"] == "tmDiscovered"
    assert any(s["log_name"] == "explorer-faucet-source" for s in runner.steps)
    assert any(s["log_name"] == "explorer-faucet-rpc-check" for s in runner.steps)
    write_env = next(s for s in runner.steps if s["log_name"] == "explorer-write-env")
    assert "FAUCET_ENABLED=true" in write_env["input_text"]
    assert "FAUCET_SOURCE_ADDRESS=tmDiscovered" in write_env["input_text"]
    assert "FAUCET_AMOUNT=0.1" in write_env["input_text"]


def test_deploy_fails_when_step_fails(home, monkeypatch, tmp_path):
    exp = make_experiment(monkeypatch)
    source = make_source(tmp_path)
    exp.add_explorer(node="miner-0", source=source)
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())

    class FailingRunner(FakeRunner):
        def run(self, command, log_name, *, input_text=None):
            step = super().run(command, log_name, input_text=input_text)
            if log_name == "explorer-compose-up":
                step["ok"] = False
                step["returncode"] = 1
            return step

    deployment = ExplorerDeployment(
        exp, exp._explorer, runner=FailingRunner(), s3_runner=fake_s3_runner()
    )
    result = deployment.deploy()

    assert result["ok"] is False
    assert json.loads((exp.run_dir / "explorer.json").read_text())["status"] == "error"
    res = json.loads((exp.run_dir / "result.json").read_text())
    assert res["ok"] is False
    assert any(f["command"] == "explorer-compose-up" for f in res["failures"])


def test_dry_run_skips_remote_work(home, monkeypatch, tmp_path):
    exp = make_experiment(monkeypatch)
    source = make_source(tmp_path)
    exp.add_explorer(node="miner-0", source=source)
    monkeypatch.setattr(exp, "run_assets", lambda: _assets())

    runner = FakeRunner()
    deployment = ExplorerDeployment(exp, exp._explorer, runner=runner)
    result = deployment.deploy(dry_run=True)

    assert result["ok"] is True
    assert result["dry_run"] is True
    assert runner.steps == []  # no ssh/aws calls
