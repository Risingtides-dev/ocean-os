//! `clear-webrtc-cache` — un-poison the libwebrtc download-cache.
//!
//! ## Why this exists
//!
//! `ocean-call`'s `livekit-tap` feature pulls the native `livekit` client, which
//! depends on `webrtc-sys` / `webrtc-sys-build`. The build script downloads a
//! prebuilt libwebrtc archive into a persistent *scratch* dir under the target
//! directory. If that download is interrupted (Ctrl-C, dropped network, OOM kill)
//! the scratch dir is left **existing but incomplete**. On the next build the
//! script sees the dir, early-returns a false "success", and never re-fetches —
//! so the linker then fails with:
//!
//! ```text
//! could not find native static library `webrtc`
//! ```
//!
//! The manual fix has been:
//!
//! ```bash
//! rm -rf target/release/build/scratch-* \
//!        target/release/build/webrtc-sys-* \
//!        target/release/.fingerprint/webrtc-sys-*
//! ```
//!
//! This command does exactly that — across **both** the `debug` and `release`
//! profiles, respecting `CARGO_TARGET_DIR` — then optionally rebuilds.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use crate::paths::{dirs_with_prefix, resolve_target_dir};

/// Profiles whose build artifacts we sweep.
const PROFILES: &[&str] = &["debug", "release"];

/// `(subdir, prefix)` pairs to glob within each profile dir.
///
/// - `build/scratch-*`     — webrtc-sys-build's persistent download-cache /
///   scratch dir (the actual poisoning site; the incomplete archive lands here).
/// - `build/webrtc-sys-*`  — the per-crate build-script output dir.
/// - `.fingerprint/webrtc-sys-*` — cargo's fingerprint that otherwise convinces
///   cargo the crate is already built, so it won't re-run the build script.
const TARGETS: &[(&str, &str)] = &[
    ("build", "scratch-"),
    ("build", "webrtc-sys-"),
    (".fingerprint", "webrtc-sys-"),
];

pub fn run(args: &[String]) -> ExitCode {
    let mut rebuild = false;
    let mut release = false;
    for a in args {
        match a.as_str() {
            "--rebuild" => rebuild = true,
            "--release" => release = true,
            other => {
                eprintln!("clear-webrtc-cache: unknown flag `{other}`");
                eprintln!("  supported: --rebuild, --release");
                return ExitCode::FAILURE;
            }
        }
    }

    let target_dir = resolve_target_dir();
    println!("Target dir: {}", target_dir.display());

    if !target_dir.exists() {
        println!("Nothing to clear — target dir does not exist yet.");
        return maybe_rebuild(rebuild, release);
    }

    let mut removed: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<(PathBuf, std::io::Error)> = Vec::new();

    for profile in PROFILES {
        let profile_dir = target_dir.join(profile);
        if !profile_dir.exists() {
            continue;
        }
        for (subdir, prefix) in TARGETS {
            let scan = profile_dir.join(subdir);
            for dir in dirs_with_prefix(&scan, prefix) {
                match fs::remove_dir_all(&dir) {
                    Ok(()) => removed.push(dir),
                    Err(e) => errors.push((dir, e)),
                }
            }
        }
    }

    if removed.is_empty() && errors.is_empty() {
        println!("Nothing to clear — no poisoned webrtc artifacts found.");
        println!("(Looked for build/scratch-*, build/webrtc-sys-*, .fingerprint/webrtc-sys-* under debug + release.)");
    } else {
        if !removed.is_empty() {
            println!("Removed {} poisoned artifact dir(s):", removed.len());
            for p in &removed {
                println!("  - {}", p.display());
            }
        }
        if !errors.is_empty() {
            eprintln!("\nFailed to remove {} dir(s):", errors.len());
            for (p, e) in &errors {
                eprintln!("  - {}: {e}", p.display());
            }
            return ExitCode::FAILURE;
        }
        println!("\nlibwebrtc download-cache cleared. The next build will re-fetch it.");
    }

    maybe_rebuild(rebuild, release)
}

fn maybe_rebuild(rebuild: bool, release: bool) -> ExitCode {
    if !rebuild {
        println!(
            "\nNext: rebuild with the native-WebRTC feature, e.g.\n    \
             cargo build -p ocean-call --features livekit-tap"
        );
        return ExitCode::SUCCESS;
    }

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "-p", "ocean-call", "--features", "livekit-tap"]);
    if release {
        cmd.arg("--release");
    }
    println!(
        "\nRebuilding: cargo build -p ocean-call --features livekit-tap{}",
        if release { " --release" } else { "" }
    );

    match cmd.status() {
        Ok(s) if s.success() => {
            println!("Rebuild succeeded.");
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("Rebuild failed (exit {:?}).", s.code());
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Could not launch cargo: {e}");
            ExitCode::FAILURE
        }
    }
}
