from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def test_console_script_targets_application_entrypoint() -> None:
    metadata = tomllib.loads((PROJECT_ROOT / "pyproject.toml").read_text())

    assert metadata["project"]["requires-python"] == ">=3.11"
    assert (
        metadata["project"]["scripts"]["openrouter-video"]
        == "openrouter_video_studio.__main__:main"
    )


def test_runtime_dependencies_are_bounded() -> None:
    metadata = tomllib.loads((PROJECT_ROOT / "pyproject.toml").read_text())
    dependencies = metadata["project"]["dependencies"]

    assert "httpx>=0.28,<0.29" in dependencies
    assert "keyring>=25,<26" in dependencies
    assert "platformdirs>=4,<5" in dependencies
    assert "textual>=4,<5" in dependencies
    assert any(item.startswith("SecretStorage") for item in dependencies)


def test_installer_has_valid_bash_syntax() -> None:
    completed = subprocess.run(
        ["bash", "-n", str(PROJECT_ROOT / "install.sh")],
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr


def test_supported_interpreter_can_compile_package() -> None:
    package = PROJECT_ROOT / "src" / "openrouter_video_studio"
    completed = subprocess.run(
        [sys.executable, "-m", "compileall", "-q", str(package)],
        check=False,
        capture_output=True,
        text=True,
    )

    assert completed.returncode == 0, completed.stderr
