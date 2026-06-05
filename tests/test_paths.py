from __future__ import annotations

import pytest

from kresko import paths


def test_kresko_home_uses_env_override(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.kresko_home() == tmp_path.resolve()
    assert paths.assets_dir() == (tmp_path / "assets").resolve()
    assert paths.fleets_dir() == (tmp_path / "fleets").resolve()


def test_ensure_home_creates_subdirs(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    paths.ensure_home()
    for sub in ("fleets", "assets", "cache"):
        assert (tmp_path / sub).is_dir()


def test_fleet_dir_validates_slug(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.fleet_dir("ci-abc123").name == "ci-abc123"
    assert paths.fleet_dir("ci-abc123").parent == (tmp_path / "fleets").resolve()
    with pytest.raises(ValueError):
        paths.fleet_dir("Smoke")
    with pytest.raises(ValueError):
        paths.fleet_dir("ci abc")


def test_asset_path_includes_provider_and_id(monkeypatch, tmp_path):
    monkeypatch.setenv(paths.KRESKO_HOME_ENV, str(tmp_path))
    assert paths.asset_path("digitalocean", "12345").name == "digitalocean-12345.json"
    with pytest.raises(ValueError):
        paths.asset_path("", "1")
    with pytest.raises(ValueError):
        paths.asset_path("digitalocean", "")
