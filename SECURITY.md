# Security policy

## Reporting a vulnerability

Please use GitHub's private **Report a vulnerability** form for this repository
when it is available. Do not include API keys, signed media URLs, provider job
IDs, prompts, or private files in a public issue. If private reporting is not
available, open a public issue containing only contact details and a general
description so the maintainer can arrange a private channel.

Security fixes are made against the current 0.7.x release line. Older builds
may be asked to upgrade rather than receive a separate patch.

## Security boundaries

Video Harness is local-first. It has no Video Harness account, telemetry,
cloud sync, remote control service, or automatic updater. It nevertheless
communicates with the provider selected by the user and processes untrusted
provider responses, download URLs, model metadata, and media.

The application is designed around these boundaries:

- API keys are held in the operating system credential store or in process
  memory. They must not enter settings, drafts, history, logs, renderer events,
  URLs, or release artifacts.
- Authorization is attached only to validated provider API hosts. Provider
  output downloads do not receive the generation API credential.
- Local reference files remain local until the user approves the clearly
  disclosed public-by-link upload. Upload approval and the later paid
  generation approval are separate actions.
- A paid request is submitted once. An ambiguous submission creates durable
  local safety state and is not retried automatically.
- Download and deletion code operates only on app-owned temporary files or
  canonical regular files inside the configured Videos/Movies folder. Existing
  unrelated paths are never overwritten.
- The Tauri renderer receives a narrow command capability rather than generic
  filesystem, shell, process, or HTTP access.

The user and operating system account are trusted to control the local device.
Provider billing, provider-side retention, people who receive a public-by-link
media URL, third-party codecs/players, and a compromised operating system are
outside the application's security boundary.

## Useful report details

Include the Video Harness version and operating system, the affected provider,
whether the issue reproduces with placeholder data, and the smallest sequence
of actions that triggers it. Redact credentials and private content. The test
suite must use mock transports and must never spend provider credits.
