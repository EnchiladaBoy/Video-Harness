# Video Harness native workspace

This directory contains the Rust backend and GTK4/libadwaita application for Video Harness v0.6.0. See the [project README](../README.md) for product behavior and installation.

## Fedora prerequisites

```bash
sudo dnf install gcc gtk4-devel libadwaita-devel
```

The intended host uses Rust 1.92 or newer. TLS uses Rustls and SQLite is bundled.

## Build and verify

```bash
cargo build --release --locked --bin video-harness
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Tests do not read the user's keyring, call a paid inference endpoint, or submit real generations.

## Install

```bash
./install.sh install
video-harness
```

The installer stages an immutable `video-harness` binary and installs the desktop, AppStream, and icon assets. It never changes `openrouter-video` or removes application data.

The same installer accepts x86_64 and aarch64 prebuilt binaries. Release
tarballs place one at `native/bin/video-harness`, so no Rust toolchain is needed
when installing a bundle. `./install.sh uninstall` removes only the managed
launcher and byte-for-byte unmodified desktop integration files; application
data and immutable releases remain available.

Compatibility-sensitive XDG and keyring IDs deliberately remain `openrouter-video-studio`. Generation history remains schema v2 in `history.sqlite3`; GUI draft/upload state uses the separate `gui-state.sqlite3` sidecar.
