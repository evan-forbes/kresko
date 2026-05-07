from __future__ import annotations

import json
import os

import pytest

from kresko_py import paths
from kresko_py.runs import (
    ENV_EXPERIMENT,
    ENV_RUN_DIR,
    ENV_RUN_NAME,
    list_runs,
    open_run,
    resolve_run_name,
    start_run,
    write_node_snapshot,
    write_result,
)


@pytest.fixture
def home(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    return tmp_path


def make_experiment_source(name: str = "smoke") -> None:
    src = paths.experiment_dir(name)
    src.mkdir(parents=True, exist_ok=True)
    (src / "run.py").write_text("print('hi')\n", encoding="utf-8")
    (src / "payload").mkdir(exist_ok=True)
    (src / "payload" / "data.txt").write_text("hello\n", encoding="utf-8")


def test_resolve_run_name_increments_on_collision(home):
    make_experiment_source()
    assert resolve_run_name("smoke") == "smoke"
    start_run("smoke")
    assert resolve_run_name("smoke") == "smoke-2"
    start_run("smoke")
    assert resolve_run_name("smoke") == "smoke-3"


def test_resolve_run_name_uses_explicit_slug(home):
    make_experiment_source()
    assert resolve_run_name("smoke", "experiment-a") == "experiment-a"


def test_start_run_copies_source_and_writes_manifest(home):
    make_experiment_source("copy-exp")
    run_path = start_run("copy-exp", argv=["run", "copy-exp"])

    assert (run_path / "run.py").exists()
    assert (run_path / "payload" / "data.txt").read_text() == "hello\n"
    assert (run_path / "nodes").is_dir()
    manifest = json.loads((run_path / "manifest.json").read_text())
    assert manifest["experiment"] == "copy-exp"
    assert manifest["run_name"] == "copy-exp"
    assert manifest["argv"] == ["run", "copy-exp"]


def test_start_run_refuses_when_source_missing(home):
    with pytest.raises(FileNotFoundError):
        start_run("missing")


def test_write_result_and_node_snapshot(home):
    make_experiment_source("rs")
    run_path = start_run("rs")

    write_result(run_path, "up", True, extra={"plan": []})
    write_node_snapshot(run_path, {"name": "miner-0", "provider": "digitalocean", "provider_id": "1"})

    result = json.loads((run_path / "result.json").read_text())
    assert result["stage"] == "up"
    assert result["ok"] is True
    snapshot = json.loads((run_path / "nodes" / "miner-0.json").read_text())
    assert snapshot["provider_id"] == "1"


def test_open_run_sets_and_restores_env(home):
    make_experiment_source("opened")
    prior = os.environ.pop(ENV_EXPERIMENT, None)
    os.environ[ENV_RUN_NAME] = "preserved"
    try:
        with open_run("opened", name="auto-001") as run_path:
            assert os.environ[ENV_EXPERIMENT] == "opened"
            assert os.environ[ENV_RUN_NAME] == run_path.name
            assert os.environ[ENV_RUN_DIR] == str(run_path)
            assert run_path.exists()
        # Outside the block, env is restored.
        assert ENV_EXPERIMENT not in os.environ
        assert os.environ.get(ENV_RUN_NAME) == "preserved"
        assert ENV_RUN_DIR not in os.environ
    finally:
        os.environ.pop(ENV_RUN_NAME, None)
        if prior is not None:
            os.environ[ENV_EXPERIMENT] = prior


def test_open_run_chdir_and_restore(home, tmp_path):
    make_experiment_source("cd-exp")
    starting = os.getcwd()
    try:
        with open_run("cd-exp", name="auto-002", chdir=True) as run_path:
            assert os.getcwd() == str(run_path)
        assert os.getcwd() == starting
    finally:
        os.chdir(starting)


def test_list_runs_returns_run_directories(home):
    make_experiment_source("listed")
    a = start_run("listed")
    b = start_run("listed")
    runs = list_runs("listed")
    assert sorted(p.name for p in runs) == sorted([a.name, b.name])
