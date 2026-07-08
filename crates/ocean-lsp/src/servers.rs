//! Builtin language-server definitions and auto-discovery.
//!
//! Scoping rule (John, port map): mechanisms over integration long-tail — ship
//! servers for the languages Ocean actually works in (Rust, TS/JS, Python, Go).
//! A server auto-enables when BOTH hold (oh-my-pi's rule):
//!   1. the workspace contains one of its `root_markers`, and
//!   2. its binary is on `$PATH`.
//!
//! Adding a language is a new [`ServerDef`] entry — never a code change
//! elsewhere.

use std::path::{Path, PathBuf};

/// One language server Ocean knows how to drive.
#[derive(Debug, Clone)]
pub struct ServerDef {
    /// Stable name, used in tool output and client registry keys.
    pub name: &'static str,
    /// Binary to spawn (must be on $PATH).
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Project files whose presence (walking cwd → root) enables the server.
    pub root_markers: &'static [&'static str],
    /// File extensions this server owns.
    pub extensions: &'static [&'static str],
}

/// The builtin server table.
pub const SERVERS: &[ServerDef] = &[
    ServerDef {
        name: "rust-analyzer",
        command: "rust-analyzer",
        args: &[],
        root_markers: &["Cargo.toml"],
        extensions: &["rs"],
    },
    ServerDef {
        name: "typescript-language-server",
        command: "typescript-language-server",
        args: &["--stdio"],
        root_markers: &["tsconfig.json", "package.json"],
        extensions: &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"],
    },
    ServerDef {
        name: "pyright",
        command: "pyright-langserver",
        args: &["--stdio"],
        root_markers: &["pyproject.toml", "setup.py", "requirements.txt", "Pipfile"],
        extensions: &["py"],
    },
    ServerDef {
        name: "gopls",
        command: "gopls",
        args: &[],
        root_markers: &["go.mod", "go.work"],
        extensions: &["go"],
    },
];

/// Whether `binary` resolves on `$PATH`.
pub fn binary_on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file()
    })
}

/// Find the nearest ancestor of `start` (inclusive) containing `marker`.
pub fn find_root(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        for marker in markers {
            if ancestor.join(marker).exists() {
                return Some(ancestor.to_path_buf());
            }
        }
    }
    None
}

/// Servers usable from `cwd`: root marker present AND binary on PATH. Returns
/// `(def, resolved_root)` pairs.
pub fn detect(cwd: &Path) -> Vec<(&'static ServerDef, PathBuf)> {
    SERVERS
        .iter()
        .filter_map(|def| {
            let root = find_root(cwd, def.root_markers)?;
            if binary_on_path(def.command) {
                Some((def, root))
            } else {
                None
            }
        })
        .collect()
}

/// The server owning `path`'s extension among `detected`.
pub fn server_for_file<'a>(
    detected: &'a [(&'static ServerDef, PathBuf)],
    path: &Path,
) -> Option<&'a (&'static ServerDef, PathBuf)> {
    let ext = path.extension()?.to_str()?;
    detected
        .iter()
        .find(|(def, _)| def.extensions.contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_root_walks_up() {
        let dir = std::env::temp_dir().join(format!("ocean-lsp-root-{}", std::process::id()));
        let nested = dir.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(find_root(&nested, &["Cargo.toml"]).unwrap(), dir);
        assert!(find_root(&nested, &["nonexistent.xyz"]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
