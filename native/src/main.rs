use std::process::ExitCode;

const HELP: &str = "Video Harness TUI

Usage: video-harness-tui [OPTIONS]

Options:
  -h, --help       Print help
  -V, --version    Print version

Run without arguments to launch the terminal interface.";

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => run_tui(),
        [argument] if argument == "-h" || argument == "--help" => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        [argument] if argument == "-V" || argument == "--version" => {
            println!("video-harness-tui {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [argument, ..] => {
            eprintln!(
                "error: unexpected argument '{}'\n\n{HELP}",
                argument.to_string_lossy()
            );
            ExitCode::from(2)
        }
    }
}

fn run_tui() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(video_harness::app::run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Video Harness TUI exited with an error: {error}");
            ExitCode::FAILURE
        }
    }
}
