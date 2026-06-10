//! ocean-context — the handoff as a context primitive.
//!
//! A handoff is a set of claims, each with provenance, that the receiving
//! session distrusts by default and reverifies against ground truth.
//! Spec: docs/specs/ocean-context-handoff-engine.md

pub mod claim;
pub mod extract;
pub mod replay;
pub mod seams;
pub mod store;

pub use claim::*;
pub use extract::{extract_claims, ExtractCtx};
pub use seams::{
    Borrowed, Borrower, ConfidenceRecencyTrust, FileExistsResolver, NoBorrow, Resolution,
    Resolver, Retriever, SubstringRetriever, TrustContext, TrustModel, WORKTREE,
};

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Write a codified handoff into `dir`. Returns the path written.
pub fn write_handoff(dir: &Path, handoff: &Handoff) -> Result<PathBuf> {
    store::write_handoff(dir, handoff)
}

/// Most recent handoff for (repo, branch), claims sorted most-trusted-first
/// by the stub TrustModel. `now` is unix seconds (passed in, never computed).
pub fn read_freshest(dir: &Path, repo: &str, branch: &str, now: i64) -> Result<Option<Handoff>> {
    let Some(mut h) = store::read_freshest(dir, repo, branch)? else {
        return Ok(None);
    };
    let trust = ConfidenceRecencyTrust::default();
    let ctx = TrustContext { now };
    h.claims.sort_by(|a, b| trust.trust(b, &ctx).total_cmp(&trust.trust(a, &ctx)));
    Ok(Some(h))
}

/// Re-resolve every anchored claim at `at_commit` (or [`WORKTREE`]); update
/// statuses and append history events. Anchorless claims are left untouched —
/// there is nothing to check. Returns (claim id, resolution) per checked claim.
pub fn reverify(
    handoff: &mut Handoff,
    resolver: &dyn Resolver,
    at_commit: &str,
    now: i64,
    by_session: &str,
) -> Vec<(String, Resolution)> {
    fn rank(r: Resolution) -> u8 {
        match r {
            Resolution::Resolves(_) => 3,
            Resolution::Renamed => 2,
            Resolution::Stale => 1,
            Resolution::Dead => 0,
        }
    }
    let mut out = Vec::new();
    for claim in &mut handoff.claims {
        if claim.provenance.anchors.is_empty() {
            continue;
        }
        let mut best = Resolution::Dead;
        for anchor in &claim.provenance.anchors {
            let r = resolver.resolve(anchor, at_commit);
            if rank(r) > rank(best) {
                best = r;
            }
        }
        claim.status = match best {
            Resolution::Resolves(_) => ClaimStatus::Verified,
            Resolution::Renamed => ClaimStatus::Reverify,
            Resolution::Stale => ClaimStatus::Stale,
            Resolution::Dead => ClaimStatus::Dead,
        };
        claim.history.push(ClaimEvent {
            at: now,
            event: format!("reverified:{best:?}"),
            by_session: by_session.to_string(),
        });
        out.push((claim.id.clone(), best));
    }
    out
}
