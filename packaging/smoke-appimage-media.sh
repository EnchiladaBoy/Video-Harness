#!/usr/bin/env bash
set -euo pipefail

APP_DIR="${1:-}"
if [[ $# -ne 1 || ! -d "${APP_DIR}" ]]; then
    echo "Usage: packaging/smoke-appimage-media.sh EXTRACTED-APPDIR" >&2
    exit 2
fi
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_B64="${SCRIPT_DIR}/fixtures/h264-aac.mp4.b64"
FIXTURE_SHA256="3610362c5ffadcc89d7da820beb2b4155218d45a54f2ef461af4e595ff996bc0"

for command_name in base64 gst-inspect-1.0 gst-launch-1.0 sha256sum; do
    command -v "${command_name}" >/dev/null 2>&1 || {
        echo "${command_name} is required for the AppImage media smoke test" >&2
        exit 1
    }
done
[[ -f "${FIXTURE_B64}" ]] || {
    echo "The checked-in H.264/AAC smoke fixture is missing" >&2
    exit 1
}

mapfile -d '' plugin_dirs < <(
    find "${APP_DIR}" -type d -name gstreamer-1.0 -print0 | LC_ALL=C sort -z
)
[[ "${#plugin_dirs[@]}" -gt 0 ]] || {
    echo "The AppImage has no bundled GStreamer plugin directory" >&2
    exit 1
}

library_roots=()
for candidate in "${APP_DIR}/usr/lib" "${APP_DIR}/usr/lib64"; do
    [[ -d "${candidate}" ]] && library_roots+=("${candidate}")
done
[[ "${#library_roots[@]}" -gt 0 ]] || {
    echo "The AppImage has no bundled library directory" >&2
    exit 1
}
mapfile -d '' library_dirs < <(
    find "${library_roots[@]}" -type d -print0 | LC_ALL=C sort -z
)

plugin_path="$(IFS=:; echo "${plugin_dirs[*]}")"
library_path="$(IFS=:; echo "${library_dirs[*]}")"
smoke_root="$(mktemp -d)"
cleanup() {
    rm -rf -- "${smoke_root}"
}
trap cleanup EXIT

gst_environment=(
    env
    "LD_LIBRARY_PATH=${library_path}"
    "GST_PLUGIN_PATH_1_0="
    "GST_PLUGIN_SYSTEM_PATH_1_0=${plugin_path}"
    "GST_REGISTRY_1_0=${smoke_root}/registry.bin"
)
plugin_scanner="$(find "${APP_DIR}" -type f -name gst-plugin-scanner -print -quit)"
if [[ -n "${plugin_scanner}" ]]; then
    gst_environment+=("GST_PLUGIN_SCANNER_1_0=${plugin_scanner}")
fi

for factory in playbin qtdemux h264parse avdec_h264 aacparse avdec_aac; do
    "${gst_environment[@]}" gst-inspect-1.0 "${factory}" >/dev/null || {
        echo "Bundled GStreamer factory ${factory} cannot be loaded" >&2
        exit 1
    }
done

fixture="${smoke_root}/h264-aac.mp4"
base64 --decode -- "${FIXTURE_B64}" >"${fixture}"
printf '%s  %s\n' "${FIXTURE_SHA256}" "${fixture}" | sha256sum --check --status

"${gst_environment[@]}" gst-launch-1.0 -q \
    filesrc location="${fixture}" ! qtdemux name=demux \
    demux.video_0 ! queue ! h264parse ! avdec_h264 ! fakesink sync=false \
    demux.audio_0 ! queue ! aacparse ! avdec_aac ! fakesink sync=false

echo "Bundled GStreamer decoded H.264 video and AAC audio from an MP4 fixture."
