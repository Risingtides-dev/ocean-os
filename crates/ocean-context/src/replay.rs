//! The replay harness: walk a repo's git history forward from each claim's
//! anchor commit, resolving anchors at every commit. The first commit at
//! which no anchor resolves is the verdict a human judges against reality.
//! This is the same code path production reverification uses — you tune the
//! replay; the thing you tuned is the engine.

use crate::claim::Claim;
use crate::seams::{Baseline, Resolution, Resolver};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub struct ReplayVerdict {
    pub claim_id: String,
    /// First 60 chars of the claim text, for human-readable verdict tables.
    pub claim_text: String,
    pub anchor_commit: String,
    pub commits_walked: usize,
    /// First commit (full sha) at which no anchor resolved. None = held to
    /// HEAD. When the walk is empty (claim anchored at HEAD), this is the
    /// anchor commit itself, checked once.
    pub first_fail_commit: Option<String>,
    /// HOW the claim failed at `first_fail_commit` — the most-alive checkable
    /// resolution among its anchors there (`Stale` = anchor resolves to
    /// changed structure, `Dead` = anchor gone). None while the claim holds.
    pub first_fail_resolution: Option<Resolution>,
    /// True when the resolver could not check ANY of the claim's anchors
    /// (all `Resolution::Unresolvable`) — "cannot check" is a distinct
    /// verdict, never a FAIL (schema friction #2).
    pub unresolvable: bool,
    /// Set when the claim couldn't be replayed (no anchors, bad anchor commit).
    pub note: Option<String>,
}

/// Replay `claims` against `repo_root`'s history from each claim's
/// `provenance.commit_sha` (exclusive) to HEAD.
pub fn replay(
    repo_root: &Path,
    claims: &[Claim],
    resolver: &dyn Resolver,
) -> Result<Vec<ReplayVerdict>> {
    // Stamp write-time baselines before walking — same lifecycle rule as
    // `reverify`: a symbol-bearing anchor that arrived unseeded (everything
    // extraction produces) must compare shapes against its birth state, not
    // verify by name through the whole walk. Resolvers without a notion of
    // shape return `Unsupported` and are untouched.
    //
    // Stamping is claim-level via the birth check, but EXCLUSION is
    // anchor-level: in a multi-anchor claim, one valid anchor passes the birth
    // check while an absent-at-birth sibling stays unseeded. If that valid
    // anchor later dies and a same-named ghost symbol appears at the sibling,
    // `resolve` would return `Resolves(1.0)` by name and the claim would read
    // HELD. An anchor that never attested at its own birth commit is not
    // evidence later — record it per claim and bar it from holding the claim
    // for the rest of the walk.
    let mut claims: Vec<Claim> = claims.to_vec();
    let mut excluded_per_claim: Vec<std::collections::HashSet<usize>> =
        Vec::with_capacity(claims.len());
    for claim in claims.iter_mut() {
        let birth = claim.provenance.commit_sha.clone();
        let mut excluded = std::collections::HashSet::new();
        if !birth.is_empty() {
            for (i, anchor) in claim.provenance.anchors.iter_mut().enumerate() {
                if anchor.symbol.is_some() && anchor.sig_hash.is_none() {
                    match resolver.baseline_at(anchor, &birth) {
                        Baseline::Stamped(hash) => anchor.sig_hash = Some(hash),
                        // Never attestable at birth → a later name match is a
                        // ghost, not the originally-claimed symbol.
                        Baseline::Unattestable | Baseline::Unparseable => {
                            excluded.insert(i);
                        }
                        // Unsupported (v1 resolvers, no-shape anchors): keep the
                        // pre-baseline semantics — nothing to exclude.
                        Baseline::Unsupported => {}
                    }
                }
            }
        }
        excluded_per_claim.push(excluded);
    }
    let mut verdicts = Vec::new();
    for (claim, excluded) in claims.iter().zip(excluded_per_claim.iter()) {
        let mut verdict = ReplayVerdict {
            claim_id: claim.id.clone(),
            claim_text: claim.text.chars().take(60).collect(),
            anchor_commit: claim.provenance.commit_sha.clone(),
            commits_walked: 0,
            first_fail_commit: None,
            first_fail_resolution: None,
            unresolvable: false,
            note: None,
        };
        if claim.provenance.anchors.is_empty() {
            verdict.note = Some("no structural anchors — nothing to replay".to_string());
            verdicts.push(verdict);
            continue;
        }
        let commits = match rev_list(repo_root, &claim.provenance.commit_sha) {
            Ok(c) => c,
            Err(e) => {
                verdict.note = Some(format!("rev-list failed: {e}"));
                verdicts.push(verdict);
                continue;
            }
        };
        verdict.commits_walked = commits.len();
        // Birth check: a claim must be attestable at its OWN anchor commit.
        // An anchor that never resolved at birth (misspelled symbol, file not
        // yet committed when the handoff was written) must fail there — not
        // read HELD because a same-named symbol appears later in the walk
        // with no baseline to compare against. This also covers claims
        // anchored at HEAD, where there is nothing later to walk at all.
        //
        // The birth check uses the RAW resolver verdict (no exclusion): the
        // anchor commit is precisely where an unattestable anchor's Dead/
        // Unresolvable must register. Exclusion suppresses only LATER ghosts,
        // so it applies to the post-birth walk below, never here.
        let no_exclusion = std::collections::HashSet::new();
        let mut ends_unresolvable = false;
        match check_at(resolver, claim, &claim.provenance.commit_sha, &no_exclusion) {
            Step::Held => {}
            Step::Unresolvable => ends_unresolvable = true,
            Step::Failed(r) => {
                verdict.first_fail_commit = Some(claim.provenance.commit_sha.clone());
                verdict.first_fail_resolution = Some(r);
                verdicts.push(verdict);
                continue;
            }
        }
        if commits.is_empty() {
            verdict.unresolvable = ends_unresolvable;
            verdicts.push(verdict);
            continue;
        }
        for commit in &commits {
            match check_at(resolver, claim, commit, excluded) {
                Step::Held => ends_unresolvable = false,
                // A single uncheckable revision (mid-edit blob, unparseable
                // merge artifact) is no evidence — keep walking; later
                // commits can still attest the claim either way.
                Step::Unresolvable => ends_unresolvable = true,
                Step::Failed(r) => {
                    verdict.first_fail_commit = Some(commit.clone());
                    verdict.first_fail_resolution = Some(r);
                    break;
                }
            }
        }
        // What matters is the state the walk ENDS in: a transient
        // uncheckable window inside the walk is absorbed by a later held
        // step, but a claim whose tail through HEAD cannot be checked must
        // not read HELD — its current state is unattested (and a claim
        // nothing could ever check stays unresolvable end to end).
        verdict.unresolvable = ends_unresolvable && verdict.first_fail_commit.is_none();
        verdicts.push(verdict);
    }
    Ok(verdicts)
}

/// Outcome of resolving one claim's anchors at one commit.
enum Step {
    /// At least one anchor resolves.
    Held,
    /// Every anchor is `Resolution::Unresolvable` — nothing this resolver can
    /// check, an honest non-verdict, never a FAIL (schema friction #2).
    Unresolvable,
    /// No anchor resolves and at least one was checkable; carries the
    /// most-alive checkable resolution (Renamed > Stale > Dead) so the
    /// verdict can say HOW the claim broke, not just where.
    Failed(Resolution),
}

fn check_at(
    resolver: &dyn Resolver,
    claim: &Claim,
    commit: &str,
    excluded: &std::collections::HashSet<usize>,
) -> Step {
    let resolutions: Vec<Resolution> = claim
        .provenance
        .anchors
        .iter()
        .enumerate()
        .map(|(i, a)| {
            // An anchor that never attested at its own birth commit carries no
            // evidence later — in EITHER direction. A name match is a ghost
            // (must not hold the claim), and a Dead/Stale on the same anchor is
            // not a real failure either (must not become the claim's verdict
            // when a sibling is merely uncheckable). Force it Unresolvable for
            // every outcome: "can't trust this anchor", full stop.
            if excluded.contains(&i) {
                Resolution::Unresolvable
            } else {
                resolver.resolve(a, commit)
            }
        })
        .collect();
    if resolutions
        .iter()
        .any(|r| matches!(r, Resolution::Resolves(_)))
    {
        return Step::Held;
    }
    if resolutions
        .iter()
        .all(|r| matches!(r, Resolution::Unresolvable))
    {
        return Step::Unresolvable;
    }
    // Most-alive checkable resolution, mirroring `reverify`'s ranking.
    let rank = |r: &Resolution| match r {
        Resolution::Renamed => 3,
        Resolution::Stale => 2,
        Resolution::Dead => 1,
        Resolution::Resolves(_) | Resolution::Unresolvable => 0,
    };
    let best = resolutions
        .iter()
        .filter(|r| !matches!(r, Resolution::Unresolvable))
        .max_by_key(|r| rank(r))
        .copied()
        .unwrap_or(Resolution::Dead);
    Step::Failed(best)
}

fn rev_list(repo_root: &Path, from: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-list", "--reverse", &format!("{from}..HEAD")])
        .output()
        .context("running git rev-list")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}
