const MANIFEST: &str = include_str!("../Cargo.toml");
const LEGACY_POLICY: &str = include_str!("../LEGACY-GTK.md");

#[test]
fn portable_library_does_not_enable_gtk_by_default() {
    assert!(MANIFEST.contains("default = []"));
    assert!(MANIFEST.contains("legacy-gtk = [\"dep:adw\", \"dep:gtk\"]"));
    assert!(!MANIFEST.contains("default = [\"legacy-gtk\"]"));
}

#[test]
fn legacy_binary_is_explicit_and_cannot_shadow_the_canonical_name() {
    assert!(MANIFEST.contains("name = \"video-harness-gtk\""));
    assert!(MANIFEST.contains("path = \"src/bin/video-harness-gtk.rs\""));
    assert!(MANIFEST.contains("required-features = [\"legacy-gtk\"]"));
    assert!(!MANIFEST.contains("name = \"video-harness\"\npath = \"src/bin/"));
}

#[test]
fn native_archive_installer_is_explicitly_linux_only() {
    let installer = include_str!("../install.sh");
    assert!(
        installer.contains("native/install.sh is Linux-only"),
        "the legacy archive installer must fail clearly outside Linux"
    );

    let installer_tests = include_str!("installer.rs");
    assert!(
        installer_tests.contains("#![cfg(target_os = \"linux\")]"),
        "GNU/XDG installer tests must not run against macOS packaging"
    );
}

#[test]
fn deprecation_policy_names_the_canonical_frontend_and_removal_target() {
    assert!(LEGACY_POLICY.contains("Svelte interface hosted by Tauri is the canonical"));
    assert!(LEGACY_POLICY.contains("**0.8.0:** planned removal point"));
    assert!(LEGACY_POLICY.contains("History, drafts, settings, credentials, and generated files"));
}

#[cfg(feature = "legacy-gtk")]
#[test]
fn legacy_help_is_unambiguously_deprecated() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_video-harness-gtk"))
        .arg("--help")
        .output()
        .expect("run legacy help");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 help");

    assert!(output.status.success());
    assert!(stdout.contains("maintenance-only frontend is deprecated"));
    assert!(stdout.contains("Tauri/Svelte executable named `video-harness`"));
}
