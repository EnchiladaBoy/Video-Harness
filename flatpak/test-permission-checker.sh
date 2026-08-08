#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
test_root="$(mktemp -d)"
cleanup() {
    rm -rf -- "${test_root}"
}
trap cleanup EXIT

cat >"${test_root}/valid.txt" <<'EOF'
[Context]
shared=network;ipc;
sockets=pulseaudio;wayland;fallback-x11;
devices=dri;
filesystems=xdg-videos:create;~/.local/share/openrouter-video-studio:ro;~/.config/openrouter-video-studio:ro;~/.cache/openrouter-video-studio:ro;

[Session Bus Policy]
org.freedesktop.secrets=talk
EOF
"${SCRIPT_DIR}/check-installed-permissions.sh" "${test_root}/valid.txt"

cat >"${test_root}/extra.txt" <<'EOF'
[Context]
shared=network;ipc;
sockets=pulseaudio;wayland;fallback-x11;session-bus;
devices=dri;
filesystems=xdg-videos:create;~/.local/share/openrouter-video-studio:ro;~/.config/openrouter-video-studio:ro;~/.cache/openrouter-video-studio:ro;

[Session Bus Policy]
org.freedesktop.secrets=talk
org.example.Unreviewed=talk
EOF
if "${SCRIPT_DIR}/check-installed-permissions.sh" "${test_root}/extra.txt" \
    >/dev/null 2>&1; then
    echo "Permission checker accepted unreviewed socket and D-Bus grants" >&2
    exit 1
fi

echo "Installed permission checker accepts only the exact reviewed policy."
