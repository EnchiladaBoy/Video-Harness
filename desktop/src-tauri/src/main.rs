#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarlyExit {
    Help,
    Version,
}

fn early_exit(args: impl IntoIterator<Item = OsString>) -> Option<EarlyExit> {
    args.into_iter().find_map(|arg| match arg.to_str() {
        Some("-V" | "--version") => Some(EarlyExit::Version),
        Some("-h" | "--help") => Some(EarlyExit::Help),
        _ => None,
    })
}

fn print_help() {
    println!(
        "Video Harness\n\nUsage: video-harness [OPTIONS]\n\nOptions:\n  -V, --version  Print version\n  -h, --help     Print help"
    );
}

fn main() {
    match early_exit(std::env::args_os().skip(1)) {
        Some(EarlyExit::Version) => {
            println!("video-harness {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some(EarlyExit::Help) => {
            print_help();
            return;
        }
        None => {}
    }

    video_harness_desktop::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_both_version_flags() {
        assert_eq!(early_exit(["-V".into()]), Some(EarlyExit::Version));
        assert_eq!(early_exit(["--version".into()]), Some(EarlyExit::Version));
    }

    #[test]
    fn recognizes_both_help_flags() {
        assert_eq!(early_exit(["-h".into()]), Some(EarlyExit::Help));
        assert_eq!(early_exit(["--help".into()]), Some(EarlyExit::Help));
    }

    #[test]
    fn leaves_desktop_launch_arguments_alone() {
        assert_eq!(early_exit([]), None);
        assert_eq!(early_exit(["video.mp4".into()]), None);
    }
}
