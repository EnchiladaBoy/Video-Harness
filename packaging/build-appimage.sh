#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"
OUTPUT_DIR="${VIDEO_HARNESS_DIST_DIR:-${PROJECT_DIR}/dist}"

usage() {
    cat <<'EOF'
Usage: packaging/build-appimage.sh [--output-dir DIR]

Build one unsigned Video Harness AppImage for the current Linux architecture.
The locked UI dependencies and compiled Svelte bundle must already exist.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
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

[[ "$(uname -s)" == Linux ]] || {
    echo "AppImages can only be built on Linux" >&2
    exit 1
}
case "$(uname -m)" in
    x86_64|amd64)
        RELEASE_ARCH=x86_64
        FILE_ARCH_PATTERN='x86-64|x86_64'
        ;;
    aarch64|arm64)
        RELEASE_ARCH=aarch64
        FILE_ARCH_PATTERN='aarch64|ARM aarch64'
        ;;
    *)
        echo "Unsupported AppImage architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' \
    "${PROJECT_DIR}/desktop/src-tauri/Cargo.toml" | head -n 1)"
[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][A-Za-z0-9.-]+)?$ ]] || {
    echo "Unsafe package version: ${VERSION}" >&2
    exit 1
}

[[ -f "${PROJECT_DIR}/ui/dist/index.html" ]] || {
    echo "Desktop UI is absent; run 'npm --prefix ui ci && npm --prefix ui run build' first" >&2
    exit 1
}
TAURI_CLI="${PROJECT_DIR}/ui/node_modules/.bin/tauri"
[[ -x "${TAURI_CLI}" ]] || {
    echo "Tauri CLI is absent; run 'npm --prefix ui ci' first" >&2
    exit 1
}

export CARGO_TARGET_DIR="${PROJECT_DIR}/desktop/src-tauri/target"
BUNDLE_DIR="${CARGO_TARGET_DIR}/release/bundle/appimage"
PACKAGE_DIR="${CARGO_TARGET_DIR}/release/bundle/appimage_deb"
TAURI_TOOLS_DIR="${CARGO_TARGET_DIR}/.tauri"
rm -rf -- "${BUNDLE_DIR}" "${PACKAGE_DIR}"
"${SCRIPT_DIR}/prepare-tauri-appimage-tools.sh" "${TAURI_TOOLS_DIR}"
(
    cd -- "${PROJECT_DIR}/desktop"
    CI=true "${TAURI_CLI}" build --ci --bundles appimage --no-sign -- --locked
)

mapfile -d '' APPIMAGES < <(
    find "${BUNDLE_DIR}" -maxdepth 1 -type f -name '*.AppImage' -print0
)
[[ "${#APPIMAGES[@]}" -eq 1 ]] || {
    echo "Expected one AppImage in ${BUNDLE_DIR}, found ${#APPIMAGES[@]}" >&2
    exit 1
}

BUILT_APPIMAGE="${APPIMAGES[0]}"
if command -v file >/dev/null 2>&1; then
    file --brief -- "${BUILT_APPIMAGE}" | grep -Eq -- "${FILE_ARCH_PATTERN}" || {
        echo "AppImage does not match ${RELEASE_ARCH}: $(file --brief -- "${BUILT_APPIMAGE}")" >&2
        exit 1
    }
fi

mkdir -p -- "${OUTPUT_DIR}"
OUTPUT_APPIMAGE="${OUTPUT_DIR}/Video-Harness-${VERSION}-linux-${RELEASE_ARCH}.AppImage"
OUTPUT_TEMP="${OUTPUT_APPIMAGE}.new"
EXTRACT_ROOT="$(mktemp -d)"
cleanup() {
    rm -rf -- "${EXTRACT_ROOT}"
    rm -f -- "${OUTPUT_TEMP}"
}
trap cleanup EXIT

install -m 0755 -- "${BUILT_APPIMAGE}" "${OUTPUT_TEMP}"

APPIMAGE_EXTRACT_AND_RUN=1 "${OUTPUT_TEMP}" --version \
    | grep -Fx -- "video-harness ${VERSION}" >/dev/null
(
    cd -- "${EXTRACT_ROOT}"
    "${OUTPUT_TEMP}" --appimage-extract >/dev/null
)
for REQUIRED_MEMBER in \
    WebKitNetworkProcess \
    WebKitWebProcess \
    libgstisomp4.so \
    libgstlibav.so \
    libgstplayback.so \
    libgstvideoparsersbad.so
do
    find "${EXTRACT_ROOT}/squashfs-root" -name "${REQUIRED_MEMBER}" -print -quit \
        | grep -q . || {
            echo "AppImage is missing required runtime member ${REQUIRED_MEMBER}" >&2
            exit 1
        }
done
"${SCRIPT_DIR}/smoke-appimage-media.sh" "${EXTRACT_ROOT}/squashfs-root"

mv -f -- "${OUTPUT_TEMP}" "${OUTPUT_APPIMAGE}"

echo "Created ${OUTPUT_APPIMAGE}"
