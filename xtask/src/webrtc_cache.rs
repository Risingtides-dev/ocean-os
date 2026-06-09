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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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

/// Resolve the cargo target directory:
/// 1. `$CARGO_TARGET_DIR` if set (absolute, or relative to the workspace root),
/// 2. else `<workspace_root>/target`.
///
/// The workspace root is the parent of this crate's manifest dir, because
/// `xtask` lives at `<workspace_root>/xtask`. `CARGO_MANIFEST_DIR` is set by
/// cargo at *build* time of xtask, so it's baked into the binary and correct
/// regardless of the cwd the operator runs from.
fn resolve_target_dir() -> PathBuf {
    let workspace_root = workspace_root();
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(v) if !v.is_empty() => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                workspace_root.join(p)
            }
        }
        _ => workspace_root.join("target"),
    }
}

/// Workspace root = parent of the xtask manifest dir.
///
/// Falls back to the current dir if `CARGO_MANIFEST_DIR` is somehow unset
/// (e.g. the binary was moved and invoked directly), which keeps the relative
/// `target/` behavior sane when run from the repo root.
fn workspace_root() -> PathBuf {
    match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(manifest_dir) => {
            let p = PathBuf::from(manifest_dir);
            p.parent().map(Path::to_path_buf).unwrap_or(p)
        }
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// Return the immediate subdirectories of `dir` whose file name starts with
/// `prefix`. Empty if `dir` does not exist or can't be read.
fn dirs_with_prefix(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // file_type() avoids a stat where possible; fall back to is_dir().
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or_else(|_| path.is_dir());
        if !is_dir {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with(prefix) {
                out.push(path);
            }
        }
    }
    out
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
    println!("\nRebuilding: cargo build -p ocean-call --features livekit-tap{}", if release { " --release" } else { "" });

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("xtask-webrtc-test-{tag}-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn dirs_with_prefix_matches_only_prefixed_dirs() {
        let root = unique_tmp("prefix");
        fs::create_dir_all(root.join("scratch-abc123")).unwrap();
        fs::create_dir_all(root.join("scratch-def456")).unwrap();
        fs::create_dir_all(root.join("webrtc-sys-xyz")).unwrap();
        fs::create_dir_all(root.join("unrelated-crate")).unwrap();
        fs::write(root.join("scratch-not-a-dir"), b"file").unwrap();

        let mut got: Vec<String> = dirs_with_prefix(&root, "scratch-")
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["scratch-abc123", "scratch-def456"]);

        // The bare `scratch-not-a-dir` *file* must be excluded.
        assert!(!got.iter().any(|n| n == "scratch-not-a-dir"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dirs_with_prefix_missing_dir_is_empty() {
        let missing = unique_tmp("missing").join("does-not-exist");
        assert!(dirs_with_prefix(&missing, "scratch-").is_empty());
    }

    #[test]
    fn resolve_target_dir_honors_absolute_cargo_target_dir() {
        // Snapshot + restore the env var so we don't disturb a parallel build.
        let prev = std::env::var_os("CARGO_TARGET_DIR");
        let abs = unique_tmp("abs-target");
        std::env::set_var("CARGO_TARGET_DIR", &abs);
        assert_eq!(resolve_target_dir(), abs);
        match prev {
            Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
            None => std::env::remove_var("CARGO_TARGET_DIR"),
        }
        fs::remove_dir_all(&abs).ok();
    }
}
