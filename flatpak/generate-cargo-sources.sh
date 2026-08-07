#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LOCK_FILE="${SCRIPT_DIR}/../desktop/src-tauri/Cargo.lock"
OUTPUT_FILE="${1:-${SCRIPT_DIR}/cargo-sources.json}"
TEMP_FILE="${OUTPUT_FILE}.new.$$"

trap 'rm -f -- "${TEMP_FILE}"' EXIT

awk '
function reset_package() {
    package_name = ""
    package_version = ""
    package_source = ""
    package_checksum = ""
}
function json_value(line, value) {
    value = line
    sub(/^[^=]*= "/, "", value)
    sub(/"$/, "", value)
    return value
}
function separator() {
    if (source_count > 0) {
        print ","
    }
    source_count++
}
function emit_package(dest) {
    if (package_source == "") {
        return
    }
    if (package_source !~ /^registry\+https:\/\/github.com\/rust-lang\/crates.io-index$/) {
        print "Unsupported Cargo.lock source for " package_name ": " package_source > "/dev/stderr"
        exit 4
    }
    if (package_name == "" || package_version == "" || package_checksum == "") {
        print "Incomplete crates.io package in Cargo.lock" > "/dev/stderr"
        exit 3
    }

    dest = "cargo/vendor/" package_name "-" package_version
    separator()
    print "  {"
    print "    \"type\": \"archive\","
    print "    \"archive-type\": \"tar-gzip\","
    print "    \"url\": \"https://static.crates.io/crates/" package_name "/" package_name "-" package_version ".crate\","
    print "    \"sha256\": \"" package_checksum "\","
    print "    \"dest\": \"" dest "\""
    print "  },"
    print "  {"
    print "    \"type\": \"inline\","
    print "    \"contents\": \"{\\\"package\\\":\\\"" package_checksum "\\\",\\\"files\\\":{}}\","
    print "    \"dest\": \"" dest "\","
    print "    \"dest-filename\": \".cargo-checksum.json\""
    print "  }"
}
BEGIN {
    print "["
    source_count = 0
    reset_package()
}
/^\[\[package\]\]$/ {
    emit_package()
    reset_package()
    next
}
/^name = "/ { package_name = json_value($0); next }
/^version = "/ { package_version = json_value($0); next }
/^source = "/ { package_source = json_value($0); next }
/^checksum = "/ { package_checksum = json_value($0); next }
END {
    emit_package()
    separator()
    print "  {"
    print "    \"type\": \"inline\","
    print "    \"contents\": \"[source.crates-io]\\nreplace-with = \\\"vendored-sources\\\"\\n\\n[source.vendored-sources]\\ndirectory = \\\"vendor\\\"\\n\","
    print "    \"dest\": \"cargo\","
    print "    \"dest-filename\": \"config.toml\""
    print "  }"
    print "]"
}
' "${LOCK_FILE}" >"${TEMP_FILE}"

mv -f -- "${TEMP_FILE}" "${OUTPUT_FILE}"
trap - EXIT
