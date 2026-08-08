# Video Harness release runbook

Releases publish the canonical Tauri/Svelte desktop application for Linux,
Windows, and macOS. Linux AppImages are unsigned; Windows installers must have
valid Authenticode signatures, and macOS disk images must contain a hardened,
Developer ID-signed application and a stapled Apple notarization ticket. The
workflow refuses to create a partial or unsigned desktop release.

Every release also includes a pinned SPDX JSON software bill of materials,
SHA-256 checksums, and GitHub keyless build-provenance attestations. Releases
do not add telemetry, accounts, cloud sync, or an update service.

## Release policy

- Build Linux x86_64 and aarch64, Windows x86_64, and macOS x86_64 and Apple
  Silicon on native GitHub-hosted runners.
- Pin npm, Cargo, GitHub Actions, and release-tool inputs. A build must not
  modify a lockfile. AppImage build executables and plugin scripts must pass
  the reviewed hashes in `packaging/prepare-tauri-appimage-tools.sh` before
  Tauri can execute them. Before any Windows package is executed or uploaded,
  require the two bundled WebView2 offline-runtime copies to match and pass
  Microsoft's Authenticode publisher, code-signing, and timestamp checks.
- Require an annotated release tag whose commit is contained in
  `main`, the exact commit's full push CI run to have succeeded or been rerun
  within seven days, and every version-bearing file to match the tag. Repeat
  RustSec and npm advisory checks during the release workflow so a newly
  disclosed issue cannot hide behind an older green run.
- Never publish an unsigned Windows or macOS artifact. Signing, timestamping,
  notarization, stapling, and local verification are mandatory workflow gates.
- Never replace an existing GitHub release. Correct a bad release with a new
  version.

## Protected release secrets

Enable GitHub's Dependency Graph under **Settings → Security and analysis**.
The pull-request dependency-review job deliberately fails closed when that
repository feature is unavailable; it must be green before a dependency update
is merged.

Create a protected GitHub Actions environment named `release`, restrict it to
the `v*` tag pattern, require maintainer review, and configure these as that
environment's secrets before pushing a tag. The first approval starts the
native signing jobs, which validate that their required secrets are present;
private-key values are exposed solely to those jobs. A second approval remains
blocked until all signed artifacts are ready for real-hardware review.

- `WINDOWS_CERTIFICATE`: base64-encoded password-protected, exportable
  Authenticode PFX. Hardware-backed or remote modern OV/EV certificates need a
  separately implemented and reviewed Tauri `signCommand` path.
- `WINDOWS_CERTIFICATE_PASSWORD`: the PFX password.
- `APPLE_CERTIFICATE`: base64-encoded Developer ID Application `.p12`.
- `APPLE_CERTIFICATE_PASSWORD`: the `.p12` password.
- `APPLE_SIGNING_IDENTITY`: the complete Developer ID Application identity.
- `APPLE_API_ISSUER`: App Store Connect API issuer ID.
- `APPLE_API_KEY`: App Store Connect API key ID.
- `APPLE_API_KEY_CONTENT`: base64-encoded private `.p8` key.

The runners import credentials into temporary current-user certificate stores
or keychains and delete those stores and decoded files in an `always()` cleanup
step. No certificate thumbprint, password, private key, or notarization token is
committed or uploaded as an artifact.

Protect the same `v*` release tag pattern against deletion or force-updates.
The workflow binds every checkout to the initially verified tag object and
commit and rechecks that binding immediately before publication, but repository
tag protection is the authoritative control against a later tag move.

## Cut v0.7.1

1. Confirm the complete CI run on `main` is green, including unsigned NSIS/MSI
   and both unsigned macOS package smokes, both AppImage builds and runtime
   smokes, and Apple Silicon tests.
2. Download the unsigned CI package-smoke artifacts and complete the real
   hardware checks from `packaging/PLATFORM-RELEASES.md` on Windows 10/11,
   Intel Mac, and Apple Silicon Mac. Record that evidence before approving the
   protected `release` environment; hosted process-launch smokes are not a
   substitute for playback, native picker, keyring, and non-ASCII path checks.
3. Confirm all protected release secrets above are present and the Windows
   certificate and Apple Developer membership are valid.
4. Run `packaging/check-release-version.sh v0.7.1` with `appstreamcli` and
   `desktop-file-validate` installed.
5. Create a signed annotated source tag when a maintainer signing identity is
   available, then push it:

   ```bash
   git tag -s v0.7.1 -m "Video Harness 0.7.1"
   git push origin v0.7.1
   ```

   An unsigned annotated tag is accepted only when the protected `v*` tag rule
   and protected `release` environment are active; the workflow rejects an
   invalid or unverifiable signature rather than treating it as unsigned.

6. The release workflow builds and verifies exactly these installers, uploads
   them as short-lived workflow artifacts, and waits at the second protected
   environment approval before publication:

   - `Video-Harness-0.7.1-linux-x86_64.AppImage`
   - `Video-Harness-0.7.1-linux-aarch64.AppImage`
   - `Video-Harness-0.7.1-windows-x86_64-setup.exe`
   - `Video-Harness-0.7.1-windows-x86_64.msi`
   - `Video-Harness-0.7.1-macos-x86_64.dmg`
   - `Video-Harness-0.7.1-macos-aarch64.dmg`
   - `Video-Harness-v0.7.1.spdx.json`
   - `SHA256SUMS`

7. While the publication job is waiting, download the signed workflow artifacts
   and repeat the platform checks from `packaging/PLATFORM-RELEASES.md` on real
   Windows 10/11, Intel Mac, and Apple Silicon Mac hardware. Include the
   checked-in H.264/AAC fixture, file selection, credential storage, non-ASCII
   paths, and a redirected or cross-volume Videos/Movies folder. Approve the
   final protected-environment deployment only after those checks pass.
8. After publication, download all release assets and run
   `sha256sum --check SHA256SUMS`. Verify GitHub's provenance attestations
   before installing anything.

The AppImages target glibc-based desktop Linux; Alpine/musl and unconfigured
NixOS are not supported. Windows N editions require Microsoft's Media Feature
Pack for H.264/AAC playback. Standard hosted runners install or mount and launch
the packaged application, but final playback, native picker/keyring, and window
interaction remain mandatory real-hardware release checks.
