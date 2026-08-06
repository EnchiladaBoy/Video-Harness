# Release packaging

The primary package is the signed, self-hosted Flatpak described in
[`../flatpak/README.md`](../flatpak/README.md). Native tarballs are a secondary,
best-effort option.

Release CI builds each native archive on its matching GitHub runner with:

```bash
docker run --rm \
  -e SOURCE_DATE_EPOCH \
  -v "$PWD:/source" \
  -w /source \
  rust:1.92-trixie \
  bash packaging/build-tarball-container.sh
```

The expected names are:

- `video-harness-0.6.0-linux-x86_64.tar.xz`
- `video-harness-0.6.0-linux-aarch64.tar.xz`

`packaging/build-tarball.sh` can also package an already-built executable. It
checks the version and machine architecture before writing the archive.
