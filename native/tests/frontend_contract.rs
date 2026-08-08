const MANIFEST: &str = include_str!("../Cargo.toml");
const LEGACY_POLICY: &str = include_str!("../LEGACY-GTK.md");
const CI_WORKFLOW: &str = include_str!("../../.github/workflows/ci.yml");
const RELEASE_WORKFLOW: &str = include_str!("../../.github/workflows/release.yml");

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

#[test]
fn windows_gui_version_smokes_wait_for_the_real_process_exit() {
    for (name, workflow, expected_calls) in
        [("CI", CI_WORKFLOW, 3), ("release", RELEASE_WORKFLOW, 1)]
    {
        assert!(
            workflow.contains("function Assert-VersionSmoke"),
            "{name} must use the controlled Windows GUI process helper"
        );
        assert!(
            workflow.contains("-ArgumentList @(\"--version\") -PassThru")
                && workflow.contains("$process.WaitForExit(30000)")
                && workflow.contains("if ($process.ExitCode -ne 0)"),
            "{name} must bound the version wait and read its actual exit code"
        );
        assert_eq!(
            workflow.matches("Assert-VersionSmoke -Executable").count(),
            expected_calls,
            "{name} must route every Windows version smoke through the helper"
        );
        for unsafe_call in [
            "& $binary.FullName --version",
            "& $binary --version",
            "& $installed[0].FullName --version",
            "& $msiBinary[0].FullName --version",
        ] {
            assert!(
                !workflow.contains(unsafe_call),
                "{name} must not rely on LASTEXITCODE after directly invoking a GUI executable"
            );
        }
    }
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
