//! Lightweight git surface: shells out to `git` (no libgit2 dependency) for
//! branch status and per-line gutter marks.
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Default)]
pub struct Status {
    pub is_repo: bool,
    pub branch: String,
    pub dirty: usize,
    pub ahead: usize,
    pub behind: usize,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

pub fn status(root: &Path) -> Status {
    let Some(branch) = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return Status::default();
    };
    let branch = branch.trim().to_string();
    let dirty = git(root, &["status", "--porcelain"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    let (mut ahead, mut behind) = (0, 0);
    if let Some(lr) = git(
        root,
        &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
    ) {
        let mut it = lr.split_whitespace();
        behind = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        ahead = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    }
    Status {
        is_repo: true,
        branch,
        dirty,
        ahead,
        behind,
    }
}

/// Per-line gutter marks for one file (0-based rows), from `git diff -U0`.
pub fn changed_lines(root: &Path, file: &Path) -> HashMap<usize, Mark> {
    let f = file.to_string_lossy().to_string();
    match git(root, &["diff", "HEAD", "-U0", "--", &f]) {
        Some(out) => parse_hunks(&out),
        None => HashMap::new(),
    }
}

/// Parse `git diff -U0` hunk headers into per-row gutter marks (pure, testable).
pub fn parse_hunks(diff: &str) -> HashMap<usize, Mark> {
    let mut marks = HashMap::new();
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        // @@ -oldStart,oldCount +newStart,newCount @@
        let Some(plus) = line.split('+').nth(1) else {
            continue;
        };
        let newspec = plus.split('@').next().unwrap_or("").trim();
        let mut parts = newspec.split(',');
        let new_start: usize = parts
            .next()
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(0);
        let new_count: usize = parts
            .next()
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(1);
        let oldspec = line.split('-').nth(1).unwrap_or("");
        let old_count: usize = oldspec
            .split(' ')
            .next()
            .and_then(|s| s.split(',').nth(1))
            .and_then(|x| x.parse().ok())
            .unwrap_or(1);
        if new_count == 0 {
            marks.insert(new_start.saturating_sub(1), Mark::Deleted);
        } else {
            let kind = if old_count == 0 {
                Mark::Added
            } else {
                Mark::Modified
            };
            for r in 0..new_count {
                marks.insert((new_start + r).saturating_sub(1), kind);
            }
        }
    }
    marks
}
