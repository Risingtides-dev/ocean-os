//! Integration tests mirroring oh-my-pi's hashline contract: hash stability,
//! every op type, stale→mismatch, the three recovery strategies, boundary
//! cases, and the no-op loop guard. Hermetic — in-memory strings only.

use ocean_hashline::{
    apply_patch, compute_file_hash, ApplyError, InsertPos, NoopLoopGuard, Op, Patch, Recovery,
    Section, SnapshotStore, RECOVERY_EXTERNAL_WARNING, RECOVERY_LINE_REMAP_WARNING,
    RECOVERY_SESSION_REPLAY_WARNING,
};

/// Build a single-section patch bound to `text`'s current hash.
fn patch_for(text: &str, path: &str, body: &str) -> Patch {
    let h = compute_file_hash(text);
    Patch::parse(&format!("[{path}#{h}]\n{body}")).expect("patch parses")
}

/// Apply a body against `text` (hash computed from `text`).
fn apply(text: &str, body: &str) -> String {
    let patch = patch_for(text, "f.rs", body);
    apply_patch(text, &patch).expect("applies cleanly")
}

// ── Hash stability ──────────────────────────────────────────────────────────

#[test]
fn hash_round_trips_and_is_stable() {
    let t = "fn main() {\n    println!(\"hi\");\n}\n";
    assert_eq!(compute_file_hash(t), compute_file_hash(t));
    assert_eq!(compute_file_hash(t).len(), 4);
}

#[test]
fn hash_ignores_trailing_whitespace_and_crlf() {
    assert_eq!(compute_file_hash("a\nb\n"), compute_file_hash("a  \nb\t\n"));
    assert_eq!(compute_file_hash("a\nb\n"), compute_file_hash("a\r\nb\r\n"));
}

// ── Each op type ────────────────────────────────────────────────────────────

#[test]
fn swap_single_line() {
    assert_eq!(apply("a\nb\nc\n", "SWAP 2:\n+B\n"), "a\nB\nc\n");
}

#[test]
fn swap_range_multiline_body() {
    assert_eq!(
        apply("a\nb\nc\nd\n", "SWAP 2.=3:\n+X\n+Y\n+Z\n"),
        "a\nX\nY\nZ\nd\n"
    );
}

#[test]
fn swap_empty_body_degrades_to_delete() {
    // A SWAP with no body is a range delete (OMP semantics).
    let patch = patch_for("a\nb\nc\n", "f.rs", "SWAP 2:\n");
    assert!(matches!(
        patch.sections[0].ops[0],
        Op::Del { start: 2, end: 2 }
    ));
    assert_eq!(apply_patch("a\nb\nc\n", &patch).unwrap(), "a\nc\n");
}

#[test]
fn del_single_and_range() {
    assert_eq!(apply("a\nb\nc\n", "DEL 2\n"), "a\nc\n");
    assert_eq!(apply("a\nb\nc\nd\n", "DEL 2.=3\n"), "a\nd\n");
}

#[test]
fn ins_pre_and_post() {
    assert_eq!(apply("a\nb\nc\n", "INS.PRE 2:\n+X\n"), "a\nX\nb\nc\n");
    assert_eq!(apply("a\nb\nc\n", "INS.POST 2:\n+X\n"), "a\nb\nX\nc\n");
}

#[test]
fn ins_head_and_tail() {
    assert_eq!(apply("a\nb\n", "INS.HEAD:\n+X\n"), "X\na\nb\n");
    assert_eq!(apply("a\nb\n", "INS.TAIL:\n+X\n"), "a\nb\nX\n");
}

#[test]
fn ins_head_on_empty_file() {
    assert_eq!(apply("", "INS.HEAD:\n+hello\n"), "hello");
    assert_eq!(apply("", "INS.TAIL:\n+hello\n"), "hello");
}

#[test]
fn multi_op_single_section_bottom_to_top() {
    // Two non-overlapping ops in one section apply against the ORIGINAL line
    // numbers regardless of ordering.
    let out = apply("a\nb\nc\nd\ne\n", "DEL 1\nSWAP 4:\n+D\n");
    assert_eq!(out, "b\nc\nD\ne\n");
}

#[test]
fn trailing_newline_preserved() {
    assert_eq!(apply("a\nb\nc\n", "SWAP 1:\n+A\n"), "A\nb\nc\n");
    // File without trailing newline stays without one.
    assert_eq!(apply("a\nb\nc", "SWAP 1:\n+A\n"), "A\nb\nc");
}

#[test]
fn swap_last_line_with_trailing_newline() {
    assert_eq!(apply("a\nb\nc\n", "SWAP 3:\n+C\n"), "a\nb\nC\n");
}

// ── Stale detection ─────────────────────────────────────────────────────────

#[test]
fn stale_hash_yields_mismatch_not_recognized() {
    // Build a patch bound to some OTHER text's hash, apply against a changed file.
    let patch = Patch::parse("[f.rs#0000]\nSWAP 1:\n+A\n").unwrap();
    let err = apply_patch("totally different\n", &patch).unwrap_err();
    match err {
        ApplyError::Mismatch(m) => {
            assert_eq!(m.expected_file_hash, "0000");
            assert_ne!(m.actual_file_hash, "0000");
            assert!(!m.hash_recognized);
            // The rendered message names both hashes.
            let msg = m.to_string();
            assert!(msg.contains("0000"));
        }
        other => panic!("expected mismatch, got {other:?}"),
    }
}

#[test]
fn out_of_bounds_anchor_is_distinct_error() {
    let patch = patch_for("a\nb\n", "f.rs", "SWAP 9:\n+X\n");
    match apply_patch("a\nb\n", &patch).unwrap_err() {
        ApplyError::LineOutOfBounds { line, .. } => assert_eq!(line, 9),
        other => panic!("expected out-of-bounds, got {other:?}"),
    }
}

// ── Recovery: strategy 1 (external write, transplant) ───────────────────────

#[test]
fn recovery_external_write_transplant() {
    let snapshot = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    let mut store = SnapshotStore::default();
    let h = store.record("f.rs", snapshot, []);

    // External change far from the edit (line 1), so the edit's context is clean.
    let current = "ONE\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    let section = Section {
        path: "f.rs".into(),
        expected_hash: h,
        ops: vec![Op::Swap {
            start: 8,
            end: 8,
            body: vec!["EIGHT".into()],
        }],
    };
    let (merged, warning) = Recovery::try_recover(&store, &section, current).expect("recovers");
    assert_eq!(merged, "ONE\n2\n3\n4\n5\n6\n7\nEIGHT\n9\n10\n");
    assert_eq!(warning, RECOVERY_EXTERNAL_WARNING);
}

// ── Recovery: strategy 2 (line remap, consistent offset) ────────────────────

#[test]
fn recovery_line_remap_after_insertion() {
    let v_old = "a\nb\nTARGET\nd\ne\n";
    let current = "a\nb\nINS\nTARGET\nd\ne\n"; // a prior insertion shifted TARGET down
    let mut store = SnapshotStore::default();
    let h_old = store.record("f.rs", v_old, []);
    store.record("f.rs", current, []); // becomes head; v_old is now non-head

    let section = Section {
        path: "f.rs".into(),
        expected_hash: h_old,
        ops: vec![Op::Swap {
            start: 3,
            end: 3,
            body: vec!["NEWTARGET".into()],
        }],
    };
    let (merged, warning) = Recovery::try_recover(&store, &section, current).expect("recovers");
    assert_eq!(merged, "a\nb\nINS\nNEWTARGET\nd\ne\n");
    assert_eq!(warning, RECOVERY_LINE_REMAP_WARNING);
}

// ── Recovery: strategy 3 (session-chain replay) ─────────────────────────────

#[test]
fn recovery_session_chain_replay() {
    let v_old = "a\nb\nTARGET\nd\ne\n";
    let current = "a\nBEE\nTARGET\nd\ne\n"; // prior in-session edit changed line 2
    let mut store = SnapshotStore::default();
    let h_old = store.record("f.rs", v_old, []);
    store.record("f.rs", current, []); // head; v_old non-head

    let section = Section {
        path: "f.rs".into(),
        expected_hash: h_old,
        ops: vec![Op::Swap {
            start: 3,
            end: 3,
            body: vec!["NEWTARGET".into()],
        }],
    };
    let (merged, warning) = Recovery::try_recover(&store, &section, current).expect("recovers");
    assert_eq!(merged, "a\nBEE\nNEWTARGET\nd\ne\n");
    assert_eq!(warning, RECOVERY_SESSION_REPLAY_WARNING);
}

#[test]
fn recovery_none_when_tag_unknown() {
    let store = SnapshotStore::default();
    let section = Section {
        path: "f.rs".into(),
        expected_hash: "ABCD".into(),
        ops: vec![Op::Del { start: 1, end: 1 }],
    };
    assert!(Recovery::try_recover(&store, &section, "a\nb\n").is_none());
}

// ── Snapshot store ──────────────────────────────────────────────────────────

#[test]
fn store_head_by_hash_by_content() {
    let mut store = SnapshotStore::default();
    let h1 = store.record("f.rs", "v1\n", []);
    let h2 = store.record("f.rs", "v2\n", []);
    assert_eq!(store.head("f.rs").unwrap().text, "v2\n");
    assert_eq!(store.by_hash("f.rs", &h1).unwrap().text, "v1\n");
    assert_eq!(store.by_hash("f.rs", &h2).unwrap().text, "v2\n");
    assert_eq!(store.by_content("f.rs", "v1\n").unwrap().hash, h1);
    assert!(store.recognizes("f.rs", &h1));
    assert!(!store.recognizes("f.rs", "ZZZZ"));
}

#[test]
fn store_dedup_fuses_identical_content() {
    let mut store = SnapshotStore::default();
    let a = store.record("f.rs", "same\n", [(1, 1)]);
    let b = store.record("f.rs", "same\n", [(2, 2)]);
    assert_eq!(a, b);
    // Only one version retained, with unioned seen-lines.
    let head = store.head("f.rs").unwrap();
    assert!(head.has_seen(1) && head.has_seen(2));
}

#[test]
fn store_caps_versions_per_path() {
    let mut store = SnapshotStore::new(30, 4);
    for i in 0..8 {
        store.record("f.rs", &format!("v{i}\n"), []);
    }
    // Only the 4 most-recent versions remain; the oldest aged out.
    assert!(store.by_content("f.rs", "v7\n").is_some());
    assert!(store.by_content("f.rs", "v3\n").is_none());
}

#[test]
fn store_evicts_least_recent_path() {
    let mut store = SnapshotStore::new(2, 4);
    store.record("a.rs", "a\n", []);
    store.record("b.rs", "b\n", []);
    store.record("c.rs", "c\n", []); // evicts a.rs (least recent)
    assert!(store.head("a.rs").is_none());
    assert!(store.head("b.rs").is_some());
    assert!(store.head("c.rs").is_some());
    assert_eq!(store.tracked_paths(), 2);
}

#[test]
fn store_seen_lines_ranges() {
    let mut store = SnapshotStore::default();
    store.record("f.rs", "1\n2\n3\n4\n5\n", [(2, 4)]);
    let snap = store.head("f.rs").unwrap();
    assert!(snap.has_seen_range(2, 4));
    assert!(!snap.has_seen(1));
    assert!(!snap.has_seen(5));
}

// ── Parse rejection / validation ────────────────────────────────────────────

#[test]
fn rejects_out_of_scope_block_and_file_ops() {
    for body in [
        "SWAP.BLK 3:\n+x\n",
        "DEL.BLK 3\n",
        "INS.BLK.POST 3:\n+x\n",
        "REM\n",
        "MV dest.rs\n",
    ] {
        let src = format!("[f.rs#1A2B]\n{body}");
        assert!(Patch::parse(&src).is_err(), "should reject: {body:?}");
    }
}

#[test]
fn rejects_del_with_body() {
    assert!(Patch::parse("[f.rs#1A2B]\nDEL 3\n+oops\n").is_err());
}

#[test]
fn rejects_empty_insert() {
    assert!(Patch::parse("[f.rs#1A2B]\nINS.PRE 3:\n").is_err());
}

#[test]
fn rejects_minus_row() {
    assert!(Patch::parse("[f.rs#1A2B]\nSWAP 3:\n-removed\n").is_err());
}

#[test]
fn rejects_payload_without_header() {
    assert!(Patch::parse("[f.rs#1A2B]\n+orphan\n").is_err());
    assert!(Patch::parse("+no section either\n").is_err());
}

#[test]
fn rejects_overlapping_ranges() {
    assert!(Patch::parse("[f.rs#1A2B]\nSWAP 2.=4:\n+x\nDEL 3\n").is_err());
}

#[test]
fn rejects_reversed_range() {
    assert!(Patch::parse("[f.rs#1A2B]\nSWAP 5.=2:\n+x\n").is_err());
}

#[test]
fn rejects_malformed_header_with_hash_in_path() {
    // `#` inside the path (non-trailing-tag) is malformed.
    assert!(Patch::parse("[f#oo.rs#1A2B]\nDEL 1\n").is_err());
}

// ── Parser leniency: bare body auto-pipe ────────────────────────────────────

#[test]
fn bare_body_auto_pipes() {
    // A bare row after a SWAP header is treated as body content.
    let out = apply("a\nb\nc\n", "SWAP 2:\nBEE\n");
    assert_eq!(out, "a\nBEE\nc\n");
}

// ── Display round-trip ──────────────────────────────────────────────────────

#[test]
fn display_round_trips_through_parser() {
    let src = "[src/foo.rs#1A2B]\nSWAP 5.=7:\n+one\n+two\nDEL 10\nINS.POST 12:\n+tail\n";
    let patch = Patch::parse(src).unwrap();
    let rendered = patch.to_string();
    let reparsed = Patch::parse(&rendered).unwrap();
    assert_eq!(patch, reparsed);
}

#[test]
fn ins_variants_round_trip() {
    let src = "[f.rs#ABCD]\nINS.HEAD:\n+h\nINS.TAIL:\n+t\nINS.PRE 3:\n+p\n";
    let patch = Patch::parse(src).unwrap();
    assert_eq!(patch, Patch::parse(&patch.to_string()).unwrap());
    // Structural spot check.
    assert!(matches!(
        patch.sections[0].ops[0],
        Op::Ins {
            pos: InsertPos::Head,
            ..
        }
    ));
}

// ── No-op loop guard ────────────────────────────────────────────────────────

#[test]
fn noop_guard_trips_after_repeats() {
    let mut guard = NoopLoopGuard::new(2);
    // First identical no-op: tolerated.
    assert!(guard.observe_noop("f.rs", "SWAP 1:\n+A\n").is_ok());
    // Second identical no-op: trips.
    assert!(guard.observe_noop("f.rs", "SWAP 1:\n+A\n").is_err());
}

#[test]
fn noop_guard_resets_on_different_patch() {
    let mut guard = NoopLoopGuard::new(2);
    assert!(guard.observe_noop("f.rs", "SWAP 1:\n+A\n").is_ok());
    // A different patch resets the counter.
    assert!(guard.observe_noop("f.rs", "SWAP 2:\n+B\n").is_ok());
    assert!(guard.observe_noop("f.rs", "SWAP 2:\n+B\n").is_err());
}

#[test]
fn noop_guard_reset_clears_state() {
    let mut guard = NoopLoopGuard::new(2);
    assert!(guard.observe_noop("f.rs", "SWAP 1:\n+A\n").is_ok());
    guard.reset("f.rs");
    // After reset, the same patch is tolerated again as the first observation.
    assert!(guard.observe_noop("f.rs", "SWAP 1:\n+A\n").is_ok());
}

// ── serde round-trip ────────────────────────────────────────────────────────

#[test]
fn patch_serde_round_trip() {
    let patch = Patch::parse("[f.rs#1A2B]\nSWAP 1.=2:\n+x\nDEL 5\n").unwrap();
    let json = serde_json::to_string(&patch).unwrap();
    let back: Patch = serde_json::from_str(&json).unwrap();
    assert_eq!(patch, back);
}

// ── Landing / boundary edge cases ───────────────────────────────────────────

#[test]
fn ins_post_last_real_line_appends_before_phantom() {
    // INS.POST on the last content line lands before the trailing newline.
    assert_eq!(apply("a\nb\n", "INS.POST 2:\n+X\n"), "a\nb\nX\n");
}

#[test]
fn del_whole_range_to_eof() {
    assert_eq!(apply("a\nb\nc\n", "DEL 2.=3\n"), "a\n");
}

#[test]
fn swap_then_apply_matches_recomputed_hash() {
    // After a clean apply, the new text has a fresh, stable hash.
    let out = apply("a\nb\nc\n", "SWAP 2:\n+B\n");
    let h1 = compute_file_hash(&out);
    let h2 = compute_file_hash("a\nB\nc\n");
    assert_eq!(h1, h2);
}
