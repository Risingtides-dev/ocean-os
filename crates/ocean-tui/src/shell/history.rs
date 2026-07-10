//! Persisted composer prompt history — the scoped slice of OMP's editor stack
//! (docs/specs/2026-07-03-omp-port-map.md, Slice 4: "persisted prompt history").
//!
//! Every submitted prompt is appended to a JSON-lines file under the ocean
//! config dir (`OCEAN_CONFIG_DIR` → `XDG_CONFIG_HOME/ocean-rs` → `~/.config/
//! ocean-rs`, file `tui_history`), capped and consecutive-deduped. The chat
//! composer loads it at startup for ↑/↓ recall and ⌃R fuzzy search.
//!
//! JSON-lines (one JSON string per line) rather than raw lines so multi-line
//! prompts (⌃J newline) round-trip without corrupting the ledger. All I/O is
//! **non-fatal**: a missing dir/file yields empty history, and write failures
//! are swallowed with a `tracing::warn` — history is a convenience, never a
//! reason to fail a turn.

use std::path::PathBuf;

/// Max prompts retained on disk and in memory. Old entries fall off the front.
const HISTORY_CAP: usize = 200;

/// The composer's prompt history: newest last, deduped on consecutive repeats.
#[derive(Default)]
pub(crate) struct PromptHistory {
    entries: Vec<String>,
    /// Resolved history file; `None` when no config dir could be resolved (then
    /// history is in-memory only and never persisted).
    path: Option<PathBuf>,
}

impl PromptHistory {
    /// Load history from disk, resolving the config dir like the rest of ocean.
    /// Any error (missing dir/file, unreadable, malformed lines) degrades to an
    /// empty-but-writable history rather than failing.
    pub(crate) fn load() -> Self {
        let path = history_path();
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str::<String>(l).ok())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut this = Self { entries, path };
        this.truncate_front();
        this
    }

    /// Number of entries (newest last).
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry at `idx` (0 = oldest), if present.
    pub(crate) fn get(&self, idx: usize) -> Option<&str> {
        self.entries.get(idx).map(String::as_str)
    }

    /// Read-only view, oldest first.
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record a submitted prompt: skip blanks and consecutive repeats, cap the
    /// ring, then persist (non-fatal). Returns whether it was actually appended.
    pub(crate) fn push(&mut self, prompt: &str) -> bool {
        let prompt = prompt.trim_end_matches('\n');
        if prompt.trim().is_empty() {
            return false;
        }
        if self.entries.last().map(String::as_str) == Some(prompt) {
            return false; // dedupe consecutive repeats
        }
        self.entries.push(prompt.to_string());
        self.truncate_front();
        self.persist();
        true
    }

    /// Keep only the newest [`HISTORY_CAP`] entries.
    fn truncate_front(&mut self) {
        let n = self.entries.len();
        if n > HISTORY_CAP {
            self.entries.drain(0..n - HISTORY_CAP);
        }
    }

    /// Rewrite the history file from the in-memory ring. Cheap (≤200 short
    /// lines) and keeps the on-disk cap enforced without a separate compaction
    /// pass. Errors are logged and ignored.
    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(error = %e, "tui: could not create history dir");
                return;
            }
        }
        let mut body = String::new();
        for e in &self.entries {
            match serde_json::to_string(e) {
                Ok(line) => {
                    body.push_str(&line);
                    body.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "tui: could not encode history entry"),
            }
        }
        if let Err(e) = std::fs::write(path, body) {
            tracing::warn!(error = %e, "tui: could not write prompt history");
        }
    }
}

/// Resolve the history file path: `OCEAN_CONFIG_DIR` → `XDG_CONFIG_HOME/ocean-rs`
/// → `~/.config/ocean-rs`, file `tui_history`. `None` when none resolve (rare —
/// no `$HOME`), in which case history stays in-memory only.
fn history_path() -> Option<PathBuf> {
    let dir = if let Some(p) = std::env::var_os("OCEAN_CONFIG_DIR") {
        PathBuf::from(p)
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("ocean-rs")
    } else {
        let home = std::env::var_os("HOME")?;
        PathBuf::from(home).join(".config").join("ocean-rs")
    };
    Some(dir.join("tui_history"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A history with no backing file — exercises the in-memory ring logic
    /// without touching the real config dir.
    fn mem() -> PromptHistory {
        PromptHistory {
            entries: Vec::new(),
            path: None,
        }
    }

    #[test]
    fn push_appends_newest_last() {
        let mut h = mem();
        assert!(h.push("first"));
        assert!(h.push("second"));
        assert_eq!(h.entries(), &["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn consecutive_repeats_are_deduped() {
        let mut h = mem();
        assert!(h.push("same"));
        assert!(!h.push("same"), "consecutive repeat is not re-appended");
        assert!(h.push("other"));
        assert!(h.push("same"), "non-consecutive repeat is kept");
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn blank_prompts_are_ignored() {
        let mut h = mem();
        assert!(!h.push("   "));
        assert!(!h.push("\n"));
        assert!(h.is_empty());
    }

    #[test]
    fn cap_drops_from_the_front() {
        let mut h = mem();
        for i in 0..(HISTORY_CAP + 25) {
            h.push(&format!("entry {i}"));
        }
        assert_eq!(h.len(), HISTORY_CAP);
        // Oldest surviving entry is #25; newest is the last pushed.
        assert_eq!(h.get(0), Some("entry 25"));
        assert_eq!(h.get(HISTORY_CAP - 1), Some(&format!("entry {}", HISTORY_CAP + 24)[..]));
    }

    #[test]
    fn trailing_newline_is_trimmed_on_push() {
        let mut h = mem();
        h.push("cmd\n");
        assert_eq!(h.get(0), Some("cmd"));
    }

    #[test]
    fn roundtrips_through_a_temp_file() {
        // Point OCEAN_CONFIG_DIR at a temp dir; write, reload, assert.
        let tmp = std::env::temp_dir().join(format!("ocean-tui-hist-{}", uuid::Uuid::new_v4()));
        std::env::set_var("OCEAN_CONFIG_DIR", &tmp);
        {
            let mut h = PromptHistory::load();
            assert!(h.is_empty());
            h.push("line one");
            h.push("multi\nline\nprompt");
        }
        {
            let h = PromptHistory::load();
            assert_eq!(h.len(), 2);
            assert_eq!(h.get(0), Some("line one"));
            assert_eq!(h.get(1), Some("multi\nline\nprompt"));
        }
        std::env::remove_var("OCEAN_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
