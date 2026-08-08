#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    echo "Usage: packaging/smoke-appimage-gui.sh APPIMAGE" >&2
}

if [[ "${1:-}" == --inside-session ]]; then
    [[ $# -eq 3 ]] || { usage; exit 2; }
    APP_DIR="$2"
    EXTRACT_ROOT="$3"
    LOG_FILE="${EXTRACT_ROOT}/gui-smoke.log"
    APP_PID=

    # Invoked indirectly by the EXIT trap below.
    # shellcheck disable=SC2329
    cleanup() {
        if [[ -n "${APP_PID}" ]] && kill -0 "${APP_PID}" 2>/dev/null; then
            kill "${APP_PID}" 2>/dev/null || true
            wait "${APP_PID}" 2>/dev/null || true
        fi
        rm -rf -- "${EXTRACT_ROOT}"
    }
    trap cleanup EXIT

    XDG_CONFIG_HOME="${EXTRACT_ROOT}/config" \
    XDG_CACHE_HOME="${EXTRACT_ROOT}/cache" \
    XDG_DATA_HOME="${EXTRACT_ROOT}/data" \
        "${APP_DIR}/AppRun" >"${LOG_FILE}" 2>&1 &
    APP_PID=$!

    DEADLINE=$((SECONDS + 30))
    while (( SECONDS < DEADLINE )); do
        if ! kill -0 "${APP_PID}" 2>/dev/null; then
            echo "Video Harness exited before opening a window" >&2
            sed -n '1,240p' "${LOG_FILE}" >&2
            exit 1
        fi
        if xdotool search --onlyvisible --name 'Video Harness' >/dev/null 2>&1; then
            echo "Video Harness opened a visible window."
            exit 0
        fi
        sleep 0.25
    done

    echo "Video Harness did not open a visible window within 30 seconds" >&2
    sed -n '1,240p' "${LOG_FILE}" >&2
    exit 1
fi

[[ $# -eq 1 ]] || { usage; exit 2; }
APPIMAGE="$1"
[[ -x "${APPIMAGE}" ]] || {
    echo "AppImage is missing or not executable: ${APPIMAGE}" >&2
    exit 1
}
for COMMAND in dbus-run-session xvfb-run xdotool; do
    command -v "${COMMAND}" >/dev/null 2>&1 || {
        echo "${COMMAND} is required for the GUI smoke test" >&2
        exit 1
    }
done

APPIMAGE="$(realpath -- "${APPIMAGE}")"
EXTRACT_ROOT="$(mktemp -d)"
cleanup_extract() {
    rm -rf -- "${EXTRACT_ROOT}"
}
trap cleanup_extract EXIT
(
    cd -- "${EXTRACT_ROOT}"
    "${APPIMAGE}" --appimage-extract >/dev/null
)
trap - EXIT

exec dbus-run-session -- xvfb-run -a env \
    GDK_BACKEND=x11 \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 \
    "${SCRIPT_DIR}/smoke-appimage-gui.sh" \
    --inside-session "${EXTRACT_ROOT}/squashfs-root" "${EXTRACT_ROOT}"
