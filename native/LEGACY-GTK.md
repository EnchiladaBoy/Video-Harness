# Legacy GTK frontend status and parity

The Svelte interface hosted by Tauri is the canonical Video Harness desktop
application. Its executable owns the unsuffixed `video-harness` name and is the
only frontend shipped in release packages.

The GTK4/libadwaita frontend is maintenance-only. It remains available for
Linux developers who need it, but it is opt-in and identifies itself as
`video-harness-gtk` in Cargo, command output, the window title, and an in-app
banner. It is not a release fallback and should not receive new product
features merely to close parity gaps.

## Lifecycle

- **0.7.x:** keep `legacy-gtk` buildable; accept narrowly scoped correctness,
  data-safety, and security fixes.
- **0.8.0:** planned removal point for the GTK binary, feature, dependencies,
  and `native/src/gui`. The platform-neutral `video_harness` library and user
  data formats are not part of that removal.

No calendar date is committed for 0.8.0. If the removal target changes, update
this document before publishing the release that changes the policy.

## Building it explicitly

The portable library has no GUI dependencies by default:

```bash
cargo build --manifest-path native/Cargo.toml --locked --lib
cargo test --manifest-path native/Cargo.toml --locked --lib --tests
```

The deprecated frontend requires GTK 4.10+, libadwaita 1.6+, and an explicit
feature and binary selection:

```bash
cargo run --manifest-path native/Cargo.toml --locked \
  --features legacy-gtk --bin video-harness-gtk
```

There is intentionally no GTK binary named `video-harness`. This prevents an
ad-hoc legacy build from being mistaken for the supported Tauri application.

## Feature-parity audit

This matrix reflects the repository at 0.7.2. “Shared” means both frontends
delegate the behavior to the same Rust workflow/domain layer, not that their
presentation code is identical.

| Capability | Tauri/Svelte (canonical) | GTK (legacy) | Disposition |
| --- | --- | --- | --- |
| OpenRouter and fal.ai generation workflows | Shared engine | Shared engine | Equivalent core behavior |
| Credential validation, keyring persistence, and forgetting keys | Supported | Supported | Equivalent core behavior |
| Provider catalog, model settings, exact size/resolution, seed, audio, and advanced JSON | Supported | Supported | Equivalent core behavior |
| Local/remote image, video, and audio inputs with roles and ordering | Supported | Supported | Equivalent core behavior |
| Upload disclosure, quote/review, prepared-request expiry, and duplicate-paid-request safety holds | Supported | Supported | Equivalent safety path |
| Draft persistence, history restoration, recovery records, and resumable jobs | Supported | Supported | Shared persistence; frontend projections differ |
| Actor-authoritative per-job pause/resume | Supported | Supported | GTK now waits for `MonitorStarted`/`MonitorStopped` before enabling controls |
| Pause all / resume all | Supported | Supported | Actor-authoritative bulk controls in both frontends |
| Remove history entry and optionally delete its output | Supported | Not exposed | Intentional GTK parity gap |
| Inline playback | Renderer-scoped playback grant/cache plus external open | Direct local GTK video plus external open | Tauri has the stronger isolation/lifecycle model |
| Caption-track presentation | UI hook exists; current backend summaries do not populate a caption URL | Not exposed | Shared end-to-end gap until providers supply caption metadata |
| Copy job identifier | Dedicated clipboard action | Selectable identifier text | Minor GTK usability gap |
| Event-gap recovery | Sequenced envelopes and authoritative snapshot resync | In-process channel; no renderer boundary | Different architectures; no GTK work needed |
| Safe close | Renderer acknowledgement, draft flush, and monitor coordination | In-process draft flush and monitor coordination | Equivalent intent; architecture-specific protocol |
| Responsive/accessibility validation | Automated Svelte interaction/readiness tests | Native accessible roles plus display-optional smoke tests | Canonical frontend has broader automation |
| Supported packaging and platforms | Linux, Windows, and macOS release packaging | Unpackaged Linux-only developer build | Tauri is canonical |

## Remaining blockers before removal

1. Keep explicit CI compile and contract-test checks for
   `--features legacy-gtk --bin video-harness-gtk` throughout 0.7.x. Default
   library checks deliberately no longer compile optional GTK code.
2. GTK behavior tests that instantiate widgets remain display-optional. A
   real end-to-end GTK test requires a virtual/display server and is not a
   prerequisite for the canonical Tauri release.

History, drafts, settings, credentials, and generated files must remain
readable by the canonical application after GTK source removal. Removing GTK
must never imply deleting or relocating user data.
