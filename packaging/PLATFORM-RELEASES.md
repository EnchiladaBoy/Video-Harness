# Supported desktop releases

The supported desktop targets use the canonical Tauri/Svelte application and
the same local Rust service. They do not add an account, telemetry, cloud sync,
or a background updater. Provider API keys stay in the operating system's
credential store and generation state stays on the machine.

## Supported targets

| Platform | Release artifact | Runtime baseline |
| --- | --- | --- |
| Linux x86_64 | unsigned `.AppImage` | glibc-based desktop Linux |
| Linux aarch64 | unsigned `.AppImage` | glibc-based desktop Linux |
| Windows x64 | unsigned NSIS setup `.exe` | Windows 10 22H2 or Windows 11 |
| macOS Apple Silicon | unsigned aarch64 `.dmg` | macOS 12 or newer |

Intel Macs, Windows on ARM, and Windows MSI packages are not supported release
targets. The project intentionally publishes a small set of prebuilt community
binaries instead of maintaining commercial code-signing and Apple notarization
infrastructure.

All of these packages are unsigned. That makes installation less seamless:
Windows normally displays a Microsoft Defender SmartScreen unknown-publisher
warning, and macOS Gatekeeper normally blocks the app's first launch. Those
warnings are an expected consequence of this release policy, not evidence that
the operating system has verified the download. Verify both the SHA-256 checksum
and GitHub build-provenance attestation before allowing a package to run.

The Windows setup executable installs for the current user without elevation
and includes the offline WebView2 runtime, so the interface does not depend on a
download during installation. Downgrades are blocked because an older build may
not understand newer local state. Immediately after packaging—and before CI
executes or uploads the setup executable—the offline runtime is checked against
Microsoft's trusted Authenticode publisher, code-signing purpose, and timestamp.
The evergreen runtime URL can move to a newer Microsoft-signed build, so the
bundled runtime is publisher-verified but not byte-for-byte reproducible across
build dates. The enclosing Video Harness setup executable remains unsigned.

The macOS artifact is a direct-download Apple Silicon build, not a Mac App Store
or universal build. Video Harness does not claim compatibility with Intel Macs.

## Verify a release before running it

Download the package and `SHA256SUMS` from the same GitHub release. From the
download directory, compute the package checksum on Linux:

```bash
sha256sum Video-Harness-0.7.1-linux-x86_64.AppImage
```

macOS also includes `shasum` if `sha256sum` is unavailable:

```bash
shasum -a 256 Video-Harness-0.7.1-macos-aarch64.dmg
```

Compare that result with the corresponding value in `SHA256SUMS`. On Windows,
use PowerShell and compare the result the same way:

```powershell
Get-FileHash .\Video-Harness-0.7.1-windows-x86_64-setup.exe -Algorithm SHA256
```

The checksum detects a damaged or substituted asset relative to the release
manifest. The GitHub keyless attestation separately ties the asset to this
repository's release workflow. With GitHub CLI installed, retrieve and verify
the attestation for each package:

```bash
gh attestation verify Video-Harness-0.7.1-linux-x86_64.AppImage \
  --repo EnchiladaBoy/Video-Harness
```

Replace the filename with the package for the current platform. Do not continue
if either verification fails or the filename is absent from `SHA256SUMS`.

After both checks pass:

- On Windows, open the setup executable. If SmartScreen blocks it, choose
  **More info → Run anyway** to allow only this verified installer.
- On macOS, copy Video Harness to Applications and try to open it. After
  Gatekeeper blocks it, use **Privacy & Security → Open Anyway** in System
  Settings (or **Security & Privacy** in System Preferences on macOS 12) to
  allow only this verified app.

Do not disable SmartScreen or Gatekeeper globally, and do not use terminal
commands that strip the download's quarantine metadata.

## Local storage

Tauri resolves known folders rather than reading `HOME` or assembling paths
with string separators. The Windows executable opts into long-path support,
and Open file passes native OS paths without a lossy UTF-8 conversion.

On Windows:

- history, drafts, and settings: `%APPDATA%\io.github.EnchiladaBoy.VideoHarness`
- cache and temporary playback grants:
  `%LOCALAPPDATA%\io.github.EnchiladaBoy.VideoHarness`
- finished generations: the user's Windows Videos known folder, including a
  redirected or OneDrive-managed Videos folder
- API keys: Windows Credential Manager under the compatibility-sensitive
  `openrouter-video-studio` service name

On macOS:

- history, drafts, and settings:
  `~/Library/Application Support/io.github.EnchiladaBoy.VideoHarness`
- cache and temporary playback grants:
  `~/Library/Caches/io.github.EnchiladaBoy.VideoHarness`
- finished generations: `~/Movies`
- API keys: macOS Keychain under the compatibility-sensitive
  `openrouter-video-studio` service name

If Credential Manager or Keychain is unavailable or does not respond in time,
the app says so and keeps the key in process memory for that session. It never
falls back to a plaintext settings file.

Generated files are opened or deleted only after canonicalization confirms
that they are regular files inside the configured Videos/Movies directory.
Playback receives a short-lived opaque grant in the private cache. It uses a
hard link when possible and a create-new copy when Videos/Movies is on another
volume. It never overwrites an existing target, and cleanup remains confined
to the private cache's generated playback-grant names.

## Local builds

Install Node.js 24.18.0 and Rust 1.95, then build the locked interface from the
repository root:

```bash
npm --prefix ui ci
npm --prefix ui run build
```

On Windows, from PowerShell or Git Bash:

```text
cd desktop
../ui/node_modules/.bin/tauri build --ci --no-sign --bundles nsis -- --locked
```

The setup executable is placed below
`desktop/src-tauri/target/release/bundle/nsis`.

On an Apple Silicon Mac:

```bash
cd desktop
../ui/node_modules/.bin/tauri build --ci --no-sign --bundles app,dmg -- --locked
```

Artifacts are placed below
`desktop/src-tauri/target/release/bundle/macos` and
`desktop/src-tauri/target/release/bundle/dmg`. Do not label this
single-architecture binary as universal or compatible with Intel Macs.

Linux AppImage build details are in [`README.md`](README.md).

## Playback verification and platform limits

Before tagging a release, exercise the package candidates on real Windows x64
and Apple Silicon Mac hardware: require a visible window, play the checked-in
H.264/AAC fixture through the in-app video element, use Open file and the native
file picker, verify the platform credential store, test a path containing spaces
and non-ASCII characters, and use a Videos/Movies folder on another volume.
Repeat the checksum and provenance verification against the published assets.
Hosted CI validates package structure, installation or mounting, and a process
launch; only the Linux AppImage smoke currently automates media decode and
visible-window detection.

macOS and standard Windows 10/11 installations provide H.264/AAC decoding.
Windows N editions require Microsoft's Media Feature Pack; this is an
operating-system component and cannot be installed silently by Video Harness.
Provider outputs using codecs unsupported by WebView2 or WebKit may require
Open file and a third-party player.

Unsigned Windows downloads may continue to show SmartScreen warnings until a
file develops sufficient reputation, and an unsigned macOS app should be
expected to require the per-app Gatekeeper override on each newly downloaded
version. This is a known usability cost of the unsigned release policy. Intel
Mac testing and compatibility work are intentionally out of scope.

On Linux, Tauri's current WebKitGTK runtime transitively uses the unmaintained
GTK 3 Rust bindings and `glib` 0.18.5. The latter has a RustSec unsoundness
advisory limited to `VariantStrIter`; the locked application and dependency
sources do not call that API. CI denies every other unsoundness advisory and
allows this one only while that exact package version remains active. Removing
the exception depends on Tauri's Linux runtime moving to the maintained GTK 4 /
`glib` 0.20-or-newer stack.

RustSec also reports informational maintenance warnings for `proc-macro-error`
and the `rust-unic` crates pulled through Tauri's locked utility stack. They are
not known vulnerabilities, but they remain upstream maintenance risks to review
when upgrading Tauri.
