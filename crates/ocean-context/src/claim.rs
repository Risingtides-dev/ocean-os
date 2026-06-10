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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    /// What reverification re-resolves.
    pub anchors: Vec<Anchor>,
    pub ticket: Option<String>,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    pub file: String,
    pub symbol: Option<String>,
    /// May be empty — symbol-only and file-only anchors are common (handoff finding F5).
    pub lines: Vec<u32>,
    /// Layer B (tree-sitter signature hash). None in v1.
    pub sig_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClaimStatus {
    Verified,
    Reverify,
    Stale,
    Dead,
    Asserted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KnowledgeTier {
    Common,
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
                        file: "crates/ocean-runtime/src/tools/browser/input.rs".into(),
                        symbol: Some("requires_permission".into()),
                        lines: vec![29, 67, 97, 130],
                        sig_hash: None,
                    }],
                    ticket: Some("OCEAN-16".into()),
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
}
