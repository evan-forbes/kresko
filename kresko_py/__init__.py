"""Python orchestration helpers for Kresko experiments."""

from kresko_py.env import find_repo_root, load_experiment_env
from kresko_py.experiment import (
    DigitalOcean,
    DigitalOceanNodeType,
    Experiment,
    node_type,
    run_pyinfra,
)
from kresko_py.paths import (
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
from kresko_py.runs import open_run

__all__ = [
    "DigitalOcean",
    "DigitalOceanNodeType",
    "Experiment",
    "asset_path",
    "assets_dir",
    "cache_dir",
    "config_file",
    "ensure_home",
    "env_file",
    "experiment_dir",
    "experiments_dir",
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
    # Lazy import so `from kresko_py import run_experiment` works without
    # pulling in the heavier CLI machinery on package import.
    if name == "run_experiment":
        from kresko_py.cli import run_experiment

        return run_experiment
    raise AttributeError(f"module 'kresko_py' has no attribute {name!r}")
