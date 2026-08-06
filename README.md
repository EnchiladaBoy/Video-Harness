# OpenRouter Video Studio

OpenRouter Video Studio is a Linux-first terminal interface for generating videos without composing API requests by hand. Paste an OpenRouter API key, choose a current video model, write a prompt, confirm the estimated cost, and follow the real job status while a small terminal animation plays. Completed videos are downloaded to your normal **Videos** folder and can be opened with a single key.

> Video generation is a paid OpenRouter operation. The app always asks for confirmation before submitting a job. A displayed estimate is informational; OpenRouter's final usage cost is authoritative.

## Requirements

- Linux and Python 3.11 or newer
- A terminal with color support (the interface also remains usable without color)
- An [OpenRouter API key](https://openrouter.ai/settings/keys) with sufficient credit
- `xdg-open` to use the open-video hotkey
- A desktop Secret Service provider, such as GNOME Keyring, for persistent credential storage

The API key is masked while typing. When a supported keyring is available it is stored there; otherwise it stays only in memory for the current run. It is never written to project configuration, application history, or logs.

## Install

From this directory, run:

```bash
chmod +x install.sh
./install.sh
```

The installer creates an isolated `.venv`, installs the project in editable mode, and links `openrouter-video` into `~/.local/bin`. It does not use `sudo` or install global Python packages.

Launch it with:

```bash
openrouter-video
```

If `~/.local/bin` is not on `PATH`, run `~/.local/bin/openrouter-video` or add that directory to your shell's `PATH`.

## Use

1. On first launch, paste an API key and validate it.
2. Enter a prompt and select a model. Settings shown by the app adapt to that model's advertised capabilities.
3. Start generation and review the final settings and cost estimate before confirming.
4. Keep the app open to watch the remote state and elapsed time. Leaving the progress view does not cancel a paid remote job.
5. When complete, press `O` or `Enter` to open the saved video, or `N` to start another.

Videos are saved directly in the XDG Videos directory. On a typical Linux install—and on the intended system here—that is `/home/alex/Videos`. Partial downloads use a `.part` suffix and are only renamed to `.mp4` after a successful non-empty download.

Common keys are displayed in the footer. `Ctrl+Enter` begins the confirmation flow, `H` opens local history, `Esc` returns or closes a dialog, and `Q` quits where shown.

## Reliability and privacy

- The live model catalog comes from OpenRouter and the last successful catalog is cached for temporary offline use.
- Safe reads and downloads use bounded retries. Submission is never automatically retried after an ambiguous network failure, preventing accidental duplicate paid jobs.
- Authorization is attached only to validated OpenRouter API URLs. It is not sent to provider-hosted unsigned download URLs.
- Pending job metadata and prompts are retained only in the local history database so monitoring can resume after a restart. To clear all history, close the app and remove the database shown by your XDG data path (normally `~/.local/share/openrouter-video-studio/history.sqlite3`).
- A local timeout pauses monitoring—it does not claim the remote generation failed. Resume it from history later.

## Development and tests

Install the test extras into the private environment and run the offline suite:

```bash
.venv/bin/python -m pip install --editable '.[test]'
.venv/bin/python -m pytest
```

Tests mock the HTTP transport and never create paid video jobs. Do not use a real API key in tests.

## Multi-provider native Rust beta

A one-executable Fedora ARM64 edition is developed alongside this Python implementation. Native v0.2 presents the temporary title **Video Studio Beta** and supports provider adapters for OpenRouter and fal.ai while preserving the existing Python command as the stable rollback target. It uses compatibility-safe application paths, isolated provider credentials, and additive provider-qualified history. Native build, test, beta alias, atomic promotion, and rollback instructions are in [native/README.md](native/README.md).

## Troubleshooting

- **Key is not remembered:** unlock or install a Secret Service provider, then restart. The memory-only fallback is intentional when secure persistence is unavailable.
- **Video does not open:** the file is still saved; install `xdg-utils` or open the reported path in your preferred player.
- **Model list is marked stale:** check connectivity to OpenRouter. You can browse cached details, but submission still needs network access.
- **Job appears stuck:** use history to resume polling. Do not submit the same prompt again unless you intend to pay for another generation.
