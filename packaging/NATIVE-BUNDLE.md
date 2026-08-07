# Native bundle notes

This archive is the best-effort Video Harness package for its named CPU
architecture. Flatpak is the primary cross-distribution release.

The release binary is built in the official Rust 1.95 Debian 13 (Trixie)
container. It embeds the Svelte interface and dynamically uses the host's
glibc, GTK 3, WebKitGTK 4.1, graphics, sound, and GStreamer stack. H.264
playback also needs the appropriate GStreamer plugins for the distribution.
Use the Flatpak when these native dependencies are unavailable or incompatible.

Install for the current user:

```bash
./install.sh
```

The installer places an immutable binary below
`~/.local/lib/openrouter-video-studio/releases/`, a launcher in
`~/.local/bin`, and standard desktop metadata in `~/.local/share`. The legacy
directory name preserves compatibility with existing installations.

Uninstall the managed launcher and unmodified desktop metadata:

```bash
./install.sh uninstall
```

Uninstall never removes releases, credentials, settings, history, generated
videos, provider data, or any modified integration file. Native bundles do not
update automatically; install a newer archive manually.
