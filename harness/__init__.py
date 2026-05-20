"""Python orchestration helpers for Kresko experiments."""

from harness.env import find_repo_root, load_experiment_env
from harness.experiment import (
    DigitalOcean,
    Experiment,
    NodeType,
    Vultr,
    node_type,
    run_pyinfra,
)
from harness.paths import (
    asset_path,
    assets_dir,
    cache_dir,
    config_file,
    ensure_home,
    env_file,
    experiment_dir,
    experiments_dir,
    kresko_home,
    run_dir,
    runs_dir,
)
from harness.runs import open_run

__all__ = [
    "DigitalOcean",
    "Experiment",
    "NodeType",
    "Vultr",
    "asset_path",
    "assets_dir",
    "cache_dir",
    "config_file",
    "ensure_home",
    "env_file",
    "experiment_dir",
    "experiments_dir",
    "explorer_actions",
    "find_repo_root",
    "kresko_home",
    "load_experiment_env",
    "node_type",
    "open_run",
    "run_dir",
    "run_experiment",
    "run_pyinfra",
    "runs_dir",
]


def __getattr__(name: str):
    # Lazy import so `from harness import run_experiment` works without
    # pulling in the heavier CLI machinery on package import.
    if name == "run_experiment":
        from harness.cli import run_experiment

        return run_experiment
    if name == "explorer_actions":
        from harness.explorer import explorer_actions

        return explorer_actions
    raise AttributeError(f"module 'harness' has no attribute {name!r}")
