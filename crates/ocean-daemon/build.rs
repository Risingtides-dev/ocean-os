//! Build script for `ocean-daemon`: embeds the git revision the binary was
//! compiled from as the `OCEAN_BUILD_REV` compile-time env, surfaced on
//! `/health` and `/ready` as `rev`. Provenance lets an operator verify the
//! supervised daemon is actually running the commit they expect — the freshness
//! and provenance the daemon's launchd supervisor can't otherwise prove.
//!
//! - `git rev-parse --short=12 HEAD` -> the short sha.
//! - `git diff-index HEAD` reports modified TRACKED files -> append `-dirty`.
//!   Untracked files are deliberately NOT dirt (TASK-25).
//! - Any git failure (no git on PATH, nonzero status, empty sha) -> `unknown`,
//!   so a deployed binary can never claim a precise commit it cannot prove.

fn main() {
    // `.git/HEAD` usually contains only `ref: refs/heads/main`, so its bytes do
    // not change as normal commits advance the branch. Watch HEAD, the resolved
    // symbolic ref, and packed-refs using git-aware absolute paths. This also
    // works in linked worktrees where `.git` is a pointer file and branch refs
    // live in the common git directory.
    for path in git_rerun_paths() {
        println!("cargo::rerun-if-changed={path}");
    }
    println!("cargo::rustc-env=OCEAN_BUILD_REV={}", build_rev());
}

fn git_rerun_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = git_path("HEAD") {
        paths.push(path);
    }
    if let Some(reference) = git_stdout(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(&reference) {
            paths.push(path);
        }
    }
    if let Some(path) = git_path("packed-refs") {
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn git_path(logical: &str) -> Option<String> {
    git_stdout(&["rev-parse", "--path-format=absolute", "--git-path", logical])
        .or_else(|| git_stdout(&["rev-parse", "--git-path", logical]))
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
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
    // Dirtiness means MODIFIED TRACKED CONTENT — not the presence of untracked
    // files (TASK-25). `git status --porcelain` reports untracked paths too, so
    // a stray build artifact or an unrelated scratch directory made a build
    // whose tracked tree is byte-identical to main stamp itself `-dirty`,
    // which then reads as "someone deployed unreviewed code" during an
    // incident. `diff-index` compares the working tree against HEAD over
    // tracked paths only.
    //
    // `update-index --refresh` first: diff-index compares stat metadata, so a
    // fresh checkout with rewritten mtimes (every deploy worktree) reports
    // false modifications until the index is refreshed. Its exit status is
    // deliberately ignored — it is nonzero exactly when it finds files needing
    // refresh, which is the normal case here, not an error.
    let _ = std::process::Command::new("git")
        .args(["update-index", "--refresh"])
        .output();
    // A failed `diff-index` (spawn error or an unexpected exit) means dirtiness
    // is unverifiable: fall back to `unknown` for the whole rev rather than
    // risk stamping a clean-looking sha for a worktree we couldn't inspect.
    let status = std::process::Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .ok()?;
    // diff-index --quiet: 0 = clean, 1 = tracked modifications, anything else
    // is a real failure.
    let dirty = match status.code() {
        Some(0) => false,
        Some(1) => true,
        _ => return None,
    };
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}
