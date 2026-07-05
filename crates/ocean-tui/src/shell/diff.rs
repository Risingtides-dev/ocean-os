//! Edit-preview **diff cards** — the OMP `Diff.diffWords` rendering ported to
//! ratatui (docs/specs/2026-07-03-omp-port-map.md, Slice 4).
//!
//! When a tool call is an edit tool the chat surface renders its card as a diff
//! instead of raw output. This module owns the *pure* classification — args →
//! a `Vec<DiffRow>` — so the rendering in `components::chat` stays a thin,
//! theme-coloured projection and the interesting logic is unit-testable without
//! a terminal.
//!
//! Two shapes of edit tool are recognised:
//!  * `edit` / `write` — args carry `old_string` / `new_string` (a `write` is a
//!    whole-file replace, so `old_string` defaults to empty). We run a
//!    [`similar`] line diff and, for a replaced line *pair* that stays similar,
//!    a word-level diff whose changed runs are flagged for `Modifier::REVERSED`.
//!  * `hashline_edit` — the `patch` arg is already line-op formatted
//!    (`SWAP`/`DEL`/`INS…` headers + `+`-sigil body rows). We render the header
//!    rows dim and the `+` body rows as additions, without re-diffing.
//!
//! Everything here is defensive: malformed / missing args return `None` so the
//! caller falls back to the plain tool card, and nothing indexes unchecked.

use serde_json::Value;
use similar::{ChangeTag, DiffTag, TextDiff};

/// The role of a diff row, which the renderer maps to a gutter sigil + colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffKind {
    /// Unchanged context line (dim, no gutter sigil).
    Context,
    /// Removed line (`-` gutter, red-tinted).
    Del,
    /// Added line (`+` gutter, green-tinted).
    Add,
    /// A hashline op header (`SWAP 5.=7` / `DEL 10` …) — dim, set-off.
    Header,
}

/// One span of a diff row. `changed` marks a word-level intra-line difference,
/// which the renderer paints with `Modifier::REVERSED` (SGR inverse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffSeg {
    pub text: String,
    pub changed: bool,
}

impl DiffSeg {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: false,
        }
    }
    fn changed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: true,
        }
    }
}

/// One rendered diff row: a kind (gutter/colour) plus the styled segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRow {
    pub kind: DiffKind,
    pub segs: Vec<DiffSeg>,
}

impl DiffRow {
    fn simple(kind: DiffKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            segs: vec![DiffSeg::plain(text)],
        }
    }
}

/// Below this word-similarity ratio a replaced line pair is treated as two
/// unrelated lines (plain del + add) rather than a word-level edit — mirrors
/// OMP's "don't inverse-highlight a wholesale rewrite" heuristic.
const WORD_SIMILARITY_FLOOR: f32 = 0.5;

/// Names this module renders as a diff card. Anything else → plain card.
pub(crate) fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit" | "hashline_edit" | "write")
}

/// Classify an edit tool's args into diff rows, or `None` to fall back to the
/// plain card (unknown tool, missing/wrong-typed args, nothing to show).
pub(crate) fn edit_tool_diff(name: &str, args: &Value) -> Option<Vec<DiffRow>> {
    match name {
        "hashline_edit" => {
            let patch = args.get("patch").and_then(Value::as_str)?;
            let rows = hashline_rows(patch);
            (!rows.is_empty()).then_some(rows)
        }
        "edit" | "write" => {
            // `write` is a whole-file replace: no `old_string` ⇒ pure addition.
            let old = args.get("old_string").and_then(Value::as_str).unwrap_or("");
            let new = args
                .get("new_string")
                .or_else(|| args.get("content"))
                .and_then(Value::as_str)?;
            let rows = string_rows(old, new);
            (!rows.is_empty()).then_some(rows)
        }
        _ => None,
    }
}

/// Line diff `old` → `new`, upgrading similar replaced pairs to word-level.
pub(crate) fn string_rows(old: &str, new: &str) -> Vec<DiffRow> {
    let diff = TextDiff::from_lines(old, new);
    let olds = diff.old_slices();
    let news = diff.new_slices();
    let mut rows: Vec<DiffRow> = Vec::new();

    for op in diff.ops() {
        match op.tag() {
            DiffTag::Equal => {
                for s in &olds[op.old_range()] {
                    rows.push(DiffRow::simple(DiffKind::Context, trim_eol(s)));
                }
            }
            DiffTag::Delete => {
                for s in &olds[op.old_range()] {
                    rows.push(DiffRow::simple(DiffKind::Del, trim_eol(s)));
                }
            }
            DiffTag::Insert => {
                for s in &news[op.new_range()] {
                    rows.push(DiffRow::simple(DiffKind::Add, trim_eol(s)));
                }
            }
            DiffTag::Replace => {
                let od = &olds[op.old_range()];
                let nw = &news[op.new_range()];
                rows.extend(replace_rows(od, nw));
            }
        }
    }
    rows
}

/// Render a replaced region. Paired del/add lines that stay word-similar get an
/// intra-line word diff (REVERSED changed runs); the rest render as plain
/// del-then-add rows. All removed lines are emitted before all added lines so
/// the card reads as a hunk, not interleaved.
fn replace_rows(old: &[&str], new: &[&str]) -> Vec<DiffRow> {
    let mut del_rows: Vec<DiffRow> = Vec::new();
    let mut add_rows: Vec<DiffRow> = Vec::new();
    let pairs = old.len().min(new.len());

    for i in 0..pairs {
        let o = trim_eol(old[i]);
        let n = trim_eol(new[i]);
        match word_segments(o, n) {
            Some((dsegs, asegs)) => {
                del_rows.push(DiffRow {
                    kind: DiffKind::Del,
                    segs: dsegs,
                });
                add_rows.push(DiffRow {
                    kind: DiffKind::Add,
                    segs: asegs,
                });
            }
            None => {
                del_rows.push(DiffRow::simple(DiffKind::Del, o));
                add_rows.push(DiffRow::simple(DiffKind::Add, n));
            }
        }
    }
    // Unpaired remainder (line counts differ): trailing dels or adds.
    for s in old.iter().skip(pairs) {
        del_rows.push(DiffRow::simple(DiffKind::Del, trim_eol(s)));
    }
    for s in new.iter().skip(pairs) {
        add_rows.push(DiffRow::simple(DiffKind::Add, trim_eol(s)));
    }
    del_rows.extend(add_rows);
    del_rows
}

/// Word-level diff of a del/add line pair. Returns `(del_segs, add_segs)` with
/// changed runs flagged, or `None` when the pair is too dissimilar to be a
/// meaningful intra-line edit (caller falls back to plain rows).
fn word_segments(del: &str, add: &str) -> Option<(Vec<DiffSeg>, Vec<DiffSeg>)> {
    if del == add {
        return None; // identical — no intra-line highlight to show
    }
    let wd = TextDiff::from_words(del, add);
    if wd.ratio() < WORD_SIMILARITY_FLOOR {
        return None;
    }
    let mut dsegs: Vec<DiffSeg> = Vec::new();
    let mut asegs: Vec<DiffSeg> = Vec::new();
    for ch in wd.iter_all_changes() {
        let v = ch.value();
        match ch.tag() {
            ChangeTag::Equal => {
                push_seg(&mut dsegs, DiffSeg::plain(v));
                push_seg(&mut asegs, DiffSeg::plain(v));
            }
            ChangeTag::Delete => push_seg(&mut dsegs, DiffSeg::changed(v)),
            ChangeTag::Insert => push_seg(&mut asegs, DiffSeg::changed(v)),
        }
    }
    Some((dsegs, asegs))
}

/// Append a segment, coalescing with the previous one when the `changed` flag
/// matches (keeps span counts low and REVERSED runs contiguous).
fn push_seg(segs: &mut Vec<DiffSeg>, seg: DiffSeg) {
    if seg.text.is_empty() {
        return;
    }
    if let Some(last) = segs.last_mut() {
        if last.changed == seg.changed {
            last.text.push_str(&seg.text);
            return;
        }
    }
    segs.push(seg);
}

/// Render a hashline patch body: `[path#tag]` and `SWAP/DEL/INS…` headers dim,
/// `+`-sigil rows as additions, anything else as context. No re-diffing — the
/// patch is already an edit script.
pub(crate) fn hashline_rows(patch: &str) -> Vec<DiffRow> {
    let mut rows: Vec<DiffRow> = Vec::new();
    for raw in patch.split('\n') {
        let line = raw.trim_end_matches('\r');
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        if let Some(body) = t.strip_prefix('+') {
            rows.push(DiffRow::simple(DiffKind::Add, body));
        } else if is_hashline_header(t) {
            rows.push(DiffRow::simple(DiffKind::Header, t));
        } else {
            rows.push(DiffRow::simple(DiffKind::Context, line));
        }
    }
    rows
}

/// A hashline header row: a `[…]` file-section line or a verb line
/// (`SWAP`/`DEL`/`INS…`). Deletions in hashline have no body, so a `DEL` header
/// *is* the removal signal.
fn is_hashline_header(t: &str) -> bool {
    if t.starts_with('[') {
        return true;
    }
    const VERBS: &[&str] = &["SWAP", "DEL", "INS"];
    VERBS.iter().any(|v| {
        t.strip_prefix(v)
            .map(|rest| rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric()))
            .unwrap_or(false)
    })
}

/// Strip a single trailing `\n`/`\r\n` for display (similar's line slices keep
/// their terminator).
fn trim_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(rows: &[DiffRow]) -> Vec<DiffKind> {
        rows.iter().map(|r| r.kind).collect()
    }
    fn text(row: &DiffRow) -> String {
        row.segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn single_line_change_is_word_paired() {
        let rows = string_rows("let x = 1;\n", "let x = 2;\n");
        // One del + one add, both word-segmented.
        assert_eq!(kinds(&rows), vec![DiffKind::Del, DiffKind::Add]);
        // The `1` / `2` token is the only changed run.
        let del = &rows[0];
        let add = &rows[1];
        assert!(del.segs.iter().any(|s| s.changed && s.text.contains('1')));
        assert!(add.segs.iter().any(|s| s.changed && s.text.contains('2')));
        // The shared prefix is NOT flagged changed.
        assert!(del.segs.iter().any(|s| !s.changed && s.text.contains("let x")));
    }

    #[test]
    fn context_lines_are_preserved_around_a_change() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let rows = string_rows(old, new);
        assert_eq!(
            kinds(&rows),
            vec![DiffKind::Context, DiffKind::Del, DiffKind::Add, DiffKind::Context]
        );
        assert_eq!(text(&rows[0]), "a");
        assert_eq!(text(&rows[3]), "c");
    }

    #[test]
    fn dissimilar_pair_is_not_word_flagged() {
        // A wholesale rewrite: no shared words → plain del + add, nothing changed-flagged
        // beyond the whole line.
        let rows = string_rows("the quick brown fox\n", "zzz\n");
        assert_eq!(kinds(&rows), vec![DiffKind::Del, DiffKind::Add]);
        // Below the similarity floor → single plain seg per row (no partial highlight).
        assert!(rows[0].segs.iter().all(|s| !s.changed));
        assert!(rows[1].segs.iter().all(|s| !s.changed));
    }

    #[test]
    fn pure_insertion_and_deletion() {
        let rows = string_rows("keep\n", "keep\nadded\n");
        assert_eq!(kinds(&rows), vec![DiffKind::Context, DiffKind::Add]);
        let rows = string_rows("keep\ngone\n", "keep\n");
        assert_eq!(kinds(&rows), vec![DiffKind::Context, DiffKind::Del]);
    }

    #[test]
    fn write_tool_is_all_additions() {
        let rows =
            edit_tool_diff("write", &json!({ "content": "line one\nline two\n" })).unwrap();
        assert_eq!(kinds(&rows), vec![DiffKind::Add, DiffKind::Add]);
    }

    #[test]
    fn edit_tool_uses_old_and_new_string() {
        let rows = edit_tool_diff(
            "edit",
            &json!({ "old_string": "foo = 1\n", "new_string": "foo = 2\n" }),
        )
        .unwrap();
        assert_eq!(kinds(&rows), vec![DiffKind::Del, DiffKind::Add]);
    }

    #[test]
    fn malformed_args_fall_back_to_none() {
        // edit without new_string → None (plain card).
        assert!(edit_tool_diff("edit", &json!({ "old_string": "x" })).is_none());
        // hashline_edit without a string patch → None.
        assert!(edit_tool_diff("hashline_edit", &json!({ "patch": 42 })).is_none());
        // Unknown tool → None.
        assert!(edit_tool_diff("bash", &json!({ "command": "ls" })).is_none());
        // Non-object args never panic.
        assert!(edit_tool_diff("edit", &json!("nope")).is_none());
    }

    #[test]
    fn identical_strings_yield_only_context() {
        let rows = string_rows("same\n", "same\n");
        assert_eq!(kinds(&rows), vec![DiffKind::Context]);
    }

    #[test]
    fn hashline_patch_classifies_headers_and_bodies() {
        let patch = "[src/main.rs#1A2B]\nSWAP 5.=7:\n+let x = 1;\n+let y = 2;\nDEL 10";
        let rows = hashline_rows(patch);
        assert_eq!(
            kinds(&rows),
            vec![
                DiffKind::Header, // [src/main.rs#1A2B]
                DiffKind::Header, // SWAP 5.=7:
                DiffKind::Add,    // +let x = 1;
                DiffKind::Add,    // +let y = 2;
                DiffKind::Header, // DEL 10
            ]
        );
        assert_eq!(text(&rows[2]), "let x = 1;");
    }

    #[test]
    fn hashline_ins_header_is_recognised() {
        let rows = hashline_rows("INS.POST 12:\n+appended");
        assert_eq!(kinds(&rows), vec![DiffKind::Header, DiffKind::Add]);
    }

    #[test]
    fn is_edit_tool_gate() {
        assert!(is_edit_tool("edit"));
        assert!(is_edit_tool("hashline_edit"));
        assert!(is_edit_tool("write"));
        assert!(!is_edit_tool("bash"));
    }
}
