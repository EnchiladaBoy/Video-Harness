#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "${SCRIPT_DIR}")"

npm --prefix "${PROJECT_DIR}/ui" ci
npm --prefix "${PROJECT_DIR}/ui" run check
npm --prefix "${PROJECT_DIR}/ui" test
npm --prefix "${PROJECT_DIR}/ui" run build

git -C "${PROJECT_DIR}" diff --exit-code -- ui/package-lock.json
[[ -f "${PROJECT_DIR}/ui/dist/index.html" ]] || {
    echo "Locked UI build did not create ui/dist/index.html" >&2
    exit 1
}
"${SCRIPT_DIR}/hash-ui-source.sh" >"${PROJECT_DIR}/ui/dist/.source-sha256"

echo "Built and validated the Flatpak UI from ui/package-lock.json."
