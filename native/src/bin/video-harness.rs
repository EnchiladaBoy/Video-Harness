use std::process::ExitCode;

const HELP: &str = "Video Harness

Usage: video-harness [OPTIONS]

Options:
  -h, --help       Print help
  -V, --version    Print version

Run without arguments to launch the graphical interface.";

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => {}
        [argument] if argument == "-h" || argument == "--help" => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        [argument] if argument == "-V" || argument == "--version" => {
            println!("video-harness {}", env!("CARGO_PKG_VERSION"));
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
    match video_harness::gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Video Harness could not start: {error}");
            ExitCode::FAILURE
        }
    }
}
