# Release packaging

The primary package is the signed, self-hosted Flatpak described in
[`../flatpak/README.md`](../flatpak/README.md). Both it and the secondary native
tarballs contain the Tauri/Svelte GUI; the retired GTK frontend is not part of
stable release artifacts.

Build the locked web UI, then build each native archive in a container matching
the host CPU architecture:

```bash
npm --prefix ui ci
npm --prefix ui run build
SOURCE_DATE_EPOCH="$(git show -s --format=%ct)"
export SOURCE_DATE_EPOCH
docker run --rm \
  -e SOURCE_DATE_EPOCH \
  -v "$PWD:/source" \
  -w /source \
  rust:1.92-trixie \
  bash packaging/build-tarball-container.sh
```

The expected names are:

- `video-harness-0.7.0-linux-x86_64.tar.xz`
- `video-harness-0.7.0-linux-aarch64.tar.xz`

`packaging/build-tarball.sh` can build the Tauri executable after the UI exists,
or package an already-built executable. It checks the version and machine
architecture before writing the archive.
