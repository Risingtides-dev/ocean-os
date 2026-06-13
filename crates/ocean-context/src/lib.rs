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
pub mod treesitter;

pub use claim::*;
pub use extract::{extract_claims, ExtractCtx};
pub use seams::{
    Baseline, Borrowed, Borrower, ConfidenceRecencyTrust, FileExistsResolver, NoBorrow,
    Resolution, Resolver, Retriever, SubstringRetriever, TrustContext, TrustModel,
    VelocityDecayTrust, VelocityMeter, VELOCITY_WINDOW_DAYS, WORKTREE,
};
pub use treesitter::TreeSitterResolver;

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
    let ctx = TrustContext::new(now, h.written_at);
    h.claims.sort_by(|a, b| trust.trust(b, &ctx).total_cmp(&trust.trust(a, &ctx)));
    Ok(Some(h))
}

/// Re-resolve every anchored claim at `at_commit` (or [`WORKTREE`]); update
/// statuses and append history events. Anchorless claims are left untouched —
/// there is nothing to check. Returns (claim id, resolution) per checked claim.
///
/// **The first reverify stamps baselines.** Every symbol-bearing anchor that
/// arrived without a `sig_hash` (everything `extract_claims` and hand-written
/// handoffs produce) gets its write-time shape hash computed at the claim's
/// anchor commit via [`Resolver::baseline_at`] and stamped onto the in-memory
/// handoff — the caller persists it (the store round-trips `sig_hash`), so
/// the SECOND reverify can flag a shape change. Resolvers without baseline
/// support (the v1 file-exists stub) return `Unsupported` and behave exactly
/// as before. When the baseline CANNOT be computed at the anchor commit, the
/// claim was never attestable — the birth-check analog: absent-at-birth is
/// `Dead`, an unparseable birth revision is `Unresolvable`. It must never
/// fall back to verified-by-name.
pub fn reverify(
    handoff: &mut Handoff,
    resolver: &dyn Resolver,
    at_commit: &str,
    now: i64,
    by_session: &str,
) -> Vec<(String, Resolution)> {
    // Evidence-first ranking: any CHECKABLE resolution — positive (Resolves)
    // or negative (Dead) — outranks Unresolvable, so an uncheckable sibling
    // anchor can never mask a dead one. Unresolvable wins only when NO anchor
    // produced evidence at all (schema friction #2). Among checkable
    // resolutions, most-alive wins: a claim with one resolving anchor still
    // holds, matching the replay harness's any-resolves semantics.
    fn rank(r: Resolution) -> u8 {
        match r {
            Resolution::Resolves(_) => 4,
            Resolution::Renamed => 3,
            Resolution::Stale => 2,
            Resolution::Dead => 1,
            Resolution::Unresolvable => 0,
        }
    }
    let mut out = Vec::new();
    for claim in &mut handoff.claims {
        if claim.provenance.anchors.is_empty() {
            continue;
        }
        let birth_commit = claim.provenance.commit_sha.clone();
        let outcomes: Vec<AnchorOutcome> = claim
            .provenance
            .anchors
            .iter_mut()
            .map(|anchor| resolve_stamping_baseline(resolver, anchor, &birth_commit, at_commit))
            .collect();
        // Does the claim have an anchor that genuinely ATTESTED at birth (the
        // real subject)? If so, anchors that were unattestable at birth are
        // non-attesting in either direction — excluded to Unresolvable, exactly
        // as the replay walk does — so a ghost neither holds the claim nor lets
        // its `Dead` become the verdict. If NO anchor attested at birth, none
        // is excluded: birth verdicts stand, so a lone never-true anchor is
        // still Dead. Note: an Unsupported/uncheckable anchor (e.g. a B1 anchor
        // on a language it can't parse) attested nothing and does NOT count.
        let has_attestable_sibling = outcomes.iter().any(|o| o.attested_at_birth);
        let mut best = Resolution::Unresolvable;
        for o in &outcomes {
            let r = if o.birth_unattestable && has_attestable_sibling {
                Resolution::Unresolvable
            } else {
                o.resolution
            };
            if rank(r) > rank(best) {
                best = r;
            }
        }
        claim.status = match best {
            Resolution::Resolves(_) => ClaimStatus::Verified,
            Resolution::Renamed => ClaimStatus::Reverify,
            Resolution::Stale => ClaimStatus::Stale,
            // No anchor this resolver can check: flag for attention — the
            // machine neither confirmed nor refuted anything.
            Resolution::Unresolvable => ClaimStatus::Reverify,
            Resolution::Dead => ClaimStatus::Dead,
        };
        // Event vocabulary: "reverified" when the claim still stands in some
        // form, "killed" when its anchors are gone, "unresolvable" when the
        // resolver could not check it (an honest non-verdict; never recorded
        // as a reverification).
        let event = match best {
            Resolution::Dead => "killed".to_string(),
            Resolution::Unresolvable => "unresolvable".to_string(),
            _ => "reverified".to_string(),
        };
        claim.history.push(ClaimEvent { at: now, event, by_session: by_session.to_string() });
        out.push((claim.id.clone(), best));
    }
    out
}

/// One anchor's reverify outcome.
///
/// - `resolution`: its resolution at `at_commit`.
/// - `birth_unattestable`: the anchor was NOT attestable at its own birth
///   commit (absent symbol, or unreadable/unparseable birth blob). Its later
///   `resolve` is a ghost and its `Dead` is not the claim's death — but only
///   when a SIBLING genuinely attested at birth (see below).
/// - `attested_at_birth`: the anchor genuinely had evidence at birth — a
///   stamped symbol baseline, or a non-symbol/Unsupported anchor that still
///   resolves to a CHECKABLE result now. This is the "real subject" test: an
///   `Unsupported` anchor that only resolves `Unresolvable` (e.g. a
///   `TreeSitterResolver` anchor on a language B1 can't parse) attested
///   nothing and must NOT license suppressing a sibling's real `Dead`.
struct AnchorOutcome {
    resolution: Resolution,
    birth_unattestable: bool,
    attested_at_birth: bool,
}

/// Resolve one anchor, stamping its write-time baseline first when it is
/// symbol-bearing and unseeded. An empty/unknown birth commit cannot be
/// probed — skip seeding and resolve as before (never guess a baseline).
fn resolve_stamping_baseline(
    resolver: &dyn Resolver,
    anchor: &mut claim::Anchor,
    birth_commit: &str,
    at_commit: &str,
) -> AnchorOutcome {
    // `stamped_baseline` is true when this anchor got a real write-time shape
    // hash at birth — it attested regardless of how it resolves now.
    let mut stamped_baseline = false;
    if anchor.symbol.is_some() && anchor.sig_hash.is_none() && !birth_commit.is_empty() {
        match resolver.baseline_at(anchor, birth_commit) {
            Baseline::Unsupported => {}
            Baseline::Stamped(hash) => {
                anchor.sig_hash = Some(hash);
                stamped_baseline = true;
            }
            // Never attestable at its own anchor commit. The birth verdict
            // (Dead / Unresolvable) holds if this is the claim's only kind of
            // anchor, but a sibling that DID attest must not be dragged down by
            // it — the caller decides, using `birth_unattestable`.
            Baseline::Unattestable => {
                return AnchorOutcome {
                    resolution: Resolution::Dead,
                    birth_unattestable: true,
                    attested_at_birth: false,
                }
            }
            Baseline::Unparseable => {
                return AnchorOutcome {
                    resolution: Resolution::Unresolvable,
                    birth_unattestable: true,
                    attested_at_birth: false,
                }
            }
        }
    }
    let resolution = resolver.resolve(anchor, at_commit);
    // Attested at birth iff: a symbol baseline was stamped (B1), OR this is a
    // non-symbol / Unsupported anchor that still produces CHECKABLE evidence
    // now (a live file-only anchor). An Unsupported anchor that only resolves
    // Unresolvable attested nothing and is not the claim's "real subject".
    let attested_at_birth =
        stamped_baseline || !matches!(resolution, Resolution::Unresolvable);
    AnchorOutcome { resolution, birth_unattestable: false, attested_at_birth }
}
