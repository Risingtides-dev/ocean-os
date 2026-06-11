use ocean_context::claim::Anchor;
use ocean_context::extract::{extract_claims, ExtractCtx};
use ocean_context::replay::replay;
use ocean_context::seams::FileExistsResolver;
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

#[test]
fn replay_finds_the_commit_where_an_anchor_dies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: add a.rs and b.rs"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    std::fs::write(root.join("b.rs"), "fn b() { /* changed */ }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: touch b.rs"]);

    git(root, &["rm", "-q", "a.rs"]);
    git(root, &["commit", "-qm", "c3: remove a.rs"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    let prose = "The a feature lives in a.rs as designed.\n\
                 The b feature lives in b.rs and is stable.";
    let ctx = ExtractCtx { commit_sha: &c1, now: 0, by_session: "replay-test" };
    let claims = extract_claims(prose, &ctx);
    assert_eq!(claims.len(), 2);

    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();

    assert_eq!(verdicts.len(), 2);
    // a.rs dies at c3
    assert_eq!(verdicts[0].first_fail_commit.as_deref(), Some(c3.as_str()));
    // b.rs survives the whole walk
    assert_eq!(verdicts[1].first_fail_commit, None);
    assert_eq!(verdicts[1].commits_walked, 2); // c2 and c3
}

/// Schema friction #2 in the harness: a claim whose anchors the resolver
/// cannot check (symbol-only, untracked/escape paths) is UNRESOLVABLE, not a
/// FAIL — "can't check" must never read as "anchor died here".
#[test]
fn replay_marks_uncheckable_claims_unresolvable_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2"]);

    let ctx = ExtractCtx { commit_sha: &c1, now: 0, by_session: "replay-test" };
    let mut claims = extract_claims("A checkable claim about a.rs that holds fine.", &ctx);
    assert_eq!(claims.len(), 1);
    let mut symbol_only = claims[0].clone();
    symbol_only.id = "c-sym".into();
    symbol_only.provenance.anchors =
        vec![Anchor { file: None, symbol: Some("a".into()), lines: vec![], sig_hash: None }];
    claims.push(symbol_only);

    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();
    assert_eq!(verdicts.len(), 2);
    // checkable claim held, untouched by the unresolvable arm
    assert!(!verdicts[0].unresolvable);
    assert_eq!(verdicts[0].first_fail_commit, None);
    // symbol-only claim: unresolvable, NOT failed
    assert!(verdicts[1].unresolvable);
    assert_eq!(verdicts[1].first_fail_commit, None);
}

/// Codex P2 on PR #205: a claim anchored at HEAD has an empty walk — the
/// resolver must still run ONCE at the anchor commit. A symbol-only claim
/// reports unresolvable (not a silent "held through 0 commits" false
/// positive); a valid file anchor reports held-at-anchor; a dead anchor
/// fails at the anchor commit.
#[test]
fn empty_walk_still_checks_the_anchor_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "only commit"]);
    let head = git(root, &["rev-parse", "HEAD"]);

    // All three claims anchored at HEAD: zero commits to walk.
    let ctx = ExtractCtx { commit_sha: &head, now: 0, by_session: "replay-test" };
    let mut claims = extract_claims("A live claim about a.rs anchored at HEAD itself.", &ctx);
    assert_eq!(claims.len(), 1);
    let mut symbol_only = claims[0].clone();
    symbol_only.id = "c-sym".into();
    symbol_only.provenance.anchors =
        vec![Anchor { file: None, symbol: Some("a".into()), lines: vec![], sig_hash: None }];
    let mut dead = claims[0].clone();
    dead.id = "c-dead".into();
    dead.provenance.anchors =
        vec![Anchor { file: Some("gone.rs".into()), symbol: None, lines: vec![], sig_hash: None }];
    claims.push(symbol_only);
    claims.push(dead);

    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();
    assert_eq!(verdicts.len(), 3);
    assert!(verdicts.iter().all(|v| v.commits_walked == 0));

    // valid file anchor: held at the anchor, not silently held
    assert!(!verdicts[0].unresolvable);
    assert_eq!(verdicts[0].first_fail_commit, None);
    // symbol-only anchor: unresolvable, never a false "held"
    assert!(verdicts[1].unresolvable);
    assert_eq!(verdicts[1].first_fail_commit, None);
    // dead anchor: fails at the anchor commit itself
    assert!(!verdicts[2].unresolvable);
    assert_eq!(verdicts[2].first_fail_commit.as_deref(), Some(head.as_str()));
}

#[test]
fn replay_notes_unwalkable_claims_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "only commit"]);

    let ctx = ExtractCtx { commit_sha: "deadbeef", now: 0, by_session: "replay-test" };
    let claims = extract_claims("Anchored claim about x.rs with a bogus anchor commit.", &ctx);
    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].note.as_deref().unwrap_or("").contains("rev-list failed"));
}
