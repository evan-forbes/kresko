"""Python orchestration for Kresko: spin up fleets, deploy payloads, run
commands, collect measurements.

A **fleet** is a named, tagged set of cloud nodes plus its state under
``~/.kresko/fleets/<name>/``. Author orchestration in plain Python::

    from kresko import Fleet, Vultr

    fleet = Fleet("ci-abc123", ssh={"key_name": "kresko-key"})
    fleet.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
    fleet.up()
    fleet.deploy("payload/")
    fleet.run("kresko mine ...", role="miner", background="mine")
    fleet.collect("/root/traces", role="miner")
    fleet.down()
"""

from kresko.env import find_repo_root, load_experiment_env
from kresko.fleet import DigitalOcean, Fleet, Vultr, run_pyinfra
from kresko.paths import (
    asset_path,
    assets_dir,
    cache_dir,
    config_file,
    ensure_home,
    env_file,
    fleet_dir,
    fleets_dir,
    kresko_home,
)

__all__ = [
    "DigitalOcean",
    "Fleet",
    "Vultr",
    "asset_path",
    "assets_dir",
    "cache_dir",
    "config_file",
    "ensure_home",
    "env_file",
    "find_repo_root",
    "fleet_dir",
    "fleets_dir",
    "kresko_home",
    "load_experiment_env",
    "run_pyinfra",
]
