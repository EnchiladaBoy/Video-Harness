#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
EXPECTED_TAG="${1:-}"

fail() {
    echo "Release version check failed: $*" >&2
    exit 1
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

grep -Fq -- "<release version=\"${CORE_VERSION}\"" \
    "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
    || fail "AppStream has no ${CORE_VERSION} release"
grep -Fq -- "Version ${CORE_VERSION}" "${PROJECT_DIR}/README.md" \
    || fail "README does not identify Version ${CORE_VERSION}"

if [[ -n "${EXPECTED_TAG}" && "${EXPECTED_TAG}" != "v${CORE_VERSION}" ]]; then
    fail "tag ${EXPECTED_TAG} does not match v${CORE_VERSION}"
fi

command -v desktop-file-validate >/dev/null 2>&1 \
    || fail "desktop-file-validate is required"
command -v appstreamcli >/dev/null 2>&1 \
    || fail "appstreamcli is required"
desktop-file-validate \
    "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
appstreamcli validate --no-net \
    "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"

echo "Core, desktop, UI, AppImage, and release metadata agree on ${CORE_VERSION}."
