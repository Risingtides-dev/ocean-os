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
}

/// Does this claim's anchor still resolve? v1 stub: file-exists.
/// Layer B (B1): tree-sitter AST + signature hash.
pub trait Resolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution;
}

pub struct TrustContext {
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub now: i64,
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

impl Resolver for FileExistsResolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution {
        if at_commit == WORKTREE {
            if self.repo_root.join(&anchor.file).exists() {
                return Resolution::Resolves(1.0);
            }
            if !anchor.file.contains('/') {
                if let Some(listing) = self.git(&["ls-files"]) {
                    if self.basename_match(&listing, &anchor.file) {
                        return Resolution::Resolves(0.5);
                    }
                }
            }
            return Resolution::Dead;
        }
        let spec = format!("{at_commit}:{}", anchor.file);
        if self.git(&["cat-file", "-e", &spec]).is_some() {
            return Resolution::Resolves(1.0);
        }
        if !anchor.file.contains('/') {
            if let Some(listing) = self.git(&["ls-tree", "-r", "--name-only", at_commit]) {
                if self.basename_match(&listing, &anchor.file) {
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
        let written = claim.written_at().unwrap_or(ctx.now);
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

    #[test]
    fn trust_decays_with_age_and_scales_with_confidence() {
        let h = sample_handoff();
        let claim = &h.claims[0]; // confidence 0.9, written at 1_780_980_000
        let trust = ConfidenceRecencyTrust::default();
        let fresh = trust.trust(claim, &TrustContext { now: 1_780_980_000 });
        assert!((fresh - 0.9).abs() < 1e-6);
        // one half-life (30 days) later → half the trust
        let later = trust.trust(claim, &TrustContext { now: 1_780_980_000 + 30 * 24 * 3600 });
        assert!((later - 0.45).abs() < 1e-3);
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

    #[test]
    fn file_exists_resolver_in_worktree_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
        let r = FileExistsResolver { repo_root: dir.path().to_path_buf() };
        let present = Anchor { file: "src/a.rs".into(), symbol: None, lines: vec![], sig_hash: None };
        let missing = Anchor { file: "src/gone.rs".into(), symbol: None, lines: vec![], sig_hash: None };
        assert!(matches!(r.resolve(&present, WORKTREE), Resolution::Resolves(_)));
        assert!(matches!(r.resolve(&missing, WORKTREE), Resolution::Dead));
    }
}
