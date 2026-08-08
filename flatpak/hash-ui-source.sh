#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"

git -C "${PROJECT_DIR}" ls-files --cached --others --exclude-standard -z -- ui \
    | LC_ALL=C sort -z \
    | while IFS= read -r -d '' source_file; do
        source_hash="$(sha256sum -- "${PROJECT_DIR}/${source_file}" | cut -d ' ' -f 1)"
        printf '%s  %s\n' "${source_hash}" "${source_file}"
    done \
    | sha256sum \
    | cut -d ' ' -f 1
