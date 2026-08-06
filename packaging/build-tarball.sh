#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
OUTPUT_DIR="${VIDEO_HARNESS_DIST_DIR:-${PROJECT_DIR}/dist}"
BINARY=""

usage() {
    cat <<'EOF'
Usage: packaging/build-tarball.sh [--binary PATH] [--output-dir DIR]

Create the best-effort native Video Harness archive for the current machine.
Without --binary, build native/target/release/video-harness first.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            BINARY="$2"
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            OUTPUT_DIR="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

case "$(uname -m)" in
    x86_64|amd64) RELEASE_ARCH="x86_64"; FILE_ARCH_PATTERN='x86-64|x86_64' ;;
    aarch64|arm64) RELEASE_ARCH="aarch64"; FILE_ARCH_PATTERN='aarch64|ARM aarch64' ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${PROJECT_DIR}/native/Cargo.toml" | head -n 1)"
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][A-Za-z0-9.-]+)?$ ]] \
    || { echo "Unsafe package version: ${VERSION}" >&2; exit 1; }

if [[ -z "${BINARY}" ]]; then
    cargo build --manifest-path "${PROJECT_DIR}/native/Cargo.toml" \
        --release --locked --bin video-harness
    BINARY="${PROJECT_DIR}/native/target/release/video-harness"
fi
[[ -x "${BINARY}" ]] || { echo "Executable not found: ${BINARY}" >&2; exit 1; }
"${BINARY}" --version | grep -Fq -- "${VERSION}" \
    || { echo "Binary version does not match ${VERSION}" >&2; exit 1; }
if command -v file >/dev/null 2>&1; then
    file --brief -- "${BINARY}" | grep -Eq -- "${FILE_ARCH_PATTERN}" \
        || { echo "Binary does not match ${RELEASE_ARCH}: $(file --brief -- "${BINARY}")" >&2; exit 1; }
fi

mkdir -p -- "${OUTPUT_DIR}"
STAGING_ROOT="$(mktemp -d)"
trap 'rm -rf -- "${STAGING_ROOT}"' EXIT
PACKAGE_NAME="video-harness-${VERSION}"
PACKAGE_ROOT="${STAGING_ROOT}/${PACKAGE_NAME}"

install -Dm0755 -- "${PROJECT_DIR}/install.sh" "${PACKAGE_ROOT}/install.sh"
install -Dm0755 -- "${PROJECT_DIR}/native/install.sh" "${PACKAGE_ROOT}/native/install.sh"
install -Dm0755 -- "${BINARY}" "${PACKAGE_ROOT}/native/bin/video-harness"
install -Dm0644 -- "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop" \
    "${PACKAGE_ROOT}/native/data/io.github.EnchiladaBoy.VideoHarness.desktop"
install -Dm0644 -- "${PROJECT_DIR}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
    "${PACKAGE_ROOT}/native/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
install -Dm0644 -- "${PROJECT_DIR}/native/data/icons/io.github.EnchiladaBoy.VideoHarness.svg" \
    "${PACKAGE_ROOT}/native/data/icons/io.github.EnchiladaBoy.VideoHarness.svg"
install -Dm0644 -- "${PROJECT_DIR}/README.md" "${PACKAGE_ROOT}/README.md"
install -Dm0644 -- "${PROJECT_DIR}/LICENSE" "${PACKAGE_ROOT}/LICENSE"
install -Dm0644 -- "${SCRIPT_DIR}/NATIVE-BUNDLE.md" "${PACKAGE_ROOT}/NATIVE-BUNDLE.md"

ARCHIVE="${OUTPUT_DIR}/video-harness-${VERSION}-linux-${RELEASE_ARCH}.tar.xz"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"
tar --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 --group=0 --numeric-owner \
    -C "${STAGING_ROOT}" -cJf "${ARCHIVE}.new" "${PACKAGE_NAME}"
mv -f -- "${ARCHIVE}.new" "${ARCHIVE}"

echo "Created ${ARCHIVE}"
