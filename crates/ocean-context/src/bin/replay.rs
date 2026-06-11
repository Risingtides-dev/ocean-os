//! Replay a handoff's claims against a repo's real git history and print
//! per-claim verdicts for a human to judge.
//!
//! Accepts either a CODIFIED handoff (the ocean-context store format, claims
//! carried in TOML frontmatter) or a prose HANDOFF.md (claims extracted by the
//! regex pass, in which case --anchor supplies the commit they are dated at):
//!
//!   ocean-context-replay --repo ~/dev/ocean-os .ocean/handoffs/<file>.handoff.md
//!   ocean-context-replay --repo ~/dev/ocean-os --anchor d9a9bc9 HANDOFF.md
//!
//! `--resolver tree-sitter` swaps in the B1 symbol-presence + sig-hash
//! resolver (symbol-bearing anchors get their write-time baseline seeded at
//! the claim's anchor commit). `--compare` runs BOTH resolvers and prints a
//! per-claim verdict diff as a markdown table.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use ocean_context::claim::Claim;
use ocean_context::extract::{extract_claims, ExtractCtx};
use ocean_context::replay::{replay, ReplayVerdict};
use ocean_context::seams::{FileExistsResolver, Resolution};
use ocean_context::store;
use ocean_context::treesitter::TreeSitterResolver;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ResolverKind {
    /// Layer A stub: a claim holds while its anchor file exists.
    FileExists,
    /// B1: symbol presence + signature/shape hash (tree-sitter for Rust,
    /// structured key probe for TOML).
    TreeSitter,
}

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
    /// Which resolver checks the anchors
    #[arg(long, value_enum, default_value = "file-exists")]
    resolver: ResolverKind,
    /// Run BOTH resolvers and print a per-claim verdict diff
    #[arg(long)]
    compare: bool,
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

    if args.compare {
        let fe = run(&args, &claims, ResolverKind::FileExists)?;
        let ts = run(&args, &claims, ResolverKind::TreeSitter)?;
        print_comparison(&fe, &ts);
        return Ok(());
    }

    let verdicts = run(&args, &claims, args.resolver)?;
    let (mut held, mut failed, mut skipped, mut unresolvable) = (0usize, 0usize, 0usize, 0usize);
    for v in &verdicts {
        match (&v.first_fail_commit, v.unresolvable, &v.note) {
            (_, _, Some(_)) => skipped += 1,
            (_, true, _) => unresolvable += 1,
            (Some(_), _, _) => failed += 1,
            (None, _, _) => held += 1,
        }
        println!("{:<5} {:<62} {}", v.claim_id, v.claim_text, fate(v));
    }
    eprintln!(
        "\n{held} held, {failed} failed, {unresolvable} unresolvable, {skipped} skipped — judge the FAILs against reality."
    );
    Ok(())
}

/// Run one resolver over the claims. Tree-sitter mode seeds write-time
/// sig-hash baselines for symbol-bearing anchors at each claim's anchor
/// commit, so "shape changed since the claim was written" is detectable.
fn run(args: &Args, claims: &[Claim], kind: ResolverKind) -> Result<Vec<ReplayVerdict>> {
    let mut claims = claims.to_vec();
    match kind {
        ResolverKind::FileExists => {
            let resolver = FileExistsResolver { repo_root: args.repo.clone() };
            replay(&args.repo, &claims, &resolver)
        }
        ResolverKind::TreeSitter => {
            let resolver = TreeSitterResolver { repo_root: args.repo.clone() };
            resolver.seed_sig_hashes(&mut claims);
            replay(&args.repo, &claims, &resolver)
        }
    }
}

/// Human-readable fate of one verdict, with the failure KIND when known.
fn fate(v: &ReplayVerdict) -> String {
    match (&v.first_fail_commit, v.unresolvable, &v.note) {
        (_, _, Some(n)) => format!("SKIP  ({n})"),
        (_, true, _) => {
            if v.commits_walked == 0 {
                "UNRESOLVABLE  (no later commits; unresolvable at anchor)".to_string()
            } else {
                "UNRESOLVABLE  (no anchor this resolver can check)".to_string()
            }
        }
        (Some(c), _, _) => {
            let kind = match v.first_fail_resolution {
                Some(Resolution::Stale) => "FAIL(Stale)",
                Some(Resolution::Renamed) => "FAIL(Renamed)",
                _ => "FAIL(Dead)",
            };
            format!("{kind} @ {}", &c[..10.min(c.len())])
        }
        (None, _, _) => {
            if v.commits_walked == 0 {
                "HELD  at anchor (no later commits)".to_string()
            } else {
                format!("HELD  through {} commits", v.commits_walked)
            }
        }
    }
}

/// Per-claim verdict diff between the two resolvers, as a markdown table a
/// human (or a PR body) can consume directly.
fn print_comparison(fe: &[ReplayVerdict], ts: &[ReplayVerdict]) {
    println!("| claim | text | file-exists | tree-sitter | diff |");
    println!("|-------|------|-------------|-------------|------|");
    let mut diffs = 0usize;
    for (a, b) in fe.iter().zip(ts.iter()) {
        debug_assert_eq!(a.claim_id, b.claim_id);
        let (fa, fb) = (fate(a), fate(b));
        let marker = if fa == fb { "" } else { "**≠**" };
        if fa != fb {
            diffs += 1;
        }
        println!("| {} | {} | {} | {} | {} |", a.claim_id, a.claim_text.trim(), fa, fb, marker);
    }
    eprintln!("\n{diffs} verdict(s) differ — judge each ≠ against reality.");
}
