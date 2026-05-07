from __future__ import annotations

import io
import json
import sys
import textwrap

import pytest

from kresko_py import assets, paths
from kresko_py.cli import main, run_experiment


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def make_asset(home, **overrides):
    base = {
        "provider": "digitalocean",
        "provider_id": "1",
        "name": "miner-0",
        "role": "miner",
        "experiment": "smoke",
        "run": "smoke",
        "public_ip": "203.0.113.1",
        "status": "active",
        "tags": ["kresko", "kresko-exp-smoke", "kresko-role-miner", "kresko-run-smoke"],
    }
    base.update(overrides)
    assets.write_asset(base)


def test_assets_list_filters_by_tag(home, capsys):
    make_asset(home, provider_id="1")
    make_asset(
        home,
        provider_id="2",
        name="rpc-0",
        role="rpc",
        tags=["kresko", "kresko-exp-smoke", "kresko-role-rpc", "kresko-run-smoke"],
    )

    rc = main(["assets", "list", "--tag", "kresko-role-miner"])
    captured = capsys.readouterr()

    assert rc == 0
    out = json.loads(captured.out)
    assert [a["provider_id"] for a in out] == ["1"]


def test_assets_show_outputs_full_asset(home, capsys):
    make_asset(home)
    rc = main(["assets", "show", "digitalocean", "1"])
    captured = capsys.readouterr()

    assert rc == 0
    asset = json.loads(captured.out)
    assert asset["name"] == "miner-0"
    assert asset["public_ip"] == "203.0.113.1"


def test_runs_list_returns_empty_when_no_runs(home, capsys):
    rc = main(["runs", "list", "missing"])
    captured = capsys.readouterr()
    assert rc == 0
    assert json.loads(captured.out) == []


class StubExperiment:
    """Records dispatched method calls for run_experiment tests."""

    def __init__(self):
        self.calls: list[tuple[str, dict]] = []

    def _record(self, _stage, **kwargs):
        self.calls.append((_stage, kwargs))
        return {"stage": _stage, "ok": True, **kwargs}

    def plan(self): return self._record("plan")
    def up(self, **kw): return self._record("up", **kw)
    def deploy(self, **kw): return self._record("deploy", **kw)
    def run_command(self, command, **kw): return self._record("run", command=command, **kw)
    def collect(self, paths_to_collect, **kw):
        return self._record("collect", _paths=paths_to_collect, **kw)
    def down(self, **kw): return self._record("down", **kw)


def test_run_experiment_dispatches_default_verbs(capsys):
    stub = StubExperiment()
    rc = run_experiment(lambda: stub, argv=["up", "--retry-failed"])
    out = json.loads(capsys.readouterr().out)
    assert rc == 0
    assert stub.calls == [("up", {"dry_run": False, "retry_failed": True})]
    assert out["stage"] == "up"


def test_run_experiment_passes_filters_to_deploy(capsys):
    stub = StubExperiment()
    rc = run_experiment(
        lambda: stub,
        argv=["deploy", "--role", "miner", "--pattern", "miner-*", "--dry-run"],
    )
    capsys.readouterr()
    assert rc == 0
    name, kwargs = stub.calls[0]
    assert name == "deploy"
    assert kwargs["role"] == ["miner"]
    assert kwargs["pattern"] == ["miner-*"]
    assert kwargs["dry_run"] is True


def test_run_experiment_extra_action(capsys):
    stub = StubExperiment()
    seen = {}

    def smoke(exp, args):
        seen["called"] = True
        seen["role"] = args.role
        return {"stage": "smoke", "ok": True}

    rc = run_experiment(
        lambda: stub,
        extra_actions={"smoke": smoke},
        argv=["smoke", "--role", "miner"],
    )
    capsys.readouterr()
    assert rc == 0
    assert seen == {"called": True, "role": ["miner"]}


def test_run_experiment_returns_nonzero_when_not_ok(capsys):
    class FailExperiment(StubExperiment):
        def up(self, **kw):
            return {"stage": "up", "ok": False, "failed": [{"name": "miner-0"}]}

    rc = run_experiment(lambda: FailExperiment(), argv=["up"])
    capsys.readouterr()
    assert rc == 1


def test_run_experiment_collect_requires_path(capsys):
    stub = StubExperiment()
    with pytest.raises(SystemExit):
        run_experiment(lambda: stub, argv=["collect"])


def test_run_executes_copied_run_py(home, capsys):
    src = paths.experiment_dir("hello")
    src.mkdir(parents=True)
    (src / "run.py").write_text(
        textwrap.dedent(
            """\
            import os, json
            print(json.dumps({
                "experiment": os.environ["KRESKO_EXPERIMENT"],
                "run_name": os.environ["KRESKO_RUN_NAME"],
                "cwd_is_run_dir": os.getcwd() == os.environ["KRESKO_RUN_DIR"],
            }))
            """
        ),
        encoding="utf-8",
    )

    rc = main(["run", "hello"])
    captured = capsys.readouterr()

    assert rc == 0, captured.err
    line = next(line for line in captured.out.splitlines() if line.startswith("{"))
    parsed = json.loads(line)
    assert parsed == {"experiment": "hello", "run_name": "hello", "cwd_is_run_dir": True}

    # Output is also tee'd into the run dir.
    run_dir = paths.run_dir("hello", "hello")
    assert (run_dir / "stdout.log").read_text().strip().startswith("{")
