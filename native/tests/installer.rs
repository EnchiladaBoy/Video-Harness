#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "openrouter-video-installer-{}-{nonce}",
        std::process::id()
    ))
}

fn run_installer(root: &Path, arguments: &[&str]) -> std::process::Output {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Command::new("bash")
        .arg(manifest.join("install.sh"))
        .args(arguments)
        .env("OPENROUTER_VIDEO_LIB_DIR", root.join("lib"))
        .env("OPENROUTER_VIDEO_BIN_DIR", root.join("bin"))
        .output()
        .expect("run installer")
}

fn assert_success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "installer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: std::process::Output, message: &str) {
    assert!(!output.status.success(), "installer unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "expected stderr to contain {message:?}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("resolve symlink")
}

fn cleanup(root: &Path) {
    let releases = root.join("lib/releases");
    if let Ok(entries) = fs::read_dir(&releases) {
        for entry in entries.flatten() {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
        }
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn beta_install_promotion_and_rollback_preserve_python_target() {
    if std::env::consts::ARCH != "aarch64" {
        // The production installer intentionally rejects non-ARM64 hosts.
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-openrouter-video.sh");
    let root = unique_temp_dir();
    fs::create_dir_all(root.join("bin")).expect("create test bin directory");

    let python_target = root.join("python-openrouter-video");
    fs::copy(&fixture, &python_target).expect("copy fake Python launcher");
    fs::set_permissions(&python_target, fs::Permissions::from_mode(0o755))
        .expect("make fake Python launcher executable");
    let stable = root.join("bin/openrouter-video");
    symlink(&python_target, &stable).expect("create current stable launcher");

    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));
    // Reinstalling identical bytes is idempotent.
    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));

    // A release version is immutable even if another executable reports the same version.
    let altered_fixture = root.join("altered-openrouter-video");
    fs::copy(&fixture, &altered_fixture).expect("copy altered fixture");
    fs::OpenOptions::new()
        .append(true)
        .open(&altered_fixture)
        .expect("open altered fixture")
        .write_all(b"\n# deliberately different fixture bytes\n")
        .expect("alter fixture bytes");
    fs::set_permissions(&altered_fixture, fs::Permissions::from_mode(0o755))
        .expect("make altered fixture executable");
    assert_failure(
        run_installer(
            &root,
            &[
                "install",
                altered_fixture
                    .to_str()
                    .expect("UTF-8 altered fixture path"),
            ],
        ),
        "already exists with different bytes",
    );

    let beta = root.join("bin/openrouter-video-rs");
    let python_alias = root.join("bin/openrouter-video-python");
    assert_eq!(resolved(&stable), python_target);
    assert_eq!(resolved(&python_alias), python_target);
    assert_eq!(
        resolved(&beta),
        root.join("lib/releases/0.2.0-test/openrouter-video")
    );
    assert_eq!(
        fs::metadata(root.join("lib/releases/0.2.0-test"))
            .expect("release metadata")
            .permissions()
            .mode()
            & 0o777,
        0o555
    );

    assert_success(run_installer(&root, &["promote"]));
    assert_eq!(resolved(&stable), resolved(&beta));
    assert_eq!(
        resolved(&root.join("lib/rollback/openrouter-video.previous")),
        python_target
    );

    // Repeating promotion is a no-op and must not replace Python rollback metadata.
    assert_success(run_installer(&root, &["promote"]));
    assert_eq!(
        resolved(&root.join("lib/rollback/openrouter-video.previous")),
        python_target
    );

    // A user-created regular file is never overwritten during rollback.
    fs::remove_file(&stable).expect("remove test stable symlink");
    fs::copy(&fixture, &stable).expect("create regular stable file");
    let output = run_installer(&root, &["rollback"]);
    assert_failure(output, "Refusing to replace the regular file");
    assert!(
        !fs::symlink_metadata(&stable)
            .expect("regular stable metadata")
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&stable).expect("remove regular stable fixture");
    symlink(&beta, &stable).expect("restore native stable symlink");

    assert_success(run_installer(&root, &["rollback"]));
    assert_eq!(resolved(&stable), python_target);
    assert_eq!(resolved(&python_alias), python_target);

    cleanup(&root);
}
