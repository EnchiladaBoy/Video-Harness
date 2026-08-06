# Video Harness native workspace

This directory contains the Rust backend, GTK4/libadwaita application, and transition-release Ratatui interface for Video Harness v0.3. See the [project README](../README.md) for product behavior and installation.

## Fedora prerequisites

```bash
sudo dnf install gcc gtk4-devel libadwaita-devel
```

The intended host uses Rust 1.92 or newer. TLS uses Rustls and SQLite is bundled.

## Build and verify

```bash
cargo build --release --locked --bins
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

The installer stages immutable `video-harness` and `video-harness-tui` binaries, installs the desktop/AppStream/icon assets, and atomically updates their user-local links. `openrouter-video-rs` remains a compatibility alias for the TUI. `openrouter-video` stays unchanged unless the legacy `promote` command is explicitly used; `rollback` restores its recorded target.

Compatibility-sensitive XDG and keyring IDs deliberately remain `openrouter-video-studio`. Generation history remains schema v2 in `history.sqlite3`; GUI draft/upload state uses the separate `gui-state.sqlite3` sidecar.
