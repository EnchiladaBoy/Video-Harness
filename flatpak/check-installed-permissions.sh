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

field_has() {
    local field="$1"
    local value="$2"
    local values
    values="$(sed -n "s/^${field}=//p" "${PERMISSIONS_FILE}" | head -n 1)"
    case ";${values}" in
        *";${value};"*) ;;
        *) fail "${field} does not contain ${value}" ;;
    esac
}

field_has shared network
field_has shared ipc
field_has sockets wayland
field_has sockets fallback-x11
field_has sockets pulseaudio
field_has devices dri
field_has filesystems xdg-videos:create
field_has filesystems '~/.local/share/openrouter-video-studio:ro'
field_has filesystems '~/.config/openrouter-video-studio:ro'
field_has filesystems '~/.cache/openrouter-video-studio:ro'
grep -Fq -- 'org.freedesktop.secrets=talk' "${PERMISSIONS_FILE}" \
    || fail "Secret Service talk permission is absent"

filesystems="$(sed -n 's/^filesystems=//p' "${PERMISSIONS_FILE}" | head -n 1)"
case ";${filesystems}" in
    *';host;'*|*';host:ro;'*|*';home;'*|*';home:ro;'*)
        fail "broad host or home access is present"
        ;;
esac

echo "Installed Flatpak permissions match the narrow policy."
