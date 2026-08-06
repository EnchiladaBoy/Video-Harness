"""Application paths and safe output filename helpers."""

from __future__ import annotations

import os
import re
import unicodedata
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path


APP_NAME = "openrouter-video-studio"
DEFAULT_VIDEO_SUFFIX = ".mp4"


def _home() -> Path:
    return Path.home()


def _xdg_dir(env_name: str, default: Path) -> Path:
    value = os.environ.get(env_name)
    if not value:
        return default
    candidate = Path(value).expanduser()
    return candidate if candidate.is_absolute() else default


def _parse_user_dir(value: str, home: Path) -> Path | None:
    """Parse the deliberately small value grammar used by user-dirs.dirs.

    Shell evaluation is intentionally avoided.  The freedesktop file normally
    uses only ``$HOME`` plus a literal suffix, which is all we expand here.
    """

    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] == '"':
        value = value[1:-1]
    value = value.replace("${HOME}", str(home)).replace("$HOME", str(home))
    if "$" in value or "`" in value:
        return None
    candidate = Path(value).expanduser()
    return candidate if candidate.is_absolute() else None


def discover_videos_dir(*, home: Path | None = None) -> Path:
    """Return the user's XDG Videos directory without executing shell input."""

    home = home or _home()
    explicit = os.environ.get("XDG_VIDEOS_DIR")
    if explicit:
        parsed = _parse_user_dir(explicit, home)
        if parsed is not None:
            return parsed

    config_home = _xdg_dir("XDG_CONFIG_HOME", home / ".config")
    user_dirs = config_home / "user-dirs.dirs"
    try:
        for raw_line in user_dirs.read_text(encoding="utf-8").splitlines():
            line = raw_line.strip()
            if not line.startswith("XDG_VIDEOS_DIR="):
                continue
            parsed = _parse_user_dir(line.split("=", 1)[1], home)
            if parsed is not None:
                return parsed
    except (FileNotFoundError, OSError, UnicodeError):
        pass
    return home / "Videos"


@dataclass(frozen=True, slots=True)
class AppPaths:
    """All filesystem locations used by the application."""

    data_dir: Path
    cache_dir: Path
    config_dir: Path
    videos_dir: Path

    @classmethod
    def discover(cls, *, home: Path | None = None) -> "AppPaths":
        home = home or _home()
        return cls(
            data_dir=_xdg_dir("XDG_DATA_HOME", home / ".local" / "share") / APP_NAME,
            cache_dir=_xdg_dir("XDG_CACHE_HOME", home / ".cache") / APP_NAME,
            config_dir=_xdg_dir("XDG_CONFIG_HOME", home / ".config") / APP_NAME,
            videos_dir=discover_videos_dir(home=home),
        )

    @property
    def history_db(self) -> Path:
        return self.data_dir / "history.sqlite3"

    @property
    def catalog_cache(self) -> Path:
        return self.cache_dir / "video-models.json"

    def ensure(self) -> "AppPaths":
        """Create application and output directories and return this instance."""

        for directory in (
            self.data_dir,
            self.cache_dir,
            self.config_dir,
            self.videos_dir,
        ):
            directory.mkdir(parents=True, exist_ok=True)
        return self


def slugify_prompt(prompt: str, *, max_length: int = 48) -> str:
    """Turn a prompt into a short, portable filename component."""

    normalized = unicodedata.normalize("NFKD", prompt)
    ascii_text = normalized.encode("ascii", "ignore").decode("ascii").lower()
    slug = re.sub(r"[^a-z0-9]+", "-", ascii_text).strip("-")
    slug = slug[:max_length].rstrip("-")
    return slug or "video"


def _safe_component(value: str, *, fallback: str, max_length: int) -> str:
    component = re.sub(r"[^A-Za-z0-9_-]+", "-", value).strip("-")
    return component[:max_length] or fallback


def make_output_path(
    prompt: str,
    job_id: str,
    *,
    videos_dir: Path | None = None,
    now: datetime | None = None,
    suffix: str = DEFAULT_VIDEO_SUFFIX,
) -> Path:
    """Create a collision-free target path; no file is created."""

    directory = videos_dir or discover_videos_dir()
    timestamp = (now or datetime.now().astimezone()).strftime("%Y%m%d-%H%M%S")
    safe_job_id = _safe_component(job_id, fallback="job", max_length=20)
    clean_suffix = suffix if re.fullmatch(r"\.[A-Za-z0-9]{1,8}", suffix) else DEFAULT_VIDEO_SUFFIX
    stem = f"{timestamp}-{slugify_prompt(prompt)}-{safe_job_id}"
    candidate = directory / f"{stem}{clean_suffix.lower()}"
    index = 2
    while candidate.exists() or candidate.with_name(candidate.name + ".part").exists():
        candidate = directory / f"{stem}-{index}{clean_suffix.lower()}"
        index += 1
    return candidate
