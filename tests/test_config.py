from __future__ import annotations

from datetime import datetime
from pathlib import Path

from openrouter_video_studio.config import (
    AppPaths,
    discover_videos_dir,
    make_output_path,
    slugify_prompt,
)


def test_app_paths_follow_xdg_locations(monkeypatch, tmp_path) -> None:
    home = tmp_path / "home"
    data = tmp_path / "data"
    cache = tmp_path / "cache"
    config = tmp_path / "config"
    videos = tmp_path / "My Videos"
    monkeypatch.setenv("XDG_DATA_HOME", str(data))
    monkeypatch.setenv("XDG_CACHE_HOME", str(cache))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(config))
    monkeypatch.setenv("XDG_VIDEOS_DIR", str(videos))

    paths = AppPaths.discover(home=home).ensure()

    assert paths.data_dir == data / "openrouter-video-studio"
    assert paths.catalog_cache == cache / "openrouter-video-studio/video-models.json"
    assert paths.history_db == data / "openrouter-video-studio/history.sqlite3"
    assert paths.videos_dir == videos
    assert all(
        path.is_dir()
        for path in (paths.data_dir, paths.cache_dir, paths.config_dir, paths.videos_dir)
    )


def test_videos_dir_reads_literal_xdg_home_reference(monkeypatch, tmp_path) -> None:
    home = tmp_path / "person"
    config = tmp_path / "config"
    config.mkdir()
    (config / "user-dirs.dirs").write_text(
        'XDG_DOWNLOAD_DIR="$HOME/Downloads"\n'
        'XDG_VIDEOS_DIR="$HOME/Movies"\n',
        encoding="utf-8",
    )
    monkeypatch.delenv("XDG_VIDEOS_DIR", raising=False)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(config))

    assert discover_videos_dir(home=home) == home / "Movies"


def test_videos_dir_never_evaluates_shell_syntax(monkeypatch, tmp_path) -> None:
    home = tmp_path / "person"
    config = tmp_path / "config"
    config.mkdir()
    (config / "user-dirs.dirs").write_text(
        'XDG_VIDEOS_DIR="$(touch /tmp/should-never-exist)"\n',
        encoding="utf-8",
    )
    monkeypatch.delenv("XDG_VIDEOS_DIR", raising=False)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(config))

    assert discover_videos_dir(home=home) == home / "Videos"


def test_output_name_is_safe_and_resolves_partial_collision(tmp_path) -> None:
    now = datetime(2026, 8, 6, 14, 30, 15)
    first = make_output_path(
        "Clouds / stars: a café!",
        "job/../../unsafe",
        videos_dir=tmp_path,
        now=now,
    )
    partial = first.with_name(first.name + ".part")
    partial.write_bytes(b"incomplete")

    second = make_output_path(
        "Clouds / stars: a café!",
        "job/../../unsafe",
        videos_dir=tmp_path,
        now=now,
    )

    assert first.parent == tmp_path
    assert first.name == "20260806-143015-clouds-stars-a-cafe-job-unsafe.mp4"
    assert second.name == "20260806-143015-clouds-stars-a-cafe-job-unsafe-2.mp4"


def test_slugify_prompt_has_a_portable_fallback() -> None:
    assert slugify_prompt("   🌧️ 🎬   ") == "video"
    assert "/" not in slugify_prompt("one/two")
