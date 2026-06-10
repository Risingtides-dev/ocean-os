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
