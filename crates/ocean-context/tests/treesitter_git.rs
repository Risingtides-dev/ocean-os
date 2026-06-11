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

    // worktree mode DOES see the dirty file: garbage parses to no `gate`
    assert_eq!(ts.resolve(&anchor, WORKTREE), Resolution::Dead);

    // file missing at-commit → Dead; path traversal stays Unresolvable
    let gone =
        Anchor { file: Some("nope.rs".into()), symbol: None, lines: vec![], sig_hash: None };
    assert_eq!(ts.resolve(&gone, &c1), Resolution::Dead);
    let escape =
        Anchor { file: Some("../x.rs".into()), symbol: Some("gate".into()), lines: vec![], sig_hash: None };
    assert_eq!(ts.resolve(&escape, &c1), Resolution::Unresolvable);
    assert_eq!(ts.resolve(&escape, WORKTREE), Resolution::Unresolvable);
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
