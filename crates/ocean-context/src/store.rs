//! On-disk codified handoffs: TOML frontmatter (machine-owned) + markdown
//! narrative (human-owned), one file per handoff. Layer B may move this to
//! pg/graph behind the same functions.

use crate::claim::{Claim, Handoff, ScopeRing, Velocity};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DELIM: &str = "+++";

/// Everything except the narrative lives in frontmatter.
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
    claims: Vec<Claim>,
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
        claims: h.claims.clone(),
    };
    let toml = toml::to_string(&fm).context("serializing handoff frontmatter")?;
    Ok(format!("{DELIM}\n{toml}{DELIM}\n\n{}", h.narrative))
}

pub fn from_markdown(text: &str) -> Result<Handoff> {
    let rest = text.strip_prefix(DELIM).context("missing opening +++ frontmatter delimiter")?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    // The closing "\n+++" could in principle also appear inside a TOML
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
    // The writer separates the closing delimiter from the narrative with
    // exactly one blank line; strip exactly that, so narratives that
    // themselves start with newlines round-trip losslessly.
    let narrative = body.strip_prefix("\n\n").unwrap_or(body).to_string();
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
        claims: fm.claims,
    })
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
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
        if !path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".handoff.md")) {
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
            Err(e) => eprintln!("ocean-context: skipping unparseable {}: {e}", path.display()),
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
    /// parent_session, ps_anchor, borrowed_from, ticket, every status, an
    /// empty-lines anchor and a symbol-only anchor (F5), multi-event history,
    /// and a hostile narrative (leading newline, +++ line, fenced toml block).
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
        h.velocity_at_write = Velocity { v_code: 0.25, v_sem: 0.125 };
        h.narrative =
            "\nleading newline kept\n+++\nnot a delimiter\n```toml\nx = 1\n```\ntrailing\n".into();
        h.claims = vec![
            Claim {
                id: "v1".into(),
                text: "file-only anchor, no lines (F5)".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: "Cargo.toml".into(),
                        symbol: None,
                        lines: vec![],
                        sig_hash: None,
                    }],
                    ticket: None,
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
                text: "symbol-only anchor (F5) and a ticket".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: String::new(),
                        symbol: Some("workspace.members".into()),
                        lines: vec![],
                        sig_hash: Some("deadbeef".into()),
                    }],
                    ticket: Some("OCEAN-306".into()),
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

        let got = read_freshest(dir.path(), "ocean-os", "main").unwrap().unwrap();
        assert_eq!(got.session_id, "sess-new");
        assert!(read_freshest(dir.path(), "ocean-os", "nope").unwrap().is_none());
    }
}
