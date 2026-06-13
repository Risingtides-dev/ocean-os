//! B2 acceptance (OCEAN-313): `VelocityMeter` must read a real churn-rate
//! signal off git history — a constantly-rewritten file measures molten, a
//! built-then-abandoned file measures frozen — and that signal must shorten
//! `VelocityDecayTrust`'s half-life for the molten anchor and leave the frozen
//! one at the v1 baseline. Commits are backdated with `GIT_*_DATE` so a real
//! trailing-window rate exists inside a fixture repo. The meter's trust
//! boundary (out-of-tree paths, symlinks) mirrors B1's `TreeSitterResolver`.

use ocean_context::claim::{Anchor, Claim, ClaimEvent, ClaimStatus, KnowledgeTier, Provenance};
use ocean_context::seams::{TrustContext, TrustModel, VelocityDecayTrust, VelocityMeter};
use std::path::Path;

/// Run git with deterministic identity AND a fixed author/committer date so the
/// trailing-window math is reproducible across machines and clocks.
fn git_at(dir: &Path, when: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, "2026-01-01T00:00:00 +0000", args)
}

fn anchored_claim(id: &str, file: &str, commit: &str) -> Claim {
    Claim {
        id: id.into(),
        text: format!("a claim about {file}"),
        provenance: Provenance {
            anchors: vec![Anchor {
                file: Some(file.into()),
                symbol: None,
                lines: vec![],
                sig_hash: None,
            }],
            tickets: vec![],
            commit_sha: commit.into(),
        },
        status: ClaimStatus::Verified,
        knowledge_tier: KnowledgeTier::Individual,
        ps_anchor: None,
        confidence: 1.0,
        borrowed_from: None,
        history: vec![ClaimEvent { at: 0, event: "written".into(), by_session: "t".into() }],
    }
}

/// Build a repo where `frozen.rs` is born on day 1 and never touched again,
/// while `hot.rs` is rewritten on six consecutive days (16..=21) ending at
/// HEAD. Anchored at HEAD's date (day 21), a 7-day trailing window (days
/// 14..21) sees all six hot commits and ZERO frozen ones — frozen's only
/// commit (day 1) sits two weeks outside the window.
fn build_repo(root: &Path) -> String {
    git(root, &["init", "-q"]);
    // Day 1: both files born (well before the trailing window).
    std::fs::write(root.join("frozen.rs"), "fn frozen() { /* v1 */ }\n").unwrap();
    std::fs::write(root.join("hot.rs"), "fn hot() { let x = 0; }\n").unwrap();
    git_at(root, "2026-06-01T12:00:00 +0000", &["add", "."]);
    git_at(root, "2026-06-01T12:00:00 +0000", &["commit", "-qm", "day1: born"]);
    // Days 16..=21: hot.rs rewritten substantially each day; frozen.rs untouched.
    for day in 16..=21 {
        let when = format!("2026-06-{day:02}T12:00:00 +0000");
        let body: String = (0..day * 8).map(|i| format!("    let v{i} = {i};\n")).collect();
        std::fs::write(root.join("hot.rs"), format!("fn hot() {{\n{body}}}\n")).unwrap();
        git_at(root, &when, &["add", "hot.rs"]);
        git_at(root, &when, &["commit", "-qm", &format!("day{day}: churn hot")]);
    }
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn molten_file_measures_higher_velocity_than_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let head = build_repo(root);

    let meter = VelocityMeter::new(root.to_path_buf());
    let hot = meter.measure(&anchored_claim("hot", "hot.rs", &head));
    let frozen = meter.measure(&anchored_claim("frozen", "frozen.rs", &head));

    // Frozen file: not touched in the trailing 7 days ⇒ exactly zero on both
    // axes ⇒ reduces VelocityDecayTrust to the v1 baseline.
    assert_eq!(frozen.v_code, 0.0, "frozen file has no recent commits");
    assert_eq!(frozen.v_sem, 0.0, "frozen file has zero semantic velocity");

    // Hot file: six commits in the window ⇒ a real positive rate, and v_sem
    // (churn-weighted) exceeds v_code (raw rate) because each commit moved many
    // lines.
    assert!(hot.v_code > 0.0, "hot file has recent commits, got {}", hot.v_code);
    assert!(hot.v_sem > hot.v_code, "v_sem ({}) weights churn above v_code ({})", hot.v_sem, hot.v_code);
    // 6 commits / 7-day window.
    assert!((hot.v_code - 6.0 / 7.0).abs() < 1e-6, "v_code = commits/window, got {}", hot.v_code);
}

#[test]
fn velocity_shortens_the_half_life_for_the_molten_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let head = build_repo(root);

    let meter = VelocityMeter::new(root.to_path_buf());
    let hot_v = meter.measure(&anchored_claim("hot", "hot.rs", &head));
    let frozen_v = meter.measure(&anchored_claim("frozen", "frozen.rs", &head));

    let b2 = VelocityDecayTrust::default();
    // The frozen anchor keeps the full 30-day half-life; the molten one's is
    // strictly (and substantially) shorter.
    let h_frozen = b2.half_life_secs(frozen_v.v_sem);
    let h_hot = b2.half_life_secs(hot_v.v_sem);
    assert!((h_frozen - b2.h0_secs).abs() < 1e-6, "frozen ⇒ H₀");
    assert!(h_hot < h_frozen, "molten half-life {h_hot} !< frozen {h_frozen}");

    // End to end through trust(): one claim on each file, written at HEAD's
    // date, scored a week later. The molten claim has lost far more trust.
    let claim_hot = anchored_claim("hot", "hot.rs", &head);
    let written = 1_780_980_000_i64;
    let now = written + 7 * 24 * 3600;
    let mut c = claim_hot.clone();
    c.history[0].at = written;
    let ctx_hot = TrustContext::new(now, written).with_velocity(hot_v);
    let ctx_frozen = TrustContext::new(now, written).with_velocity(frozen_v);
    let t_hot = b2.trust(&c, &ctx_hot);
    let t_frozen = b2.trust(&c, &ctx_frozen);
    assert!(t_frozen > t_hot, "frozen trust {t_frozen} !> molten {t_hot}");
}

#[test]
fn out_of_tree_and_symlinked_anchors_measure_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let head = build_repo(root);
    let meter = VelocityMeter::new(root.to_path_buf());

    // Absolute path and `..` escape: never probed, velocity stays zero — the
    // same Unresolvable trust boundary B1 enforces, expressed as "no signal".
    for bad in ["/etc/hosts", "../hot.rs", "src/../../hot.rs"] {
        let v = meter.measure(&anchored_claim("x", bad, &head));
        assert_eq!(v.v_sem, 0.0, "{bad} must not yield velocity");
        assert_eq!(v.v_code, 0.0, "{bad} must not yield velocity");
    }

    // A symlink whose string is repo-relative still points out of the trust
    // boundary; its history is the link's, not the anchored region's.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(root.join("hot.rs"), root.join("link.rs")).unwrap();
        let v = meter.measure(&anchored_claim("link", "link.rs", &head));
        assert_eq!(v.v_sem, 0.0, "symlinked anchor must measure zero (no trust laundering)");
        assert_eq!(v.v_code, 0.0);
        // Control: the real file behind it still measures hot.
        let real = meter.measure(&anchored_claim("hot", "hot.rs", &head));
        assert!(real.v_sem > 0.0, "the real file is still molten");
    }
}

#[test]
fn anchorless_and_empty_commit_claims_measure_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let head = build_repo(root);
    let meter = VelocityMeter::new(root.to_path_buf());

    // No anchor files at all.
    let mut anchorless = anchored_claim("none", "hot.rs", &head);
    anchorless.provenance.anchors.clear();
    assert_eq!(meter.measure(&anchorless).v_sem, 0.0);

    // Empty anchor commit: nothing to date the window against.
    let no_commit = anchored_claim("nc", "hot.rs", "");
    assert_eq!(meter.measure(&no_commit).v_sem, 0.0);
}
