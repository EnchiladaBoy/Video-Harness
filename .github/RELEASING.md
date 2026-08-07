# Video Harness release runbook

Releases publish the Tauri/Svelte application as signed Flatpaks for x86_64 and
aarch64, a signed update repository on GitHub Pages, and best-effort native
tarballs containing the same desktop GUI. No live provider request belongs in
release verification.

## One-time repository setup

1. Confirm the repository is `EnchiladaBoy/Video-Harness` and the local
   remote is `git@github.com:EnchiladaBoy/Video-Harness.git`.
2. Keep the repository public so both native architecture runners are
   available.
3. In **Settings → Pages**, select **GitHub Actions** as the source.
4. Create an environment named `release`, add required reviewers, prevent
   self-review if desired, and restrict deployment branches/tags to protected
   release tags.
5. Create a separate `github-pages` environment if GitHub has not made it
   automatically.

## Signing key

Use the dedicated identity:

```text
Video Harness Release <EnchiladaBoy@users.noreply.github.com>
```

Create an offline, expiry-dated certification primary key and an expiry-dated
signing subkey on an offline machine. Back up the revocation certificate and
primary secret key offline. Export only the signing subkey for CI. Review the
subkey fingerprint and expiry before every release.

Add these encrypted secrets to the protected `release` environment:

- `FLATPAK_GPG_PRIVATE_KEY`: ASCII-armored export containing the signing subkey
  and a stub, not the usable primary secret key.
- `FLATPAK_GPG_PASSPHRASE`: the signing subkey passphrase.
- `FLATPAK_GPG_KEY_ID`: the full signing subkey fingerprint.

Never place key exports in the workspace or repository. The release job exports
only the public key and attaches it to the release.

## Cut v0.7.0

1. Confirm `native/Cargo.toml`, `native/Cargo.lock`, desktop manifests,
   AppStream, and README all say `0.7.0`; run
   `packaging/check-release-version.sh`.
2. Run CI from `main` and require both Flatpak architecture jobs to pass,
   including `--version`, permissions, and H.264 discovery.
3. Create and push an annotated signed tag: `git tag -s v0.7.0` followed by
   `git push origin v0.7.0`.
4. GitHub must report the tag signature as verified. The workflow independently
   checks that verification before building.
5. Approve the protected `release` environment only after reviewing both
   unsigned architecture artifacts.
6. Verify the GitHub release contains both `.flatpak` bundles, both native
   archives, `SHA256SUMS`, its detached signature, the public key,
   `.flatpakref`, and `.flatpakrepo`. Verify the artifact attestations too.
7. Install `VideoHarness.flatpakref` on one x86_64 and one aarch64 machine, then
   confirm a subsequent repository update succeeds.

The signing job creates independent signed architecture commits, signs the
combined repository summary, and deploys that exact repository to
<https://enchiladaboy.github.io/Video-Harness/>. Native tarballs update manually.
