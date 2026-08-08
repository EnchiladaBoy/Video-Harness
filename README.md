# Video-Harness

Video-Harness is a friendly desktop workspace for generating AI video across
providers. It gives prompts, reference media, model controls, price review,
job monitoring, downloads, and playback one clear graphical home.

Version 0.7.0 makes the Tauri 2 + Svelte interface the stable Video Harness
frontend. Portable AppImages ship for x86_64 and aarch64 Linux. Windows x64
and macOS x64 builds are compile- and test-checked in CI, but they are future
targets rather than supported release platforms today. The older
GTK4/libadwaita frontend remains in the source tree only as a compatibility and
developer fallback.

Finished videos are saved to your platform's Videos directory, normally
`~/Videos` on Linux.

## What it does

- Builds a generation visually with a prompt, model options, and ordered image,
  video, or audio references.
- Supports OpenRouter and fal.ai, while keeping provider-specific capabilities
  and validation visible.
- Shows a fresh quote and complete request summary before enabling the single
  paid **Generate** action.
- Monitors multiple accepted jobs independently, with an animated pixel-art
  waiting scene and reduced-motion support.
- Plays completed work inside the app or hands it to the system player.
- Keeps the full provider job ID visible and copyable for troubleshooting.
- Removes a render from the local reel, with a separate choice to delete its
  downloaded video safely.
- Autosaves draft text, options, and source paths locally. It never copies draft
  media or writes API keys into settings or history.

OpenRouter's video API requires reference media to arrive as directly
downloadable public HTTPS URLs. You can paste those URLs directly without a
fal.ai account. If an OpenRouter draft contains local files, Video Harness
requires a connected fal.ai key and asks for confirmation on every **Review**
before uploading anything. Approved files are staged on fal.ai's public-by-link
CDN and their HTTPS URLs are sent through OpenRouter to the selected model
provider. Anyone with a staged link can retrieve the file; Video Harness
requests a 24-hour expiry and keeps reusable upload receipts only until that
expiry. Cancelling the confirmation leaves the files local and sends nothing.

Native fal.ai generations also stage supported local files when you choose
**Review**. Video and audio inputs fail closed unless the current model catalog
explicitly advertises that capability.

## Install on Linux

Download the AppImage for your CPU from the
[latest release](https://github.com/EnchiladaBoy/Video-Harness/releases/latest):

- `Video-Harness-0.7.0-linux-x86_64.AppImage` for most Intel and AMD PCs.
- `Video-Harness-0.7.0-linux-aarch64.AppImage` for 64-bit ARM systems.

Make the downloaded file executable and run it directly; nothing is installed:

```bash
chmod +x Video-Harness-0.7.0-linux-x86_64.AppImage
./Video-Harness-0.7.0-linux-x86_64.AppImage
```

The AppImage bundles the Tauri GUI and its media framework, including common
MP4 playback support. It targets glibc-based desktop Linux and is built on
Ubuntu 22.04 for a broad compatibility baseline; Alpine/musl and unusual
unconfigured environments such as NixOS are not supported. If FUSE mounting is
unavailable, add `--appimage-extract-and-run` when launching the same file.

AppImages update manually: download the newer file when a release is available.
`SHA256SUMS` is attached beside each release for download-integrity checks.

## Build from source on Linux

Install Node.js 24.18.0, Rust 1.95, a C toolchain, and the Tauri 2 system libraries.
On Debian or Ubuntu, the packages used by CI are:

```bash
sudo apt install build-essential libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf pkg-config
```

See the official
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for Fedora,
Arch Linux, openSUSE, and other distributions.

From the repository root, build the locked web interface first and then the
desktop host:

```bash
npm --prefix ui ci
npm --prefix ui run build
cargo build --release --locked \
  --manifest-path desktop/src-tauri/Cargo.toml \
  --bin video-harness
```

Run the result directly:

```bash
./desktop/src-tauri/target/release/video-harness
```

To build the same unsigned portable executable used by releases:

```bash
packaging/build-appimage.sh
```

The result is written to `dist/` for the current CPU architecture. AppImage
media bundling is fully supported on Ubuntu build systems; see
[packaging/README.md](packaging/README.md) for the complete dependency list.

Or, from the repository root, install the build for the current user:

```bash
./install.sh install desktop/src-tauri/target/release/video-harness
```

This installs an immutable release below
`~/.local/lib/openrouter-video-studio/releases/0.7.0/`, a `video-harness`
launcher in `~/.local/bin`, and the standard desktop metadata. The legacy
internal directory name is intentional: it preserves existing credentials,
catalog caches, settings, and `history.sqlite3`. Draft and upload state lives in
the separate `gui-state.sqlite3` sidecar.

For a development run, build `ui/dist` as above and use:

```bash
cargo run --locked --manifest-path desktop/src-tauri/Cargo.toml
```

The browser-only `npm run dev` view uses a mock bridge; it is useful for UI
work, but it does not submit provider requests.

## First generation

1. Open **Providers**, paste a key for the provider you want, and connect it.
   Also connect fal.ai if you want to use local files in an OpenRouter
   generation. Keys are masked and stored in Secret Service when available;
   otherwise they stay in memory for that session.
2. In **Create**, choose a provider and model, write the prompt, and add any
   supported reference media.
3. Choose **Review**. Video Harness validates the draft, stages approved local
   files, refreshes the quote, and shows exactly what will be submitted.
4. Choose **Generate — one paid request** once. Video Harness never retries an
   ambiguous paid submission automatically.
5. Follow the job in **Renders**. Closing the app pauses local monitoring only;
   the remote provider continues. Resume its updates after relaunch.

If a paid request is accepted, the remote job ID is surfaced before later
local persistence work so it remains recoverable if a disk write fails. If the
connection disappears before an ID comes back, a durable safety hold blocks
that exact draft across restarts. Video Harness asks you to check the provider
dashboard before explicitly allowing another paid attempt.

## Development and tests

```bash
cd ui
npm ci
npm run check
npm test
npm run build

cd ../desktop/src-tauri
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

cd ../../native
cargo fmt --check
cargo clippy --locked --lib --tests --no-default-features -- -D warnings
cargo test --locked --lib --tests --no-default-features

cd ..
packaging/check-release-version.sh
packaging/test-installer.sh
```

The Rust integration suite uses in-memory credentials, temporary databases,
and deterministic mock transports. It does not contact inference providers or
spend credits. The legacy GTK frontend can still be built explicitly from
`native/`; see [native/README.md](native/README.md).

## Data and privacy

- API keys keep the existing `openrouter-video-studio` Secret Service identity
  for compatibility and are never shown again by the app.
- Prompts, remote job IDs, and request metadata are stored locally to support
  history and resuming.
- Draft persistence stores source paths and URLs, never source file contents.
- OpenRouter local references leave your computer only after the Review
  confirmation. They are uploaded to fal.ai's public-by-link CDN with a
  requested 24-hour expiry, then shared with OpenRouter and the selected model
  provider. Direct HTTPS references are used as supplied and are not
  re-uploaded.
- Half-written seed, Advanced JSON, and schema-control text is restored exactly;
  credential-like fields and active provider keys fail closed instead of being
  written.
- Downloads use owned `.part` files, enforce a 4 GiB transfer ceiling, and
  become completed video files only after a successful, non-empty transfer.
- Authorization is restricted to validated provider API endpoints and is never
  attached to unsigned output URLs.
- Removing a render from the reel does not delete the provider's remote job or
  copy. Local video deletion is a separate, explicit choice.

Video generation is a paid provider operation. Quotes are informational; the
provider's final usage charge is authoritative.

## Release channels

- [GitHub releases](https://github.com/EnchiladaBoy/Video-Harness/releases)
  provide the x86_64 and aarch64 AppImages, `SHA256SUMS`, and an SPDX software
  bill of materials.
- Releases are unsigned and require no project signing key. GitHub Actions adds
  keyless build-provenance attestations without a user-managed secret.
- AppImages update manually by replacing the downloaded executable with the
  file from a newer release.
