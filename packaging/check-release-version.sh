#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
EXPECTED_TAG="${1:-}"

fail() {
    echo "Release version check failed: $*" >&2
    exit 1
}

CARGO_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${PROJECT_DIR}/native/Cargo.toml" | head -n 1)"
[[ "${CARGO_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][A-Za-z0-9.-]+)?$ ]] \
    || fail "invalid Cargo version ${CARGO_VERSION}"

LOCK_VERSION="$(awk '
    /^name = "video-harness"$/ { found = 1; next }
    found && /^version = "/ {
        value = $0
        sub(/^version = "/, "", value)
        sub(/"$/, "", value)
        print value
        exit
    }
' "${PROJECT_DIR}/native/Cargo.lock")"
[[ "${LOCK_VERSION}" == "${CARGO_VERSION}" ]] \
    || fail "Cargo.lock has ${LOCK_VERSION}, expected ${CARGO_VERSION}"

grep -Fq -- "<release version=\"${CARGO_VERSION}\"" \
    "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
    || fail "AppStream has no ${CARGO_VERSION} release"
grep -Fq -- "v${CARGO_VERSION}" "${PROJECT_DIR}/README.md" \
    || fail "README does not identify v${CARGO_VERSION}"

if [[ -n "${EXPECTED_TAG}" && "${EXPECTED_TAG}" != "v${CARGO_VERSION}" ]]; then
    fail "tag ${EXPECTED_TAG} does not match v${CARGO_VERSION}"
fi

"${PROJECT_DIR}/flatpak/check-manifest.sh"
echo "Release metadata agrees on ${CARGO_VERSION}."
