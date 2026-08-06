# Video Harness

Video Harness is a native Linux workspace for generating AI video across providers. It gives prompts, reference frames, model controls, price review, job monitoring, downloads, and playback a proper graphical home instead of making you assemble API requests by hand.

The v0.3 release supports OpenRouter and fal.ai. It saves finished videos to your XDG Videos directory (normally `~/Videos`) and keeps the transition-release terminal interface available as `video-harness-tui`.

## What it does

- Builds a generation visually with a prompt, model options, and ordered reference media.
- Accepts local image files through a picker or drag and drop, as well as public HTTPS URLs.
- Checks each model/provider combination before Review instead of silently dropping unsupported inputs.
- Shows a fresh quote and a complete request summary before enabling the single paid **Generate** action.
- Monitors multiple accepted remote jobs independently, with honest provider states and a small waiting animation.
- Downloads atomically to `~/Videos`, plays completed work in the app, and can open it in the system player.
- Autosaves draft text, options, and source paths locally. It never copies draft media or writes API keys into settings/history.

Local reference files are uploaded only when you choose **Review**. fal.ai inputs are staged on fal's public CDN with a 24-hour expiry preference and reusable receipts are cached until expiry. OpenRouter currently documents video references as stable public HTTPS URLs, so Video Harness clearly blocks local files for OpenRouter rather than uploading them somewhere you did not choose.

## Install on Fedora

Install the native build prerequisites once:

```bash
sudo dnf install gcc gtk4-devel libadwaita-devel
```

Then build and install both native interfaces plus the desktop entry:

```bash
chmod +x install.sh
./install.sh
```

Launch **Video Harness** from GNOME's app grid or run:

```bash
video-harness
```

The immutable release lives under `~/.local/lib/openrouter-video-studio/releases/0.3.0/`. The legacy internal directory is intentional: it preserves existing credentials, catalog caches, settings, and `history.sqlite3`. New GUI-only state is isolated in `gui-state.sqlite3`.

## First generation

1. Open **Providers & Settings**, paste a provider key, and connect it. Keys are masked and stored in Secret Service when available; otherwise they remain in memory for that session.
2. In **New Generation**, choose a provider/model, write the prompt, and add any reference media.
3. Choose **Review**. Video Harness validates the draft, stages supported local files, refreshes the quote, and shows exactly what will be submitted.
4. Choose **Generate — one paid request** once. Video Harness never automatically retries an ambiguous paid submission.
5. Follow the job in **Jobs**. Closing the app pauses local monitoring only; the remote provider continues. Use **Resume all** after relaunch.

If a paid request is accepted, the remote job ID is surfaced before local persistence work so it remains recoverable even if a later disk write fails.
If the connection disappears before an ID comes back, a durable safety hold blocks that exact draft across restarts. Video Harness asks you to check the provider dashboard before explicitly allowing another paid attempt; editing creates a distinct draft, and undoing the edit restores the hold.

## Terminal and Python transition

The Rust terminal UI remains available for one transition release:

```bash
video-harness-tui
# compatibility alias
openrouter-video-rs
```

`openrouter-video` and its Python environment are not replaced by a normal Video Harness install. Existing installations are captured as `openrouter-video-python`; the native installer's explicit `promote` and `rollback` commands retain the previous safety behavior. To install the old Python interface directly, use `./install-python-legacy.sh`.

## Development

```bash
cd native
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

The Rust integration suite uses in-memory credentials, temporary databases, and deterministic mock transports. It does not contact inference providers or spend credits. The preserved Python compatibility suite can be run with `pytest` from its existing virtual environment.

## Data and privacy

- API keys use the existing `openrouter-video-studio` Secret Service identity for compatibility and are never shown again by the app.
- Prompts, remote job IDs, and request metadata are stored locally to support history and resuming.
- Draft persistence stores source paths/URLs, never source file contents.
- Half-written seed, Advanced JSON, and schema-control text is restored exactly; credential-like fields and active provider keys fail closed instead of being written.
- Downloads use `.part` files and become `.mp4` only after a successful, non-empty transfer.
- Authorization is restricted to validated provider API endpoints and is never attached to unsigned output URLs.

Video generation is a paid provider operation. Quotes are informational; the provider's final usage charge is authoritative.
