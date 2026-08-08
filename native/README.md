# Video-Harness portable Rust engine

This directory contains the provider workflows, safety checks, persistence,
downloads, and platform-neutral application services used by Video Harness
v0.7.1. The stable desktop interface lives in [`../ui`](../ui) and is hosted by
Tauri from [`../desktop/src-tauri`](../desktop/src-tauri).

The older GTK4/libadwaita frontend is retained behind the `legacy-gtk` feature
as a deprecated, maintenance-only developer tool. It is not a supported
release frontend. See [LEGACY-GTK.md](LEGACY-GTK.md) for the parity audit and
removal policy, and the [project README](../README.md) for normal desktop use.

## Build and test the portable engine

The intended toolchain is Rust 1.95 or newer. TLS uses Rustls and SQLite is
bundled, so the engine does not need OpenSSL or a system SQLite development
package.

```bash
cargo build --locked --lib --no-default-features
cargo fmt --check
cargo clippy --locked --lib --tests --no-default-features -- -D warnings
cargo test --locked --lib --tests --no-default-features
```

These are the core commands used for Linux, Windows x64, and macOS Apple
Silicon checks. Supported packages are produced by the canonical
Tauri/Svelte application, not this crate's legacy frontend.

Tests use temporary databases, deterministic transports, and in-memory
credentials. They do not read the user's keyring, contact a paid inference
endpoint, or submit a real generation.

## Deprecated GTK frontend

Building the compatibility frontend requires a C toolchain, GTK 4.10 or newer,
and libadwaita 1.6 or newer. Package names vary by distribution:

```bash
# Debian / Ubuntu
sudo apt install build-essential libgtk-4-dev libadwaita-1-dev

# Fedora
sudo dnf install gcc gtk4-devel libadwaita-devel

# Arch Linux
sudo pacman -S base-devel gtk4 libadwaita
```

Build, run, or test it explicitly with the `legacy-gtk` feature:

```bash
cargo build --release --locked --features legacy-gtk --bin video-harness-gtk
cargo run --locked --features legacy-gtk --bin video-harness-gtk
cargo test --all-targets --locked --features legacy-gtk
```

The compatibility binary uses the same provider engine and data identity, but
new interface work belongs in the Svelte/Tauri application.

## Installer compatibility

`native/install.sh` is the staging helper used by Linux release archives. Given
an already-built stable desktop executable, it installs an immutable binary and
the desktop, AppStream, and icon assets for the current user:

```bash
./native/install.sh install desktop/src-tauri/target/release/video-harness
```

Run that command from the repository root. Release archives include the binary,
so their top-level `./install.sh` needs no Rust or Node.js toolchain. The helper
accepts x86_64 and aarch64 Linux executables and never changes the separate
`openrouter-video` command.

`./native/install.sh uninstall` removes only the managed launcher and
byte-for-byte unmodified desktop integration files. Application data and
immutable releases remain available.

Compatibility-sensitive XDG paths and keyring IDs deliberately remain
`openrouter-video-studio`. Generation history remains schema v2 in
`history.sqlite3`; draft and upload state uses the separate
`gui-state.sqlite3` sidecar.
