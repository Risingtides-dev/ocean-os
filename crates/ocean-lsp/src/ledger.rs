//! Diagnostics ledger: dedupe so the model only sees NEW problems.
//!
//! Ported from oh-my-pi's `DiagnosticsLedger`. A diagnostic's *identity* is its
//! message with any leading `path:line:col`-style location prefix stripped —
//! line numbers shift as edits land, but the message text is stable. `reduce()`
//! returns only diagnostics not previously seen for that file and records them.
//! Net effect: after an edit, the model reads the problems the edit introduced,
//! not the file's whole pre-existing backlog again and again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::client::Diagnostic;

#[derive(Default)]
pub struct DiagnosticsLedger {
    seen: HashMap<PathBuf, HashSet<String>>,
}

impl DiagnosticsLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Identity of a diagnostic: severity + message with location prefixes
    /// stripped, so a line shift does not make an old problem look new.
    fn identity(d: &Diagnostic) -> String {
        let msg = strip_location_prefix(&d.message);
        format!("{}:{}", d.severity, msg)
    }

    /// Return only the diagnostics for `path` not seen before, recording them.
    pub fn reduce(&mut self, path: &Path, diags: &[Diagnostic]) -> Vec<Diagnostic> {
        let seen = self.seen.entry(path.to_path_buf()).or_default();
        diags
            .iter()
            .filter(|d| seen.insert(Self::identity(d)))
            .cloned()
            .collect()
    }

    /// Forget a file (e.g. the model asked for the full picture via a fresh
    /// `diagnostics` action).
    pub fn reset(&mut self, path: &Path) {
        self.seen.remove(path);
    }
}

/// Strip a leading `path:12:34:` / `12:34:` location prefix from a message.
fn strip_location_prefix(msg: &str) -> &str {
    // Find a prefix of the form `<non-space>*:<digits>(:<digits>)?:` and cut it.
    let bytes = msg.as_bytes();
    let mut idx = 0;
    let mut colon_groups = 0;
    let mut saw_digit_group = false;
    let mut last_cut = 0;
    while idx < bytes.len() && idx < 256 {
        let b = bytes[idx];
        if b == b' ' && colon_groups == 0 {
            break; // spaces before any colon → not a location prefix
        }
        if b == b':' {
            colon_groups += 1;
            // Check whether the NEXT group is digits.
            let rest = &msg[idx + 1..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                saw_digit_group = true;
                last_cut = idx + 1 + digits.len();
            }
        }
        idx += 1;
    }
    if saw_digit_group && last_cut < msg.len() && msg[last_cut..].starts_with(':') {
        msg[last_cut + 1..].trim_start()
    } else {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            line,
            severity: "error",
            message: msg.into(),
        }
    }

    #[test]
    fn reduce_reports_each_problem_once() {
        let mut ledger = DiagnosticsLedger::new();
        let path = Path::new("/w/src/main.rs");
        let first = ledger.reduce(path, &[diag(3, "cannot find value `x`")]);
        assert_eq!(first.len(), 1);
        // Same message again (even at a shifted line) → suppressed.
        let again = ledger.reduce(path, &[diag(7, "cannot find value `x`")]);
        assert!(again.is_empty(), "shifted duplicate must be suppressed");
        // A genuinely new problem still surfaces.
        let fresh = ledger.reduce(path, &[diag(9, "mismatched types")]);
        assert_eq!(fresh.len(), 1);
    }

    #[test]
    fn location_prefixes_are_stripped_from_identity() {
        assert_eq!(
            strip_location_prefix("src/main.rs:3:10: unused variable"),
            "unused variable"
        );
        assert_eq!(strip_location_prefix("plain message"), "plain message");
        assert_eq!(
            strip_location_prefix("expected `:` after field"),
            "expected `:` after field",
            "colons inside a normal message must not be treated as a location"
        );
    }
}
