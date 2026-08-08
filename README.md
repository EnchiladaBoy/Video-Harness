# Video Harness

Video Harness is a local-first desktop app for creating AI videos with
OpenRouter and fal.ai. It keeps prompts, job history, drafts, and downloaded
videos on your computer. There is no Video Harness account, telemetry, cloud
sync, or background updater.

Version 0.7.1 makes the Tauri 2 + Svelte app the only supported frontend and
prepares native release targets for Linux, Windows, and macOS. The old GTK
frontend is deprecated, opt-in, and not included in release packages.

## Create your first video

1. Open **Connections**, choose a service, and enter its API key. The key is
   saved in your operating system's credential store when available; otherwise
   it remains in memory for this session only.
2. Open **Create**, choose a video service and model, then describe the video.
   Reference images, video, and audio are optional and model-dependent.
3. Choose **Review price & details**. This validates the draft and gets a fresh
   estimate. It does not start a paid generation.
4. Check the estimate, settings, and files, then choose
   **Generate video — paid**. This sends exactly one paid provider request.
5. Follow progress in **My videos**. Closing Video Harness stops local
   monitoring, not the provider's remote job; after reopening, use **Resume
   all** to continue checking recovered jobs.

OpenRouter requires reference files to be available at public HTTPS URLs. If
an OpenRouter draft uses local files, Video Harness asks before uploading them
to fal.ai's public-by-link storage. Anyone with such a link can download the
file. Video Harness requests a 24-hour expiry, then shares the URL with
OpenRouter and the selected model provider. Cancelling keeps the files local
and sends nothing. Approving an upload still does not approve the paid request.
Direct fal.ai drafts with local files use the same consent step and
public-by-link storage, but share the resulting links only with the selected
fal.ai model.

## Install

The 0.7.1 packages below can be downloaded from the
[latest release](https://github.com/EnchiladaBoy/Video-Harness/releases/latest)
after the documented signing and hardware release gates are provisioned.

| Platform | Package | Supported baseline |
| --- | --- | --- |
| Linux x86_64 | `Video-Harness-0.7.1-linux-x86_64.AppImage` | glibc-based desktop Linux |
| Linux ARM64 | `Video-Harness-0.7.1-linux-aarch64.AppImage` | glibc-based desktop Linux |
| Windows x64 | signed setup `.exe` or `.msi` | Windows 10 22H2 or Windows 11 |
| macOS Intel | signed and notarized x86_64 `.dmg` | macOS 12 or newer |
| macOS Apple Silicon | signed and notarized aarch64 `.dmg` | macOS 12 or newer |

On Linux, make the AppImage executable and run it:

```bash
chmod +x Video-Harness-0.7.1-linux-x86_64.AppImage
./Video-Harness-0.7.1-linux-x86_64.AppImage
```

If FUSE is unavailable, add `--appimage-extract-and-run`. Alpine/musl and
unconfigured environments such as NixOS are not supported.

On Windows, the normal setup executable installs for the current user without
administrator access and includes the offline WebView2 runtime. The MSI is for
managed deployments. Windows N editions need Microsoft's Media Feature Pack
for common H.264/AAC playback.

On macOS, choose the DMG that matches **About This Mac**, open it, and drag
Video Harness to Applications. Direct-download releases require a Developer
ID signature and a stapled Apple notarization ticket.

Detailed platform paths, packaging, signing, notarization, and release checks
are in [packaging/PLATFORM-RELEASES.md](packaging/PLATFORM-RELEASES.md).

## Spending and recovery safeguards

- A review is free and separate from the clearly labelled paid button.
- Each review has a short lifetime and is invalidated when the draft changes.
- Video Harness never automatically retries an ambiguous paid submission.
- If the connection fails before a job ID is known, a durable safety block
  survives restart. You must check the provider dashboard and explicitly clear
  it before that same request can be submitted again.
- Once a provider accepts a request, its complete job ID is surfaced before
  later local persistence work. Accepted jobs are recovered independently and
  remain available to resume after restart.
- Price estimates are informational; the provider's final charge is
  authoritative.

The automated test suite uses in-memory keys, temporary databases, and mock
provider traffic. It does not submit real generations or spend credits.

## Data, credentials, and files

- API keys use Windows Credential Manager, macOS Keychain, or the Linux system
  keyring under the compatibility-sensitive `openrouter-video-studio` service
  name. They are never persisted in settings, drafts, history, browser storage,
  or logs; the entry field is cleared as soon as the key is handed to the native
  credential service.
- Prompts, provider job IDs, request metadata, and source paths are stored
  locally so drafts and monitoring can recover. Source file contents are not
  copied into draft storage.
- Finished videos go to the operating system's Videos folder (`~/Movies` on
  macOS). Redirected known folders and non-ASCII paths are supported.
- Downloads use app-owned `.part` files, a 4 GiB limit, and atomic completion.
  Authorization is sent only to validated provider API hosts, never to output
  download URLs.
- Removing a job does not delete the provider's remote copy. Deleting a local
  video is a separate action and succeeds only for a verified regular file
  inside the configured Videos/Movies folder.
- In-app playback uses an opaque, short-lived file in the private cache. It
  never overwrites an existing path and falls back to a create-new copy when a
  hard link cannot cross volumes.

Compatibility-sensitive storage locations and the threat model are documented
in [packaging/PLATFORM-RELEASES.md](packaging/PLATFORM-RELEASES.md) and
[SECURITY.md](SECURITY.md).

## Build and test

Use Node.js 24.18.0 and Rust 1.95. Linux desktop builds also need the
[Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
npm --prefix ui ci
npm --prefix ui run check
npm --prefix ui test
npm --prefix ui run build

cargo fmt --manifest-path native/Cargo.toml --check
cargo clippy --locked --manifest-path native/Cargo.toml \
  --lib --tests --no-default-features -- -D warnings
cargo test --locked --manifest-path native/Cargo.toml \
  --lib --tests --no-default-features

cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --check
cargo clippy --locked --manifest-path desktop/src-tauri/Cargo.toml \
  --all-targets -- -D warnings
cargo test --locked --manifest-path desktop/src-tauri/Cargo.toml

packaging/check-release-version.sh
packaging/test-installer.sh
```

Build or run the canonical desktop app from the repository root:

```bash
npm --prefix ui run build
cargo build --release --locked \
  --manifest-path desktop/src-tauri/Cargo.toml --bin video-harness
cargo run --locked --manifest-path desktop/src-tauri/Cargo.toml
```

The browser-only `npm --prefix ui run dev` mode uses a mock bridge and cannot
submit provider requests. Platform package commands are documented in
[packaging/README.md](packaging/README.md).

## Deprecated GTK frontend

Tauri/Svelte owns the canonical unsuffixed `video-harness` executable. The
maintenance-only GTK tool is explicitly named `video-harness-gtk`, displays a
deprecation notice, and is never shipped. Build it only when maintaining old
Linux installations:

```bash
cargo run --locked --manifest-path native/Cargo.toml \
  --features legacy-gtk --bin video-harness-gtk
```

The verified parity matrix and removal policy are in
[native/LEGACY-GTK.md](native/LEGACY-GTK.md).

## Release status

CI compiles and tests Linux x86_64/ARM64, Windows x64, macOS Intel, and macOS
Apple Silicon, and builds unsigned installer smoke artifacts. Tagged releases
add mandatory Authenticode signing for Windows and Developer ID signing,
notarization, and stapling for macOS; publication fails closed if any signing
input or verification is missing. Linux AppImages are unsigned and receive
GitHub keyless provenance attestations. Every release includes SHA-256 checksums
and an SPDX software bill of materials.

Publishing the first Windows/macOS release remains externally blocked until
the project provisions a compatible exportable Authenticode PFX (or adds a
specific hardware/remote `signCommand` integration) and a paid Apple Developer
account with notarization credentials. Final playback and window tests also
require real Windows, Intel Mac, and Apple Silicon Mac hardware. See the
[release runbook](.github/RELEASING.md) for the exact checklist.
