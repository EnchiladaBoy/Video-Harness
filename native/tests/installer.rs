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
        "video-harness-installer-{}-{nonce}",
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
        .env("VIDEO_HARNESS_DATA_DIR", root.join("share"))
        .env("VIDEO_HARNESS_PROJECT_DIR", root.join("project"))
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

fn fixture_version(fixture: &Path) -> String {
    let output = Command::new(fixture)
        .arg("--version")
        .output()
        .expect("read fixture version");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 fixture version")
        .split_whitespace()
        .last()
        .expect("version token")
        .to_owned()
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

fn installer_arch_supported() -> bool {
    matches!(std::env::consts::ARCH, "aarch64" | "x86_64")
}

#[test]
fn install_is_gui_only_and_retires_owned_native_transition_aliases() {
    if !installer_arch_supported() {
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-video-harness.sh");
    let version = fixture_version(&fixture);
    let root = unique_temp_dir();
    fs::create_dir_all(root.join("bin")).expect("create test bin directory");

    let old_release = root.join("lib/releases/0.2.0");
    fs::create_dir_all(&old_release).expect("create old release directory");
    let old_tui = old_release.join("video-harness-tui");
    fs::copy(&fixture, &old_tui).expect("copy old TUI fixture");
    fs::set_permissions(&old_tui, fs::Permissions::from_mode(0o555))
        .expect("make old TUI executable");
    symlink(&old_tui, root.join("bin/video-harness-tui")).expect("create old TUI link");
    symlink(&old_tui, root.join("bin/openrouter-video-rs")).expect("create old Rust link");

    let python_target = root.join("project/.venv/bin/openrouter-video");
    fs::create_dir_all(python_target.parent().expect("Python target parent"))
        .expect("create project Python environment");
    fs::copy(&fixture, &python_target).expect("copy external Python launcher");
    fs::set_permissions(&python_target, fs::Permissions::from_mode(0o755))
        .expect("make external Python launcher executable");
    let stable = root.join("bin/openrouter-video");
    symlink(&python_target, &stable).expect("create existing stable launcher");
    symlink(&python_target, root.join("bin/openrouter-video-python"))
        .expect("create owned Python alias");

    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));
    assert_eq!(
        resolved(&root.join("bin/video-harness")),
        root.join(format!("lib/releases/{version}/video-harness"))
    );
    assert!(!root.join("bin/video-harness-tui").exists());
    assert!(!root.join("bin/openrouter-video-rs").exists());
    assert!(!root.join("bin/openrouter-video-python").exists());
    assert!(
        python_target.is_file(),
        "retirement must not delete its target"
    );
    assert_eq!(resolved(&stable), python_target);
    assert!(
        root.join("share/applications/io.github.EnchiladaBoy.VideoHarness.desktop")
            .is_file()
    );
    assert!(
        root.join("share/metainfo/io.github.EnchiladaBoy.VideoHarness.metainfo.xml")
            .is_file()
    );
    assert!(
        root.join("share/icons/hicolor/scalable/apps/io.github.EnchiladaBoy.VideoHarness.svg")
            .is_file()
    );
    assert_eq!(
        fs::metadata(root.join(format!("lib/releases/{version}")))
            .expect("release metadata")
            .permissions()
            .mode()
            & 0o777,
        0o555
    );

    // Reinstalling identical bytes is idempotent.
    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));
    assert_eq!(resolved(&stable), python_target);

    // A release version is immutable even if another executable reports the same version.
    let altered_fixture = root.join("altered-video-harness");
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

    assert_success(run_installer(&root, &["status"]));
    assert_failure(run_installer(&root, &["promote"]), "Usage:");
    assert_failure(run_installer(&root, &["rollback"]), "Usage:");

    cleanup(&root);
}

#[test]
fn install_preserves_unowned_legacy_links_and_the_stable_command() {
    if !installer_arch_supported() {
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-video-harness.sh");
    let root = unique_temp_dir();
    fs::create_dir_all(root.join("bin")).expect("create test bin directory");

    let external_tui = root.join("external-tui");
    let external_python = root.join("external-python");
    let stable = root.join("bin/openrouter-video");
    fs::copy(&fixture, &external_tui).expect("copy external TUI");
    fs::copy(&fixture, &external_python).expect("copy external Python launcher");
    fs::copy(&fixture, &stable).expect("create regular stable launcher");
    fs::set_permissions(&external_tui, fs::Permissions::from_mode(0o755))
        .expect("make external TUI executable");
    fs::set_permissions(&external_python, fs::Permissions::from_mode(0o755))
        .expect("make external Python executable");
    fs::set_permissions(&stable, fs::Permissions::from_mode(0o755))
        .expect("make stable executable");
    symlink(&external_tui, root.join("bin/video-harness-tui")).expect("create user TUI link");
    symlink(&external_tui, root.join("bin/openrouter-video-rs")).expect("create user Rust link");
    symlink(&external_python, root.join("bin/openrouter-video-python"))
        .expect("create user Python link");

    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));
    assert_eq!(resolved(&root.join("bin/video-harness-tui")), external_tui);
    assert_eq!(
        resolved(&root.join("bin/openrouter-video-rs")),
        external_tui
    );
    assert_eq!(
        resolved(&root.join("bin/openrouter-video-python")),
        external_python
    );
    assert!(
        !fs::symlink_metadata(&stable)
            .expect("stable metadata")
            .file_type()
            .is_symlink()
    );

    cleanup(&root);
}

#[test]
fn install_preserves_regular_files_at_legacy_alias_names() {
    if !installer_arch_supported() {
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-video-harness.sh");
    let root = unique_temp_dir();
    fs::create_dir_all(root.join("bin")).expect("create test bin directory");

    for name in [
        "video-harness-tui",
        "openrouter-video-rs",
        "openrouter-video-python",
    ] {
        let path = root.join("bin").join(name);
        fs::copy(&fixture, &path).expect("create regular legacy sentinel");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make legacy sentinel executable");
    }

    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));
    for name in [
        "video-harness-tui",
        "openrouter-video-rs",
        "openrouter-video-python",
    ] {
        let metadata =
            fs::symlink_metadata(root.join("bin").join(name)).expect("legacy sentinel remains");
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
    }

    cleanup(&root);
}

#[test]
fn install_refuses_to_replace_a_regular_gui_launcher() {
    if !installer_arch_supported() {
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-video-harness.sh");
    let root = unique_temp_dir();
    fs::create_dir_all(root.join("bin")).expect("create test bin directory");
    fs::copy(&fixture, root.join("bin/video-harness")).expect("create regular GUI launcher");

    assert_failure(
        run_installer(
            &root,
            &["install", fixture.to_str().expect("UTF-8 fixture path")],
        ),
        "Refusing to replace the regular file",
    );

    cleanup(&root);
}

#[test]
fn uninstall_removes_only_owned_unmodified_integration_files() {
    if !installer_arch_supported() {
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("fixtures/fake-video-harness.sh");
    let version = fixture_version(&fixture);
    let root = unique_temp_dir();

    assert_success(run_installer(
        &root,
        &["install", fixture.to_str().expect("UTF-8 fixture path")],
    ));

    let modified_desktop =
        root.join("share/applications/io.github.EnchiladaBoy.VideoHarness.desktop");
    fs::OpenOptions::new()
        .append(true)
        .open(&modified_desktop)
        .expect("open installed desktop file")
        .write_all(b"\n# user customization\n")
        .expect("customize desktop file");
    let data_sentinel = root.join("lib/history.sqlite3");
    fs::write(&data_sentinel, b"must survive uninstall").expect("write data sentinel");

    assert_success(run_installer(&root, &["uninstall"]));
    assert!(!root.join("bin/video-harness").exists());
    assert!(
        modified_desktop.is_file(),
        "modified desktop file is preserved"
    );
    assert!(
        !root
            .join("share/metainfo/io.github.EnchiladaBoy.VideoHarness.metainfo.xml")
            .exists()
    );
    assert!(
        !root
            .join("share/icons/hicolor/scalable/apps/io.github.EnchiladaBoy.VideoHarness.svg")
            .exists()
    );
    assert!(
        root.join(format!("lib/releases/{version}/video-harness"))
            .is_file()
    );
    assert_eq!(
        fs::read(&data_sentinel).expect("read preserved data"),
        b"must survive uninstall"
    );

    // Repeating uninstall is harmless and still leaves modified/user data alone.
    assert_success(run_installer(&root, &["uninstall"]));
    assert!(modified_desktop.is_file());
    assert!(data_sentinel.is_file());

    cleanup(&root);
}
