#!/usr/bin/env bash
set -uo pipefail

if [[ "$#" -lt 2 ]]; then
    echo "usage: $0 <annotation title> <command> [args ...]" >&2
    exit 2
fi

title="$1"
shift

log_file="$(mktemp)"
trap 'rm -f -- "${log_file}"' EXIT

set +e
NO_COLOR=1 CARGO_TERM_COLOR=never "$@" 2>&1 | tee "${log_file}"
status="${PIPESTATUS[0]}"
set -e

if [[ "${status}" -ne 0 ]]; then
    summary="$(tail -n 25 "${log_file}")"
    if [[ -z "${summary}" ]]; then
        summary="Command exited with status ${status}."
    fi
    summary="${summary:0:6000}"
    title="${title//'%'/'%25'}"
    title="${title//$'\r'/'%0D'}"
    title="${title//$'\n'/'%0A'}"
    summary="${summary//'%'/'%25'}"
    summary="${summary//$'\r'/'%0D'}"
    summary="${summary//$'\n'/'%0A'}"
    printf '::error title=%s::%s\n' "${title}" "${summary}"
fi

exit "${status}"
