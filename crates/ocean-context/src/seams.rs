//! The four trait seams — THE architectural bet. Layer A declares them with
//! trivial stubs; Layer B (tree-sitter, velocity decay, BM25+embeddings,
//! feature-borrowing) implements them behind the exact same signatures.

use crate::claim::{Anchor, Claim};
use std::path::PathBuf;
use std::process::Command;

/// Pseudo-commit meaning "resolve against the working tree, not git history".
pub const WORKTREE: &str = "WORKTREE";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resolution {
    /// Anchor resolves; payload is S(c) ∈ [0,1] — structural reproducibility.
    Resolves(f32),
    Stale,
    Renamed,
    Dead,
    /// This resolver CANNOT check the anchor (no file anchor, path outside the
    /// repo). Not evidence of anything — "can't check" must never be conflated
    /// with `Stale` or `Dead` (schema friction #2).
    Unresolvable,
}

/// Does this claim's anchor still resolve? v1 stub: file-exists.
/// Layer B (B1): tree-sitter AST + signature hash.
pub trait Resolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution;
}

pub struct TrustContext {
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub now: i64,
    /// The owning handoff's `written_at` — the freshness fallback for claims
    /// whose history carries no `written` event (schema friction #6: such
    /// claims must never silently decay from epoch 0).
    pub handoff_written_at: i64,
}

/// Score a claim's live trust. v1 stub: confidence × recency.
/// Layer B: the full master equation.
pub trait TrustModel {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32;
}

/// Rank stored claims by relevance to a query. v1 stub: substring match.
/// Layer B (B3): RRF(bm25, embeddings).
pub trait Retriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)>;
}

pub struct Borrowed {
    pub from_id: String,
    pub boost: f32,
}

/// Can a thin claim borrow trust from a richer ancestor? v1 stub: never.
/// Layer B (B5): distributed knowledge over high-PS subspaces.
pub trait Borrower {
    fn borrow(&self, claim: &Claim, candidates: &[Claim]) -> Option<Borrowed>;
}

// ---------------------------------------------------------------------------
// v1 stub implementations
// ---------------------------------------------------------------------------

/// File-exists resolver. Exact path → Resolves(1.0). Basename-only match
/// elsewhere in the tree → Resolves(0.5) (corpus claims often anchor bare
/// filenames like `input.rs`; handoff finding F5). Otherwise Dead.
///
/// Symbol-only anchors (`file: None`, the other half of F5) are Unresolvable:
/// this resolver has no file evidence to attest with until B1's symbol
/// resolution exists. "Can't check" is its own verdict — Stale/Dead would
/// assert evidence we don't have (schema friction #2).
///
/// Anchors that are not repo-relative (absolute paths, `..` segments) take
/// the same Unresolvable arm: `PathBuf::join` would otherwise ignore or
/// escape `repo_root`, verifying claims against files OUTSIDE the repository —
/// breaking distrust-by-default for malformed or untrusted handoffs.
pub struct FileExistsResolver {
    pub repo_root: PathBuf,
}

impl FileExistsResolver {
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git").arg("-C").arg(&self.repo_root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn basename_match(&self, listing: &str, file: &str) -> bool {
        let needle = format!("/{file}");
        listing.lines().any(|l| l == file || l.ends_with(&needle))
    }
}

/// Lexical repo-relativity check (v1): reject empty paths (not a file this
/// resolver can attest — `join("")` is the repo dir itself), absolute paths
/// and any `..` component. Deliberately NOT filesystem canonicalization —
/// resolving symlinks would itself touch paths outside the repo. A pure
/// component walk is enough to keep `PathBuf::join` from replacing or
/// escaping `repo_root`.
pub(crate) fn is_repo_relative(file: &str) -> bool {
    use std::path::Component;
    let p = std::path::Path::new(file);
    !file.is_empty()
        && !p.is_absolute()
        && p.components()
            .all(|c| !matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

impl Resolver for FileExistsResolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution {
        // Symbol-only anchors carry no file evidence — typed absence, no
        // sentinel (schema friction #1). This resolver cannot check them.
        let Some(file) = anchor.file.as_deref() else {
            return Resolution::Unresolvable;
        };
        // Path-traversal gate BEFORE any filesystem/git probe: absolute or
        // `..`-bearing paths would make `join` ignore or escape `repo_root`
        // (and must not be handed to git either). Not checkable by us →
        // Unresolvable, never Stale/Dead (schema friction #2).
        if !is_repo_relative(file) {
            return Resolution::Unresolvable;
        }
        if at_commit == WORKTREE {
            if self.repo_root.join(file).exists() {
                return Resolution::Resolves(1.0);
            }
            if !file.contains('/') {
                if let Some(listing) = self.git(&["ls-files"]) {
                    if self.basename_match(&listing, file) {
                        return Resolution::Resolves(0.5);
                    }
                }
            }
            return Resolution::Dead;
        }
        let spec = format!("{at_commit}:{file}");
        if self.git(&["cat-file", "-e", &spec]).is_some() {
            return Resolution::Resolves(1.0);
        }
        if !file.contains('/') {
            if let Some(listing) = self.git(&["ls-tree", "-r", "--name-only", at_commit]) {
                if self.basename_match(&listing, file) {
                    return Resolution::Resolves(0.5);
                }
            }
        }
        Resolution::Dead
    }
}

/// Write-time confidence × exponential recency decay, fixed 30-day half-life.
pub struct ConfidenceRecencyTrust {
    pub half_life_secs: f64,
}

impl Default for ConfidenceRecencyTrust {
    fn default() -> Self {
        Self { half_life_secs: 30.0 * 24.0 * 3600.0 }
    }
}

impl TrustModel for ConfidenceRecencyTrust {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32 {
        // A claim with no `written` history event dates from when its handoff
        // was written — never from epoch 0, never "infinitely fresh"
        // (schema friction #6).
        let written = claim.written_at().unwrap_or(ctx.handoff_written_at);
        let dt = (ctx.now - written).max(0) as f64;
        let decay = (-(std::f64::consts::LN_2) * dt / self.half_life_secs).exp();
        claim.confidence * decay as f32
    }
}

/// Case-insensitive word-overlap scoring; zero-score claims are dropped.
pub struct SubstringRetriever;

impl Retriever for SubstringRetriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)> {
        let words: Vec<String> =
            query.to_lowercase().split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = claims
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let text = c.text.to_lowercase();
                let hits = words.iter().filter(|w| text.contains(w.as_str())).count();
                (hits > 0).then_some((i, hits as f32 / words.len() as f32))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }
}

/// Never borrows. Layer B replaces this with distributed-knowledge borrowing.
pub struct NoBorrow;

impl Borrower for NoBorrow {
    fn borrow(&self, _claim: &Claim, _candidates: &[Claim]) -> Option<Borrowed> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::tests::sample_handoff;

    fn ctx_at(now: i64) -> TrustContext {
        TrustContext { now, handoff_written_at: 1_780_980_000 }
    }

    #[test]
    fn trust_decays_with_age_and_scales_with_confidence() {
        let h = sample_handoff();
        let claim = &h.claims[0]; // confidence 0.9, written at 1_780_980_000
        let trust = ConfidenceRecencyTrust::default();
        let fresh = trust.trust(claim, &ctx_at(1_780_980_000));
        assert!((fresh - 0.9).abs() < 1e-6);
        // one half-life (30 days) later → half the trust
        let later = trust.trust(claim, &ctx_at(1_780_980_000 + 30 * 24 * 3600));
        assert!((later - 0.45).abs() < 1e-3);
    }

    /// Schema friction #6: a claim whose history has no `written` event dates
    /// from the handoff's written_at — it neither decays from epoch 0 (~zero
    /// trust) nor passes as infinitely fresh.
    #[test]
    fn empty_history_falls_back_to_handoff_written_at() {
        let h = sample_handoff();
        let mut claim = h.claims[0].clone(); // confidence 0.9
        claim.history.clear();
        let trust = ConfidenceRecencyTrust::default();
        let one_half_life = ctx_at(1_780_980_000 + 30 * 24 * 3600);
        let got = trust.trust(&claim, &one_half_life);
        assert!((got - 0.45).abs() < 1e-3, "decays from handoff written_at, got {got}");
    }

    #[test]
    fn substring_retriever_ranks_matching_claims_first_and_drops_misses() {
        let mut h = sample_handoff();
        let mut other = h.claims[0].clone();
        other.id = "c2".into();
        other.text = "daemon session save race in ocean-agent".into();
        h.claims.push(other);
        let ranked = SubstringRetriever.rank("session race", &h.claims);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 1); // index of the matching claim
    }

    #[test]
    fn no_borrow_never_borrows() {
        let h = sample_handoff();
        assert!(NoBorrow.borrow(&h.claims[0], &h.claims).is_none());
    }

    /// Symbol-only anchors (`file: None`, F5) must NOT spuriously resolve in
    /// either mode — and must come back Unresolvable ("can't check"), never
    /// Stale/Dead (schema friction #2).
    #[test]
    fn symbol_only_anchor_is_unresolvable_in_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "c1"]);
        let head = git(&["rev-parse", "HEAD"]);

        let r = FileExistsResolver { repo_root: root.to_path_buf() };
        let symbol_only = Anchor {
            file: None,
            symbol: Some("workspace.members".into()),
            lines: vec![],
            sig_hash: None,
        };
        assert_eq!(r.resolve(&symbol_only, WORKTREE), Resolution::Unresolvable);
        assert_eq!(r.resolve(&symbol_only, &head), Resolution::Unresolvable);
        // A degenerate Some("") is equally uncheckable — `join("")` is the
        // repo dir and `cat-file -e <rev>:` accepts the bare tree-ish.
        let empty_file = Anchor { file: Some(String::new()), ..symbol_only.clone() };
        assert_eq!(r.resolve(&empty_file, WORKTREE), Resolution::Unresolvable);
        assert_eq!(r.resolve(&empty_file, &head), Resolution::Unresolvable);
    }

    /// Anchors outside the repo (absolute path or `..` escape) must be
    /// Unresolvable in BOTH modes even when the outside file genuinely exists —
    /// otherwise `join` ignores/escapes repo_root and we verify foreign files.
    #[test]
    fn outside_repo_anchors_are_unresolvable_in_both_modes() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "c1"]);
        let head = git(&["rev-parse", "HEAD"]);

        // A file that genuinely exists ABOVE the repo root.
        std::fs::write(outer.path().join("escape.rs"), "fn outside() {}\n").unwrap();

        let r = FileExistsResolver { repo_root: root.clone() };
        let mk = |file: &str| Anchor {
            file: Some(file.into()),
            symbol: None,
            lines: vec![],
            sig_hash: None,
        };
        let escape = mk("../escape.rs");
        let nested_escape = mk("src/../../escape.rs");
        let absolute = mk("/etc/hosts"); // exists on the host, but not ours to attest
        for anchor in [&escape, &nested_escape, &absolute] {
            let file = anchor.file.as_deref().unwrap();
            assert_eq!(r.resolve(anchor, WORKTREE), Resolution::Unresolvable, "{file}");
            assert_eq!(r.resolve(anchor, &head), Resolution::Unresolvable, "{file}");
        }
        // Sanity: the in-repo anchor still resolves, so the gate is not over-broad.
        assert_eq!(r.resolve(&mk("a.rs"), WORKTREE), Resolution::Resolves(1.0));
        assert_eq!(r.resolve(&mk("./a.rs"), &head), Resolution::Resolves(1.0));
    }

    #[test]
    fn file_exists_resolver_in_worktree_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
        let r = FileExistsResolver { repo_root: dir.path().to_path_buf() };
        let present =
            Anchor { file: Some("src/a.rs".into()), symbol: None, lines: vec![], sig_hash: None };
        let missing =
            Anchor { file: Some("src/gone.rs".into()), symbol: None, lines: vec![], sig_hash: None };
        assert!(matches!(r.resolve(&present, WORKTREE), Resolution::Resolves(_)));
        assert!(matches!(r.resolve(&missing, WORKTREE), Resolution::Dead));
    }
}
