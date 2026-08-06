#!/usr/bin/env bash
set -euo pipefail

NATIVE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${VIDEO_HARNESS_PROJECT_DIR:-$(dirname -- "${NATIVE_DIR}")}"
LIB_ROOT="${OPENROUTER_VIDEO_LIB_DIR:-${HOME}/.local/lib/openrouter-video-studio}"
BIN_DIR="${OPENROUTER_VIDEO_BIN_DIR:-${HOME}/.local/bin}"
DATA_ROOT="${VIDEO_HARNESS_DATA_DIR:-${XDG_DATA_HOME:-${HOME}/.local/share}}"
RELEASES_DIR="${LIB_ROOT}/releases"
GUI_LINK="${BIN_DIR}/video-harness"
LEGACY_TUI_LINK="${BIN_DIR}/video-harness-tui"
LEGACY_RUST_LINK="${BIN_DIR}/openrouter-video-rs"
LEGACY_PYTHON_LINK="${BIN_DIR}/openrouter-video-python"
STABLE_LINK="${BIN_DIR}/openrouter-video"
LOCK_FILE="${LIB_ROOT}/install.lock"

usage() {
    cat <<'EOF'
Usage:
  ./install.sh install [GUI_BINARY]   Stage Video Harness and install desktop files
  ./install.sh status                 Show the installed GUI target
  ./install.sh uninstall              Remove launchers and unmodified desktop files

Install accepts x86_64 and aarch64 Linux. With no GUI_BINARY it uses a binary
bundled at native/bin/video-harness, or builds the source tree as a fallback.
It never changes openrouter-video or removes credentials, settings, history,
downloads, provider data, or immutable releases.
EOF
}

fail() {
    echo "Error: $*" >&2
    exit 1
}

link_target() {
    local link_path="$1"
    if [[ -L "${link_path}" ]]; then
        readlink -- "${link_path}"
    elif [[ -e "${link_path}" ]]; then
        printf '%s\n' "[regular file: ${link_path}]"
    else
        printf '%s\n' "[not installed]"
    fi
}

atomic_link() {
    local target="$1"
    local destination="$2"
    local temporary
    temporary="${destination}.new.$$"
    ln -s -- "${target}" "${temporary}"
    mv -Tf -- "${temporary}" "${destination}"
}

require_link_or_absent() {
    local destination="$1"
    if [[ -e "${destination}" && ! -L "${destination}" ]]; then
        fail "Refusing to replace the regular file at ${destination}"
    fi
}

binary_version() {
    local binary="$1"
    local output version
    output="$("${binary}" --version 2>/dev/null)" || fail "${binary} did not pass its --version smoke test"
    version="${output##* }"
    [[ "${version}" =~ ^[0-9][A-Za-z0-9._+-]*$ ]] || fail "Could not derive a safe release version from: ${output}"
    printf '%s\n' "${version}"
}

retire_release_alias() {
    local alias_path="$1"
    [[ -L "${alias_path}" ]] || return 0

    local resolved_target
    resolved_target="$(readlink -f -- "${alias_path}" 2>/dev/null || true)"
    case "${resolved_target}" in
        "${RELEASES_DIR}"/*/video-harness-tui|"${RELEASES_DIR}"/*/openrouter-video)
            rm -f -- "${alias_path}"
            ;;
    esac
}

retire_python_alias() {
    [[ -L "${LEGACY_PYTHON_LINK}" ]] || return 0

    local python_target known_project_target
    python_target="$(readlink -f -- "${LEGACY_PYTHON_LINK}" 2>/dev/null || true)"
    known_project_target="${PROJECT_DIR}/.venv/bin/openrouter-video"

    if [[ "${python_target}" == "${known_project_target}" ]]; then
        rm -f -- "${LEGACY_PYTHON_LINK}"
    fi
}

retire_owned_transition_aliases() {
    retire_release_alias "${LEGACY_TUI_LINK}"
    retire_release_alias "${LEGACY_RUST_LINK}"
    retire_python_alias
}

install_release() {
    local source_gui="${1:-}"
    if [[ -z "${source_gui}" ]]; then
        if [[ -x "${NATIVE_DIR}/bin/video-harness" ]]; then
            source_gui="${NATIVE_DIR}/bin/video-harness"
        else
            local cargo_bin
            cargo_bin="$(command -v cargo 2>/dev/null || true)"
            if [[ -z "${cargo_bin}" && -x "${HOME}/.cargo/bin/cargo" ]]; then
                cargo_bin="${HOME}/.cargo/bin/cargo"
            fi
            [[ -n "${cargo_bin}" ]] || fail "cargo is required to build the native release"
            "${cargo_bin}" build --release --locked --bin video-harness --manifest-path "${NATIVE_DIR}/Cargo.toml"
            source_gui="${NATIVE_DIR}/target/release/video-harness"
        fi
    fi
    [[ -f "${source_gui}" && -x "${source_gui}" ]] || fail "GUI executable not found: ${source_gui}"

    local machine
    machine="$(uname -m)"
    case "${machine}" in
        x86_64|amd64|aarch64|arm64) ;;
        *) fail "This package supports x86_64 and aarch64 Linux; found ${machine}" ;;
    esac

    local version release_dir release_gui staging
    version="$(binary_version "${source_gui}")"
    release_dir="${RELEASES_DIR}/${version}"
    release_gui="${release_dir}/video-harness"

    if [[ -e "${release_dir}" ]]; then
        [[ -x "${release_gui}" ]] || fail "Existing release directory is incomplete: ${release_dir}"
        cmp -s -- "${source_gui}" "${release_gui}" \
            || fail "Release ${version} already exists with different bytes; use a new version"
    else
        staging="$(mktemp -d "${RELEASES_DIR}/.${version}.tmp.XXXXXX")"
        trap 'chmod -R u+w -- "${staging:-/nonexistent}" 2>/dev/null || true; rm -rf -- "${staging:-/nonexistent}" 2>/dev/null || true' EXIT
        install -m 0555 -- "${source_gui}" "${staging}/video-harness"
        "${staging}/video-harness" --version >/dev/null || fail "Staged GUI failed its smoke test"
        chmod 0555 -- "${staging}"
        mv -- "${staging}" "${release_dir}"
        staging=""
        trap - EXIT
    fi

    require_link_or_absent "${GUI_LINK}"
    atomic_link "${release_gui}" "${GUI_LINK}"
    retire_owned_transition_aliases

    install -Dm0644 -- "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.desktop" \
        "${DATA_ROOT}/applications/io.github.EnchiladaBoy.VideoHarness.desktop"
    install -Dm0644 -- "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
        "${DATA_ROOT}/metainfo/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
    install -Dm0644 -- "${NATIVE_DIR}/data/icons/io.github.EnchiladaBoy.VideoHarness.svg" \
        "${DATA_ROOT}/icons/hicolor/scalable/apps/io.github.EnchiladaBoy.VideoHarness.svg"

    echo "Installed immutable Video Harness release: ${release_dir}"
    echo "GUI command: ${GUI_LINK}"
    echo "Existing openrouter-video command was not changed: ${STABLE_LINK} -> $(link_target "${STABLE_LINK}")"
}

show_status() {
    echo "GUI: ${GUI_LINK} -> $(link_target "${GUI_LINK}")"
}

remove_if_unmodified() {
    local source="$1"
    local destination="$2"
    [[ -e "${destination}" || -L "${destination}" ]] || return 0

    if [[ -f "${destination}" && ! -L "${destination}" ]] && cmp -s -- "${source}" "${destination}"; then
        rm -f -- "${destination}"
    else
        echo "Preserved modified or unexpected file: ${destination}" >&2
    fi
}

uninstall_release() {
    if [[ -L "${GUI_LINK}" ]]; then
        local target
        target="$(readlink -f -- "${GUI_LINK}" 2>/dev/null || true)"
        case "${target}" in
            "${RELEASES_DIR}"/*/video-harness) rm -f -- "${GUI_LINK}" ;;
            *) echo "Preserved unowned launcher: ${GUI_LINK}" >&2 ;;
        esac
    elif [[ -e "${GUI_LINK}" ]]; then
        echo "Preserved regular launcher: ${GUI_LINK}" >&2
    fi

    remove_if_unmodified \
        "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.desktop" \
        "${DATA_ROOT}/applications/io.github.EnchiladaBoy.VideoHarness.desktop"
    remove_if_unmodified \
        "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
        "${DATA_ROOT}/metainfo/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
    remove_if_unmodified \
        "${NATIVE_DIR}/data/icons/io.github.EnchiladaBoy.VideoHarness.svg" \
        "${DATA_ROOT}/icons/hicolor/scalable/apps/io.github.EnchiladaBoy.VideoHarness.svg"

    echo "Removed Video Harness integration files that were still unmodified."
    echo "Application data and immutable releases were preserved under ${LIB_ROOT}."
}

command_name="${1:-install}"
if [[ $# -gt 0 ]]; then
    shift
fi
case "${command_name}" in
    install|status|uninstall) ;;
    -h|--help|help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

mkdir -p -- "${LIB_ROOT}" "${RELEASES_DIR}" "${BIN_DIR}"
exec 9>"${LOCK_FILE}"
if command -v flock >/dev/null 2>&1; then
    flock 9
fi

case "${command_name}" in
    install) [[ $# -le 1 ]] || fail "install accepts at most one binary path"; install_release "${1:-}" ;;
    status) [[ $# -eq 0 ]] || fail "status takes no arguments"; show_status ;;
    uninstall) [[ $# -eq 0 ]] || fail "uninstall takes no arguments"; uninstall_release ;;
esac
