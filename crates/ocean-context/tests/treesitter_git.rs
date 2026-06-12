//! B1 acceptance, pinned (OCEAN-307): symbol-presence + sig-hash must beat
//! file-exists on the replay. The demonstrated failure: a claim anchored at
//! `Cargo.toml#workspace.members` HELD under file-exists through an actual
//! members change, because file-exists can't see inside files. Here that
//! exact shape is reproduced in a fixture repo and the verdict diff is
//! frozen as a regression test.

use ocean_context::claim::{Anchor, Claim, ClaimEvent, ClaimStatus, KnowledgeTier, Provenance};
use ocean_context::replay::replay;
use ocean_context::seams::{FileExistsResolver, Resolution, Resolver, WORKTREE};
use ocean_context::treesitter::TreeSitterResolver;
use std::path::Path;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn claim(id: &str, file: &str, symbol: Option<&str>, commit: &str) -> Claim {
    Claim {
        id: id.into(),
        text: format!("claim about {file}#{symbol:?}"),
        provenance: Provenance {
            anchors: vec![Anchor {
                file: Some(file.into()),
                symbol: symbol.map(Into::into),
                lines: vec![],
                sig_hash: None,
            }],
            tickets: vec![],
            commit_sha: commit.into(),
        },
        status: ClaimStatus::Verified,
        knowledge_tier: KnowledgeTier::Individual,
        ps_anchor: None,
        confidence: 0.9,
        borrowed_from: None,
        history: vec![ClaimEvent { at: 0, event: "written".into(), by_session: "t".into() }],
    }
}

const MEMBERS_V1: &str = "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n";
const MEMBERS_V2: &str = "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\nresolver = \"2\"\n";

/// THE B1 acceptance shape: members content changes at c2, file never
/// vanishes. file-exists HOLDS straight through (the demonstrated blindness);
/// tree-sitter flags Stale exactly at c2. A control claim whose file truly
/// dies must get the SAME verdict from both — no regression on Dead cases.
#[test]
fn tree_sitter_flags_the_members_change_file_exists_misses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    std::fs::write(root.join("Cargo.toml"), MEMBERS_V1).unwrap();
    std::fs::write(root.join("spec.md"), "the spec\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: baseline"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    std::fs::write(root.join("Cargo.toml"), MEMBERS_V2).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: members change"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    git(root, &["rm", "-q", "spec.md"]);
    git(root, &["commit", "-qm", "c3: spec dies"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    let mut claims = vec![
        claim("members", "Cargo.toml", Some("workspace.members"), &c1),
        claim("spec", "spec.md", None, &c1),
    ];

    // file-exists: blind to the members change, sees only the spec death.
    let fe = FileExistsResolver { repo_root: root.to_path_buf() };
    let fe_verdicts = replay(root, &claims, &fe).unwrap();
    assert_eq!(fe_verdicts[0].first_fail_commit, None, "file-exists HELD through the change");
    assert_eq!(fe_verdicts[1].first_fail_commit.as_deref(), Some(c3.as_str()));

    // tree-sitter: seed write-time baselines, then walk the same history.
    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    ts.seed_sig_hashes(&mut claims);
    assert!(claims[0].provenance.anchors[0].sig_hash.is_some(), "symbol anchor seeded at c1");
    assert!(claims[1].provenance.anchors[0].sig_hash.is_none(), "file-only anchor NOT seeded");
    let ts_verdicts = replay(root, &claims, &ts).unwrap();

    // flags the content change AT the commit it happened, as Stale
    assert_eq!(ts_verdicts[0].first_fail_commit.as_deref(), Some(c2.as_str()));
    assert_eq!(ts_verdicts[0].first_fail_resolution, Some(Resolution::Stale));
    // and does NOT regress the verdict file-exists got right
    assert_eq!(ts_verdicts[1].first_fail_commit.as_deref(), Some(c3.as_str()));
    assert_eq!(ts_verdicts[1].first_fail_resolution, Some(Resolution::Dead));
}

/// At-commit mode end-to-end on a Rust symbol: resolve the same anchor at
/// three commits via `git show` blobs (never a checkout) — alive at c1,
/// Stale at the signature change, Dead at the rename. The working tree is
/// then dirtied to prove at-commit reads don't touch it.
#[test]
fn at_commit_mode_tracks_a_rust_symbol_through_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    let v1 = "pub fn gate(x: u8) -> bool { x > 0 }\n";
    std::fs::write(root.join("lib.rs"), v1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // body-only edit: same signature
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 1 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: body only"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    // signature change
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8, strict: bool) -> bool { x > 0 }\n")
        .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c3: signature change"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    // rename: symbol gone
    std::fs::write(root.join("lib.rs"), "pub fn portal(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c4: rename"]);
    let c4 = git(root, &["rev-parse", "HEAD"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    let mut anchor =
        Anchor { file: Some("lib.rs".into()), symbol: Some("gate".into()), lines: vec![], sig_hash: None };
    anchor.sig_hash = ts.sig_hash_at(&anchor, &c1);
    assert!(anchor.sig_hash.is_some());

    // dirty the working tree with garbage — at-commit reads must not see it
    std::fs::write(root.join("lib.rs"), "not even rust ((((\n").unwrap();

    assert_eq!(ts.resolve(&anchor, &c1), Resolution::Resolves(1.0));
    assert_eq!(ts.resolve(&anchor, &c2), Resolution::Resolves(1.0), "body edit is not shape");
    assert_eq!(ts.resolve(&anchor, &c3), Resolution::Stale);
    assert_eq!(ts.resolve(&anchor, &c4), Resolution::Dead);

    // worktree mode DOES see the dirty file — but a blob that doesn't parse
    // is no evidence of removal: uncheckable, never Dead (Codex P2, PR #209).
    assert_eq!(ts.resolve(&anchor, WORKTREE), Resolution::Unresolvable);

    // Library-path seeding (Codex round-8): replay() called directly with
    // unseeded claims (extraction's shape) must stamp baselines itself — a
    // signature change after the anchor commit has to FAIL the walk even
    // though nobody called seed_sig_hashes. Reuses the c1→c3 history above:
    // gate's signature changed at c3, so an unseeded claim anchored at c1
    // must fail there instead of holding by name.
    {
        let unseeded = vec![claim("r8", "lib.rs", Some("gate"), &c1)];
        assert!(unseeded[0].provenance.anchors[0].sig_hash.is_none());
        let v = replay(root, &unseeded, &ts).unwrap();
        assert_eq!(
            v[0].first_fail_commit.as_deref(),
            Some(c3.as_str()),
            "unseeded symbol must not verify by name through a shape change"
        );
        assert_eq!(v[0].first_fail_resolution, Some(Resolution::Stale));
    }

    // file missing at-commit → Dead; path traversal stays Unresolvable
    let gone =
        Anchor { file: Some("nope.rs".into()), symbol: None, lines: vec![], sig_hash: None };
    assert_eq!(ts.resolve(&gone, &c1), Resolution::Dead);
    let escape =
        Anchor { file: Some("../x.rs".into()), symbol: Some("gate".into()), lines: vec![], sig_hash: None };
    assert_eq!(ts.resolve(&escape, &c1), Resolution::Unresolvable);
    assert_eq!(ts.resolve(&escape, WORKTREE), Resolution::Unresolvable);
}

/// TypeScript at-commit (Codex round-5 P2: the spec's B1 row names
/// rust/TYPESCRIPT grammars): a `.ts` anchor walks the same lifecycle as
/// Rust — alive at c1, held through a body-only edit, Stale at the
/// signature change, Dead at the rename — via `git show` blobs.
#[test]
fn at_commit_mode_tracks_a_typescript_symbol_through_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    let v1 = "export class Auth {\n  requiresPermission(action: string): boolean {\n    return action !== \"read\";\n  }\n}\n";
    std::fs::write(root.join("auth.ts"), v1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // body-only edit: same signature
    std::fs::write(root.join("auth.ts"), v1.replace("!== \"read\"", "!== \"read\" && action !== \"list\"")).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: body only"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    // signature change
    std::fs::write(
        root.join("auth.ts"),
        v1.replace("requiresPermission(action: string)", "requiresPermission(action: string, strict: boolean)"),
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c3: signature change"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    // rename: symbol gone
    std::fs::write(root.join("auth.ts"), v1.replace("requiresPermission", "checkAccess")).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c4: rename"]);
    let c4 = git(root, &["rev-parse", "HEAD"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    // handoff-style qualified anchor: Class.method
    let mut anchor = Anchor {
        file: Some("auth.ts".into()),
        symbol: Some("Auth.requiresPermission".into()),
        lines: vec![],
        sig_hash: None,
    };
    anchor.sig_hash = ts.sig_hash_at(&anchor, &c1);
    assert!(anchor.sig_hash.is_some(), "TS anchor must seed at its anchor commit");

    assert_eq!(ts.resolve(&anchor, &c1), Resolution::Resolves(1.0));
    assert_eq!(ts.resolve(&anchor, &c2), Resolution::Resolves(1.0), "body edit is not shape");
    assert_eq!(ts.resolve(&anchor, &c3), Resolution::Stale);
    assert_eq!(ts.resolve(&anchor, &c4), Resolution::Dead);
}

/// TOML at-commit: the dotted key holds while only siblings change, goes
/// Stale when its value changes — same blindness-beating check, other lang.
#[test]
fn at_commit_mode_tracks_a_toml_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    std::fs::write(root.join("Cargo.toml"), MEMBERS_V1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // sibling-only change: members untouched
    std::fs::write(root.join("Cargo.toml"), MEMBERS_V1.replace("\"2\"", "\"3\"")).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: sibling"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    std::fs::write(root.join("Cargo.toml"), MEMBERS_V2).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c3: members"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    let mut anchor = Anchor {
        file: Some("Cargo.toml".into()),
        symbol: Some("workspace.members".into()),
        lines: vec![],
        sig_hash: None,
    };
    anchor.sig_hash = ts.sig_hash_at(&anchor, &c1);

    assert_eq!(ts.resolve(&anchor, &c1), Resolution::Resolves(1.0));
    assert_eq!(ts.resolve(&anchor, &c2), Resolution::Resolves(1.0), "sibling churn is not change");
    assert_eq!(ts.resolve(&anchor, &c3), Resolution::Stale);
}

/// Codex P2 (PR #209): a revision where the file exists but does not parse
/// is NO evidence — never `Dead`, and a transient unparseable commit in a
/// replay walk must neither fail the claim nor mark it unresolvable when
/// later revisions attest it held.
#[test]
fn unparseable_revision_is_unresolvable_not_dead_and_walk_survives_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    // c1: valid TOML, key present (anchor commit)
    std::fs::write(root.join("Cargo.toml"), MEMBERS_V1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: valid"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // c2: file present but invalid TOML (mid-edit / merge-artifact blob)
    std::fs::write(root.join("Cargo.toml"), "[workspace\nmembers = = [\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: broken"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    // c3: valid again, key still present (same content as c1)
    std::fs::write(root.join("Cargo.toml"), MEMBERS_V1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c3: valid again"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    let anchor = Anchor {
        file: Some("Cargo.toml".into()),
        symbol: Some("workspace.members".into()),
        lines: vec![],
        sig_hash: None,
    };

    // Resolve-level: the broken revision is uncheckable, not removal evidence.
    assert_eq!(ts.resolve(&anchor, &c2), Resolution::Unresolvable);
    // The parseable revisions still attest presence.
    assert_eq!(ts.resolve(&anchor, &c1), Resolution::Resolves(1.0));

    // Walk-level: c1 → c2(broken) → c3(held). The claim must come out HELD —
    // no first_fail, no unresolvable flag (a held step outranks a transient
    // uncheckable one).
    let claims = vec![claim("t1", "Cargo.toml", Some("workspace.members"), &c1)];
    let verdicts = replay(root, &claims, &ts).unwrap();
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].first_fail_commit.is_none(), "transient breakage must not fail the claim");
    assert!(!verdicts[0].unresolvable, "held steps outrank transient uncheckability");

    // Control: a genuinely removed key is still hard evidence.
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c4: members removed"]);
    let c4 = git(root, &["rev-parse", "HEAD"]);
    assert_eq!(ts.resolve(&anchor, &c4), Resolution::Dead);

    // Tail-of-walk honesty (Codex round-2): a claim that held mid-walk but
    // whose file is broken from some commit through HEAD must NOT read HELD —
    // the current state is unattested. (c4 removed the key — hard fail — so
    // use a fresh claim anchored after c4, with only broken commits ahead.)
    std::fs::write(root.join("Tail.toml"), MEMBERS_V1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "t1: tail anchor, valid"]);
    let t1 = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("Tail.toml"), "[workspace
broken = = [
").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "t2: broken through HEAD"]);
    let tail_claims = vec![claim("t2", "Tail.toml", Some("workspace.members"), &t1)];
    let tail_verdicts = replay(root, &tail_claims, &ts).unwrap();
    assert!(tail_verdicts[0].first_fail_commit.is_none(), "breakage is not removal evidence");
    assert!(
        tail_verdicts[0].unresolvable,
        "a tail unresolvable through HEAD must not report HELD"
    );

    // Birth check (Codex round-3): an anchor that never resolved at its own
    // anchor commit (misspelled symbol, unseedable baseline) must FAIL at the
    // anchor — not read HELD because a same-named symbol shows up later in
    // the walk with no baseline to compare against.
    std::fs::write(root.join("Birth.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "b1: key never existed here"]);
    let b1 = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("Birth.toml"), MEMBERS_V1).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "b2: same-named key appears later"]);
    let birth_claims = vec![claim("b1", "Birth.toml", Some("workspace.members"), &b1)];
    let birth_verdicts = replay(root, &birth_claims, &ts).unwrap();
    assert_eq!(
        birth_verdicts[0].first_fail_commit.as_deref(),
        Some(b1.as_str()),
        "a claim that was never true at its anchor must fail AT the anchor"
    );

    // Rust mirror: a broken .rs blob is uncheckable, a clean one attests.
    std::fs::write(root.join("lib.rs"), "fn gate( ((((\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c5: broken rust"]);
    let c5 = git(root, &["rev-parse", "HEAD"]);
    let rust_anchor = Anchor {
        file: Some("lib.rs".into()),
        symbol: Some("gate".into()),
        lines: vec![],
        sig_hash: None,
    };
    assert_eq!(ts.resolve(&rust_anchor, &c5), Resolution::Unresolvable);
}

// ---------------------------------------------------------------------------
// Codex round-6 P2: baselines are part of the claim LIFECYCLE — the first
// reverify stamps them; they round-trip through the store; the second
// reverify flags shape changes. Only the replay CLI seeded before this.
// ---------------------------------------------------------------------------

fn handoff_with_claims(claims: Vec<Claim>, branch: &str) -> ocean_context::Handoff {
    ocean_context::Handoff {
        session_id: "sess-b".into(),
        parent_session: None,
        repo: "fixture".into(),
        branch: branch.into(),
        commit_anchor: claims.first().map(|c| c.provenance.commit_sha.clone()).unwrap_or_default(),
        scope_ring: ocean_context::ScopeRing::Repo,
        velocity_at_write: ocean_context::Velocity { v_code: 0.0, v_sem: 0.0 },
        written_at: 1_000,
        narrative: "lifecycle fixture".into(),
        claims,
    }
}

/// THE round-6 regression pin: an unseeded symbol anchor (what
/// extract_claims/write_handoff produce) must get its baseline stamped on
/// the FIRST reverify, survive a store round-trip, and flag Stale on the
/// SECOND reverify after the shape changes. Without the fix the second
/// reverify says Verified — the symbol still resolves by name.
#[test]
fn reverify_stamps_baseline_on_first_pass_and_flags_shape_change_on_second() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    let resolver = TreeSitterResolver { repo_root: root.to_path_buf() };
    let mut h = handoff_with_claims(vec![claim("c-gate", "lib.rs", Some("gate"), &c1)], "main");
    assert!(h.claims[0].provenance.anchors[0].sig_hash.is_none(), "arrives unseeded");

    // FIRST reverify: stamps the baseline and verifies.
    let r1 = ocean_context::reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-1");
    assert_eq!(r1[0].1, Resolution::Resolves(1.0));
    assert_eq!(h.claims[0].status, ClaimStatus::Verified);
    let stamped = h.claims[0].provenance.anchors[0].sig_hash.clone();
    assert!(stamped.is_some(), "first reverify must stamp the write-time baseline");

    // The stamp round-trips through the store (caller persists).
    let store_dir = root.join(".ocean/handoffs");
    ocean_context::write_handoff(&store_dir, &h).unwrap();
    let mut h2 = ocean_context::read_freshest(&store_dir, "fixture", "main", 2_000)
        .unwrap()
        .expect("stored handoff");
    assert_eq!(h2.claims[0].provenance.anchors[0].sig_hash, stamped);

    // Shape change in the working tree; SECOND reverify must flag Stale.
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8, strict: bool) -> bool { x > 0 }\n")
        .unwrap();
    let r2 = ocean_context::reverify(&mut h2, &resolver, WORKTREE, 3_000, "sess-2");
    assert_eq!(r2[0].1, Resolution::Stale, "shape change after stamping must flag");
    assert_eq!(h2.claims[0].status, ClaimStatus::Stale);
}

/// Birth-check epistemics in reverify: a symbol that was NOT attestable at
/// its own anchor commit must never verify by name — even when a
/// same-named symbol exists at the commit being reverified. An unparseable
/// birth revision is an honest non-verdict (Reverify), not a death.
#[test]
fn reverify_birth_failures_never_verify_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 0 }\n").unwrap();
    std::fs::write(root.join("broken.rs"), "pub fn half( ((((\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: ghost absent, broken.rs unparseable"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // ghost arrives LATER — present by name at reverify time, absent at birth.
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 0 }\npub fn ghost() {}\n")
        .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: ghost appears"]);

    let resolver = TreeSitterResolver { repo_root: root.to_path_buf() };
    let mut h = handoff_with_claims(
        vec![
            claim("c-ghost", "lib.rs", Some("ghost"), &c1),
            claim("c-broken", "broken.rs", Some("half"), &c1),
        ],
        "main",
    );
    let results = ocean_context::reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-1");

    // absent at birth → Dead (killed), NOT verified-by-name
    assert_eq!(results[0].1, Resolution::Dead);
    assert_eq!(h.claims[0].status, ClaimStatus::Dead);
    assert!(h.claims[0].history.iter().any(|e| e.event == "killed"));
    assert!(h.claims[0].provenance.anchors[0].sig_hash.is_none(), "no baseline laundered");

    // unparseable at birth → Unresolvable (Reverify), no evidence either way
    assert_eq!(results[1].1, Resolution::Unresolvable);
    assert_eq!(h.claims[1].status, ClaimStatus::Reverify);
    assert!(h.claims[1].history.iter().any(|e| e.event == "unresolvable"));
}

/// The v1 stub keeps its exact semantics: FileExistsResolver never stamps
/// (Baseline::Unsupported default) and reverify behaves as before.
#[test]
fn reverify_with_file_exists_resolver_never_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let mut h = handoff_with_claims(vec![claim("c-gate", "lib.rs", Some("gate"), &c1)], "main");
    let results = ocean_context::reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-1");
    assert_eq!(results[0].1, Resolution::Resolves(1.0));
    assert_eq!(h.claims[0].status, ClaimStatus::Verified);
    assert!(h.claims[0].provenance.anchors[0].sig_hash.is_none(), "file-exists never stamps");
}

/// A claim with two anchors: A (valid file at birth) and B (a symbol absent at
/// the anchor commit). Codex P2 (PR #209): the claim-level birth check passes
/// because A resolves, leaving B unseeded. If A is later removed and a
/// same-named GHOST symbol appears where B points, B must NOT verify the claim
/// by name — it never attested at birth. Anchor-level exclusion bars it.
fn multi_anchor_claim(id: &str, anchors: Vec<Anchor>, commit: &str) -> Claim {
    Claim {
        id: id.into(),
        text: format!("multi-anchor claim {id}"),
        provenance: Provenance { anchors, tickets: vec![], commit_sha: commit.into() },
        status: ClaimStatus::Verified,
        knowledge_tier: KnowledgeTier::Individual,
        ps_anchor: None,
        confidence: 0.9,
        borrowed_from: None,
        history: vec![ClaimEvent { at: 0, event: "written".into(), by_session: "t".into() }],
    }
}

#[test]
fn ghost_symbol_at_an_absent_at_birth_anchor_cannot_hold_a_multi_anchor_claim() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    // c1 (birth): A = a.rs#alpha exists; B = b.rs#beta does NOT (file absent).
    std::fs::write(root.join("a.rs"), "pub fn alpha(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: only A exists"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // c2: A removed, and a ghost `beta` appears at b.rs for the first time.
    git(root, &["rm", "-q", "a.rs"]);
    std::fs::write(root.join("b.rs"), "pub fn beta(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: A dies, ghost beta is born"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    let anchor_a = Anchor { file: Some("a.rs".into()), symbol: Some("alpha".into()), lines: vec![], sig_hash: None };
    let anchor_b = Anchor { file: Some("b.rs".into()), symbol: Some("beta".into()), lines: vec![], sig_hash: None };
    let claim = multi_anchor_claim("ghost", vec![anchor_a, anchor_b], &c1);

    let v = replay(root, &[claim], &ts).unwrap();
    // The claim must FAIL at c2: A genuinely died (Dead), and B is excluded as
    // a ghost (Unresolvable, not a hold). It must NOT read HELD by name.
    assert_eq!(
        v[0].first_fail_commit.as_deref(),
        Some(c2.as_str()),
        "a ghost symbol at an absent-at-birth anchor must not hold the claim"
    );
    assert_eq!(v[0].first_fail_resolution, Some(Resolution::Dead));

    // Control: if B's symbol DID exist at birth, B legitimately holds the
    // claim after A dies — exclusion fires only on absent-at-birth anchors.
    let dir2 = tempfile::tempdir().unwrap();
    let root2 = dir2.path();
    git(root2, &["init", "-q"]);
    std::fs::write(root2.join("a.rs"), "pub fn alpha(x: u8) -> bool { x > 0 }\n").unwrap();
    std::fs::write(root2.join("b.rs"), "pub fn beta(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root2, &["add", "."]);
    git(root2, &["commit", "-qm", "c1: BOTH exist"]);
    let c1b = git(root2, &["rev-parse", "HEAD"]);
    git(root2, &["rm", "-q", "a.rs"]);
    git(root2, &["commit", "-qm", "c2: A dies, B persists"]);

    let ts2 = TreeSitterResolver { repo_root: root2.to_path_buf() };
    let a2 = Anchor { file: Some("a.rs".into()), symbol: Some("alpha".into()), lines: vec![], sig_hash: None };
    let b2 = Anchor { file: Some("b.rs".into()), symbol: Some("beta".into()), lines: vec![], sig_hash: None };
    let claim2 = multi_anchor_claim("legit", vec![a2, b2], &c1b);
    let v2 = replay(root2, &[claim2], &ts2).unwrap();
    assert!(
        v2[0].first_fail_commit.is_none(),
        "a real-at-birth sibling anchor still holds the claim after the other dies"
    );
}

/// Codex follow-up (PR #209): an excluded (absent-at-birth) anchor must be
/// non-attesting for ALL outcomes, not only positive ones. If a valid anchor A
/// goes temporarily unparseable while the excluded anchor B is Dead/absent,
/// `check_at` must report Unresolvable for that step — NOT Failed(Dead) from B,
/// which was already deemed non-attestable. The walk then continues and a later
/// valid A still holds the claim.
#[test]
fn excluded_anchor_negative_result_does_not_become_the_claim_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    // c1 (birth): A = a.rs#alpha exists; B = b.rs#beta absent (excluded).
    std::fs::write(root.join("a.rs"), "pub fn alpha(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: A exists, B absent"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    // c2: A becomes temporarily UNPARSEABLE; B still absent (b.rs not present →
    // Dead). Excluded B's Dead must not surface as the claim's failure.
    std::fs::write(root.join("a.rs"), "pub fn alpha( (((( broken\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: A unparseable, B still absent"]);

    // c3: A valid again (same shape as birth).
    std::fs::write(root.join("a.rs"), "pub fn alpha(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c3: A valid again"]);

    let ts = TreeSitterResolver { repo_root: root.to_path_buf() };
    let a = Anchor { file: Some("a.rs".into()), symbol: Some("alpha".into()), lines: vec![], sig_hash: None };
    let b = Anchor { file: Some("b.rs".into()), symbol: Some("beta".into()), lines: vec![], sig_hash: None };
    let claim = multi_anchor_claim("neg", vec![a, b], &c1);

    let v = replay(root, &[claim], &ts).unwrap();
    // The c2 step is Unresolvable (A uncheckable, B excluded), NOT a Dead
    // failure — so the claim never fails, and c3's valid A leaves it HELD.
    assert!(
        v[0].first_fail_commit.is_none(),
        "an excluded anchor's Dead must not become the claim's verdict; got {:?}",
        v[0].first_fail_resolution
    );
    assert!(!v[0].unresolvable, "the walk ends on a valid A, so the claim holds");
}

/// Codex P2 (PR #209): when a claim's birth commit is unreadable — a shallow
/// or pruned clone where `provenance.commit_sha` isn't present — the resolver
/// cannot establish a write-time baseline. That is "can't see the past", NOT
/// "the symbol is dead": a currently-valid symbol must NOT be killed for lack
/// of history. baseline_at returns Unparseable (→ reverify Unresolvable →
/// status Reverify), never Unattestable (→ Dead).
#[test]
fn unreadable_birth_commit_does_not_kill_a_currently_valid_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("lib.rs"), "pub fn gate(x: u8) -> bool { x > 0 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);

    let resolver = TreeSitterResolver { repo_root: root.to_path_buf() };

    // A well-formed SHA that is NOT in this clone — the birth commit can't be
    // read, but `gate` is alive in the working tree right now.
    let phantom_birth = "0123456789abcdef0123456789abcdef01234567";
    let mut h = handoff_with_claims(
        vec![claim("c-gate", "lib.rs", Some("gate"), phantom_birth)],
        "main",
    );
    assert!(h.claims[0].provenance.anchors[0].sig_hash.is_none(), "arrives unseeded");

    let r = ocean_context::reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-1");
    // Must NOT be Dead — an unreadable birth is uncheckable, not death.
    assert_ne!(r[0].1, Resolution::Dead, "unreadable birth must never kill a live symbol");
    assert_eq!(r[0].1, Resolution::Unresolvable, "no baseline establishable → uncheckable");
    assert_eq!(h.claims[0].status, ClaimStatus::Reverify, "flag for attention, not killed");
    // And nothing was stamped from a revision we couldn't read.
    assert!(h.claims[0].provenance.anchors[0].sig_hash.is_none(), "no baseline from unreadable history");

    // Direct resolver check at the phantom commit: Unresolvable, not Dead.
    let anchor = Anchor { file: Some("lib.rs".into()), symbol: Some("gate".into()), lines: vec![], sig_hash: None };
    assert_eq!(resolver.resolve(&anchor, phantom_birth), Resolution::Unresolvable);
}
