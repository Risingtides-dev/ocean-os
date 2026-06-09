//! Ocean OS dev-task runner — the cargo-xtask pattern.
//!
//! A plain workspace binary (no third-party deps) invoked as `cargo xtask <cmd>`
//! via the alias in `/.cargo/config.toml`, or directly with
//! `cargo run -p xtask -- <cmd>`.
//!
//! Commands:
//!   clear-webrtc-cache [--rebuild]   Remove poisoned libwebrtc build artifacts.
//!   help                             Print usage.

use std::process::ExitCode;

mod webrtc_cache;

fn main() -> ExitCode {
    // args[0] is the binary path; the first real arg is the subcommand.
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();

    match cmd.as_deref() {
        Some("clear-webrtc-cache") => webrtc_cache::run(&rest),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`\n");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!(
        "\
Ocean OS xtask — repo dev-task runner

USAGE:
    cargo xtask <COMMAND> [ARGS]
    cargo run -p xtask -- <COMMAND> [ARGS]

COMMANDS:
    clear-webrtc-cache [--rebuild]
        Remove the poisoned libwebrtc download-cache + webrtc-sys build
        artifacts (across debug and release) so the next build re-fetches
        libwebrtc cleanly. Fixes:
            \"could not find native static library `webrtc`\"
        which recurs when an interrupted download leaves webrtc-sys-build's
        scratch dir existing-but-incomplete and the build script early-returns
        a false \"success\".

        Flags:
            --rebuild   After clearing, run `cargo build -p ocean-call
                        --features livekit-tap` to re-fetch + rebuild.
            --release   With --rebuild, build the release profile too.

    help
        Print this message.
"
    );
}
