# Video Harness release runbook

Releases publish the Tauri/Svelte desktop application as unsigned, single-file
AppImages for x86_64 and aarch64 Linux. GitHub hosts the executables and a plain
`SHA256SUMS` file. No personal signing key, release secret, Flatpak repository,
or GitHub Pages deployment is required.

## Release policy

- Build each architecture natively on GitHub's Ubuntu 22.04 runner. This keeps
  the glibc baseline older than the development runners and avoids unsupported
  ARM cross-bundling.
- Pin npm, Cargo, and GitHub Action inputs. A release build must not modify any
  lockfile.
- Require an unsigned annotated release tag whose commit is contained in
  `main`, and require every version-bearing file to match the tag.
- Never replace an existing GitHub release. Correct a bad release with a new
  version.
- Keep GitHub's keyless artifact attestations. They require no user-managed key
  or passphrase and record which workflow produced each file.

## Cut v0.7.0

1. Confirm the complete CI run on `main` is green, including both Portable
   AppImage jobs.
2. Run `packaging/check-release-version.sh v0.7.0` with `appstreamcli` and
   `desktop-file-validate` installed.
3. Create an annotated but unsigned tag and push it:

   ```bash
   git -c tag.gpgSign=false tag -a v0.7.0 -m "Video Harness 0.7.0"
   git push origin v0.7.0
   ```

4. The release workflow builds and publishes exactly these files:

   - `Video-Harness-0.7.0-linux-x86_64.AppImage`
   - `Video-Harness-0.7.0-linux-aarch64.AppImage`
   - `SHA256SUMS`

5. Download the assets and verify the hashes:

   ```bash
   sha256sum --check SHA256SUMS
   ```

6. On both CPU architectures, make the matching AppImage executable and smoke
   its version before opening the GUI. For example, on x86_64:

   ```bash
   chmod +x Video-Harness-0.7.0-linux-x86_64.AppImage
   ./Video-Harness-0.7.0-linux-x86_64.AppImage --version
   ```

7. Verify H.264/AAC playback, mute, seeking, file selection, downloads into the
   XDG Videos directory, and opening a completed render externally.

AppImage targets glibc-based desktop Linux. It does not promise Alpine/musl or
unconfigured NixOS compatibility. If FUSE mounting is unavailable, run the
same file with `--appimage-extract-and-run`.
