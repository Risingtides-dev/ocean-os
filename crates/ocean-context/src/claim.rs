//! The codified handoff schema. Human prose stays in `narrative`;
//! the machine-checkable substance is `claims`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    pub session_id: String,
    pub parent_session: Option<String>,
    pub repo: String,
    pub branch: String,
    /// The clock claims are dated against (short sha).
    pub commit_anchor: String,
    pub scope_ring: ScopeRing,
    /// v1: zeros. Layer B fills these.
    pub velocity_at_write: Velocity,
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub written_at: i64,
    pub narrative: String,
    pub claims: Vec<Claim>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub id: String,
    pub text: String,
    pub provenance: Provenance,
    pub status: ClaimStatus,
    /// v1: defaults to Individual. Layer B computes it.
    pub knowledge_tier: KnowledgeTier,
    /// Layer B (Wang-Fusi parallelism score). None in v1.
    pub ps_anchor: Option<f32>,
    /// Trust at write-time.
    pub confidence: f32,
    /// Distributed-knowledge edge (Layer B).
    pub borrowed_from: Option<String>,
    /// written | reverified | promoted | killed — self-versioning.
    pub history: Vec<ClaimEvent>,
}

impl Claim {
    /// Unix time of the original `written` event, if recorded.
    pub fn written_at(&self) -> Option<i64> {
        self.history.iter().find(|e| e.event == "written").map(|e| e.at)
    }

    /// Derive write-time confidence from anchor richness instead of letting the
    /// writer free-type a number (handoff finding F3). The extractor uses this;
    /// hand-written claims should too.
    ///
    /// Deterministic and monotonic in evidence:
    /// - base 0.25, or 0.50 when the surrounding doc section declared the fact
    ///   verified (`declared_verified`);
    /// - +0.10 per anchor, capped at 3 anchors;
    /// - +0.10 if any anchor pins line numbers;
    /// - +0.10 if any anchor names a symbol.
    ///
    /// Maximum is exactly 1.0 (0.5 + 0.3 + 0.1 + 0.1).
    pub fn derive_confidence(anchors: &[Anchor], declared_verified: bool) -> f32 {
        let base: f32 = if declared_verified { 0.50 } else { 0.25 };
        let count_term = 0.10 * (anchors.len().min(3) as f32);
        let line_term = if anchors.iter().any(|a| !a.lines.is_empty()) { 0.10 } else { 0.0 };
        let symbol_term = if anchors.iter().any(|a| a.symbol.is_some()) { 0.10 } else { 0.0 };
        (base + count_term + line_term + symbol_term).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    /// What reverification re-resolves.
    pub anchors: Vec<Anchor>,
    /// A line can carry several tickets (schema friction #3). Old handoffs
    /// stored a single optional `ticket`; the alias + compat deserializer
    /// still accept that shape.
    #[serde(alias = "ticket", default, deserialize_with = "tickets_compat")]
    pub tickets: Vec<String>,
    pub commit_sha: String,
}

/// Accept the legacy singular `ticket` (string or null) as well as the
/// current `tickets` list, so handoffs stored before the schema-friction
/// round still parse.
fn tickets_compat<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Compat {
        Many(Vec<String>),
        One(String),
    }
    Ok(match Option::<Compat>::deserialize(de)? {
        Some(Compat::Many(v)) => v,
        Some(Compat::One(s)) => vec![s],
        None => Vec::new(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    /// `None` for symbol-only anchors (handoff finding F5) — there is no
    /// empty-string sentinel; absence is typed (schema friction #1). Legacy
    /// handoffs encoded symbol-only anchors as `file = ""`; the compat
    /// deserializer normalizes empty/whitespace to `None` on load, so the
    /// old sentinel can never survive a read and violate the
    /// `file.is_none() == symbol-only` contract downstream.
    #[serde(default, deserialize_with = "file_compat")]
    pub file: Option<String>,
    pub symbol: Option<String>,
    /// May be empty — symbol-only and file-only anchors are common (handoff finding F5).
    pub lines: Vec<u32>,
    /// Layer B (tree-sitter signature hash). None in v1.
    pub sig_hash: Option<String>,
}

/// Map the legacy empty-string file sentinel (and whitespace-only noise) to
/// typed absence on load.
fn file_compat<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(de)?.filter(|s| !s.trim().is_empty()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClaimStatus {
    Verified,
    Reverify,
    Stale,
    Dead,
    Asserted,
}

/// WHERE a claim is knowable — orthogonal to `ClaimStatus` (handoff finding F2:
/// tier = where it's knowable, status = whether it currently reproduces). The
/// ENGINE computes the tier and defaults to `Individual`; writers are never
/// asked for it. Layer B (B6) promotes via the epistemic similarity graph.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum KnowledgeTier {
    Common,
    #[default]
    Individual,
    Distributed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScopeRing {
    Session,
    Branch,
    Repo,
    Brain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Velocity {
    pub v_code: f32,
    pub v_sem: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimEvent {
    pub at: i64,
    pub event: String,
    pub by_session: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_handoff() -> Handoff {
        Handoff {
            session_id: "sess-1".into(),
            parent_session: None,
            repo: "ocean-os".into(),
            branch: "main".into(),
            commit_anchor: "d9a9bc9".into(),
            scope_ring: ScopeRing::Repo,
            velocity_at_write: Velocity { v_code: 0.0, v_sem: 0.0 },
            written_at: 1_780_980_000,
            narrative: "The prose handoff.\n".into(),
            claims: vec![Claim {
                id: "c1".into(),
                text: "mutators implement requires_permission".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: Some("crates/ocean-runtime/src/tools/browser/input.rs".into()),
                        symbol: Some("requires_permission".into()),
                        lines: vec![29, 67, 97, 130],
                        sig_hash: None,
                    }],
                    tickets: vec!["OCEAN-16".into()],
                    commit_sha: "d9a9bc9".into(),
                },
                status: ClaimStatus::Verified,
                knowledge_tier: KnowledgeTier::Individual,
                ps_anchor: None,
                confidence: 0.9,
                borrowed_from: None,
                history: vec![ClaimEvent {
                    at: 1_780_980_000,
                    event: "written".into(),
                    by_session: "sess-1".into(),
                }],
            }],
        }
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let h = sample_handoff();
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn claim_written_at_reads_history() {
        let h = sample_handoff();
        assert_eq!(h.claims[0].written_at(), Some(1_780_980_000));
    }

    fn anchor(file: &str, symbol: Option<&str>, lines: Vec<u32>) -> Anchor {
        Anchor { file: Some(file.into()), symbol: symbol.map(Into::into), lines, sig_hash: None }
    }

    #[test]
    fn legacy_empty_file_sentinel_normalizes_to_none() {
        // Codex P2 round 2 on PR #205: old handoffs encoded symbol-only
        // anchors as `file = ""`. The sentinel must not survive a load —
        // downstream relies on `file.is_none() == symbol-only`.
        let a: Anchor =
            serde_json::from_str(r#"{"file":"","symbol":"workspace.members","lines":[]}"#).unwrap();
        assert_eq!(a.file, None);
        let a: Anchor = toml::from_str("file = \"\"\nsymbol = \"workspace.members\"\nlines = []")
            .unwrap();
        assert_eq!(a.file, None);
        // whitespace-only is equally meaningless
        let a: Anchor = serde_json::from_str(r#"{"file":"  ","lines":[]}"#).unwrap();
        assert_eq!(a.file, None);
        // absent and null still mean None; real paths survive untouched
        let a: Anchor = serde_json::from_str(r#"{"lines":[]}"#).unwrap();
        assert_eq!(a.file, None);
        let a: Anchor = serde_json::from_str(r#"{"file":null,"lines":[]}"#).unwrap();
        assert_eq!(a.file, None);
        let a: Anchor = serde_json::from_str(r#"{"file":"src/a.rs","lines":[]}"#).unwrap();
        assert_eq!(a.file.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn legacy_singular_ticket_still_parses() {
        // Wire-compat (schema friction #3): old stored handoffs carry
        // `ticket = "OCEAN-16"` (or null in JSON); both map onto `tickets`.
        let p: Provenance =
            serde_json::from_str(r#"{"anchors":[],"ticket":"OCEAN-16","commit_sha":"abc"}"#)
                .unwrap();
        assert_eq!(p.tickets, vec!["OCEAN-16".to_string()]);
        let p: Provenance =
            serde_json::from_str(r#"{"anchors":[],"ticket":null,"commit_sha":"abc"}"#).unwrap();
        assert!(p.tickets.is_empty());
        let p: Provenance = serde_json::from_str(r#"{"anchors":[],"commit_sha":"abc"}"#).unwrap();
        assert!(p.tickets.is_empty());
        let p: Provenance = toml::from_str("anchors = []\nticket = \"OCEAN-9\"\ncommit_sha = \"abc\"")
            .unwrap();
        assert_eq!(p.tickets, vec!["OCEAN-9".to_string()]);
    }

    #[test]
    fn derive_confidence_is_monotonic_in_evidence() {
        let bare = [anchor("a.rs", None, vec![])];
        let lined = [anchor("a.rs", None, vec![10])];
        let symboled = [anchor("a.rs", Some("f"), vec![10])];
        let asserted = Claim::derive_confidence(&bare, false);
        let verified = Claim::derive_confidence(&bare, true);
        assert!(verified > asserted);
        assert!(Claim::derive_confidence(&lined, false) > asserted);
        assert!(Claim::derive_confidence(&symboled, true) > Claim::derive_confidence(&lined, true));
    }

    #[test]
    fn derive_confidence_caps_at_one_and_handles_no_anchors() {
        let rich: Vec<Anchor> =
            (0..5).map(|i| anchor(&format!("f{i}.rs"), Some("sym"), vec![1, 2])).collect();
        assert!((Claim::derive_confidence(&rich, true) - 1.0).abs() < f32::EPSILON);
        assert!((Claim::derive_confidence(&[], false) - 0.25).abs() < f32::EPSILON);
    }
}
