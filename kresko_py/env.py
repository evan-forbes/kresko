"""Layered .env loading with strict shell-wins precedence.

Precedence (high → low):
    1. explicit shell environment (never overridden)
    2. repo `.env`                 (fills gaps; never overrides shell)
    3. experiment `.env`           (fills remaining gaps; never overrides repo)

Loading both .env files with `override=False` is what enforces this — the
old implementation captured `protected` once at entry and let the
experiment .env clobber values the repo .env had just set.
"""

from __future__ import annotations

from pathlib import Path

try:
    from dotenv import load_dotenv as _load_dotenv
except ImportError:  # pragma: no cover - dependency is installed by uv for real runs.

    def _load_dotenv(*_args: object, **_kwargs: object) -> None:
        return None


def find_repo_root(start: str | Path) -> Path:
    path = Path(start).resolve()
    if path.is_file():
        path = path.parent
    for candidate in (path, *path.parents):
        if (candidate / "pyproject.toml").exists() or (candidate / ".git").exists():
            return candidate
    return path


def load_experiment_env(
    experiment_root: str | Path = ".",
    repo_root: str | Path | None = None,
) -> None:
    experiment_root = Path(experiment_root).resolve()
    if experiment_root.is_file():
        experiment_root = experiment_root.parent
    repo = Path(repo_root).resolve() if repo_root else find_repo_root(experiment_root)
    _load_dotenv(repo / ".env", override=False)
    _load_dotenv(experiment_root / ".env", override=False)
