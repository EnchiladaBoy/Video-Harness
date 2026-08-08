#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 MANIFEST_PATH LOCKFILE_PATH" >&2
  exit 2
}

[[ "$#" -eq 2 ]] || usage

manifest_path="$1"
lockfile_path="$2"
ignored_advisory="RUSTSEC-2026-0235"
ignored_package="rkyv v0.7.46"
reviewed_glib_advisory="RUSTSEC-2024-0429"
reviewed_glib_package="glib v0.18.5"

[[ -f "${manifest_path}" ]] || {
  echo "manifest not found: ${manifest_path}" >&2
  exit 2
}
[[ -f "${lockfile_path}" ]] || {
  echo "lockfile not found: ${lockfile_path}" >&2
  exit 2
}

# Keep the exception coupled to the one reviewed lockfile entry. If Cargo
# removes or changes it, stop and require a fresh advisory review instead of
# silently carrying the ignore forward to a different rkyv release.
if ! awk '
  $0 == "name = \"rkyv\"" {
    getline
    if ($0 == "version = \"0.7.46\"") reviewed++
  }
  END { exit reviewed == 1 ? 0 : 1 }
' "${lockfile_path}"; then
  echo "${lockfile_path} no longer has exactly one reviewed ${ignored_package} entry; review ${ignored_advisory}" >&2
  exit 1
fi

# rust_decimal 1.42.1 declares rkyv 0.7 as an optional integration, so Cargo
# records it in Cargo.lock even when that feature is disabled. cargo-audit scans
# every lockfile entry and reports RUSTSEC-2026-0235, while Cargo's active graph
# for this application contains no rkyv code. Do not let this exception hide a
# future feature change: fail before auditing if the vulnerable package becomes
# reachable for any supported target.
active_packages="$(
  cargo tree \
    --locked \
    --manifest-path "${manifest_path}" \
    --target all \
    --prefix none \
    --format '{p}'
)"

if grep -E '^rkyv v' <<<"${active_packages}" >/dev/null; then
  echo "an rkyv package is active; remove the ${ignored_advisory} exception and upgrade it" >&2
  exit 1
fi

audit_args=(
  --file "${lockfile_path}"
  --deny unsound
  --ignore "${ignored_advisory}"
)

# Tauri's Linux WebKitGTK runtime currently brings in GTK 3 and glib 0.18.5.
# RUSTSEC-2024-0429 affects VariantStrIter, which neither this application nor
# its locked dependency sources call. Keep this narrow exception tied to the
# exact active version: a new glib release or dependency-graph change must be
# reviewed, while every other current or future unsoundness warning fails CI.
if grep -E '^glib v0\.18\.5([[:space:]]|$)' <<<"${active_packages}" >/dev/null; then
  if ! awk '
    $0 == "name = \"glib\"" {
      getline
      if ($0 == "version = \"0.18.5\"") reviewed++
    }
    END { exit reviewed == 1 ? 0 : 1 }
  ' "${lockfile_path}"; then
    echo "${lockfile_path} no longer has exactly one reviewed ${reviewed_glib_package} entry; review ${reviewed_glib_advisory}" >&2
    exit 1
  fi
  audit_args+=(--ignore "${reviewed_glib_advisory}")
fi

cargo audit "${audit_args[@]}"
