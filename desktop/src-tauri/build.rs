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
    "pause_job",
    "resume_job",
    "delete_render",
    "open_output",
    "grant_playback",
    "release_playback",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(IPC_COMMANDS)),
    )
    .expect("failed to prepare the Video Harness Tauri application");
}
