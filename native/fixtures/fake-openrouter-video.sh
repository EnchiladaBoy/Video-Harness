#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
    echo "video-harness 0.3.0-test"
    exit 0
fi

echo "fixture executable"
