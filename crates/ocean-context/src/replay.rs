//! The replay harness: walk a repo's git history forward from each claim's
//! anchor commit, resolving anchors at every commit. The first commit at
//! which no anchor resolves is the verdict a human judges against reality.
//! This is the same code path production reverification uses — you tune the
//! replay; the thing you tuned is the engine.

use crate::claim::Claim;
use crate::seams::{Resolution, Resolver};
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
    /// First commit (full sha) at which no anchor resolved. None = held to HEAD.
    pub first_fail_commit: Option<String>,
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
    let mut verdicts = Vec::new();
    for claim in claims {
        let mut verdict = ReplayVerdict {
            claim_id: claim.id.clone(),
            claim_text: claim.text.chars().take(60).collect(),
            anchor_commit: claim.provenance.commit_sha.clone(),
            commits_walked: 0,
            first_fail_commit: None,
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
        for commit in &commits {
            let resolves = claim
                .provenance
                .anchors
                .iter()
                .any(|a| matches!(resolver.resolve(a, commit), Resolution::Resolves(_)));
            if !resolves {
                verdict.first_fail_commit = Some(commit.clone());
                break;
            }
        }
        verdicts.push(verdict);
    }
    Ok(verdicts)
}

fn rev_list(repo_root: &Path, from: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-list", "--reverse", &format!("{from}..HEAD")])
        .output()
        .context("running git rev-list")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}
