//! Deterministic replay harness for tuning the [`QuorumEngine`](crate::quorum).
//!
//! A live council is slow, costs LLM calls, and is non-deterministic — the
//! models say something different every run, so you can never cleanly A/B a
//! threshold change against it. This module fixes that.
//!
//! The insight: the engine is **pure** and takes `now_ms` explicitly, so the
//! *only* thing that drives convergence is the ordered stream of marks
//! (propose / endorse / inhibit, each with an author, a target proposal, and a
//! timestamp). Capture that stream **once** from a real council
//! ([`Recording`]), then [`replay`] it through a fresh engine under any
//! [`QuorumConfig`] — instantly, for free, and identically every time.
//!
//! That turns "tune by watching 30-second councils" into "scrub a grid of
//! configs over a fixed recording in milliseconds." Reviewer credentials are
//! captured too, so correlation caps replay exactly. The deck stays the *watch*
//! surface for live feel; this is the *tune* surface.
//!
//! ```
//! use ocean_longhouse::replay::{Recording, RecordedMark, RecordedMarkKind};
//! use ocean_longhouse::quorum::QuorumConfig;
//! use uuid::Uuid;
//!
//! let (pa, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
//! let rec = Recording {
//!     question: "demo".into(),
//!     reviewers: vec![],
//!     marks: vec![
//!         RecordedMark { at_ms: 0, author: a, kind: RecordedMarkKind::Propose { proposal: pa } },
//!         RecordedMark { at_ms: 10, author: b, kind: RecordedMarkKind::Endorse { proposal: pa } },
//!     ],
//! };
//! let result = ocean_longhouse::replay::replay(&rec, QuorumConfig::default());
//! assert_eq!(result.replayed_marks, 2);
//! ```

use ocean_agent_sdk::AbortReason;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::evidence::ConvergenceBasis;
use crate::evidence::ReviewerCredential;
use crate::quorum::{QuorumConfig, QuorumEngine, QuorumOutcome};

/// What a recorded mark did to the blackboard. Mirrors the three engine inputs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedMarkKind {
    /// A new proposal entered the field (the proposer implicitly endorses it).
    Propose { proposal: Uuid },
    /// An endorsement of an existing proposal.
    Endorse { proposal: Uuid },
    /// A cross-inhibition of an existing proposal.
    Inhibit { proposal: Uuid },
}

impl RecordedMarkKind {
    /// The proposal this mark targets (every kind targets exactly one).
    pub fn proposal(&self) -> Uuid {
        match *self {
            RecordedMarkKind::Propose { proposal }
            | RecordedMarkKind::Endorse { proposal }
            | RecordedMarkKind::Inhibit { proposal } => proposal,
        }
    }
}

/// One mark in a recorded council: who, when (ms relative to council start),
/// and what. `at_ms` is relative so a recording is portable across runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RecordedMark {
    /// Milliseconds since the council started — drives the engine's time-decay.
    pub at_ms: i64,
    /// Stable id of the agent that posted the mark.
    pub author: Uuid,
    #[serde(flatten)]
    pub kind: RecordedMarkKind,
}

/// Reviewer metadata required to replay correlation-aware evidence exactly.
/// Older recordings omit this list and remain backward compatible: their
/// authors are treated as independent reviewers with the configured prior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedReviewer {
    pub agent_id: Uuid,
    pub correlation_group: String,
    pub reliability_prior: f64,
}

impl From<&ReviewerCredential> for RecordedReviewer {
    fn from(credential: &ReviewerCredential) -> Self {
        Self {
            agent_id: credential.agent_id(),
            correlation_group: credential.correlation_group().to_string(),
            reliability_prior: credential.reliability_prior(),
        }
    }
}

/// A captured council: the question plus its full ordered mark-stream. This is
/// the unit you record once and replay many times under different configs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    /// The deliberated question (for labeling; not fed to the engine).
    pub question: String,
    /// Daemon-owned correlation groups and reliability priors for reproducible
    /// sequential evidence. Missing in legacy recordings by design.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<RecordedReviewer>,
    /// Marks in the exact order the live council fed them to the engine.
    pub marks: Vec<RecordedMark>,
}

impl Recording {
    /// Distinct proposals that appear anywhere in the stream.
    pub fn proposal_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = Vec::new();
        for m in &self.marks {
            let p = m.kind.proposal();
            if !ids.contains(&p) {
                ids.push(p);
            }
        }
        ids
    }
}

/// The outcome of replaying a [`Recording`] under one [`QuorumConfig`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayResult {
    /// `Some(proposal)` if the live stream alone drove the engine to converge,
    /// `None` if it stayed pending through the whole recording (the live flow
    /// would then force-resolve at the deadline — see [`force_resolved`]).
    pub converged_on: Option<Uuid>,
    /// Why the live or compatibility deadline path selected its decision.
    pub convergence_basis: Option<ConvergenceBasis>,
    /// How many marks had been fed when convergence fired (`None` if it never
    /// converged mid-stream). Lower = converges faster / more eagerly.
    pub converged_after_marks: Option<usize>,
    /// Total marks replayed.
    pub replayed_marks: usize,
    /// What a deadline force-resolve would pick if the stream didn't converge
    /// on its own. This is populated only by legacy net-weight rules;
    /// sequential evidence aborts rather than treating the deadline as support.
    pub force_resolved: Option<Uuid>,
    /// Final net weight of every proposal, sorted desc — the field at the end.
    pub final_tally: Vec<(Uuid, f32)>,
}

/// Feed a recording's marks, in order, through a fresh engine under `config`,
/// stopping the convergence check at the first mark that crosses quorum (but
/// still replaying the rest so the final tally reflects the whole field).
///
/// Pure and deterministic: same `(recording, config)` ⇒ same `ReplayResult`.
pub fn replay(recording: &Recording, config: QuorumConfig) -> ReplayResult {
    let mut engine = QuorumEngine::new(config);
    for reviewer in &recording.reviewers {
        if let Ok(credential) = ReviewerCredential::new(
            reviewer.agent_id,
            reviewer.correlation_group.clone(),
            reviewer.reliability_prior,
        ) {
            engine.register_reviewer(credential);
        }
    }
    let mut converged_on = None;
    let mut convergence_basis = None;
    let mut converged_after_marks = None;

    for (i, m) in recording.marks.iter().enumerate() {
        match m.kind {
            RecordedMarkKind::Propose { proposal } => engine.propose(proposal, m.author, m.at_ms),
            RecordedMarkKind::Endorse { proposal } => {
                engine.endorse(proposal, m.author, None, m.at_ms)
            }
            RecordedMarkKind::Inhibit { proposal } => {
                engine.inhibit(proposal, m.author, None, m.at_ms)
            }
        }
        // Evaluate at this mark's own timestamp so decay is honored exactly as
        // the live council would have seen it.
        if converged_on.is_none() {
            if let QuorumOutcome::Converged {
                decision, basis, ..
            } = engine.evaluate(m.at_ms)
            {
                converged_on = Some(decision);
                convergence_basis = Some(basis);
                converged_after_marks = Some(i + 1);
            }
        }
    }

    // Resolve at the final timestamp (or 0 for an empty recording).
    let end_ms = recording.marks.last().map(|m| m.at_ms).unwrap_or(0);

    // What would the live deadline path pick? Clone so we don't mutate the
    // reported engine state. Sequential evidence intentionally returns None.
    let (force_resolved, forced_basis) = if converged_on.is_some() {
        (None, None)
    } else {
        let mut e = engine.clone();
        let decision = e.force_resolve(end_ms, AbortReason::Timeout, true).ok();
        (decision, e.convergence_basis())
    };

    let final_tally = engine
        .tallies(end_ms)
        .into_iter()
        .map(|t| (t.proposal, t.net_weight))
        .collect();

    ReplayResult {
        converged_on,
        convergence_basis: convergence_basis.or(forced_basis),
        converged_after_marks,
        replayed_marks: recording.marks.len(),
        force_resolved,
        final_tally,
    }
}

/// Replay one recording across a whole grid of configs, returning each config's
/// result paired with the config that produced it. The caller supplies the grid
/// (e.g. evidence targets × correlation caps × query costs × ttls) and
/// prints/inspects the table.
pub fn sweep(recording: &Recording, configs: &[QuorumConfig]) -> Vec<(QuorumConfig, ReplayResult)> {
    configs
        .iter()
        .map(|cfg| (*cfg, replay(recording, *cfg)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quorum::QuorumRule;

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    fn propose(at: i64, author: u8, proposal: u8) -> RecordedMark {
        RecordedMark {
            at_ms: at,
            author: uid(author),
            kind: RecordedMarkKind::Propose {
                proposal: uid(proposal),
            },
        }
    }
    fn endorse(at: i64, author: u8, proposal: u8) -> RecordedMark {
        RecordedMark {
            at_ms: at,
            author: uid(author),
            kind: RecordedMarkKind::Endorse {
                proposal: uid(proposal),
            },
        }
    }
    fn inhibit(at: i64, author: u8, proposal: u8) -> RecordedMark {
        RecordedMark {
            at_ms: at,
            author: uid(author),
            kind: RecordedMarkKind::Inhibit {
                proposal: uid(proposal),
            },
        }
    }

    fn legacy_config() -> QuorumConfig {
        QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            ..QuorumConfig::default()
        }
    }

    // A clean climb converges mid-stream, and replay reports when.
    #[test]
    fn replay_reports_convergence_point() {
        let rec = Recording {
            question: "q".into(),
            reviewers: vec![],
            marks: vec![
                propose(0, 10, 1), // net 1.0
                endorse(0, 11, 1), // net 2.0
                endorse(0, 12, 1), // net 3.0 -> crosses default cutoff 2.0, margin ok
            ],
        };
        let r = replay(&rec, legacy_config());
        assert_eq!(r.converged_on, Some(uid(1)));
        // Default cutoff 2.0, margin 1.0: net 2.0 at mark 2 leads by 2.0 -> crosses.
        assert_eq!(r.converged_after_marks, Some(2));
        assert_eq!(r.replayed_marks, 3);
    }

    // Same recording, two configs: a high cutoff never converges mid-stream but
    // force-resolves on the clear leader. This is the whole point of the harness.
    #[test]
    fn sweep_distinguishes_configs() {
        let rec = Recording {
            question: "q".into(),
            reviewers: vec![],
            marks: vec![
                propose(0, 10, 1),
                endorse(0, 11, 1),
                propose(0, 20, 2), // rival, net 1.0; leader net 2.0
            ],
        };
        let eager = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            ..QuorumConfig::default()
        };
        let strict = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 10.0,
                margin: 1.0,
            },
            ..QuorumConfig::default()
        };
        let out = sweep(&rec, &[eager, strict]);
        // Eager: leader hits cutoff 2.0 and leads rival by exactly... 2.0 vs 1.0
        // only after the rival appears, so converges on the endorse (mark 2).
        assert_eq!(out[0].1.converged_on, Some(uid(1)));
        // Strict: never crosses mid-stream, but force-resolve picks the leader.
        assert_eq!(out[1].1.converged_on, None);
        assert_eq!(out[1].1.force_resolved, Some(uid(1)));
    }

    // Decay matters: spread the same marks across time and a fast TTL keeps the
    // leader from ever accumulating enough simultaneous weight to converge.
    #[test]
    fn fast_decay_prevents_convergence() {
        let rec = Recording {
            question: "q".into(),
            reviewers: vec![],
            marks: vec![
                propose(0, 10, 1),
                endorse(2_000, 11, 1), // 2s later
                endorse(4_000, 12, 1), // 4s later
            ],
        };
        let slow = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.5,
                margin: 0.5,
            },
            mark_ttl_ms: 600_000, // ~no decay over 4s
            tie_break_seed: 1,
        };
        let fast = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.5,
                margin: 0.5,
            },
            mark_ttl_ms: 500, // brutal decay: each prior mark nearly gone in 2s
            tie_break_seed: 1,
        };
        let slow_r = replay(&rec, slow);
        let fast_r = replay(&rec, fast);
        assert!(
            slow_r.converged_on.is_some(),
            "slow decay should accumulate to quorum"
        );
        assert!(
            fast_r.converged_on.is_none(),
            "fast decay should never let it stack up"
        );
    }

    // Cross-inhibition over a FULLY symmetric stream: when every endorse is
    // matched by the rival's inhibit at the same instant, neither camp ever
    // pulls ahead, so the field stays split. (Each proposal: proposer +
    // endorser = +2.0, minus two rival inhibits = 0.0 net.)
    #[test]
    fn symmetric_rivals_stay_split() {
        // Cutoff 3.0: each camp transiently reaches at most +2.0 before the
        // rival's inhibit pulls it back, so neither ever crosses. End state is
        // a balanced 0.0/0.0 field — a true split.
        let cfg = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 3.0,
                margin: 1.0,
            },
            ..QuorumConfig::default()
        };
        let rec = Recording {
            question: "q".into(),
            reviewers: vec![],
            marks: vec![
                propose(0, 10, 1),
                propose(0, 20, 2),
                endorse(0, 11, 1), // A: 2.0  (below cutoff 3.0)
                inhibit(0, 21, 1), // A: 1.0
                endorse(0, 21, 2), // B: 2.0
                inhibit(0, 11, 2), // B: 1.0
                inhibit(0, 12, 1), // A: 0.0
                inhibit(0, 22, 2), // B: 0.0
            ],
        };
        let r = replay(&rec, cfg);
        assert_eq!(r.converged_on, None, "symmetric camps must not converge");
        // And at the deadline, two zero-support rivals are a true Split — no
        // positive leader for force-resolve to pick.
        assert_eq!(r.force_resolved, None, "no positive leader => Split");
    }

    // FINDING (why the harness earns its keep): mid-stream evaluation means
    // mark ORDER matters. If both endorsements land before the cross-inhibitions
    // arrive, the leader latches at 2.0 vs 1.0 and converges early — the later
    // inhibits never get to balance it. The same marks in a different order
    // produce a different outcome, which a one-shot tally would hide.
    #[test]
    fn mark_order_changes_outcome() {
        let endorse_first = Recording {
            question: "q".into(),
            reviewers: vec![],
            marks: vec![
                propose(0, 10, 1),
                propose(0, 20, 2),
                endorse(0, 11, 1), // A=2.0, B=1.0 -> A latches (cutoff 2, margin 1)
                endorse(0, 21, 2),
                inhibit(0, 21, 1),
                inhibit(0, 11, 2),
            ],
        };
        let r = replay(&endorse_first, legacy_config());
        assert_eq!(
            r.converged_on,
            Some(uid(1)),
            "endorse-before-inhibit lets A latch early"
        );
    }

    #[test]
    fn recording_round_trips_through_json() {
        let rec = Recording {
            question: "hooks?".into(),
            reviewers: vec![RecordedReviewer {
                agent_id: uid(10),
                correlation_group: "provider:model".into(),
                reliability_prior: 0.75,
            }],
            marks: vec![propose(0, 10, 1), endorse(12, 11, 1), inhibit(30, 20, 1)],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: Recording = serde_json::from_str(&json).unwrap();
        assert_eq!(back.marks, rec.marks);
        assert_eq!(back.reviewers, rec.reviewers);
        assert_eq!(back.question, rec.question);
    }

    #[test]
    fn sequential_replay_reports_the_stopping_basis() {
        let reviewers = [10, 11, 12, 20]
            .into_iter()
            .map(|agent| RecordedReviewer {
                agent_id: uid(agent),
                correlation_group: format!("independent-{agent}"),
                reliability_prior: 0.75,
            })
            .collect();
        let rec = Recording {
            question: "q".into(),
            reviewers,
            marks: vec![
                propose(0, 10, 1),
                propose(0, 20, 2),
                endorse(0, 11, 1),
                endorse(0, 12, 1),
            ],
        };

        let result = replay(&rec, QuorumConfig::default());
        assert_eq!(result.converged_on, Some(uid(1)));
        assert_eq!(
            result.convergence_basis,
            Some(ConvergenceBasis::EvidenceBound)
        );
        assert_eq!(result.force_resolved, None);
    }
}
