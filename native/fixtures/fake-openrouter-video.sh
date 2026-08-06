#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
    echo "openrouter-video 0.2.0-test"
    exit 0
fi

echo "fixture executable"

