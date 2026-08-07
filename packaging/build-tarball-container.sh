#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates \
    file \
    gcc \
    libayatana-appindicator3-dev \
    librsvg2-dev \
    libwebkit2gtk-4.1-dev \
    pkg-config \
    xz-utils
rm -rf -- /var/lib/apt/lists/*

[[ -f "${PROJECT_DIR}/ui/dist/index.html" ]] || {
    echo "Desktop UI is absent; build it with locked npm dependencies before entering the container" >&2
    exit 1
}

cargo build --manifest-path "${PROJECT_DIR}/desktop/src-tauri/Cargo.toml" \
    --release --locked --bin video-harness
"${PROJECT_DIR}/packaging/build-tarball.sh" \
    --binary "${PROJECT_DIR}/desktop/src-tauri/target/release/video-harness"
