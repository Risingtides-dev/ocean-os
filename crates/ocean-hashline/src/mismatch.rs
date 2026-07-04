//! Stale-tag mismatch error.
//!
//! Raised when a section's snapshot tag does not match the live file content and
//! recovery is unavailable / has failed. Faithful to oh-my-pi
//! `packages/hashline/src/mismatch.ts`.

use std::fmt;

/// Lines of context shown either side of a hash mismatch anchor.
pub const MISMATCH_CONTEXT: usize = 2;

/// Error raised when a hashline section's snapshot tag doesn't match the live
/// file's content (and recovery declined the merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MismatchError {
    /// The file path the section targeted, if known.
    pub path: Option<String>,
    /// The content-hash tag the section was bound to.
    pub expected_file_hash: String,
    /// The hash the live file actually computes to.
    pub actual_file_hash: String,
    /// The live file's lines (LF-split), for rendering anchored context.
    pub file_lines: Vec<String>,
    /// The 1-indexed anchor lines the edit referenced.
    pub anchor_lines: Vec<usize>,
    /// `true` when the expected hash resolved to a recorded snapshot (file
    /// content drifted since that snapshot); `false` when no snapshot was ever
    /// recorded for the hash (likely fabricated or carried from a prior
    /// session). Drives a more actionable rejection message.
    pub hash_recognized: bool,
}

impl MismatchError {
    /// The human-facing rejection header lines, matching OMP's two-branch
    /// message (drifted-since-snapshot vs never-seen tag).
    pub fn rejection_header(&self) -> Vec<String> {
        let path_text = match &self.path {
            Some(p) => format!(" for {p}"),
            None => String::new(),
        };
        if !self.hash_recognized {
            vec![
                format!(
                    "Edit rejected{path_text}: hash #{} is not from this session.",
                    self.expected_file_hash
                ),
                format!(
                    "The current file hashes to #{}. Re-read the file to copy a current [path#tag] header — never invent the tag and never reuse one from a prior session.",
                    self.actual_file_hash
                ),
            ]
        } else {
            vec![
                format!("Edit rejected{path_text}: file changed between read and edit."),
                format!(
                    "Section is bound to #{}, but the current file hashes to #{}. If a prior edit in this session modified this file, copy the [path#newhash] header from that edit's response; otherwise re-read the file to refresh the tag before retrying.",
                    self.expected_file_hash, self.actual_file_hash
                ),
            ]
        }
    }

    /// Numbered `LINE:TEXT` rows around each anchor (±[`MISMATCH_CONTEXT`]),
    /// `*`-marking the anchors and `...` between non-adjacent runs.
    pub fn anchored_context(&self) -> Vec<String> {
        format_anchored_context(&self.anchor_lines, &self.file_lines)
    }
}

impl fmt::Display for MismatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = self.rejection_header();
        let context = self.anchored_context();
        if !context.is_empty() {
            lines.push(String::new());
            lines.extend(context);
        }
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for MismatchError {}

/// Numbered `LINE:TEXT` context rows around `anchor_lines`.
pub fn format_anchored_context(anchor_lines: &[usize], file_lines: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut display: BTreeSet<usize> = BTreeSet::new();
    for &line in anchor_lines {
        if line < 1 || line > file_lines.len() {
            continue;
        }
        let lo = line.saturating_sub(MISMATCH_CONTEXT).max(1);
        let hi = (line + MISMATCH_CONTEXT).min(file_lines.len());
        for l in lo..=hi {
            display.insert(l);
        }
    }
    let anchor_set: BTreeSet<usize> = anchor_lines.iter().copied().collect();
    let mut rows = Vec::new();
    let mut previous: Option<usize> = None;
    for line_num in display {
        if let Some(prev) = previous {
            if line_num > prev + 1 {
                rows.push("...".to_string());
            }
        }
        previous = Some(line_num);
        let marker = if anchor_set.contains(&line_num) {
            "*"
        } else {
            " "
        };
        let text = file_lines
            .get(line_num - 1)
            .map(String::as_str)
            .unwrap_or("");
        rows.push(format!("{marker}{line_num}:{text}"));
    }
    rows
}
