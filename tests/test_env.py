from __future__ import annotations

import os

from kresko_py.env import load_experiment_env


def test_shell_env_wins_over_both_dotenv_files(monkeypatch, tmp_path):
    repo = tmp_path / "repo"
    experiment = repo / ".kresko" / "experiments" / "demo"
    experiment.mkdir(parents=True)
    (repo / "pyproject.toml").write_text("[project]\nname = 'demo'\n", encoding="utf-8")
    (repo / ".env").write_text("KRESKO_SSH_KEY_PATH=~/.ssh/id_ed25519\n", encoding="utf-8")
    (experiment / ".env").write_text("KRESKO_SSH_KEY_PATH=~/.ssh/exp_key\n", encoding="utf-8")

    # Empty string from the shell is still an explicit value the user set.
    monkeypatch.setenv("KRESKO_SSH_KEY_PATH", "")

    load_experiment_env(experiment)

    assert os.environ["KRESKO_SSH_KEY_PATH"] == ""


def test_repo_env_wins_over_experiment_env(monkeypatch, tmp_path):
    repo = tmp_path / "repo"
    experiment = repo / ".kresko" / "experiments" / "demo"
    experiment.mkdir(parents=True)
    (repo / "pyproject.toml").write_text("[project]\nname = 'demo'\n", encoding="utf-8")
    (repo / ".env").write_text("DIGITALOCEAN_TOKEN=from-repo\n", encoding="utf-8")
    (experiment / ".env").write_text("DIGITALOCEAN_TOKEN=from-experiment\n", encoding="utf-8")

    monkeypatch.delenv("DIGITALOCEAN_TOKEN", raising=False)

    load_experiment_env(experiment)

    assert os.environ["DIGITALOCEAN_TOKEN"] == "from-repo"


def test_experiment_env_fills_gaps(monkeypatch, tmp_path):
    repo = tmp_path / "repo"
    experiment = repo / ".kresko" / "experiments" / "demo"
    experiment.mkdir(parents=True)
    (repo / "pyproject.toml").write_text("[project]\nname = 'demo'\n", encoding="utf-8")
    (experiment / ".env").write_text("KRESKO_DEMO_ONLY=yes\n", encoding="utf-8")

    monkeypatch.delenv("KRESKO_DEMO_ONLY", raising=False)

    load_experiment_env(experiment)

    assert os.environ["KRESKO_DEMO_ONLY"] == "yes"
