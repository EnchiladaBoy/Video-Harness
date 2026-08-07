#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PUBLIC_KEY="${1:-}"
OUTPUT_DIR="${2:-}"
REPOSITORY_URL="${3:-https://enchiladaboy.github.io/Video-Harness/}"

if [[ -z "${PUBLIC_KEY}" || -z "${OUTPUT_DIR}" ]]; then
    echo "Usage: $0 PUBLIC_KEY OUTPUT_DIR [REPOSITORY_URL]" >&2
    exit 2
fi
if [[ ! -f "${PUBLIC_KEY}" ]]; then
    echo "Release public key not found: ${PUBLIC_KEY}" >&2
    exit 1
fi
if [[ "${REPOSITORY_URL}" != https://* || "${REPOSITORY_URL}" != */ ]]; then
    echo "Repository URL must be HTTPS and end with '/': ${REPOSITORY_URL}" >&2
    exit 1
fi

mkdir -p -- "${OUTPUT_DIR}"
KEY_BASE64="$(base64 --wrap=0 -- "${PUBLIC_KEY}")"

for basename in VideoHarness.flatpakref VideoHarness.flatpakrepo; do
    sed \
        -e "s|@REPO_URL@|${REPOSITORY_URL}|g" \
        -e "s|@GPG_KEY_BASE64@|${KEY_BASE64}|g" \
        "${SCRIPT_DIR}/${basename}.in" >"${OUTPUT_DIR}/${basename}.new"
    mv -f -- "${OUTPUT_DIR}/${basename}.new" "${OUTPUT_DIR}/${basename}"
done

install -m 0644 -- "${PUBLIC_KEY}" "${OUTPUT_DIR}/VideoHarness-release-key.asc"
