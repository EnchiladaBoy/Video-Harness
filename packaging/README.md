# Release packaging

The primary Linux packages are unsigned Tauri AppImages: one directly runnable
file for x86_64 and one for aarch64. They embed the Svelte interface,
WebKitGTK/GTK runtime libraries, and the GStreamer media framework. They do not
need a project signing key or installation step.

Build AppImages natively on Ubuntu 22.04. Tauri's `linuxdeploy` tooling cannot
cross-build ARM AppImages, and newer build systems raise the minimum glibc
version. Install the locked Node and Rust toolchains plus:

```text
build-essential
file
gstreamer1.0-libav
gstreamer1.0-plugins-bad
gstreamer1.0-plugins-base
gstreamer1.0-plugins-good
gstreamer1.0-pulseaudio
libayatana-appindicator3-dev
libfuse2
librsvg2-dev
libssl-dev
libwebkit2gtk-4.1-dev
libxdo-dev
patchelf
pkg-config
xdg-utils
```

Do not add `gstreamer1.0-plugins-ugly` without a redistribution-license review.

From the repository root:

```bash
npm --prefix ui ci
npm --prefix ui run build
RUSTUP_TOOLCHAIN=1.95 packaging/build-appimage.sh
```

The expected output for the current machine is one of:

- `dist/Video-Harness-0.7.0-linux-x86_64.AppImage`
- `dist/Video-Harness-0.7.0-linux-aarch64.AppImage`

The builder checks the CPU architecture, exact application version, extraction
fallback, and the bundled GStreamer playback/libav plugins. CI additionally
opens the GUI under D-Bus and Xvfb before accepting the artifact.

The older native `.tar.xz` builder remains available as a best-effort fallback
for developers who prefer a conventional per-user installation. It is not the
portable release because it dynamically uses the host GTK, WebKitGTK, and
GStreamer stack; see [`NATIVE-BUNDLE.md`](NATIVE-BUNDLE.md).
