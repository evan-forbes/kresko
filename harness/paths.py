"""Filesystem layout for ~/.kresko.

Single source of truth for where things live on disk. Override the root with
the `KRESKO_HOME` environment variable; otherwise it resolves to
`~/.kresko/`.

Layout::

    ~/.kresko/
    ├── .env
    ├── config.toml
    ├── cache/
    ├── experiments/<experiment>/
    ├── runs/<experiment>/<run-name>/
    └── assets/<provider>-<provider-id>.json
"""

from __future__ import annotations

import os
import re
from pathlib import Path

KRESKO_HOME_ENV = "KRESKO_HOME"
SLUG_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def kresko_home() -> Path:
    override = os.environ.get(KRESKO_HOME_ENV)
    if override:
        return Path(override).expanduser().resolve()
    return Path("~/.kresko").expanduser().resolve()


def ensure_home() -> Path:
    home = kresko_home()
    for sub in ("experiments", "runs", "assets", "cache"):
        (home / sub).mkdir(parents=True, exist_ok=True)
    return home


def experiments_dir() -> Path:
    return kresko_home() / "experiments"


def experiment_dir(experiment: str) -> Path:
    validate_slug(experiment, kind="experiment")
    return experiments_dir() / experiment


def runs_dir() -> Path:
    return kresko_home() / "runs"


def experiment_runs_dir(experiment: str) -> Path:
    validate_slug(experiment, kind="experiment")
    return runs_dir() / experiment


def run_dir(experiment: str, run_name: str) -> Path:
    validate_slug(experiment, kind="experiment")
    validate_slug(run_name, kind="run")
    return experiment_runs_dir(experiment) / run_name


def assets_dir() -> Path:
    return kresko_home() / "assets"


def asset_path(provider: str, provider_id: str) -> Path:
    if not provider:
        raise ValueError("provider is required for asset path")
    if not provider_id:
        raise ValueError("provider_id is required for asset path")
    safe_id = str(provider_id).replace("/", "_")
    return assets_dir() / f"{provider}-{safe_id}.json"


def cache_dir() -> Path:
    return kresko_home() / "cache"


def env_file() -> Path:
    return kresko_home() / ".env"


def config_file() -> Path:
    return kresko_home() / "config.toml"


def validate_slug(value: str, *, kind: str = "slug") -> None:
    if not value or not SLUG_RE.match(value):
        raise ValueError(
            f"invalid {kind} {value!r}: must match [a-z0-9][a-z0-9-]*"
        )
