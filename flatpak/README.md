# Flatpak packaging

The manifest builds `io.github.EnchiladaBoy.VideoHarness` against GNOME 50.
Cargo dependencies are pinned by `native/Cargo.lock` and materialized by
`cargo-sources.json`, so the actual Flatpak build runs with Cargo networking
disabled.

After changing dependencies, regenerate and verify the source list:

```bash
flatpak/generate-cargo-sources.sh
flatpak/check-manifest.sh
```

Install the build inputs from Flathub:

```bash
flatpak install --user flathub \
  org.gnome.Platform//50 \
  org.gnome.Sdk//50 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08 \
  org.freedesktop.Platform.codecs-extra//25.08-extra
```

Then build an unsigned local repository and smoke-test it:

```bash
flatpak-builder --user --force-clean --default-branch=stable \
  --repo=flatpak-repo \
  flatpak-build flatpak/io.github.EnchiladaBoy.VideoHarness.yml
flatpak --user remote-add --if-not-exists --no-gpg-verify \
  video-harness-local flatpak-repo
flatpak --user install -y video-harness-local \
  io.github.EnchiladaBoy.VideoHarness//stable
flatpak run io.github.EnchiladaBoy.VideoHarness --version
flatpak run --command=sh io.github.EnchiladaBoy.VideoHarness \
  -c 'gst-inspect-1.0 avdec_h264'
```

CI also passes `flatpak info --show-permissions` through
`check-installed-permissions.sh`, which requires every intended grant and
rejects broad host or home access.

`codecs-extra` is an add-on to the Freedesktop base used by GNOME 50. It is
installed explicitly in CI so H.264 MP4 discovery is tested on both
architectures.

The release workflow assembles both architecture refs in one OSTree repository,
then pauses at GitHub's protected `release` environment. Only that job imports
the time-limited signing subkey. Set these environment secrets:

- `FLATPAK_GPG_PRIVATE_KEY`: ASCII-armored private signing subkey.
- `FLATPAK_GPG_PASSPHRASE`: subkey passphrase.
- `FLATPAK_GPG_KEY_ID`: full signing subkey fingerprint.

Configure required reviewers on the `release` environment. The corresponding
primary identity is `Video Harness Release
<EnchiladaBoy@users.noreply.github.com>`. Keep the primary key offline; never
store it, the subkey, or either passphrase in this repository.
