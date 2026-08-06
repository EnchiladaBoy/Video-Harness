#!/usr/bin/env bash
set -euo pipefail

NATIVE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LIB_ROOT="${OPENROUTER_VIDEO_LIB_DIR:-${HOME}/.local/lib/openrouter-video-studio}"
BIN_DIR="${OPENROUTER_VIDEO_BIN_DIR:-${HOME}/.local/bin}"
RELEASES_DIR="${LIB_ROOT}/releases"
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
  ./install.sh install [PATH_TO_BINARY]  Stage a release and create Rust/Python explicit aliases
  ./install.sh promote                   Atomically point openrouter-video at the staged Rust beta
  ./install.sh rollback                  Atomically restore the pre-promotion openrouter-video target
  ./install.sh status                    Show beta, stable, and rollback targets

Install never changes openrouter-video. Promotion is explicit and rollback keeps
the Python environment and application data untouched.
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

binary_version() {
    local binary="$1"
    local output version
    output="$("${binary}" --version 2>/dev/null)" || fail "${binary} did not pass its --version smoke test"
    version="${output##* }"
    [[ "${version}" =~ ^[0-9][A-Za-z0-9._+-]*$ ]] || fail "Could not derive a safe release version from: ${output}"
    printf '%s\n' "${version}"
}

install_release() {
    local source_binary="${1:-}"
    if [[ -z "${source_binary}" ]]; then
        local cargo_bin
        cargo_bin="$(command -v cargo 2>/dev/null || true)"
        if [[ -z "${cargo_bin}" && -x "${HOME}/.cargo/bin/cargo" ]]; then
            cargo_bin="${HOME}/.cargo/bin/cargo"
        fi
        [[ -n "${cargo_bin}" ]] || fail "cargo is required to build the native release"
        "${cargo_bin}" build --release --locked --manifest-path "${NATIVE_DIR}/Cargo.toml"
        source_binary="${NATIVE_DIR}/target/release/openrouter-video"
    fi
    [[ -f "${source_binary}" && -x "${source_binary}" ]] || fail "Native executable not found: ${source_binary}"

    local machine
    machine="$(uname -m)"
    [[ "${machine}" == "aarch64" ]] || fail "This installer currently supports Fedora ARM64; found ${machine}"

    local version release_dir release_binary staging
    version="$(binary_version "${source_binary}")"
    release_dir="${RELEASES_DIR}/${version}"
    release_binary="${release_dir}/openrouter-video"

    if [[ -e "${release_dir}" ]]; then
        [[ -x "${release_binary}" ]] || fail "Existing release directory is incomplete: ${release_dir}"
        cmp -s -- "${source_binary}" "${release_binary}" \
            || fail "Release ${version} already exists with different bytes; use a new version"
    else
        staging="$(mktemp -d "${RELEASES_DIR}/.${version}.tmp.XXXXXX")"
        trap 'chmod -R u+w -- "${staging:-/nonexistent}" 2>/dev/null || true; rm -rf -- "${staging:-/nonexistent}" 2>/dev/null || true' EXIT
        install -m 0555 -- "${source_binary}" "${staging}/openrouter-video"
        "${staging}/openrouter-video" --version >/dev/null \
            || fail "Staged executable failed its smoke test"
        chmod 0555 -- "${staging}"
        mv -- "${staging}" "${release_dir}"
        staging=""
        trap - EXIT
    fi

    if [[ -e "${PYTHON_LINK}" && ! -L "${PYTHON_LINK}" ]]; then
        fail "Refusing to replace the regular file at ${PYTHON_LINK}"
    fi
    if [[ ! -L "${PYTHON_LINK}" ]]; then
        [[ -L "${STABLE_LINK}" ]] \
            || fail "The current Python launcher is not a symlink at ${STABLE_LINK}"
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
    atomic_link "${release_binary}" "${BETA_LINK}"
    echo "Installed immutable native release: ${release_binary}"
    echo "Beta command: ${BETA_LINK}"
    echo "Python rollback command: ${PYTHON_LINK} -> $(link_target "${PYTHON_LINK}")"
    echo "Stable command was not changed: ${STABLE_LINK} -> $(link_target "${STABLE_LINK}")"
}

promote_release() {
    [[ -L "${BETA_LINK}" ]] || fail "Install and test openrouter-video-rs before promotion"
    local beta_target
    beta_target="$(readlink -f -- "${BETA_LINK}")"
    [[ -x "${beta_target}" ]] || fail "Beta target is not executable: ${beta_target}"
    case "${beta_target}" in
        "${RELEASES_DIR}"/*/openrouter-video) ;;
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
    echo "Beta:    ${BETA_LINK} -> $(link_target "${BETA_LINK}")"
    echo "Stable:  ${STABLE_LINK} -> $(link_target "${STABLE_LINK}")"
    echo "Python:  ${PYTHON_LINK} -> $(link_target "${PYTHON_LINK}")"
    echo "Rollback:${ROLLBACK_LINK} -> $(link_target "${ROLLBACK_LINK}")"
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
    install) [[ $# -le 1 ]] || fail "install accepts at most one binary path"; install_release "${1:-}" ;;
    promote) [[ $# -eq 0 ]] || fail "promote takes no arguments"; promote_release ;;
    rollback) [[ $# -eq 0 ]] || fail "rollback takes no arguments"; rollback_release ;;
    status) [[ $# -eq 0 ]] || fail "status takes no arguments"; show_status ;;
esac
