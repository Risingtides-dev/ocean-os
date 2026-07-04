//! Patch application + stale detection.
//!
//! [`apply_patch`] / [`apply_section`] re-hash the current file; if it matches
//! the section's expected tag the ops are applied against exact 1-indexed line
//! numbers, otherwise a [`MismatchError`] is returned.
//!
//! The low-level line engine ([`apply_edits`]) is a faithful port of oh-my-pi
//! `packages/hashline/src/apply.ts` `applyEdits`, minus the model-mistake
//! leniency repair passes (`repairReplacementBoundaries`,
//! `repairAfterInsertLandings`) which are out of scope for v1.

use crate::format::{InsertPos, Op, Patch, Section};
use crate::hash::compute_file_hash;
use crate::mismatch::MismatchError;
use std::collections::BTreeMap;
use std::fmt;

/// Failure modes of [`apply_patch`] / [`apply_section`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The live file hash did not match the section's expected tag. Boxed to
    /// keep `ApplyError` small (the error carries the file's lines for context).
    Mismatch(Box<MismatchError>),
    /// An anchor referenced a line outside the file.
    LineOutOfBounds { line: usize, file_lines: usize },
    /// The patch had no sections, or [`apply_patch`] got a multi-section patch.
    Shape(String),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Mismatch(m) => write!(f, "{m}"),
            ApplyError::LineOutOfBounds { line, file_lines } => {
                write!(
                    f,
                    "Line {line} does not exist (file has {file_lines} lines)"
                )
            }
            ApplyError::Shape(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<MismatchError> for ApplyError {
    fn from(m: MismatchError) -> Self {
        ApplyError::Mismatch(Box::new(m))
    }
}

// ── Public entry points ─────────────────────────────────────────────────────

/// Apply a single-section patch to `current_text`.
///
/// Re-hashes `current_text`; on a match applies the section's ops and returns
/// the new text. On a hash mismatch returns [`ApplyError::Mismatch`] with
/// `hash_recognized = false` (no snapshot store is consulted on this pure path —
/// use [`crate::recovery::Recovery`] for store-aware stale recovery).
pub fn apply_patch(current_text: &str, patch: &Patch) -> Result<String, ApplyError> {
    match patch.sections.as_slice() {
        [] => Err(ApplyError::Shape("patch has no sections".into())),
        [section] => apply_section(current_text, section),
        _ => Err(ApplyError::Shape(
            "apply_patch expects a single-section patch (one file); use apply_section per file"
                .into(),
        )),
    }
}

/// Apply one section to `current_text` with strict (zero-drift) hash matching.
pub fn apply_section(current_text: &str, section: &Section) -> Result<String, ApplyError> {
    let actual = compute_file_hash(current_text);
    if actual != section.expected_hash {
        let anchor_lines: Vec<usize> = section.ops.iter().flat_map(Op::anchor_lines).collect();
        return Err(ApplyError::Mismatch(Box::new(MismatchError {
            path: Some(section.path.clone()),
            expected_file_hash: section.expected_hash.clone(),
            actual_file_hash: actual,
            file_lines: split_lines(current_text),
            anchor_lines,
            hash_recognized: false,
        })));
    }
    apply_ops(current_text, &section.ops)
}

/// Apply a section's ops to `text` WITHOUT the hash check (used by recovery,
/// which validates against a snapshot rather than the live tag).
pub fn apply_ops(text: &str, ops: &[Op]) -> Result<String, ApplyError> {
    let edits = lower_ops(ops);
    apply_edits(text, &edits)
}

// ── Low-level edit model ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cursor {
    Bof,
    Eof,
    Before(usize),
    After(usize),
}

#[derive(Debug, Clone)]
enum Edit {
    Insert {
        cursor: Cursor,
        text: String,
        replacement: bool,
        order: usize,
    },
    Delete {
        line: usize,
        order: usize,
    },
}

/// Lower section ops into the flat insert/delete edit list, mirroring OMP's
/// parser `#flushPending` lowering.
fn lower_ops(ops: &[Op]) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut order = 0usize;
    let mut next = || {
        let o = order;
        order += 1;
        o
    };
    for op in ops {
        match op {
            Op::Del { start, end } => {
                for line in *start..=*end {
                    edits.push(Edit::Delete {
                        line,
                        order: next(),
                    });
                }
            }
            Op::Swap { start, end, body } => {
                // Replacement inserts before the range start, then the range is
                // deleted line-by-line.
                for text in body {
                    edits.push(Edit::Insert {
                        cursor: Cursor::Before(*start),
                        text: text.clone(),
                        replacement: true,
                        order: next(),
                    });
                }
                for line in *start..=*end {
                    edits.push(Edit::Delete {
                        line,
                        order: next(),
                    });
                }
            }
            Op::Ins { pos, body } => {
                let make = |text: &String, order: usize| match pos {
                    InsertPos::Pre(l) => Edit::Insert {
                        cursor: Cursor::Before(*l),
                        text: text.clone(),
                        replacement: false,
                        order,
                    },
                    InsertPos::Post(l) => Edit::Insert {
                        cursor: Cursor::After(*l),
                        text: text.clone(),
                        replacement: false,
                        order,
                    },
                    InsertPos::Head => Edit::Insert {
                        cursor: Cursor::Bof,
                        text: text.clone(),
                        replacement: false,
                        order,
                    },
                    InsertPos::Tail => Edit::Insert {
                        cursor: Cursor::Eof,
                        text: text.clone(),
                        replacement: false,
                        order,
                    },
                };
                for text in body {
                    let o = next();
                    edits.push(make(text, o));
                }
            }
        }
    }
    edits
}

/// Split text into lines the way OMP does: `text.split('\n')`. A newline-
/// terminated file yields a trailing `""` phantom sentinel.
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_string).collect()
}

/// The trailing phantom line number (1-indexed), or 0 when there is none.
fn trailing_phantom_line(file_lines: &[String]) -> usize {
    if file_lines.len() > 1 && file_lines.last().map(String::as_str) == Some("") {
        file_lines.len()
    } else {
        0
    }
}

/// Insert `add` at the start of `lines`, collapsing an empty single-line file.
fn insert_at_start(lines: &mut Vec<String>, add: &[String]) {
    if add.is_empty() {
        return;
    }
    if lines.len() == 1 && lines[0].is_empty() {
        lines.splice(0..1, add.iter().cloned());
    } else {
        lines.splice(0..0, add.iter().cloned());
    }
}

/// Insert `add` at the end of `lines`, before the trailing-newline phantom.
/// Returns the 1-indexed line that changed, if any.
fn insert_at_end(lines: &mut Vec<String>, add: &[String]) -> Option<usize> {
    if add.is_empty() {
        return None;
    }
    if lines.len() == 1 && lines[0].is_empty() {
        lines.splice(0..1, add.iter().cloned());
        return Some(1);
    }
    let has_trailing_newline = lines.last().map(String::as_str) == Some("");
    let insert_index = if has_trailing_newline {
        lines.len() - 1
    } else {
        lines.len()
    };
    lines.splice(insert_index..insert_index, add.iter().cloned());
    Some(insert_index + 1)
}

/// Apply a flat edit list to `text`. Port of OMP `applyEdits` (core loop only).
fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, ApplyError> {
    if edits.is_empty() {
        return Ok(text.to_string());
    }
    let mut file_lines = split_lines(text);
    let phantom = trailing_phantom_line(&file_lines);

    // Drop deletes that land on the trailing phantom line (stripping the final
    // newline is not an intended edit).
    let edits: Vec<&Edit> = edits
        .iter()
        .filter(|e| !matches!(e, Edit::Delete { line, .. } if *line == phantom))
        .collect();

    // Validate that every anchored edit points at an existing line.
    for e in &edits {
        let anchor = match e {
            Edit::Delete { line, .. } => Some(*line),
            Edit::Insert {
                cursor: Cursor::Before(l) | Cursor::After(l),
                ..
            } => Some(*l),
            Edit::Insert { .. } => None,
        };
        if let Some(line) = anchor {
            if line < 1 || line > file_lines.len() {
                return Err(ApplyError::LineOutOfBounds {
                    line,
                    file_lines: file_lines.len(),
                });
            }
        }
    }

    // Partition into bof / eof / anchor buckets.
    let mut bof_lines: Vec<String> = Vec::new();
    let mut eof_lines: Vec<String> = Vec::new();
    // line -> Vec<&Edit> (kept in source order via order key).
    let mut by_line: BTreeMap<usize, Vec<&Edit>> = BTreeMap::new();

    for e in &edits {
        match e {
            Edit::Insert {
                cursor: Cursor::Bof,
                text,
                ..
            } => bof_lines.push(text.clone()),
            Edit::Insert {
                cursor: Cursor::Eof,
                text,
                ..
            } => eof_lines.push(text.clone()),
            Edit::Delete { line, .. } => by_line.entry(*line).or_default().push(e),
            Edit::Insert {
                cursor: Cursor::Before(l) | Cursor::After(l),
                ..
            } => by_line.entry(*l).or_default().push(e),
        }
    }

    // Apply per-line buckets bottom-up so earlier indices stay valid.
    let lines_desc: Vec<usize> = by_line.keys().rev().copied().collect();
    for line in lines_desc {
        let mut bucket = by_line.remove(&line).unwrap();
        bucket.sort_by_key(|e| match e {
            Edit::Insert { order, .. } | Edit::Delete { order, .. } => *order,
        });

        let idx = line - 1;
        let current_line = file_lines.get(idx).cloned().unwrap_or_default();
        let mut before_inserts: Vec<String> = Vec::new();
        let mut after_inserts: Vec<String> = Vec::new();
        let mut replacement_lines: Vec<String> = Vec::new();
        let mut delete_line = false;

        for e in bucket {
            match e {
                Edit::Insert {
                    replacement: true,
                    text,
                    ..
                } => replacement_lines.push(text.clone()),
                Edit::Insert {
                    cursor: Cursor::After(_),
                    text,
                    ..
                } => after_inserts.push(text.clone()),
                Edit::Insert { text, .. } => before_inserts.push(text.clone()),
                Edit::Delete { .. } => delete_line = true,
            }
        }

        let mut replacement: Vec<String> = Vec::new();
        replacement.extend(before_inserts);
        replacement.extend(replacement_lines);
        if !delete_line {
            replacement.push(current_line);
        }
        replacement.extend(after_inserts);

        file_lines.splice(idx..idx + 1, replacement);
    }

    if !bof_lines.is_empty() {
        insert_at_start(&mut file_lines, &bof_lines);
    }
    insert_at_end(&mut file_lines, &eof_lines);

    Ok(file_lines.join("\n"))
}
