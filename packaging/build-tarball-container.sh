#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates \
    file \
    gcc \
    libadwaita-1-dev \
    libgtk-4-dev \
    pkg-config \
    xz-utils
rm -rf -- /var/lib/apt/lists/*

cargo build --manifest-path "${PROJECT_DIR}/native/Cargo.toml" \
    --release --locked --bin video-harness
"${PROJECT_DIR}/packaging/build-tarball.sh" \
    --binary "${PROJECT_DIR}/native/target/release/video-harness"
