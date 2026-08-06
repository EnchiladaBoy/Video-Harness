#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
MANIFEST="${SCRIPT_DIR}/io.github.EnchiladaBoy.VideoHarness.yml"
CARGO_TOML="${PROJECT_DIR}/native/Cargo.toml"
METAINFO="${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"

fail() {
    echo "Flatpak check failed: $*" >&2
    exit 1
}

require_literal() {
    local text="$1"
    grep -Fq -- "${text}" "${MANIFEST}" || fail "manifest is missing ${text}"
}

for permission in \
    --share=network \
    --share=ipc \
    --socket=wayland \
    --socket=fallback-x11 \
    --device=dri \
    --socket=pulseaudio \
    --filesystem=xdg-videos:create \
    --filesystem=~/.local/share/openrouter-video-studio:ro \
    --filesystem=~/.config/openrouter-video-studio:ro \
    --filesystem=~/.cache/openrouter-video-studio:ro \
    --talk-name=org.freedesktop.secrets; do
    require_literal "${permission}"
done

finish_arg_count="$(sed -n '/^finish-args:/,/^build-options:/p' "${MANIFEST}" \
    | grep -c '^  - --')"
[[ "${finish_arg_count}" -eq 11 ]] \
    || fail "finish-args must contain exactly the 11 reviewed permissions"

if grep -Eq -- '--filesystem=(home|host)(:|$)' "${MANIFEST}"; then
    fail "broad home or host filesystem access is forbidden"
fi
require_literal 'runtime-version: "50"'
require_literal 'org.freedesktop.Sdk.Extension.rust-stable'
require_literal 'cargo build --release --locked --offline'

package_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${CARGO_TOML}" | head -n 1)"
grep -Fq -- "<release version=\"${package_version}\"" "${METAINFO}" \
    || fail "Cargo and AppStream versions differ (${package_version})"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
fi
if command -v appstreamcli >/dev/null 2>&1; then
    appstreamcli validate --no-net "${METAINFO}"
fi
if command -v flatpak-builder >/dev/null 2>&1; then
    flatpak-builder --show-manifest "${MANIFEST}" >/dev/null
fi

generated_sources="$(mktemp)"
trap 'rm -f -- "${generated_sources}"' EXIT
"${SCRIPT_DIR}/generate-cargo-sources.sh" "${generated_sources}"
cmp -s -- "${generated_sources}" "${SCRIPT_DIR}/cargo-sources.json" \
    || fail "cargo-sources.json is stale; run flatpak/generate-cargo-sources.sh"
rm -f -- "${generated_sources}"
trap - EXIT

echo "Flatpak manifest and metadata checks passed for ${package_version}."
