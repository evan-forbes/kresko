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
        "tags": ["kresko", "experiment-smoke", "role-miner", "run-smoke"],
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
        tags=["kresko", "experiment-smoke", "role-rpc", "run-smoke"],
    )

    rc = main(["assets", "list", "--tag", "role-miner"])
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


def test_run_experiment_applies_provider_overrides(capsys):
    """--size/--image/--count/--region flags must reach Experiment.override."""
    overrides: list[tuple] = []

    class OverridableStub(StubExperiment):
        _node_specs = [(type("N", (), {"role": "miner"})(), 1)]

        def override(self, role=None, **kw):
            overrides.append((role, kw))

    rc = run_experiment(
        lambda: OverridableStub(),
        argv=["up", "--size", "miner=s-8vcpu-16gb", "--count", "miner=8", "--region", "ams3"],
    )
    capsys.readouterr()
    assert rc == 0
    # --size and --count are role-scoped; bare --region applies to all roles (None).
    assert ("miner", {"size": "s-8vcpu-16gb"}) in overrides
    assert (None, {"region": "ams3"}) in overrides
    assert ("miner", {"count": 8}) in overrides


def test_run_experiment_rejects_unknown_role_in_overrides(capsys):
    class OverridableStub(StubExperiment):
        _node_specs = [(type("N", (), {"role": "miner"})(), 1)]

        def override(self, role=None, **kw):
            pass

    with pytest.raises(SystemExit):
        run_experiment(lambda: OverridableStub(), argv=["up", "--size", "rpc=foo"])
    err = capsys.readouterr().err
    assert "unknown role" in err and "rpc" in err


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

    rc = main(["run", "hello", "--run-name", "smoke-1", "--python", sys.executable])
    captured = capsys.readouterr()

    assert rc == 0, captured.err
    line = next(line for line in captured.out.splitlines() if line.startswith("{"))
    parsed = json.loads(line)
    assert parsed == {"experiment": "hello", "run_name": "smoke-1", "cwd_is_run_dir": True}

    # Output is also tee'd into the run dir.
    run_dir = paths.run_dir("hello", "smoke-1")
    assert (run_dir / "stdout.log").read_text().strip().startswith("{")


def test_run_default_name_is_short_timestamped_slug(home, capsys):
    src = paths.experiment_dir("hello")
    src.mkdir(parents=True)
    (src / "run.py").write_text("print('ok')\n", encoding="utf-8")

    rc = main(["run", "hello", "--python", sys.executable])
    captured = capsys.readouterr()
    assert rc == 0, captured.err

    runs = list(paths.experiment_runs_dir("hello").iterdir())
    assert len(runs) == 1
    name = runs[0].name
    # r-YYYYmmdd-HHMMSS — much shorter than the experiment name when the
    # experiment slug is long.
    assert name.startswith("r-") and len(name) == len("r-20260507-141502")


def test_build_run_command_defaults_to_uv_run(monkeypatch, tmp_path):
    from kresko_py.cli import _build_run_command

    monkeypatch.setattr("kresko_py.cli._kresko_project_root", lambda: tmp_path)
    monkeypatch.setattr("kresko_py.cli._which", lambda name: f"/usr/bin/{name}")

    cmd = _build_run_command(None, tmp_path / "run.py", ["launch"])

    assert cmd[:2] == ["uv", "run"]
    assert "--project" in cmd
    assert cmd[-2:] == [str(tmp_path / "run.py"), "launch"]


def test_build_run_command_falls_back_when_uv_missing(monkeypatch, tmp_path):
    from kresko_py.cli import _build_run_command

    monkeypatch.setattr("kresko_py.cli._kresko_project_root", lambda: tmp_path)
    monkeypatch.setattr("kresko_py.cli._which", lambda name: None)

    cmd = _build_run_command(None, tmp_path / "run.py", [])

    assert cmd[0] == sys.executable


def test_build_run_command_explicit_python_skips_uv(tmp_path):
    from kresko_py.cli import _build_run_command

    cmd = _build_run_command("/custom/python", tmp_path / "run.py", ["a"])

    assert cmd == ["/custom/python", str(tmp_path / "run.py"), "a"]


def test_run_requires_double_dash_before_forwarded_args(home, capsys):
    src = paths.experiment_dir("hello")
    src.mkdir(parents=True)
    (src / "run.py").write_text("print('ran')\n", encoding="utf-8")

    # Forwarded args without `--` should error so `kresko run hello --name foo`
    # cannot silently capture --name as a node filter or run dir name.
    with pytest.raises(SystemExit):
        main(["run", "hello", "--name", "foo", "launch"])
    err = capsys.readouterr().err
    assert "--" in err
