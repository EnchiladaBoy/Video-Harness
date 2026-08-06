#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
FIXTURE="${PROJECT_DIR}/native/fixtures/fake-video-harness.sh"
TEST_ROOT="$(mktemp -d)"
trap 'chmod -R u+w -- "${TEST_ROOT}" 2>/dev/null || true; rm -rf -- "${TEST_ROOT}"' EXIT
BUNDLE_ROOT="${TEST_ROOT}/bundle"
INSTALLER="${BUNDLE_ROOT}/install.sh"

install -Dm0755 -- "${PROJECT_DIR}/install.sh" "${INSTALLER}"
install -Dm0755 -- "${PROJECT_DIR}/native/install.sh" "${BUNDLE_ROOT}/native/install.sh"
install -Dm0755 -- "${FIXTURE}" "${BUNDLE_ROOT}/native/bin/video-harness"
install -Dm0644 -- "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop" \
    "${BUNDLE_ROOT}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
install -Dm0644 -- "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
    "${BUNDLE_ROOT}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
install -Dm0644 -- "${PROJECT_DIR}/native/data/icons/io.github.EnchiladaBoy.VideoHarness.svg" \
    "${BUNDLE_ROOT}/native/data/icons/io.github.EnchiladaBoy.VideoHarness.svg"

run_installer() {
    env \
        OPENROUTER_VIDEO_LIB_DIR="${TEST_ROOT}/lib" \
        OPENROUTER_VIDEO_BIN_DIR="${TEST_ROOT}/bin" \
        VIDEO_HARNESS_DATA_DIR="${TEST_ROOT}/share" \
        VIDEO_HARNESS_PROJECT_DIR="${TEST_ROOT}/project" \
        "${INSTALLER}" "$@"
}

run_installer install
run_installer status
[[ -L "${TEST_ROOT}/bin/video-harness" ]]
[[ -f "${TEST_ROOT}/share/applications/io.github.EnchiladaBoy.VideoHarness.desktop" ]]

run_installer uninstall
[[ ! -e "${TEST_ROOT}/bin/video-harness" ]]
[[ ! -e "${TEST_ROOT}/share/applications/io.github.EnchiladaBoy.VideoHarness.desktop" ]]
[[ -x "${TEST_ROOT}/lib/releases/0.4.0-test/video-harness" ]]

echo "Installer smoke test passed on $(uname -m)."
