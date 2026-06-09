//! Ocean OS dev-task runner — the cargo-xtask pattern.
//!
//! A plain workspace binary (no third-party deps) invoked as `cargo xtask <cmd>`
//! via the alias in `/.cargo/config.toml`, or directly with
//! `cargo run -p xtask -- <cmd>`.
//!
//! Commands:
//!   check-webrtc-cache [--fix]       Detect a poisoned/partial libwebrtc cache.
//!   clear-webrtc-cache [--rebuild]   Remove poisoned libwebrtc build artifacts.
//!   help                             Print usage.

use std::process::ExitCode;

mod check_webrtc_cache;
mod paths;
mod webrtc_cache;

fn main() -> ExitCode {
    // args[0] is the binary path; the first real arg is the subcommand.
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();

    match cmd.as_deref() {
        Some("check-webrtc-cache") => check_webrtc_cache::run(&rest),
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
    check-webrtc-cache [--fix]
        Detect a poisoned/partial libwebrtc download-cache *before* the build
        fails with the cryptic \"could not find native static library `webrtc`\".
        Validates that the cached `libwebrtc.a` actually exists and is non-
        trivially sized; a cache dir that exists but lacks a usable lib is the
        existing-but-incomplete state webrtc-sys-build's early-return can't see.

        Exits non-zero (with a clear, actionable message) when a poisoned cache
        is found, so CI / a build preflight can fail loudly instead of cryptically.

        Flags:
            --fix   Self-heal: clear the poisoned cache (and the matching
                    webrtc-sys-* build + fingerprint dirs) so the next build
                    re-downloads libwebrtc cleanly.

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
