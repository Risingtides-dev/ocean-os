//! Build script for `ocean-daemon`: embeds the git revision the binary was
//! compiled from as the `OCEAN_BUILD_REV` compile-time env, surfaced on
//! `/health` and `/ready` as `rev`. Provenance lets an operator verify the
//! supervised daemon is actually running the commit they expect — the freshness
//! and provenance the daemon's launchd supervisor can't otherwise prove.
//!
//! - `git rev-parse --short=12 HEAD` -> the short sha.
//! - `git status --porcelain` non-empty -> append `-dirty`.
//! - Any git failure (no git on PATH, nonzero status, empty sha) -> `unknown`,
//!   so a deployed binary can never claim a precise commit it cannot prove.

fn main() {
    // Rebuild whenever the checked-out HEAD moves so the embedded rev tracks the
    // actually-deployed commit rather than a stale build. In a linked git
    // worktree `.git` is a gitdir pointer rather than a directory, so this path
    // may not resolve there — cargo treats an unresolvable hint as a no-op, and
    // the value is still captured at that build.
    println!("cargo::rerun-if-changed=../../.git/HEAD");
    println!("cargo::rustc-env=OCEAN_BUILD_REV={}", build_rev());
}

/// The short, dirtiness-annotated build revision, or `"unknown"` when git is
/// unavailable or either command fails or yields no usable sha.
fn build_rev() -> String {
    try_build_rev().unwrap_or_else(|| "unknown".to_string())
}

fn try_build_rev() -> Option<String> {
    // `cargo` runs build scripts with the crate manifest dir as the working
    // directory; `git` walks up from there to the repo root, so this resolves
    // the same HEAD `git rev-parse` would in the checkout.
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if sha.is_empty() {
        return None;
    }
    // A failed `status` (spawn error or nonzero) means dirtiness is unverifiable:
    // fall back to `unknown` for the whole rev rather than risk stamping a
    // clean-looking sha for a worktree we couldn't actually inspect.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}
