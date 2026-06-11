//! Replay a handoff's claims against a repo's real git history and print
//! per-claim verdicts for a human to judge.
//!
//! Accepts either a CODIFIED handoff (the ocean-context store format, claims
//! carried in TOML frontmatter) or a prose HANDOFF.md (claims extracted by the
//! regex pass, in which case --anchor supplies the commit they are dated at):
//!
//!   ocean-context-replay --repo ~/dev/ocean-os .ocean/handoffs/<file>.handoff.md
//!   ocean-context-replay --repo ~/dev/ocean-os --anchor d9a9bc9 HANDOFF.md

use anyhow::{bail, Context, Result};
use clap::Parser;
use ocean_context::extract::{extract_claims, ExtractCtx};
use ocean_context::replay::replay;
use ocean_context::seams::FileExistsResolver;
use ocean_context::store;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Replay handoff claims against real git history")]
struct Args {
    /// Repo whose history to walk
    #[arg(long)]
    repo: PathBuf,
    /// Handoff file: codified (.handoff.md store format) or prose HANDOFF.md
    handoff: PathBuf,
    /// Anchor commit for PROSE docs (codified handoffs carry their own)
    #[arg(long)]
    anchor: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let text = std::fs::read_to_string(&args.handoff)
        .with_context(|| format!("reading {}", args.handoff.display()))?;

    let claims = match store::from_markdown(&text) {
        Ok(handoff) => {
            eprintln!(
                "codified handoff: {} claims, anchored at {}",
                handoff.claims.len(),
                handoff.commit_anchor
            );
            let anchor = handoff.commit_anchor.clone();
            let mut claims = handoff.claims;
            for c in &mut claims {
                if c.provenance.commit_sha.is_empty() {
                    c.provenance.commit_sha = anchor.clone();
                }
            }
            claims
        }
        Err(_) => {
            let Some(anchor) = args.anchor.as_deref() else {
                bail!(
                    "{} is not a codified handoff; pass --anchor <sha> to extract claims from prose",
                    args.handoff.display()
                );
            };
            let ctx = ExtractCtx { commit_sha: anchor, now: 0, by_session: "replay-bin" };
            let claims = extract_claims(&text, &ctx);
            eprintln!(
                "prose doc: extracted {} anchored claims from {}",
                claims.len(),
                args.handoff.display()
            );
            claims
        }
    };

    let resolver = FileExistsResolver { repo_root: args.repo.clone() };
    let verdicts = replay(&args.repo, &claims, &resolver)?;

    let (mut held, mut failed, mut skipped, mut unresolvable) = (0usize, 0usize, 0usize, 0usize);
    for v in &verdicts {
        let fate = match (&v.first_fail_commit, v.unresolvable, &v.note) {
            (_, _, Some(n)) => {
                skipped += 1;
                format!("SKIP  ({n})")
            }
            (_, true, _) => {
                unresolvable += 1;
                if v.commits_walked == 0 {
                    "UNRESOLVABLE  (no later commits; unresolvable at anchor)".to_string()
                } else {
                    "UNRESOLVABLE  (no anchor this resolver can check)".to_string()
                }
            }
            (Some(c), _, _) => {
                failed += 1;
                format!("FAIL @ {}", &c[..10.min(c.len())])
            }
            (None, _, _) => {
                held += 1;
                if v.commits_walked == 0 {
                    "HELD  at anchor (no later commits)".to_string()
                } else {
                    format!("HELD  through {} commits", v.commits_walked)
                }
            }
        };
        println!("{:<5} {:<62} {fate}", v.claim_id, v.claim_text);
    }
    eprintln!(
        "\n{held} held, {failed} failed, {unresolvable} unresolvable, {skipped} skipped — judge the FAILs against reality."
    );
    Ok(())
}
