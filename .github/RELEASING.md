# Video Harness release runbook

Releases publish the canonical Tauri/Svelte desktop application as intentionally
unsigned community binaries. The supported artifacts are Linux AppImages for
x86_64 and aarch64, one Windows x64 NSIS setup executable, and one Apple Silicon
macOS DMG. Windows MSI packages and Intel Mac builds are not release targets.

Unsigned distribution has a visible usability cost: Microsoft Defender
SmartScreen normally reports an unknown publisher, and macOS Gatekeeper normally
blocks the app's first launch. Release notes must say this plainly and direct
users to checksum and GitHub provenance verification before they use a per-app
override. Never advise users to disable either operating-system safeguard
globally or strip quarantine metadata.

Every release includes a pinned SPDX JSON software bill of materials, SHA-256
checksums, and GitHub keyless build-provenance attestations. Releases do not add
telemetry, accounts, cloud sync, or an update service.

## Release policy

- Build Linux x86_64 and aarch64, Windows x64, and macOS Apple Silicon on native
  GitHub-hosted runners.
- Publish only the two AppImages, Windows NSIS setup executable, Apple Silicon
  DMG, SPDX software bill of materials, and checksum manifest listed below.
- Pin npm, Cargo, GitHub Actions, and release-tool inputs. A build must not
  modify a lockfile. AppImage build executables and plugin scripts must pass
  the reviewed hashes in `packaging/prepare-tauri-appimage-tools.sh` before
  Tauri can execute them. Before the Windows package is executed or uploaded,
  require the bundled WebView2 offline runtime to pass Microsoft's Authenticode
  publisher, code-signing-purpose, and timestamp checks.
- Require an annotated `v*` release tag whose commit is contained in `main`, the
  exact commit's full push CI run to have succeeded or been rerun within seven
  days, and every version-bearing file to match the tag. Repeat RustSec and npm
  advisory checks during the release workflow so a newly disclosed issue cannot
  hide behind an older green run.
- Build and attest the release packages in GitHub Actions. The packages
  themselves remain unsigned; do not describe them as signed, notarized, or
  verified by Microsoft or Apple.
- Never replace an existing GitHub release. Correct a bad release with a new
  version.

The workflow publishes automatically only after an explicit annotated tag
passes all exact-CI, version, advisory, package, checksum, SBOM, and provenance
gates. It needs no code-signing certificate, Apple Developer membership,
notarization credential, protected environment, or manual deployment approval.
The repository's normal `GITHUB_TOKEN` is sufficient for release publication
and keyless attestations.

An active repository tag ruleset for `v*` that prevents deletion and force
updates is recommended hardening, but it is not a release prerequisite. Never
move or reuse a published release tag even when repository settings permit it.

Enable GitHub's Dependency Graph under **Settings → Security and analysis** so
the pull-request dependency-review job can assess dependency changes.

## Cut v0.7.2

1. Confirm the complete push CI run for the exact `main` commit is green,
   including Windows x64, both Linux AppImage architectures, Apple Silicon
   macOS, package smokes, dependency audits, and frontend tests.
2. Download the unsigned package-smoke artifacts and complete the real-hardware
   checks from `packaging/PLATFORM-RELEASES.md` on Windows 10/11 x64 and an Apple
   Silicon Mac. Record playback, native picker, credential-store, non-ASCII
   path, redirected/cross-volume Videos or Movies folder, and visible-window
   results. Intel Mac testing is not required or supported.
3. Run `packaging/check-release-version.sh v0.7.2` with `appstreamcli` and
   `desktop-file-validate` installed.
4. Create an unsigned annotated source tag and push it. Do not use a lightweight
   tag. The explicit configuration override below also works for maintainers
   whose Git configuration normally signs tags:

   ```bash
   git -c tag.gpgSign=false tag -a v0.7.2 -m "Video Harness 0.7.2"
   git push origin v0.7.2
   ```

5. The release workflow validates the tag and exact push CI run, rebuilds the
   packages, verifies their structure and launch behavior, generates checksums,
   SBOM, and keyless provenance, then automatically publishes exactly:

   - `Video-Harness-0.7.2-linux-x86_64.AppImage`
   - `Video-Harness-0.7.2-linux-aarch64.AppImage`
   - `Video-Harness-0.7.2-windows-x86_64-setup.exe`
   - `Video-Harness-0.7.2-macos-aarch64.dmg`
   - `Video-Harness-v0.7.2.spdx.json`
   - `SHA256SUMS`

6. Download each published package and `SHA256SUMS`. Compute each package's
   SHA-256 value and compare it with the manifest. Then verify its GitHub
   attestation, substituting each asset name in turn:

   ```bash
   gh attestation verify Video-Harness-0.7.2-linux-x86_64.AppImage \
     --repo EnchiladaBoy/Video-Harness
   ```

7. Confirm the release notes prominently identify the packages as unsigned,
   describe the expected SmartScreen and Gatekeeper warnings, and link to the
   per-app instructions in `packaging/PLATFORM-RELEASES.md`. Do not announce the
   release if a checksum, provenance verification, package smoke, or supported
   real-hardware check fails.

The AppImages target glibc-based desktop Linux; Alpine/musl and unconfigured
NixOS are not supported. Windows N editions require Microsoft's Media Feature
Pack for H.264/AAC playback. Hosted runners install or mount and launch each
package, but playback, native picker/keyring, and window interaction remain
real-hardware checks on supported Windows x64 and Apple Silicon Mac systems.
