#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
VENV_DIR="${PROJECT_DIR}/.venv"
BIN_DIR="${XDG_BIN_HOME:-${HOME}/.local/bin}"
LAUNCHER="${BIN_DIR}/openrouter-video"

if ! command -v python3 >/dev/null 2>&1; then
    echo "Python 3.11 or newer is required, but python3 was not found." >&2
    exit 1
fi

if ! python3 -c 'import sys; raise SystemExit(sys.version_info < (3, 11))'; then
    echo "Python 3.11 or newer is required." >&2
    exit 1
fi

echo "Creating the private Python environment..."
python3 -m venv "${VENV_DIR}"
"${VENV_DIR}/bin/python" -m ensurepip --upgrade
"${VENV_DIR}/bin/python" -m pip install --upgrade pip
"${VENV_DIR}/bin/python" -m pip install --editable "${PROJECT_DIR}"

mkdir -p "${BIN_DIR}"
if [[ -e "${LAUNCHER}" && ! -L "${LAUNCHER}" ]]; then
    echo "Refusing to replace the existing file at ${LAUNCHER}." >&2
    echo "Move it elsewhere, then run this installer again." >&2
    exit 1
fi
ln -sfn "${VENV_DIR}/bin/openrouter-video" "${LAUNCHER}"

echo
echo "Installed: ${LAUNCHER}"
echo "Run: openrouter-video"
if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
    echo "If the command is not found, add ${BIN_DIR} to PATH or run ${LAUNCHER}."
fi

