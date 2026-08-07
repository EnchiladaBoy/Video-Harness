//! Small cross-platform filesystem primitives used by persistence code.

use std::path::Path;

/// Atomically move `source` over `destination`, replacing an existing file.
///
/// Unix `rename(2)` already has replace semantics. Windows' Rust
/// `std::fs::rename` deliberately does not, so use the platform's
/// `MoveFileExW` primitive there. Both paths remain on the same filesystem
/// because callers create the temporary file beside its destination.
#[cfg(not(windows))]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both buffers are NUL-terminated UTF-16 strings and remain alive
    // for the duration of the call. Callers supply ordinary filesystem paths.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_overwrites_the_existing_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("replacement.tmp");
        let destination = directory.path().join("settings.json");
        std::fs::write(&source, b"new").expect("write source");
        std::fs::write(&destination, b"old").expect("write destination");

        replace_file(&source, &destination).expect("replace destination");

        assert_eq!(
            std::fs::read(&destination).expect("read destination"),
            b"new"
        );
        assert!(!source.exists());
    }
}
