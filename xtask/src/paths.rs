//! Shared path helpers for the webrtc-cache commands.
//!
//! Both `clear-webrtc-cache` and `check-webrtc-cache` need to find the cargo
//! target dir and enumerate prefixed subdirs the same way, so the logic lives
//! here once. std-only, like the rest of `xtask`.

use std::fs;
use std::path::{Path, PathBuf};

/// Resolve the cargo target directory:
/// 1. `$CARGO_TARGET_DIR` if set (absolute, or relative to the workspace root),
/// 2. else `<workspace_root>/target`.
///
/// The workspace root is the parent of this crate's manifest dir, because
/// `xtask` lives at `<workspace_root>/xtask`. `CARGO_MANIFEST_DIR` is set by
/// cargo at *build* time of xtask, so it's baked into the binary and correct
/// regardless of the cwd the operator runs from.
pub fn resolve_target_dir() -> PathBuf {
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
pub fn workspace_root() -> PathBuf {
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
pub fn dirs_with_prefix(dir: &Path, prefix: &str) -> Vec<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!(
            "xtask-paths-test-{tag}-{nanos}-{}",
            std::process::id()
        ));
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
