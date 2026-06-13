use ocean_context::claim::{Anchor, ClaimStatus};
use ocean_context::seams::{FileExistsResolver, Resolution, WORKTREE};
use ocean_context::{extract_claims, ExtractCtx};
use ocean_context::{read_freshest, reverify, write_handoff, Handoff, ScopeRing, Velocity};

fn handoff_with(claim_texts: &[&str]) -> Handoff {
    let prose = claim_texts.join("\n");
    let ctx = ExtractCtx {
        commit_sha: "abc1234",
        now: 1_000,
        by_session: "test",
    };
    Handoff {
        session_id: "sess-api".into(),
        parent_session: None,
        repo: "ocean-os".into(),
        branch: "main".into(),
        commit_anchor: "abc1234".into(),
        scope_ring: ScopeRing::Repo,
        velocity_at_write: Velocity {
            v_code: 0.0,
            v_sem: 0.0,
        },
        written_at: 1_000,
        narrative: prose.clone(),
        claims: extract_claims(&prose, &ctx),
    }
}

#[test]
fn write_then_read_freshest_sorts_claims_by_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handoff_with(&[
        "Low-confidence assertion about some/old/path.rs here.",
        "Another assertion touching crates/other/file.rs today.",
    ]);
    // Make claim 2 strictly more trusted.
    h.claims[0].confidence = 0.2;
    h.claims[1].confidence = 0.9;
    write_handoff(dir.path(), &h).unwrap();
    let got = read_freshest(dir.path(), "ocean-os", "main", 1_000)
        .unwrap()
        .unwrap();
    assert_eq!(got.claims[0].confidence, 0.9); // most trusted first
}

#[test]
fn reverify_updates_status_and_history_via_resolver() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/real.rs"), "// here\n").unwrap();

    let mut h = handoff_with(&[
        "A live claim anchored at src/real.rs in this repo.",
        "A dead claim anchored at src/vanished.rs long gone.",
    ]);
    let resolver = FileExistsResolver {
        repo_root: dir.path().to_path_buf(),
    };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");

    assert_eq!(results.len(), 2);
    assert!(matches!(results[0].1, Resolution::Resolves(_)));
    assert!(matches!(results[1].1, Resolution::Dead));
    assert_eq!(h.claims[0].status, ClaimStatus::Verified);
    assert_eq!(h.claims[1].status, ClaimStatus::Dead);
    // history gained a reverified event
    assert!(h.claims[0]
        .history
        .iter()
        .any(|e| e.event.starts_with("reverified")));
}

#[test]
fn reverify_skips_anchorless_claims() {
    let mut h = handoff_with(&["A live claim anchored at src/real.rs in this repo."]);
    // Hand-build an anchorless asserted claim (e.g. a plan statement).
    let mut plan = h.claims[0].clone();
    plan.id = "c-plan".into();
    plan.provenance.anchors = vec![];
    plan.status = ClaimStatus::Asserted;
    h.claims.push(plan);

    let dir = tempfile::tempdir().unwrap();
    let resolver = FileExistsResolver {
        repo_root: dir.path().to_path_buf(),
    };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");
    assert_eq!(results.len(), 1); // anchorless claim untouched
    assert_eq!(h.claims[1].status, ClaimStatus::Asserted);
}

#[test]
fn anchors_can_be_file_only() {
    // F5: the resolver path must not assume line numbers.
    let a = Anchor {
        file: Some("src/real.rs".into()),
        symbol: None,
        lines: vec![],
        sig_hash: None,
    };
    assert!(a.lines.is_empty());
}

#[test]
fn anchors_can_be_symbol_only() {
    // F5 / schema friction #1: absence of a file anchor is typed, not an
    // empty-string sentinel.
    let a = Anchor {
        file: None,
        symbol: Some("workspace.members".into()),
        lines: vec![],
        sig_hash: None,
    };
    assert!(a.file.is_none());
}

/// Codex P2 round 2 on PR #205: with mixed anchors, an uncheckable sibling
/// must never mask a dead anchor — checkable negative evidence outranks
/// "can't check".
#[test]
fn reverify_dead_anchor_kills_claim_despite_unresolvable_sibling() {
    let mut h = handoff_with(&["A claim anchored at src/gone.rs that vanished entirely."]);
    h.claims[0].provenance.anchors.push(Anchor {
        file: None,
        symbol: Some("apply_input".into()),
        lines: vec![],
        sig_hash: None,
    });

    let dir = tempfile::tempdir().unwrap(); // empty repo: src/gone.rs is Dead
    let resolver = FileExistsResolver {
        repo_root: dir.path().to_path_buf(),
    };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");

    assert!(matches!(results[0].1, Resolution::Dead));
    assert_eq!(h.claims[0].status, ClaimStatus::Dead);
    assert_eq!(h.claims[0].history.last().unwrap().event, "killed");
}

/// The flip side, per the replay harness's any-resolves semantics: one
/// RESOLVING anchor holds the claim even when a sibling is uncheckable —
/// positive checkable evidence outranks "can't check" too.
#[test]
fn reverify_resolving_anchor_holds_claim_despite_unresolvable_sibling() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/real.rs"), "// here\n").unwrap();

    let mut h = handoff_with(&["A live claim anchored at src/real.rs in this repo."]);
    h.claims[0].provenance.anchors.push(Anchor {
        file: None,
        symbol: Some("apply_input".into()),
        lines: vec![],
        sig_hash: None,
    });

    let resolver = FileExistsResolver {
        repo_root: dir.path().to_path_buf(),
    };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");

    assert!(matches!(results[0].1, Resolution::Resolves(_)));
    assert_eq!(h.claims[0].status, ClaimStatus::Verified);
    assert_eq!(h.claims[0].history.last().unwrap().event, "reverified");
}

#[test]
fn reverify_marks_uncheckable_claims_unresolvable_not_dead() {
    // Schema friction #2: a claim whose only anchor the resolver CANNOT check
    // (symbol-only) must not be conflated with Stale/Dead.
    let mut h = handoff_with(&["A live claim anchored at src/real.rs in this repo."]);
    let mut sym = h.claims[0].clone();
    sym.id = "c-sym".into();
    sym.provenance.anchors = vec![Anchor {
        file: None,
        symbol: Some("apply_input".into()),
        lines: vec![],
        sig_hash: None,
    }];
    h.claims.push(sym);

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/real.rs"), "// here\n").unwrap();
    let resolver = FileExistsResolver {
        repo_root: dir.path().to_path_buf(),
    };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");

    assert_eq!(results.len(), 2);
    assert!(matches!(results[1].1, Resolution::Unresolvable));
    assert_eq!(h.claims[1].status, ClaimStatus::Reverify);
    // History records the honest non-verdict — neither "reverified" nor "killed".
    assert_eq!(h.claims[1].history.last().unwrap().event, "unresolvable");
}
