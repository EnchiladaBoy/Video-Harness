#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || -z "$1" ]]; then
    echo "Usage: packaging/prepare-tauri-appimage-tools.sh TAURI_TOOLS_DIR" >&2
    exit 2
fi

tools_dir="${1%/}"
mkdir -p -- "${tools_dir}"

for required_command in curl sha256sum; do
    command -v "${required_command}" >/dev/null 2>&1 || {
        echo "${required_command} is required to prepare pinned AppImage tools" >&2
        exit 1
    }
done

case "$(uname -m)" in
    x86_64|amd64)
        tools_arch="x86_64"
        apprun_sha256="f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f"
        linuxdeploy_sha256="e762bea85c8eb0d4b3508d46e5c1f037f717d0f9303ae3b4aafc8b04991fa1ef"
        appimage_plugin_sha256="a45d3e227bc7f397e9cf6bfa4c9507494efa2293357b6e86690a3de2ca992e79"
        ;;
    aarch64|arm64)
        tools_arch="aarch64"
        apprun_sha256="072f17c0895a85c490282fe5395c5007e5fc75da727e553b3b8fb680feb11578"
        linuxdeploy_sha256="b12b5cc57bd0921e1f98d73f58aa364503bc1a27f54b7a69fd2870bce7fa2f55"
        appimage_plugin_sha256="6fdecf5bf8af4e0db03c6b2a80976acc3c96b6a4d19622fa6c6adfd308378bbc"
        ;;
    *)
        echo "Unsupported AppImage tool architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

temporary_files=()
cleanup() {
    local temporary
    for temporary in "${temporary_files[@]}"; do
        rm -f -- "${temporary}"
    done
}
trap cleanup EXIT

verify_or_download() {
    local filename="$1"
    local expected_sha256="$2"
    local url="$3"
    local destination="${tools_dir}/${filename}"
    local actual_sha256 temporary

    if [[ -f "${destination}" && ! -L "${destination}" ]]; then
        actual_sha256="$(sha256sum -- "${destination}")"
        actual_sha256="${actual_sha256%% *}"
        if [[ "${actual_sha256}" == "${expected_sha256}" ]]; then
            chmod 0755 -- "${destination}"
            echo "Verified cached AppImage tool: ${filename}"
            return
        fi
    fi

    temporary="$(mktemp "${tools_dir}/.${filename}.download.XXXXXXXX")"
    temporary_files+=("${temporary}")
    curl \
        --proto '=https' \
        --tlsv1.2 \
        --fail \
        --location \
        --retry 3 \
        --retry-all-errors \
        --connect-timeout 30 \
        --silent \
        --show-error \
        --output "${temporary}" \
        "${url}"
    actual_sha256="$(sha256sum -- "${temporary}")"
    actual_sha256="${actual_sha256%% *}"
    if [[ "${actual_sha256}" != "${expected_sha256}" ]]; then
        echo "Checksum mismatch for ${filename}: expected ${expected_sha256}, got ${actual_sha256}" >&2
        exit 1
    fi
    chmod 0755 -- "${temporary}"
    mv -fT -- "${temporary}" "${destination}"
    echo "Downloaded and verified AppImage tool: ${filename}"
}

# Tauri CLI 2.11.4 otherwise downloads these executables from mutable
# master/continuous endpoints and trusts them immediately. Keep the plugin
# scripts on immutable commits and bind every downloaded byte to a reviewed
# SHA-256. If an upstream release asset changes, packaging fails before any of
# it executes and the hashes must be reviewed deliberately.
verify_or_download \
    "AppRun-${tools_arch}" \
    "${apprun_sha256}" \
    "https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-${tools_arch}"
verify_or_download \
    "linuxdeploy-${tools_arch}.AppImage" \
    "${linuxdeploy_sha256}" \
    "https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-${tools_arch}.AppImage"
verify_or_download \
    "linuxdeploy-plugin-gtk.sh" \
    "cb379f9b0733e9ad9f8bd78f8c2fa038aef2478523bb7d4c8e64ff6a1ea3501a" \
    "https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/b5eb8d05b4c0ed40107fe2158c5d8527f94568ef/linuxdeploy-plugin-gtk.sh"
verify_or_download \
    "linuxdeploy-plugin-gstreamer.sh" \
    "c107b49d84edbffc6ab226ed1007e0626a4f7aa2c3a36b7782bef62351d49e94" \
    "https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gstreamer/2a2e67491c32995a3f279ad0ecbe77abd512b42a/linuxdeploy-plugin-gstreamer.sh"
verify_or_download \
    "linuxdeploy-plugin-appimage.AppImage" \
    "${appimage_plugin_sha256}" \
    "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-${tools_arch}.AppImage"
