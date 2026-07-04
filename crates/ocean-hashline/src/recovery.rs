//! Stale-tag recovery: replay a would-be edit against a cached snapshot and land
//! it on the live file content. Faithful to oh-my-pi
//! `packages/hashline/src/recovery.ts` — all three strategies are **zero-fuzz**
//! (they never fuzzy-slide a hunk onto a distant duplicate).
//!
//! Fires when a patch is stale but its tag names a snapshot the store retains.
//! The strategies, in order:
//!  1. Apply the edits to the SNAPSHOT text, diff snapshot→result, then transplant
//!     that diff onto the current text with zero fuzz (handles external writes to
//!     other regions).
//!  2. Remap each op's anchor lines through the snapshot→current line diff,
//!     requiring ONE consistent non-zero offset, then replay on current.
//!  3. Session-chain replay: only when snapshot and current have EQUAL line
//!     counts AND every anchored row is byte-identical between them.
//!
//! Any hit returns `(merged_text, warning)`. All miss returns `None`.

use crate::format::{Op, Section};
use crate::patcher::apply_ops;
use crate::snapshot::SnapshotStore;
use similar::TextDiff;

/// An external write matched a cached head snapshot.
pub const RECOVERY_EXTERNAL_WARNING: &str =
    "Recovered from a stale file hash using a previous read snapshot (file changed externally between read and edit).";
/// A prior in-session edit advanced the hash (3-way merge onto current).
pub const RECOVERY_SESSION_CHAIN_WARNING: &str =
    "Recovered from a stale file hash using an earlier in-session snapshot (a prior edit in this session advanced the hash).";
/// Stale anchors were relocated to unchanged live lines after drift.
pub const RECOVERY_LINE_REMAP_WARNING: &str =
    "Recovered by remapping stale line anchors to unchanged current lines (file changed since the tagged read). Verify the diff matches your intent.";
/// Session-chain replay fast-path (less certain — verify the diff).
pub const RECOVERY_SESSION_REPLAY_WARNING: &str =
    "Recovered by replaying your edits onto the current file content (a prior in-session edit changed the lines you re-targeted with a stale hash). Verify the diff matches your intent.";

/// Stateless recovery driver over a [`SnapshotStore`].
pub struct Recovery;

impl Recovery {
    /// Attempt recovery for a stale section against `current_text`. Returns
    /// `Some((merged_text, warning))` on success, or `None` when no strategy
    /// applies (the caller should then raise a hard mismatch).
    pub fn try_recover(
        store: &SnapshotStore,
        section: &Section,
        current_text: &str,
    ) -> Option<(String, String)> {
        let snapshot = store.by_hash(&section.path, &section.expected_hash)?;
        let snapshot_text = snapshot.text.clone();
        let snapshot_stamp = snapshot.recorded_at;
        let is_head = store
            .head(&section.path)
            .map(|h| h.recorded_at == snapshot_stamp && h.hash == section.expected_hash)
            .unwrap_or(false);

        let recovery_warning = if is_head {
            RECOVERY_EXTERNAL_WARNING
        } else {
            RECOVERY_SESSION_CHAIN_WARNING
        };

        // Strategy 1: apply on the snapshot, transplant the diff onto current.
        if let Some(merged) = apply_to_snapshot(&snapshot_text, current_text, &section.ops) {
            if merged != current_text {
                return Some((merged, recovery_warning.to_string()));
            }
        }

        // Strategy 2: remap anchors through the snapshot→current line diff.
        if let Some(remapped) = replay_remapped_anchors(&snapshot_text, current_text, &section.ops)
        {
            if remapped != current_text {
                return Some((remapped, RECOVERY_LINE_REMAP_WARNING.to_string()));
            }
        }

        // Strategy 3: session-chain replay (only for a non-head snapshot).
        if !is_head {
            if let Some(replayed) = replay_session_chain(&snapshot_text, current_text, &section.ops)
            {
                if replayed != current_text {
                    return Some((replayed, RECOVERY_SESSION_REPLAY_WARNING.to_string()));
                }
            }
        }

        None
    }
}

fn split(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

// ── Strategy 1: apply-to-snapshot + zero-fuzz transplant ────────────────────

fn apply_to_snapshot(snapshot_text: &str, current_text: &str, ops: &[Op]) -> Option<String> {
    let applied = apply_ops(snapshot_text, ops).ok()?;
    if applied == snapshot_text {
        return None;
    }
    let snap_lines: Vec<String> = snapshot_text.split('\n').map(str::to_string).collect();
    let applied_lines: Vec<String> = applied.split('\n').map(str::to_string).collect();
    let mut current_lines: Vec<String> = current_text.split('\n').map(str::to_string).collect();

    let snap_ref: Vec<&str> = snap_lines.iter().map(String::as_str).collect();
    let applied_ref: Vec<&str> = applied_lines.iter().map(String::as_str).collect();
    let diff = TextDiff::from_slices(&snap_ref, &applied_ref);

    // Build hunks: (old_slice, new_slice) over grouped ops with 3 lines context.
    let mut cursor = 0usize;
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        let old_start = group.first().unwrap().old_range().start;
        let old_end = group.last().unwrap().old_range().end;
        let new_start = group.first().unwrap().new_range().start;
        let new_end = group.last().unwrap().new_range().end;
        let old_slice = &snap_lines[old_start..old_end];
        let new_slice = &applied_lines[new_start..new_end];
        if old_slice.is_empty() {
            // Cannot safely locate a zero-length anchor; refuse.
            return None;
        }
        // Zero fuzz: the old block (with context) must match EXACTLY, and
        // uniquely, at or after the cursor. Ambiguity → refuse (never slide).
        let pos = unique_contiguous_match(&current_lines, old_slice, cursor)?;
        current_lines.splice(pos..pos + old_slice.len(), new_slice.iter().cloned());
        cursor = pos + new_slice.len();
    }

    Some(current_lines.join("\n"))
}

/// Find the single contiguous exact occurrence of `needle` in `haystack[from..]`.
/// Returns `None` if there are zero or more than one matches (refuse on
/// ambiguity so a hunk is never slid onto a distant duplicate).
fn unique_contiguous_match(haystack: &[String], needle: &[String], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut found: Option<usize> = None;
    let mut i = from;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()] == *needle {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(i);
        }
        i += 1;
    }
    found
}

// ── Strategy 2: anchor remap through the line diff ──────────────────────────

fn build_line_map(
    snapshot_text: &str,
    current_text: &str,
) -> std::collections::HashMap<usize, usize> {
    let snap = split(snapshot_text);
    let curr = split(current_text);
    let diff = TextDiff::from_slices(&snap, &curr);
    let mut map = std::collections::HashMap::new();
    for op in diff.ops() {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = *op
        {
            for k in 0..len {
                // 1-indexed line numbers.
                map.insert(old_index + k + 1, new_index + k + 1);
            }
        }
    }
    map
}

fn replay_remapped_anchors(snapshot_text: &str, current_text: &str, ops: &[Op]) -> Option<String> {
    let line_map = build_line_map(snapshot_text, current_text);

    // Collect a single consistent offset across every anchor line.
    let mut offset: Option<isize> = None;
    for op in ops {
        for anchor in op.anchor_lines() {
            let mapped = *line_map.get(&anchor)?;
            let delta = mapped as isize - anchor as isize;
            match offset {
                None => offset = Some(delta),
                Some(o) if o == delta => {}
                Some(_) => return None, // mixed offsets — the edit range was touched
            }
        }
    }
    let offset = offset?;
    if offset == 0 {
        return None; // no shift — strategy 1/3 territory, not a remap
    }

    let shifted = shift_ops(ops, offset)?;
    let applied = apply_ops(current_text, &shifted).ok()?;
    Some(applied)
}

fn shift_ops(ops: &[Op], offset: isize) -> Option<Vec<Op>> {
    let shift = |n: usize| -> Option<usize> {
        let v = n as isize + offset;
        if v < 1 {
            None
        } else {
            Some(v as usize)
        }
    };
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let shifted = match op {
            Op::Swap { start, end, body } => Op::Swap {
                start: shift(*start)?,
                end: shift(*end)?,
                body: body.clone(),
            },
            Op::Del { start, end } => Op::Del {
                start: shift(*start)?,
                end: shift(*end)?,
            },
            Op::Ins { pos, body } => {
                use crate::format::InsertPos::*;
                let pos = match pos {
                    Pre(l) => Pre(shift(*l)?),
                    Post(l) => Post(shift(*l)?),
                    Head => Head,
                    Tail => Tail,
                };
                Op::Ins {
                    pos,
                    body: body.clone(),
                }
            }
        };
        out.push(shifted);
    }
    Some(out)
}

// ── Strategy 3: session-chain replay ────────────────────────────────────────

fn replay_session_chain(snapshot_text: &str, current_text: &str, ops: &[Op]) -> Option<String> {
    let snap = split(snapshot_text);
    let curr = split(current_text);
    if snap.len() != curr.len() {
        return None;
    }
    // Every anchored row must be byte-identical between snapshot and current.
    for op in ops {
        for anchor in op.anchor_lines() {
            let idx = anchor.checked_sub(1)?;
            let a = snap.get(idx)?;
            let b = curr.get(idx)?;
            if a != b {
                return None;
            }
        }
    }
    apply_ops(current_text, ops).ok()
}
