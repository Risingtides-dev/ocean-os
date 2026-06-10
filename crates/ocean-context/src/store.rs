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
    let (fm_str, body) =
        rest.split_once("\n+++").context("missing closing +++ frontmatter delimiter")?;
    let fm: FrontMatter = toml::from_str(fm_str).context("parsing handoff frontmatter")?;
    Ok(Handoff {
        session_id: fm.session_id,
        parent_session: fm.parent_session,
        repo: fm.repo,
        branch: fm.branch,
        commit_anchor: fm.commit_anchor,
        scope_ring: fm.scope_ring,
        velocity_at_write: fm.velocity_at_write,
        written_at: fm.written_at,
        narrative: body.trim_start_matches('\n').to_string(),
        claims: fm.claims,
    })
}

fn file_name(h: &Handoff) -> String {
    let safe: String = h
        .session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{}-{}.handoff.md", h.written_at, safe)
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
