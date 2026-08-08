#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
MANIFEST="${SCRIPT_DIR}/io.github.EnchiladaBoy.VideoHarness.yml"
NATIVE_CARGO_TOML="${PROJECT_DIR}/native/Cargo.toml"
DESKTOP_CARGO_TOML="${PROJECT_DIR}/desktop/src-tauri/Cargo.toml"
METAINFO="${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
UI_STAMP="${PROJECT_DIR}/ui/dist/.source-sha256"

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
require_literal 'strip: true'
require_literal 'cargo build --release --locked --offline --manifest-path desktop/src-tauri/Cargo.toml --bin video-harness'
require_literal 'install -Dm0755 desktop/src-tauri/target/release/video-harness /app/bin/video-harness'
require_literal 'path: ../desktop'
require_literal 'dest: desktop'
require_literal 'path: ../native'
require_literal 'dest: native'
require_literal 'path: ../ui/dist'
require_literal 'dest: ui/dist'
dir_source_count="$(grep -c '^      - type: dir$' "${MANIFEST}")"
[[ "${dir_source_count}" -eq 3 ]] \
    || fail "manifest must contain exactly the three reviewed local source directories"
if grep -Eq '^[[:space:]]+path:[[:space:]]+\.\.[[:space:]]*$' "${MANIFEST}"; then
    fail "copying the repository root into the Flatpak build is forbidden"
fi
if grep -Fq -- 'legacy-gtk' "${MANIFEST}"; then
    fail "the stable Flatpak must not build the retired GTK frontend"
fi

package_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${DESKTOP_CARGO_TOML}" | head -n 1)"
core_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${NATIVE_CARGO_TOML}" | head -n 1)"
[[ "${package_version}" == "${core_version}" ]] \
    || fail "desktop and core Cargo versions differ (${package_version} != ${core_version})"
grep -Fq -- "<release version=\"${package_version}\"" "${METAINFO}" \
    || fail "Cargo and AppStream versions differ (${package_version})"
grep -Fq -- '<id>io.github.EnchiladaBoy.VideoHarness</id>' "${METAINFO}" \
    || fail "the installed AppStream identity changed"

[[ -f "${PROJECT_DIR}/ui/dist/index.html" && -f "${UI_STAMP}" ]] \
    || fail "the locked UI bundle is absent; run flatpak/prepare-ui.sh"
expected_ui_hash="$("${SCRIPT_DIR}/hash-ui-source.sh")"
actual_ui_hash="$(tr -d '\r\n' <"${UI_STAMP}")"
[[ "${actual_ui_hash}" == "${expected_ui_hash}" ]] \
    || fail "ui/dist is stale; run flatpak/prepare-ui.sh"

command -v desktop-file-validate >/dev/null 2>&1 \
    || fail "desktop-file-validate is required"
command -v appstreamcli >/dev/null 2>&1 \
    || fail "appstreamcli is required"
desktop-file-validate "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
# The legacy mixed-case component ID is the installed application identity.
# AppStream reports that known compatibility choice as a pedantic hint.
appstreamcli validate --no-net --strict --pedantic "${METAINFO}"

if command -v flatpak-builder >/dev/null 2>&1; then
    flatpak-builder --show-manifest "${MANIFEST}" >/dev/null
elif command -v flatpak >/dev/null 2>&1 \
    && flatpak --user info org.flatpak.Builder >/dev/null 2>&1; then
    flatpak run org.flatpak.Builder --show-manifest "${MANIFEST}" >/dev/null
else
    fail "flatpak-builder or the org.flatpak.Builder Flatpak is required"
fi

generated_sources="$(mktemp)"
trap 'rm -f -- "${generated_sources}"' EXIT
"${SCRIPT_DIR}/generate-cargo-sources.sh" "${generated_sources}"
cmp -s -- "${generated_sources}" "${SCRIPT_DIR}/cargo-sources.json" \
    || fail "cargo-sources.json is stale; run flatpak/generate-cargo-sources.sh"
grep -Fq -- 'directory = \"cargo/vendor\"' "${generated_sources}" \
    || fail "Cargo vendor source must resolve to cargo/vendor from the build root"
rm -f -- "${generated_sources}"
trap - EXIT

echo "Flatpak manifest and metadata checks passed for ${package_version}."
