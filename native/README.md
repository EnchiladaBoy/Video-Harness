# Video Studio Beta — native Rust edition

The native edition is a single Fedora ARM64 executable for provider-agnostic AI video generation. Version 0.2 ships with OpenRouter and fal.ai adapters, provider-scoped credentials and catalogs, a shared generation/history interface, and the same paid-request safety rules across both platforms.

The public title is temporarily **Video Studio Beta**. Compatibility-sensitive identifiers remain unchanged during beta: the package and executable are still named `openrouter-video`, application data remains under `openrouter-video-studio`, and the existing Python installation remains the stable rollback target.

## Build prerequisites

The Rust toolchain is installed under `~/.cargo/bin` on the intended Fedora ARM64 host. Add it to the shell and ensure Fedora's C compiler/linker package is present:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
sudo dnf install gcc
cd /home/alex/openrouter-video-studio/native
cargo build --release --locked
```

TLS uses Rustls and SQLite is bundled, so `openssl-devel`, `sqlite-devel`, and `dbus-devel` are not required. Cargo dependencies are locked; the application does not embed or require Python.

## Test without spending credits

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Integration tests use deterministic provider fixtures, memory-only credentials, temporary databases, and Ratatui's test backend. They never read the user's keyring, use a real API key, contact a paid inference endpoint, or submit a generation.

## Install the beta safely

```bash
chmod +x install.sh
./install.sh install
openrouter-video-rs
```

`install` builds and stages an immutable release at `~/.local/lib/openrouter-video-studio/releases/<version>/openrouter-video`, captures the current Python target as `openrouter-video-python`, then atomically updates only the `openrouter-video-rs` beta alias. It deliberately leaves `~/.local/bin/openrouter-video` pointing to the Python environment.

On first migration to provider-aware history, native v0.2 creates a no-clobber SQLite online backup named `history.sqlite3.pre-provider-v2.bak`. The legacy Python `jobs` table is not altered. OpenRouter rows remain compatible with Python, while provider-qualified history lives in an additive `generations` table so remote IDs from different platforms cannot collide. fal.ai rows are intentionally invisible to older OpenRouter-only executables but remain intact when rolling between versions.

The existing OpenRouter Secret Service entry is reused without copying or exposing it. Additional provider keys are stored under isolated usernames in the same legacy service. If Secret Service is unavailable, each key remains only in process memory for that session.

Check all targets at any time:

```bash
./install.sh status
```

After testing credential reuse, both provider catalogs, history, restart recovery, downloads, terminal resize, and errors through the beta alias, promotion remains explicit:

```bash
./install.sh promote
```

Promotion records the exact previous stable symlink and atomically switches `openrouter-video`. The explicit `openrouter-video-python` command remains usable throughout beta. Promotion does not delete the Python environment, credentials, history, caches, or videos.

Rollback is also atomic:

```bash
./install.sh rollback
```

Do not run the Python and native editions concurrently against the same history database. Executable rollback does not rewrite application data, and both pre-Rust/provider-migration safety backups remain available for manual recovery.
