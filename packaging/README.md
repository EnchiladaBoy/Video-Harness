# Release packaging

Video Harness ships the same Tauri/Svelte desktop application on every
supported platform. Release artifacts are unsigned AppImages for Linux x86_64
and aarch64, an unsigned NSIS setup executable for Windows x64, and an unsigned
DMG for Apple Silicon Macs. Intel Macs and Windows MSI packages are not in the
supported release matrix. Platform paths, local credential stores, build
commands, expected operating-system warnings, and release verification are
documented in [`PLATFORM-RELEASES.md`](PLATFORM-RELEASES.md).

The Linux packages are unsigned Tauri AppImages: one directly runnable file
for x86_64 and one for aarch64. They embed the Svelte interface, WebKitGTK/GTK
runtime libraries, and the GStreamer media framework. They do not need a
project signing key or installation step.

Build AppImages natively on Ubuntu 22.04. Tauri's `linuxdeploy` tooling cannot
cross-build ARM AppImages, and newer build systems raise the minimum glibc
version.

`build-appimage.sh` preloads every executable used by Tauri's AppImage bundler
into its project-local tools directory and verifies reviewed SHA-256 values
before the bundler can run it. The GTK and GStreamer plugin scripts use
immutable source commits. The upstream AppRun, linuxdeploy, and AppImage-plugin
release URLs are also checksum-bound, so a changed mutable asset fails closed
and requires an explicit pin review.

CI and releases use Node.js 24.18.0 and Rust 1.95; install those toolchains
plus:

```text
build-essential
coreutils
curl
file
gstreamer1.0-libav
gstreamer1.0-plugins-bad
gstreamer1.0-plugins-base
gstreamer1.0-plugins-good
gstreamer1.0-pulseaudio
gstreamer1.0-tools
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
The codec smoke test decodes the small synthetic fixture in
`packaging/fixtures/h264-aac.mp4.b64`; it does not rely on build-host codecs.

From the repository root:

```bash
npm --prefix ui ci
npm --prefix ui run build
RUSTUP_TOOLCHAIN=1.95 packaging/build-appimage.sh
```

The expected output for the current machine is one of:

- `dist/Video-Harness-0.7.1-linux-x86_64.AppImage`
- `dist/Video-Harness-0.7.1-linux-aarch64.AppImage`

The builder checks the CPU architecture, exact application version, extraction
fallback, and the bundled GStreamer playback/libav plugins. It decodes both
streams from the checked-in fixture using only the bundled plugin registry. CI
repeats that decode in a clean runtime container, then opens the GUI under
D-Bus and Xvfb before accepting the artifact.

The older native `.tar.xz` builder remains available as a best-effort fallback
for developers who prefer a conventional per-user installation. It is not the
portable release because it dynamically uses the host GTK, WebKitGTK, and
GStreamer stack; see [`NATIVE-BUNDLE.md`](NATIVE-BUNDLE.md).
