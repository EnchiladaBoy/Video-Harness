const IPC_COMMANDS: &[&str] = &[
    "open_session",
    "get_snapshot",
    "connect_provider",
    "forget_provider",
    "acknowledge_safety_hold",
    "choose_media",
    "attach_dropped_media",
    "add_remote_media",
    "prepare_generation",
    "submit_prepared",
    "invalidate_prepared",
    "save_draft",
    "acknowledge_close_request",
    "cancel_close_request",
    "save_draft_and_close",
    "pause_job",
    "resume_job",
    "pause_all_jobs",
    "resume_all_jobs",
    "delete_render",
    "open_output",
    "grant_playback",
    "release_playback",
];

fn main() {
    let windows = tauri_build::WindowsAttributes::new()
        .app_manifest(include_str!("windows-app-manifest.xml"));
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(windows)
            .app_manifest(tauri_build::AppManifest::new().commands(IPC_COMMANDS)),
    )
    .expect("failed to prepare the Video Harness Tauri application");
}
