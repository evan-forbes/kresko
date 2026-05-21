from __future__ import annotations

import pytest

from harness import paths


def test_kresko_home_uses_env_override(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.kresko_home() == tmp_path.resolve()
    assert paths.assets_dir() == (tmp_path / "assets").resolve()
    assert paths.experiments_dir() == (tmp_path / "experiments").resolve()
    assert paths.runs_dir() == (tmp_path / "runs").resolve()


def test_ensure_home_creates_subdirs(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    for sub in ("experiments", "runs", "assets", "cache"):
        assert (tmp_path / sub).is_dir()


def test_run_dir_validates_slugs(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.run_dir("smoke", "smoke-2").name == "smoke-2"
    with pytest.raises(ValueError):
        paths.run_dir("Smoke", "smoke-1")
    with pytest.raises(ValueError):
        paths.run_dir("smoke", "Smoke 1")


def test_asset_path_includes_provider_and_id(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.asset_path("digitalocean", "12345").name == "digitalocean-12345.json"
    with pytest.raises(ValueError):
        paths.asset_path("", "1")
    with pytest.raises(ValueError):
        paths.asset_path("digitalocean", "")
