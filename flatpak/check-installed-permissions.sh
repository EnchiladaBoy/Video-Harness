#!/usr/bin/env bash
set -euo pipefail

PERMISSIONS_FILE="${1:-}"
if [[ -z "${PERMISSIONS_FILE}" || ! -f "${PERMISSIONS_FILE}" ]]; then
    echo "Usage: $0 FLATPAK-PERMISSIONS.txt" >&2
    exit 2
fi

fail() {
    echo "Installed Flatpak permission check failed: $*" >&2
    exit 1
}

normalize_list() {
    tr ';' '\n' | sed '/^$/d' | LC_ALL=C sort | paste -sd ';' -
}

field_equals() {
    local field="$1"
    shift
    local -a matches=()
    local actual expected
    mapfile -t matches < <(sed -n "s/^${field}=//p" "${PERMISSIONS_FILE}")
    [[ "${#matches[@]}" -eq 1 ]] \
        || fail "${field} must occur exactly once"
    actual="$(printf '%s' "${matches[0]}" | normalize_list)"
    expected="$(printf '%s\n' "$@" | LC_ALL=C sort | paste -sd ';' -)"
    [[ "${actual}" == "${expected}" ]] \
        || fail "${field} is '${actual}', expected '${expected}'"
}

field_equals shared ipc network
field_equals sockets fallback-x11 pulseaudio wayland
field_equals devices dri
field_equals filesystems \
    '~/.cache/openrouter-video-studio:ro' \
    '~/.config/openrouter-video-studio:ro' \
    '~/.local/share/openrouter-video-studio:ro' \
    xdg-videos:create

unexpected="$(awk '
    /^[[:space:]]*($|#)/ { next }
    /^\[/ { section = $0; next }
    section == "[Context]" && /^(shared|sockets|devices|filesystems)=/ { next }
    section == "[Session Bus Policy]" && $0 == "org.freedesktop.secrets=talk" { next }
    { print NR ":" $0 }
' "${PERMISSIONS_FILE}")"
[[ -z "${unexpected}" ]] \
    || fail "unexpected permission entries: ${unexpected//$'\n'/, }"

secret_count="$(grep -Fxc -- 'org.freedesktop.secrets=talk' "${PERMISSIONS_FILE}" || true)"
[[ "${secret_count}" -eq 1 ]] \
    || fail "Secret Service talk permission must occur exactly once"

echo "Installed Flatpak permissions exactly match the reviewed allowlist."
