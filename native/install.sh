#!/usr/bin/env bash
set -euo pipefail

NATIVE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LIB_ROOT="${OPENROUTER_VIDEO_LIB_DIR:-${HOME}/.local/lib/openrouter-video-studio}"
BIN_DIR="${OPENROUTER_VIDEO_BIN_DIR:-${HOME}/.local/bin}"
DATA_ROOT="${VIDEO_HARNESS_DATA_DIR:-${XDG_DATA_HOME:-${HOME}/.local/share}}"
RELEASES_DIR="${LIB_ROOT}/releases"
GUI_LINK="${BIN_DIR}/video-harness"
TUI_LINK="${BIN_DIR}/video-harness-tui"
BETA_LINK="${BIN_DIR}/openrouter-video-rs"
STABLE_LINK="${BIN_DIR}/openrouter-video"
PYTHON_LINK="${BIN_DIR}/openrouter-video-python"
ROLLBACK_DIR="${LIB_ROOT}/rollback"
ROLLBACK_LINK="${ROLLBACK_DIR}/openrouter-video.previous"
ROLLBACK_ABSENT="${ROLLBACK_DIR}/openrouter-video.was-absent"
LOCK_FILE="${LIB_ROOT}/install.lock"

usage() {
    cat <<'EOF'
Usage:
  ./install.sh install [GUI_BINARY] [TUI_BINARY]
                                      Stage Video Harness and install desktop files
  ./install.sh promote                   Atomically point openrouter-video at the staged Rust beta
  ./install.sh rollback                  Atomically restore the pre-promotion openrouter-video target
  ./install.sh status                    Show GUI, TUI, compatibility, and rollback targets

Install creates video-harness and video-harness-tui. It keeps openrouter-video-rs
as a transition alias for the TUI and never changes openrouter-video. Promotion
is explicit and rollback keeps Python and all application data untouched.
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

install_release() {
    local source_gui="${1:-}"
    local source_tui="${2:-}"
    if [[ -z "${source_gui}" ]]; then
        local cargo_bin
        cargo_bin="$(command -v cargo 2>/dev/null || true)"
        if [[ -z "${cargo_bin}" && -x "${HOME}/.cargo/bin/cargo" ]]; then
            cargo_bin="${HOME}/.cargo/bin/cargo"
        fi
        [[ -n "${cargo_bin}" ]] || fail "cargo is required to build the native release"
        "${cargo_bin}" build --release --locked --bins --manifest-path "${NATIVE_DIR}/Cargo.toml"
        source_gui="${NATIVE_DIR}/target/release/video-harness"
        source_tui="${NATIVE_DIR}/target/release/video-harness-tui"
    elif [[ -z "${source_tui}" ]]; then
        # Convenient for installer fixtures and downstream single-file wrappers.
        source_tui="${source_gui}"
    fi
    [[ -f "${source_gui}" && -x "${source_gui}" ]] || fail "GUI executable not found: ${source_gui}"
    [[ -f "${source_tui}" && -x "${source_tui}" ]] || fail "TUI executable not found: ${source_tui}"

    local machine
    machine="$(uname -m)"
    [[ "${machine}" == "aarch64" ]] || fail "This installer currently supports Fedora ARM64; found ${machine}"

    local version tui_version release_dir release_gui release_tui staging
    version="$(binary_version "${source_gui}")"
    tui_version="$(binary_version "${source_tui}")"
    [[ "${version}" == "${tui_version}" ]] || fail "GUI and TUI versions differ (${version} vs ${tui_version})"
    release_dir="${RELEASES_DIR}/${version}"
    release_gui="${release_dir}/video-harness"
    release_tui="${release_dir}/video-harness-tui"

    if [[ -e "${release_dir}" ]]; then
        [[ -x "${release_gui}" && -x "${release_tui}" ]] || fail "Existing release directory is incomplete: ${release_dir}"
        cmp -s -- "${source_gui}" "${release_gui}" \
            || fail "Release ${version} already exists with different bytes; use a new version"
        cmp -s -- "${source_tui}" "${release_tui}" \
            || fail "Release ${version} already exists with different bytes; use a new version"
    else
        staging="$(mktemp -d "${RELEASES_DIR}/.${version}.tmp.XXXXXX")"
        trap 'chmod -R u+w -- "${staging:-/nonexistent}" 2>/dev/null || true; rm -rf -- "${staging:-/nonexistent}" 2>/dev/null || true' EXIT
        install -m 0555 -- "${source_gui}" "${staging}/video-harness"
        install -m 0555 -- "${source_tui}" "${staging}/video-harness-tui"
        "${staging}/video-harness" --version >/dev/null || fail "Staged GUI failed its smoke test"
        "${staging}/video-harness-tui" --version >/dev/null || fail "Staged TUI failed its smoke test"
        chmod 0555 -- "${staging}"
        mv -- "${staging}" "${release_dir}"
        staging=""
        trap - EXIT
    fi

    if [[ -e "${PYTHON_LINK}" && ! -L "${PYTHON_LINK}" ]]; then
        fail "Refusing to replace the regular file at ${PYTHON_LINK}"
    fi
    if [[ ! -L "${PYTHON_LINK}" ]]; then
        if [[ -L "${STABLE_LINK}" ]]; then
            local stable_target
            stable_target="$(readlink -f -- "${STABLE_LINK}")"
            [[ -x "${stable_target}" ]] || fail "Current stable launcher target is not executable"
            case "${stable_target}" in
                "${RELEASES_DIR}"/*)
                    fail "Current stable launcher is native and no Python alias exists; restore the Python launcher before installing"
                    ;;
            esac
            atomic_link "${stable_target}" "${PYTHON_LINK}"
        fi
    fi
    require_link_or_absent "${GUI_LINK}"
    require_link_or_absent "${TUI_LINK}"
    require_link_or_absent "${BETA_LINK}"
    atomic_link "${release_gui}" "${GUI_LINK}"
    atomic_link "${release_tui}" "${TUI_LINK}"
    atomic_link "${release_tui}" "${BETA_LINK}"

    install -Dm0644 -- "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.desktop" \
        "${DATA_ROOT}/applications/io.github.EnchiladaBoy.VideoHarness.desktop"
    install -Dm0644 -- "${NATIVE_DIR}/data/io.github.EnchiladaBoy.VideoHarness.metainfo.xml" \
        "${DATA_ROOT}/metainfo/io.github.EnchiladaBoy.VideoHarness.metainfo.xml"
    install -Dm0644 -- "${NATIVE_DIR}/data/icons/io.github.EnchiladaBoy.VideoHarness.svg" \
        "${DATA_ROOT}/icons/hicolor/scalable/apps/io.github.EnchiladaBoy.VideoHarness.svg"

    echo "Installed immutable Video Harness release: ${release_dir}"
    echo "GUI command: ${GUI_LINK}"
    echo "TUI command: ${TUI_LINK}"
    echo "Legacy Rust alias: ${BETA_LINK}"
    echo "Python rollback command: ${PYTHON_LINK} -> $(link_target "${PYTHON_LINK}")"
    echo "Stable command was not changed: ${STABLE_LINK} -> $(link_target "${STABLE_LINK}")"
}

promote_release() {
    [[ -L "${BETA_LINK}" ]] || fail "Install and test openrouter-video-rs before promotion"
    local beta_target
    beta_target="$(readlink -f -- "${BETA_LINK}")"
    [[ -x "${beta_target}" ]] || fail "Beta target is not executable: ${beta_target}"
    case "${beta_target}" in
        "${RELEASES_DIR}"/*/video-harness-tui|"${RELEASES_DIR}"/*/openrouter-video) ;;
        *) fail "Beta target is outside the immutable releases directory" ;;
    esac
    "${beta_target}" --version >/dev/null || fail "Beta target failed its smoke test"

    if [[ -e "${STABLE_LINK}" && ! -L "${STABLE_LINK}" ]]; then
        fail "Refusing to replace the regular file at ${STABLE_LINK}"
    fi

    if [[ -L "${STABLE_LINK}" && "$(readlink -f -- "${STABLE_LINK}")" == "${beta_target}" ]]; then
        echo "Stable already points to this native release; rollback metadata was preserved."
        return
    fi

    if [[ -L "${STABLE_LINK}" ]]; then
        local previous_target
        previous_target="$(readlink -f -- "${STABLE_LINK}")"
        [[ -x "${previous_target}" ]] || fail "Current stable launcher target is not executable"
        atomic_link "${previous_target}" "${ROLLBACK_LINK}"
        rm -f -- "${ROLLBACK_ABSENT}"
    else
        : > "${ROLLBACK_ABSENT}"
        rm -f -- "${ROLLBACK_LINK}"
    fi
    atomic_link "${beta_target}" "${STABLE_LINK}"
    echo "Promoted atomically: ${STABLE_LINK} -> ${beta_target}"
    echo "Rollback target: $(link_target "${ROLLBACK_LINK}")"
}

rollback_release() {
    if [[ -e "${STABLE_LINK}" && ! -L "${STABLE_LINK}" ]]; then
        fail "Refusing to replace the regular file at ${STABLE_LINK}"
    fi
    if [[ -L "${ROLLBACK_LINK}" ]]; then
        local previous_target
        previous_target="$(readlink -f -- "${ROLLBACK_LINK}")"
        [[ -x "${previous_target}" ]] || fail "Recorded rollback target is not executable"
        atomic_link "${previous_target}" "${STABLE_LINK}"
        echo "Restored atomically: ${STABLE_LINK} -> $(readlink -- "${STABLE_LINK}")"
    elif [[ -f "${ROLLBACK_ABSENT}" ]]; then
        [[ ! -e "${STABLE_LINK}" || -L "${STABLE_LINK}" ]] \
            || fail "Refusing to remove the regular file at ${STABLE_LINK}"
        rm -f -- "${STABLE_LINK}"
        echo "Restored the previous state: ${STABLE_LINK} is not installed"
    elif [[ -L "${PYTHON_LINK}" ]]; then
        local python_target
        python_target="$(readlink -f -- "${PYTHON_LINK}")"
        [[ -x "${python_target}" ]] || fail "Python rollback target is not executable"
        atomic_link "${python_target}" "${STABLE_LINK}"
        echo "Restored Python alias atomically: ${STABLE_LINK} -> ${python_target}"
    else
        fail "No recorded promotion or Python alias is available to roll back"
    fi
}

show_status() {
    echo "GUI:      ${GUI_LINK} -> $(link_target "${GUI_LINK}")"
    echo "TUI:      ${TUI_LINK} -> $(link_target "${TUI_LINK}")"
    echo "Legacy:   ${BETA_LINK} -> $(link_target "${BETA_LINK}")"
    echo "Stable:   ${STABLE_LINK} -> $(link_target "${STABLE_LINK}")"
    echo "Python:   ${PYTHON_LINK} -> $(link_target "${PYTHON_LINK}")"
    echo "Rollback: ${ROLLBACK_LINK} -> $(link_target "${ROLLBACK_LINK}")"
}

command_name="${1:-install}"
if [[ $# -gt 0 ]]; then
    shift
fi
case "${command_name}" in
    install|promote|rollback|status) ;;
    -h|--help|help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

mkdir -p -- "${LIB_ROOT}" "${RELEASES_DIR}" "${ROLLBACK_DIR}" "${BIN_DIR}"
exec 9>"${LOCK_FILE}"
if command -v flock >/dev/null 2>&1; then
    flock 9
fi

case "${command_name}" in
    install) [[ $# -le 2 ]] || fail "install accepts at most two binary paths"; install_release "${1:-}" "${2:-}" ;;
    promote) [[ $# -eq 0 ]] || fail "promote takes no arguments"; promote_release ;;
    rollback) [[ $# -eq 0 ]] || fail "rollback takes no arguments"; rollback_release ;;
    status) [[ $# -eq 0 ]] || fail "status takes no arguments"; show_status ;;
esac
