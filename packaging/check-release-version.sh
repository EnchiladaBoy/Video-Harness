#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
EXPECTED_TAG="${1:-}"

fail() {
    echo "Release version check failed: $*" >&2
    exit 1
}

require_text() {
    local file="$1"
    local text="$2"
    grep -Fq -- "${text}" "${file}" \
        || fail "${file#"${PROJECT_DIR}/"} is missing '${text}'"
}

CORE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${PROJECT_DIR}/native/Cargo.toml" | head -n 1)"
DESKTOP_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${PROJECT_DIR}/desktop/src-tauri/Cargo.toml" | head -n 1)"
TAURI_VERSION="$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "${PROJECT_DIR}/desktop/src-tauri/tauri.conf.json" | head -n 1)"
UI_VERSION="$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "${PROJECT_DIR}/ui/package.json" | head -n 1)"
UI_LOCK_VERSION="$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "${PROJECT_DIR}/ui/package-lock.json" | head -n 1)"
[[ "${CORE_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][A-Za-z0-9.-]+)?$ ]] \
    || fail "invalid core Cargo version ${CORE_VERSION}"
[[ "${DESKTOP_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "desktop Cargo has ${DESKTOP_VERSION}, expected ${CORE_VERSION}"
[[ "${TAURI_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "Tauri config has ${TAURI_VERSION}, expected ${CORE_VERSION}"
[[ "${UI_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "UI package has ${UI_VERSION}, expected ${CORE_VERSION}"
[[ "${UI_LOCK_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "UI package lock has ${UI_LOCK_VERSION}, expected ${CORE_VERSION}"

lock_version() {
    local lock_file="$1"
    local package_name="$2"
    awk -v package_name="${package_name}" '
    $0 == "name = \"" package_name "\"" { found = 1; next }
    found && /^version = "/ {
        value = $0
        sub(/^version = "/, "", value)
        sub(/"$/, "", value)
        print value
        exit
    }
' "${lock_file}"
}

CORE_LOCK_VERSION="$(lock_version "${PROJECT_DIR}/native/Cargo.lock" video-harness)"
DESKTOP_LOCK_VERSION="$(lock_version "${PROJECT_DIR}/desktop/src-tauri/Cargo.lock" video-harness-desktop)"
[[ "${CORE_LOCK_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "native Cargo.lock has ${CORE_LOCK_VERSION}, expected ${CORE_VERSION}"
[[ "${DESKTOP_LOCK_VERSION}" == "${CORE_VERSION}" ]] \
    || fail "desktop Cargo.lock has ${DESKTOP_LOCK_VERSION}, expected ${CORE_VERSION}"

release_profile() {
    awk '
        $0 == "[profile.release]" { found = 1; next }
        found && /^\[/ { exit }
        found && !/^[[:space:]]*($|#)/ {
            value = $0
            gsub(/[[:space:]]/, "", value)
            print value
        }
    ' "$1" | LC_ALL=C sort
}

CORE_RELEASE_PROFILE="$(release_profile "${PROJECT_DIR}/native/Cargo.toml")"
DESKTOP_RELEASE_PROFILE="$(release_profile "${PROJECT_DIR}/desktop/src-tauri/Cargo.toml")"
[[ -n "${CORE_RELEASE_PROFILE}" ]] || fail "native release profile is empty"
[[ "${DESKTOP_RELEASE_PROFILE}" == "${CORE_RELEASE_PROFILE}" ]] \
    || fail "desktop and native release profiles differ; Cargo ignores dependency profile tables"

METAINFO="${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
require_text "${METAINFO}" '<id>io.github.EnchiladaBoy.VideoHarness</id>'
require_text "${METAINFO}" "<release version=\"${CORE_VERSION}\""
require_text "${PROJECT_DIR}/README.md" "Version ${CORE_VERSION}"
require_text "${PROJECT_DIR}/README.md" "Video-Harness-${CORE_VERSION}-linux-x86_64.AppImage"
require_text "${PROJECT_DIR}/README.md" "Video-Harness-${CORE_VERSION}-linux-aarch64.AppImage"
require_text "${PROJECT_DIR}/packaging/README.md" "Video-Harness-${CORE_VERSION}-linux-x86_64.AppImage"
require_text "${PROJECT_DIR}/packaging/README.md" "Video-Harness-${CORE_VERSION}-linux-aarch64.AppImage"
require_text "${PROJECT_DIR}/.github/RELEASING.md" "## Cut v${CORE_VERSION}"
require_text "${PROJECT_DIR}/.github/RELEASING.md" "check-release-version.sh v${CORE_VERSION}"
for RELEASE_ASSET in \
    "Video-Harness-${CORE_VERSION}-linux-x86_64.AppImage" \
    "Video-Harness-${CORE_VERSION}-linux-aarch64.AppImage" \
    "Video-Harness-${CORE_VERSION}-windows-x86_64-setup.exe" \
    "Video-Harness-${CORE_VERSION}-macos-aarch64.dmg" \
    "Video-Harness-v${CORE_VERSION}.spdx.json"
do
    require_text "${PROJECT_DIR}/.github/RELEASING.md" "${RELEASE_ASSET}"
done

if [[ -n "${EXPECTED_TAG}" && "${EXPECTED_TAG}" != "v${CORE_VERSION}" ]]; then
    fail "tag ${EXPECTED_TAG} does not match v${CORE_VERSION}"
fi

command -v desktop-file-validate >/dev/null 2>&1 \
    || fail "desktop-file-validate is required"
command -v appstreamcli >/dev/null 2>&1 \
    || fail "appstreamcli is required"
desktop-file-validate \
    "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
appstreamcli validate --no-net --strict --pedantic "${METAINFO}"

echo "Core, desktop, UI, package, and release metadata agree on ${CORE_VERSION}."
