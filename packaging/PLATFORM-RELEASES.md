# Windows and macOS releases

The supported desktop targets use the Tauri/Svelte application and the same
local Rust service as Linux. They do not add an account, telemetry, cloud sync,
or a background updater. Provider API keys stay in the operating system's
credential store and generation state stays on the machine.

## Supported targets

| Platform | Release artifacts | Runtime baseline |
| --- | --- | --- |
| Windows x64 | signed NSIS `.exe` and MSI `.msi` installers | Windows 10 22H2 or Windows 11 |
| macOS Intel | signed and notarized `.dmg` | macOS 12 or newer |
| macOS Apple Silicon | signed and notarized `.dmg` | macOS 12 or newer |

The Windows installer includes the offline WebView2 runtime, so the interface
does not depend on a download during installation. NSIS installs for the
current user without elevation. The MSI is available for managed deployments
and keeps a fixed upgrade code across releases. Downgrades are blocked by the
installer because an older build may not understand newer local state.

Tauri obtains that runtime from Microsoft's Evergreen Standalone Installer
link. Immediately after packaging—and before CI executes or uploads an
installer—`verify-webview2-offline-installer.ps1` requires the NSIS and MSI
source copies to be byte-identical and validates Microsoft's trusted
Authenticode signature, code-signing purpose, and timestamp on both copies.
The evergreen URL can move to a newer Microsoft-signed build, so Windows
packages are publisher-verified but not byte-for-byte reproducible across
different build dates.

The macOS release uses the hardened runtime and a deliberately empty
entitlements file. It is a direct-download Developer ID build, not a Mac App
Store sandbox build. Sandboxing would require security-scoped bookmarks for
remembered local reference files, which the application does not currently
store.

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
../ui/node_modules/.bin/tauri build --ci --bundles nsis,msi -- --locked
```

Artifacts are placed below
`desktop/src-tauri/target/release/bundle/nsis` and
`desktop/src-tauri/target/release/bundle/msi`.

On macOS:

```bash
cd desktop
../ui/node_modules/.bin/tauri build --ci --bundles app,dmg -- --locked
```

Artifacts are placed below
`desktop/src-tauri/target/release/bundle/macos` and
`desktop/src-tauri/target/release/bundle/dmg`. Build once on Intel and once on
Apple Silicon; do not label a single-architecture binary as universal.

## Release signing

Signing material must be stored as protected CI secrets, never in the source
tree or an artifact. Unsigned local builds are useful for development but are
not supported release artifacts.

The checked-in workflow currently supports an exportable PFX. Many current
public-trust OV/EV certificates keep private keys in hardware or a remote
service; those require a separately reviewed Tauri `signCommand` integration
and are not interchangeable with the PFX secrets below.

Windows release signing needs:

- `WINDOWS_CERTIFICATE`: a base64-encoded PFX containing an Authenticode
  code-signing certificate and private key, imported only into the runner's
  temporary current-user certificate store;
- `WINDOWS_CERTIFICATE_PASSWORD`;
- access to the configured timestamp service.

The checked-in Windows configuration fixes SHA-256 as the digest and uses the
RFC 3161 timestamp protocol. Derive `certificateThumbprint` from the imported PFX and
supply it with an ephemeral Tauri configuration generated inside CI, then
delete the temporary certificate and configuration even when the build fails.
Verify both the executable and each installer with
`Get-AuthenticodeSignature` before publishing. Also run
`packaging/verify-webview2-offline-installer.ps1` immediately after the Tauri
build and before executing either installer; the checked-in workflows make
this a mandatory gate.

macOS release signing and notarization needs:

- `APPLE_CERTIFICATE`: base64-encoded Developer ID Application `.p12`;
- `APPLE_CERTIFICATE_PASSWORD`;
- `APPLE_SIGNING_IDENTITY`;
- App Store Connect API credentials (`APPLE_API_ISSUER`, `APPLE_API_KEY`, and
  a temporary `APPLE_API_KEY_PATH`). `APPLE_API_KEY` is the key ID; keep the
  base64-encoded private `.p8` in `APPLE_API_KEY_CONTENT`, decode it below the
  runner's temporary directory, and point `APPLE_API_KEY_PATH` there. The
  checked-in release workflow does not implement Apple ID/password
  notarization.

Tauri signs with the hardened runtime and submits for notarization when those
variables are present. Before publishing, require all of:

```bash
codesign --verify --deep --strict "Video Harness.app"
spctl --assess --type execute --verbose "Video Harness.app"
xcrun stapler validate "Video Harness.dmg"
```

Do not use `--skip-stapling` for a release artifact.

## Playback verification and platform blockers

Before the protected release environment is approved, exercise the CI package
candidates on real hardware: require a visible window, play the checked-in
H.264/AAC fixture through the in-app video element, use Open file and the native
file picker, verify the platform credential store, test a path containing spaces
and non-ASCII characters, and use a Videos/Movies folder on another volume.
Repeat those checks against the signed release artifacts before announcing the
release. The workflow waits for its second protected-environment approval after
uploading the signed build artifacts and before publication, so reviewers can
perform that check without publishing untested installers. Hosted CI validates
package structure, installation or mounting, and a process launch; only the
Linux AppImage smoke currently automates media decode and visible-window
detection.

macOS and standard Windows 10/11 installations provide H.264/AAC decoding.
Windows N editions require Microsoft's Media Feature Pack; this is an
operating-system component and cannot be installed silently by Video Harness.
Provider outputs using codecs unsupported by WebView2 or WebKit may require
Open file and a third-party player.

The remaining release blockers are external credentials and hardware: a
compatible exportable Windows signing PFX (or a future remote-signing
integration), a paid Apple Developer membership with a Developer ID certificate
and notarization credentials, and native Windows, Intel Mac, and Apple Silicon
hardware release checks. CI must refuse to publish Windows/macOS artifacts when
any required signing or verification step is unavailable.

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
