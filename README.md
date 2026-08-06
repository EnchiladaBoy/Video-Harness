# Video Harness

Video Harness is a native Linux application for generating AI video across providers. It gives prompts, typed reference media, model controls, price review, job monitoring, downloads, and playback a proper graphical home instead of making you assemble API requests by hand.

The v0.6.0 release supports OpenRouter and fal.ai on x86_64 and aarch64 Linux. Finished videos are saved to your XDG Videos directory, normally `~/Videos`.

## What it does

- Builds a generation visually with a prompt, model options, and ordered reference media.
- Accepts image, MP4/MOV video, and MP3/WAV audio inputs. fal.ai can stage local files selected through the picker or drag and drop; both providers accept public HTTPS URLs where the selected model supports them.
- Checks each model/provider combination before Review instead of silently dropping unsupported inputs.
- Shows a fresh quote and a complete request summary before enabling the single paid **Generate** action.
- Monitors multiple accepted remote jobs independently, with honest provider states and a reduced-motion-aware Tiny Cloud Cinema while work is active.
- Downloads atomically to `~/Videos`, plays completed work in the app, and can open it in the system player.
- Autosaves draft text, options, and source paths locally. It never copies draft media or writes API keys into settings/history.

Local reference files are uploaded only when you choose **Review**. fal.ai inputs are staged on fal's public CDN with a 24-hour expiry preference and reusable receipts are cached until expiry. OpenRouter reference media must use public HTTPS URLs, so Video Harness clearly blocks local files for OpenRouter rather than uploading them somewhere you did not choose. Video and audio inputs fail closed unless the current model catalog explicitly advertises that capability.

## Install

Flatpak is the recommended cross-distribution package. Download
`VideoHarness.flatpakref` from the [latest release](https://github.com/EnchiladaBoy/VideoHarness/releases/latest), then run:

```bash
flatpak install --user VideoHarness.flatpakref
flatpak run io.github.EnchiladaBoy.VideoHarness
```

For H.264 MP4 playback, install the Freedesktop codec add-on used by GNOME 50:

```bash
flatpak install --user flathub org.freedesktop.Platform.codecs-extra//25.08-extra
```

The Flatpak has only the permissions needed for provider networking, graphics,
sound, Secret Service, the file-picker portals, and creating files in your XDG
Videos directory. A one-time importer can read the three legacy
`openrouter-video-studio` data directories; it never modifies them.

Best-effort native tarballs are also attached for x86_64 and aarch64. They use
the host GTK, libadwaita, GStreamer, and glibc libraries and are intended for
advanced users. Extract the matching archive and run `./install.sh`.

## Build from source on Fedora

Install the native build prerequisites once:

```bash
sudo dnf install gcc gtk4-devel libadwaita-devel
```

Then build and install the application and desktop entry:

```bash
chmod +x install.sh
./install.sh
```

Launch **Video Harness** from GNOME's app grid or run:

```bash
video-harness
```

The immutable release lives under `~/.local/lib/openrouter-video-studio/releases/0.6.0/`. The legacy internal directory name is intentional: it preserves existing credentials, catalog caches, settings, and `history.sqlite3`. GUI draft and upload state is isolated in `gui-state.sqlite3`.

## First generation

1. Open **Providers & Settings**, paste a provider key, and connect it. Keys are masked and stored in Secret Service when available; otherwise they remain in memory for that session.
2. In **New Generation**, choose a provider/model, write the prompt, and add any reference media.
3. Choose **Review**. Video Harness validates the draft, stages supported local files, refreshes the quote, and shows exactly what will be submitted.
4. Choose **Generate — one paid request** once. Video Harness never automatically retries an ambiguous paid submission.
5. Follow the job in **Jobs**. Closing the app pauses local monitoring only; the remote provider continues. Use **Resume all** after relaunch.

If a paid request is accepted, the remote job ID is surfaced before local persistence work so it remains recoverable even if a later disk write fails. If the connection disappears before an ID comes back, a durable safety hold blocks that exact draft across restarts. Video Harness asks you to check the provider dashboard before explicitly allowing another paid attempt; editing creates a distinct draft, and undoing the edit restores the hold.

## Development

```bash
cd native
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cd ..
flatpak/check-manifest.sh
packaging/test-installer.sh
```

The Rust integration suite uses in-memory credentials, temporary databases, and deterministic mock transports. It does not contact inference providers or spend credits.

## Data and privacy

- API keys use the existing `openrouter-video-studio` Secret Service identity for compatibility and are never shown again by the app.
- Prompts, remote job IDs, and request metadata are stored locally to support history and resuming.
- Draft persistence stores source paths/URLs, never source file contents.
- Half-written seed, Advanced JSON, and schema-control text is restored exactly; credential-like fields and active provider keys fail closed instead of being written.
- Downloads use `.part` files and become `.mp4` only after a successful, non-empty transfer.
- Authorization is restricted to validated provider API endpoints and is never attached to unsigned output URLs.

Video generation is a paid provider operation. Quotes are informational; the provider's final usage charge is authoritative.

## Release channels

- The signed Flatpak update repository lives at <https://enchiladaboy.github.io/VideoHarness/> and is the primary release channel.
- Native tarballs are best-effort and update manually by installing a newer archive.
- Release checksums, their detached signature, and the dedicated release public key are attached to every GitHub release.

Release signing uses a dedicated offline primary key and a time-limited signing
subkey. The repository intentionally contains no private signing material.
