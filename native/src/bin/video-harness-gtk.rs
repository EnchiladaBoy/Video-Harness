use std::process::ExitCode;

const HELP: &str = "Video Harness legacy GTK frontend

Usage: video-harness-gtk [OPTIONS]

Options:
  -h, --help       Print help
  -V, --version    Print version

This maintenance-only frontend is deprecated. The supported Video Harness
desktop application is the Tauri/Svelte executable named `video-harness`.

Run without arguments to launch the legacy GTK interface.";

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => {}
        [argument] if argument == "-h" || argument == "--help" => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        [argument] if argument == "-V" || argument == "--version" => {
            println!(
                "video-harness-gtk {} (legacy frontend; deprecated)",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::SUCCESS;
        }
        [argument, ..] => {
            eprintln!(
                "error: unexpected argument '{}'\n\n{HELP}",
                argument.to_string_lossy()
            );
            return ExitCode::from(2);
        }
    }
    #[allow(deprecated)]
    match video_harness::gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Video Harness legacy GTK frontend could not start: {error}");
            ExitCode::FAILURE
        }
    }
}
