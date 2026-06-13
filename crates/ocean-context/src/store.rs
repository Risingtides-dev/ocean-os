//! On-disk codified handoffs: scalar TOML frontmatter (machine-owned), a
//! fenced TOML claims block (machine-owned), then the markdown narrative
//! (human-owned) — one file per handoff. Layer B may move this to pg/graph
//! behind the same functions.
//!
//! Claims live OUTSIDE the frontmatter in a backtick fence whose length is
//! chosen to exceed any backtick run in the serialized claims, so the closing
//! fence cannot collide with content (schema friction #5 — the old format
//! kept claims inside the `+++` frontmatter and had to probe candidate close
//! delimiters whenever a claim text contained a line starting `+++`). The old
//! format is still read for backward compatibility.

use crate::claim::{Claim, Handoff, ScopeRing, Velocity};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DELIM: &str = "+++";
/// Info string on the claims fence — marks the block as machine-owned.
const CLAIMS_FENCE_TAG: &str = "toml ocean-context-claims";

/// Scalar handoff fields — frontmatter only. Claims are NOT serialized here
/// (they get their own fenced block); the field exists solely so old-format
/// files, which carried claims inside the frontmatter, still deserialize.
#[derive(Serialize, Deserialize)]
struct FrontMatter {
    session_id: String,
    parent_session: Option<String>,
    repo: String,
    branch: String,
    commit_anchor: String,
    scope_ring: ScopeRing,
    velocity_at_write: Velocity,
    written_at: i64,
    #[serde(default, skip_serializing)]
    claims: Vec<Claim>,
}

/// The fenced claims block parses as this standalone TOML document.
#[derive(Serialize, Deserialize)]
struct ClaimsBlock {
    claims: Vec<Claim>,
}

/// Shortest backtick fence (≥ 3) strictly longer than any leading backtick
/// run in `content` — by construction the closing fence cannot collide.
fn fence_for(content: &str) -> String {
    let max_run = content
        .lines()
        .map(|l| l.len() - l.trim_start_matches('`').len())
        .max()
        .unwrap_or(0);
    "`".repeat((max_run + 1).max(3))
}

pub fn to_markdown(h: &Handoff) -> Result<String> {
    let fm = FrontMatter {
        session_id: h.session_id.clone(),
        parent_session: h.parent_session.clone(),
        repo: h.repo.clone(),
        branch: h.branch.clone(),
        commit_anchor: h.commit_anchor.clone(),
        scope_ring: h.scope_ring,
        velocity_at_write: h.velocity_at_write,
        written_at: h.written_at,
        claims: Vec::new(), // never serialized; see FrontMatter
    };
    let scalar_toml = toml::to_string(&fm).context("serializing handoff frontmatter")?;
    let claims_toml = toml::to_string(&ClaimsBlock {
        claims: h.claims.clone(),
    })
    .context("serializing handoff claims")?;
    let fence = fence_for(&claims_toml);
    Ok(format!(
        "{DELIM}\n{scalar_toml}{DELIM}\n\n{fence}{CLAIMS_FENCE_TAG}\n{claims_toml}{fence}\n\n{}",
        h.narrative
    ))
}

/// Split the body that follows the frontmatter into (claims, narrative) when
/// it carries a fenced claims block (current format). `Ok(None)` = no block
/// (old format); a present-but-unparseable block is a hard error, never
/// silently demoted to narrative.
fn split_claims_block(body: &str) -> Result<Option<(Vec<Claim>, String)>> {
    let Some(rest) = body.strip_prefix("\n\n") else {
        return Ok(None);
    };
    let Some(line_end) = rest.find('\n') else {
        return Ok(None);
    };
    let first_line = &rest[..line_end];
    let ticks = first_line.len() - first_line.trim_start_matches('`').len();
    if ticks < 3 || &first_line[ticks..] != CLAIMS_FENCE_TAG {
        return Ok(None);
    }
    let fence = &first_line[..ticks];
    let content = &rest[line_end + 1..];
    // The writer guarantees no content line starts with `ticks` backticks, so
    // the first line equal to the fence is the close.
    let close = format!("\n{fence}\n");
    let close_idx = content
        .find(&close)
        .context("missing closing claims fence")?;
    let block: ClaimsBlock =
        toml::from_str(&content[..close_idx + 1]).context("parsing handoff claims block")?;
    // The writer separates the closing fence from the narrative with exactly
    // one blank line; the close pattern consumed one newline, strip exactly
    // one more so narratives that start with newlines round-trip losslessly.
    let after = &content[close_idx + close.len()..];
    let narrative = after.strip_prefix('\n').unwrap_or(after).to_string();
    Ok(Some((block.claims, narrative)))
}

pub fn from_markdown(text: &str) -> Result<Handoff> {
    let rest = text
        .strip_prefix(DELIM)
        .context("missing opening +++ frontmatter delimiter")?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    // In the old format the closing "\n+++" could also appear inside a TOML
    // multi-line string (a claim text with a line starting "+++"), so try each
    // candidate close in order and take the first prefix that parses as TOML.
    let mut parsed: Option<(FrontMatter, &str)> = None;
    let mut last_err: Option<toml::de::Error> = None;
    for (idx, _) in rest.match_indices("\n+++") {
        match toml::from_str::<FrontMatter>(&rest[..idx + 1]) {
            Ok(fm) => {
                parsed = Some((fm, &rest[idx + "\n+++".len()..]));
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let (fm, body) = parsed.ok_or_else(|| match last_err {
        Some(e) => anyhow::Error::new(e).context("parsing handoff frontmatter"),
        None => anyhow::anyhow!("missing closing +++ frontmatter delimiter"),
    })?;
    let (claims, narrative) = match split_claims_block(body)? {
        Some((claims, narrative)) => (claims, narrative),
        // Old format: claims lived in the frontmatter, narrative follows the
        // closing delimiter after exactly one blank line.
        None => (
            fm.claims,
            body.strip_prefix("\n\n").unwrap_or(body).to_string(),
        ),
    };
    Ok(Handoff {
        session_id: fm.session_id,
        parent_session: fm.parent_session,
        repo: fm.repo,
        branch: fm.branch,
        commit_anchor: fm.commit_anchor,
        scope_ring: fm.scope_ring,
        velocity_at_write: fm.velocity_at_write,
        written_at: fm.written_at,
        narrative,
        claims,
    })
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Filename convention: repo, branch and written_at are all encoded (plus the
/// session id as a collision-breaker). `read_freshest` still trusts the parsed
/// frontmatter, never the filename.
fn file_name(h: &Handoff) -> String {
    format!(
        "{}__{}__{:012}-{}.handoff.md",
        sanitize(&h.repo),
        sanitize(&h.branch),
        h.written_at.max(0),
        sanitize(&h.session_id),
    )
}

/// Default handoff directory for a repo: `<repo_root>/.ocean/handoffs`.
pub fn default_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".ocean").join("handoffs")
}

/// Write a codified handoff into `dir`. Returns the path written.
pub fn write_handoff(dir: &Path, h: &Handoff) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(file_name(h));
    fs::write(&path, to_markdown(h)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Most recent handoff for (repo, branch) in `dir`, by `written_at`.
/// Unparseable files are skipped (warned to stderr), not fatal.
pub fn read_freshest(dir: &Path, repo: &str, branch: &str) -> Result<Option<Handoff>> {
    let mut best: Option<Handoff> = None;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(None), // no handoff dir yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".handoff.md"))
        {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        match from_markdown(&text) {
            Ok(h) if h.repo == repo && h.branch == branch => {
                if best.as_ref().map_or(true, |b| h.written_at > b.written_at) {
                    best = Some(h);
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!(
                "ocean-context: skipping unparseable {}: {e}",
                path.display()
            ),
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::tests::sample_handoff;

    #[test]
    fn markdown_round_trip_is_lossless() {
        let h = sample_handoff();
        let md = to_markdown(&h).unwrap();
        assert!(md.starts_with("+++\n"));
        let back = from_markdown(&md).unwrap();
        assert_eq!(h, back);
    }

    /// Acceptance #2: round-trip a handoff with ALL field variants populated —
    /// parent_session, ps_anchor, borrowed_from, multiple tickets, every
    /// status, an empty-lines anchor and a symbol-only anchor (F5),
    /// multi-event history, and a hostile narrative (leading newline, +++
    /// line, fenced toml block).
    #[test]
    fn round_trip_with_all_field_variants_is_lossless() {
        use crate::claim::{Anchor, Claim, ClaimEvent, ClaimStatus, KnowledgeTier, Provenance};
        let mk_event = |at: i64, event: &str| ClaimEvent {
            at,
            event: event.into(),
            by_session: "sess-a".into(),
        };
        let mut h = sample_handoff();
        h.parent_session = Some("sess-parent".into());
        h.velocity_at_write = Velocity {
            v_code: 0.25,
            v_sem: 0.125,
        };
        h.narrative =
            "\nleading newline kept\n+++\nnot a delimiter\n```toml\nx = 1\n```\ntrailing\n".into();
        h.claims = vec![
            Claim {
                id: "v1".into(),
                text: "file-only anchor, no lines (F5)".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: Some("Cargo.toml".into()),
                        symbol: None,
                        lines: vec![],
                        sig_hash: None,
                    }],
                    tickets: vec![],
                    commit_sha: "d9a9bc9".into(),
                },
                status: ClaimStatus::Asserted,
                knowledge_tier: KnowledgeTier::Common,
                ps_anchor: Some(0.75),
                confidence: 0.5,
                borrowed_from: Some("v2".into()),
                history: vec![mk_event(1, "written"), mk_event(2, "reverified")],
            },
            Claim {
                id: "v2".into(),
                text: "symbol-only anchor (F5) and two tickets".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: None,
                        symbol: Some("workspace.members".into()),
                        lines: vec![],
                        sig_hash: Some("deadbeef".into()),
                    }],
                    tickets: vec!["OCEAN-306".into(), "OCEAN-308".into()],
                    commit_sha: "d9a9bc9".into(),
                },
                status: ClaimStatus::Dead,
                knowledge_tier: KnowledgeTier::Distributed,
                ps_anchor: None,
                confidence: 0.85,
                borrowed_from: None,
                history: vec![mk_event(1, "written"), mk_event(3, "killed")],
            },
        ];
        let back = from_markdown(&to_markdown(&h).unwrap()).unwrap();
        assert_eq!(h, back);
    }

    /// Schema friction #5: a claim text containing lines that start with `+++`
    /// or with long backtick runs must not break the claims block — the
    /// writer escalates the fence past any backtick run in the content, so
    /// the close can never collide.
    #[test]
    fn hostile_claim_text_cannot_collide_with_delimiters() {
        let mut h = sample_handoff();
        h.claims[0].text =
            "hostile claim\n+++\nstill the claim\n````\nfour ticks\n````\nend".into();
        let md = to_markdown(&h).unwrap();
        let back = from_markdown(&md).unwrap();
        assert_eq!(h, back);
    }

    /// Backward compat (schema friction #5): handoffs written before the
    /// fenced claims block — claims inside the `+++` frontmatter, singular
    /// `ticket`, `file` as a plain string — still parse.
    #[test]
    fn old_format_with_claims_in_frontmatter_still_parses() {
        let old = r#"+++
session_id = "sess-1"
repo = "ocean-os"
branch = "main"
commit_anchor = "d9a9bc9"
scope_ring = "Repo"
written_at = 1780980000

[velocity_at_write]
v_code = 0.0
v_sem = 0.0

[[claims]]
id = "c1"
text = "an old-format claim"
status = "Verified"
knowledge_tier = "Individual"
confidence = 0.9

[claims.provenance]
ticket = "OCEAN-16"
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = "Cargo.toml"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "sess-1"

[[claims]]
id = "c2"
text = "a legacy symbol-only claim with the empty-file sentinel"
status = "Reverify"
knowledge_tier = "Individual"
confidence = 0.6

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = ""
symbol = "workspace.members"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "sess-1"
+++

The old prose narrative.
"#;
        let h = from_markdown(old).unwrap();
        assert_eq!(h.session_id, "sess-1");
        assert_eq!(h.claims.len(), 2);
        assert_eq!(h.claims[0].provenance.tickets, vec!["OCEAN-16".to_string()]);
        assert_eq!(
            h.claims[0].provenance.anchors[0].file.as_deref(),
            Some("Cargo.toml")
        );
        // Legacy `file = ""` sentinel normalizes to typed absence on load
        // (Codex P2 round 2 on PR #205).
        assert_eq!(h.claims[1].provenance.anchors[0].file, None);
        assert_eq!(
            h.claims[1].provenance.anchors[0].symbol.as_deref(),
            Some("workspace.members")
        );
        assert_eq!(h.narrative, "The old prose narrative.\n");
        // Re-writing lands in the current format, fenced claims block intact.
        let rewritten = to_markdown(&h).unwrap();
        assert!(rewritten.contains("toml ocean-context-claims"));
        assert_eq!(from_markdown(&rewritten).unwrap(), h);
    }

    /// The vendored codified fixture (replay acceptance input) parses in the
    /// current store format.
    #[test]
    fn codified_fixture_parses() {
        let text = include_str!("../tests/fixtures/handoff-ocean-context.codified.handoff.md");
        let h = from_markdown(text).unwrap();
        assert_eq!(h.claims.len(), 5);
        assert_eq!(h.commit_anchor, "d9a9bc9");
        assert!(h.claims.iter().all(|c| !c.history.is_empty()));
    }

    #[test]
    fn write_then_read_freshest_picks_latest_for_repo_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut old = sample_handoff();
        old.session_id = "sess-old".into();
        old.written_at = 100;
        let mut new = sample_handoff();
        new.session_id = "sess-new".into();
        new.written_at = 200;
        let mut other_branch = sample_handoff();
        other_branch.session_id = "sess-other".into();
        other_branch.branch = "feature/x".into();
        other_branch.written_at = 300;

        write_handoff(dir.path(), &old).unwrap();
        write_handoff(dir.path(), &new).unwrap();
        write_handoff(dir.path(), &other_branch).unwrap();

        let got = read_freshest(dir.path(), "ocean-os", "main")
            .unwrap()
            .unwrap();
        assert_eq!(got.session_id, "sess-new");
        assert!(read_freshest(dir.path(), "ocean-os", "nope")
            .unwrap()
            .is_none());
    }
}
