//! The QuorumEngine — the daemon-computed heart of the longhouse.
//!
//! This module is **pure Rust and contains no LLM logic whatsoever**. LLM
//! agents only ever *produce* marks (proposals, endorsements, inhibitions); the
//! engine *counts* them. That separation is the entire point: convergence is a
//! deterministic, fully-testable arithmetic over a credential-weighted,
//! time-decaying signal field — never a model's say-so.
//!
//! Properties it implements, all from `docs/LONGHOUSE_ORCHESTRATION.md` §5:
//!
//! * **Credential-weighted tallies (C1).** Each real agent contributes *one*
//!   weight across the *whole field* — one live stance per agent, latest wins.
//!   A chatty agent that posts 100 endorsements still counts as one credential,
//!   and an agent that endorses A then switches to B no longer counts toward A:
//!   posting a stance clears that agent's prior stance on any other proposal.
//! * **Cross-inhibition (T2).** `endorse` marks add net weight; `inhibit` marks
//!   subtract it. Two rival proposals actively suppress each other so a
//!   symmetric split cannot silently "win".
//! * **Time-decay (T3).** A mark's effective weight decays toward zero as
//!   `weight * 2^(-Δt / ttl_ms)`, so a stale signal nobody re-asserts fades out.
//! * **Quorum threshold (T4).** Convergence fires when the leader's net weight
//!   crosses a configurable cutoff (a sigmoid response, or a plain net cutoff).
//! * **Deadline / tie handling (T1).** On a forced deadline, a clear leader
//!   converges; a true tie aborts with `Split` (or a seeded tie-break).
//!
//! The engine takes time as an explicit `now_ms` argument on every call, so
//! tests are deterministic and never touch the wall clock.

use std::collections::HashMap;

use ocean_agent_sdk::{AbortReason, ProposalTally};
use uuid::Uuid;

/// How the engine decides a proposal has crossed quorum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QuorumRule {
    /// Converge when the leader's net weight is at least `cutoff` AND it leads
    /// the runner-up by at least `margin`. The margin is what makes
    /// cross-inhibition bite: two near-equal rivals never satisfy it.
    NetWeight { cutoff: f32, margin: f32 },
    /// Converge when a steep logistic response over the leader's net weight
    /// crosses `0.5` confidence — i.e. net weight exceeds `midpoint`, sharpened
    /// by `steepness` (Couzin's quorum response). Still requires `margin` over
    /// the runner-up so a tie can't trip it.
    Sigmoid {
        midpoint: f32,
        steepness: f32,
        margin: f32,
    },
}

impl QuorumRule {
    /// 0.0–1.0 confidence that `leader_net` has crossed quorum, ignoring the
    /// margin requirement (the margin is a separate gate in [`QuorumEngine`]).
    fn confidence(&self, leader_net: f32) -> f32 {
        match *self {
            QuorumRule::NetWeight { cutoff, .. } => {
                if cutoff <= 0.0 {
                    return if leader_net > 0.0 { 1.0 } else { 0.0 };
                }
                (leader_net / cutoff).clamp(0.0, 1.0)
            }
            QuorumRule::Sigmoid {
                midpoint,
                steepness,
                ..
            } => {
                let x = steepness * (leader_net - midpoint);
                1.0 / (1.0 + (-x).exp())
            }
        }
    }

    fn margin(&self) -> f32 {
        match *self {
            QuorumRule::NetWeight { margin, .. } => margin,
            QuorumRule::Sigmoid { margin, .. } => margin,
        }
    }

    /// Does `leader_net` (already net of inhibition) satisfy the raw threshold,
    /// before the margin check?
    fn crosses(&self, leader_net: f32) -> bool {
        match *self {
            QuorumRule::NetWeight { cutoff, .. } => leader_net >= cutoff && leader_net > 0.0,
            QuorumRule::Sigmoid { .. } => self.confidence(leader_net) >= 0.5,
        }
    }
}

/// Per-topic tuning. Defaults are a sane low-quorum, fast-resolving topic.
#[derive(Debug, Clone, Copy)]
pub struct QuorumConfig {
    pub rule: QuorumRule,
    /// Decay horizon for endorse/inhibit marks (ms). A mark loses half its
    /// weight every `mark_ttl_ms`.
    pub mark_ttl_ms: i64,
    /// Seed for the deterministic tie-break on a forced deadline. Same seed +
    /// same field ⇒ same winner, so tie-breaks are reproducible in tests.
    pub tie_break_seed: u64,
}

impl Default for QuorumConfig {
    fn default() -> Self {
        Self {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 0xC0FFEE,
        }
    }
}

/// A single endorse/inhibit signal toward a proposal, as the engine stores it.
/// Proposals themselves don't decay (ttl 0 in the design); only stances do.
#[derive(Debug, Clone, Copy)]
struct Stance {
    /// +1.0 base for endorse, -1.0 base for inhibit, scaled by the mark weight.
    signed_weight: f32,
    at_ms: i64,
}

impl Stance {
    /// Effective weight after exponential decay relative to `now_ms`.
    fn effective(&self, now_ms: i64, ttl_ms: i64) -> f32 {
        if ttl_ms <= 0 {
            return self.signed_weight;
        }
        let dt = (now_ms - self.at_ms).max(0) as f32;
        let decay = 2f32.powf(-dt / ttl_ms as f32);
        self.signed_weight * decay
    }
}

/// One proposal's accumulated state.
#[derive(Debug, Default, Clone)]
struct ProposalState {
    /// Most-recent stance per author *on this proposal*. The credential rule is
    /// stronger than per-proposal, though: an author holds only one live stance
    /// across the entire field (see [`QuorumEngine::clear_author_elsewhere`]),
    /// so the same id never appears in two `ProposalState`s at once. Author is
    /// the agent's stable id.
    stances: HashMap<Uuid, Stance>,
    /// Author who proposed it (also counts as the proposer's implicit endorse).
    proposer: Uuid,
    seq: u64,
}

impl ProposalState {
    fn net_weight(&self, now_ms: i64, ttl_ms: i64) -> f32 {
        self.stances
            .values()
            .map(|s| s.effective(now_ms, ttl_ms))
            .sum()
    }
}

/// The outcome of feeding a mark to the engine: did the topic just resolve?
#[derive(Debug, Clone, PartialEq)]
pub enum QuorumOutcome {
    /// No resolution yet; the topic is still open. Carries the current tallies.
    Pending {
        tallies: Vec<ProposalTally>,
        leader: Option<Uuid>,
        /// 0.0–1.0 how close the leader is to crossing quorum.
        distance_to_quorum: f32,
    },
    /// The leader crossed quorum. The topic is decided.
    Converged {
        decision: Uuid,
        tallies: Vec<ProposalTally>,
    },
}

/// The pure quorum engine for a single topic's blackboard.
///
/// Daemon-owned, LLM-free. Marks go in via [`endorse`](Self::endorse) /
/// [`inhibit`](Self::inhibit) / [`propose`](Self::propose); the engine reports
/// tallies and whether quorum crossed. Time is always passed in.
#[derive(Debug, Clone)]
pub struct QuorumEngine {
    config: QuorumConfig,
    proposals: HashMap<Uuid, ProposalState>,
    /// Monotonic counter so ties break by proposal *order*, deterministically.
    next_seq: u64,
    converged: Option<Uuid>,
}

impl QuorumEngine {
    pub fn new(config: QuorumConfig) -> Self {
        Self {
            config,
            proposals: HashMap::new(),
            next_seq: 0,
            converged: None,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(QuorumConfig::default())
    }

    /// Register a proposal. The proposer implicitly endorses their own proposal
    /// with unit weight (one credential). Re-proposing the same id is a no-op
    /// beyond refreshing the proposer's stance timestamp.
    pub fn propose(&mut self, proposal: Uuid, author: Uuid, now_ms: i64) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.proposals
            .entry(proposal)
            .or_insert_with(|| ProposalState {
                proposer: author,
                seq,
                ..Default::default()
            });
        // One stance per real agent, latest wins across the *whole* field: a
        // proposal is also the proposer's implicit endorse, so clear any prior
        // stance this author held on a different proposal first.
        self.clear_author_elsewhere(author, proposal);
        let entry = self
            .proposals
            .get_mut(&proposal)
            .expect("proposal just inserted");
        // The proposer counts as one endorsing credential.
        entry.stances.insert(
            author,
            Stance {
                signed_weight: 1.0,
                at_ms: now_ms,
            },
        );
    }

    /// Remove this author's stance from every proposal *except* `keep`. Enforces
    /// the "one stance per real agent, latest wins across the field" rule: when
    /// an agent switches its target (endorse A, then endorse/inhibit B), the
    /// stale mark on the old proposal must not keep contributing the agent's
    /// credential weight, or the agent is counted multiple times across the
    /// field and quorum converges on inflated/stale support.
    fn clear_author_elsewhere(&mut self, author: Uuid, keep: Uuid) {
        for (id, state) in self.proposals.iter_mut() {
            if *id != keep {
                state.stances.remove(&author);
            }
        }
    }

    /// Record an endorsement of `proposal` by `author` with `weight` (default
    /// 1.0 when `None`). Overwrites this author's prior stance on this proposal
    /// (credential-weighting: latest stance per agent wins).
    pub fn endorse(&mut self, proposal: Uuid, author: Uuid, weight: Option<f32>, now_ms: i64) {
        let w = weight.unwrap_or(1.0).abs();
        self.set_stance(proposal, author, w, now_ms);
    }

    /// Record an inhibition (cross-inhibition) of `proposal` by `author`. The
    /// weight is subtracted from the proposal's net support.
    pub fn inhibit(&mut self, proposal: Uuid, author: Uuid, weight: Option<f32>, now_ms: i64) {
        let w = weight.unwrap_or(1.0).abs();
        self.set_stance(proposal, author, -w, now_ms);
    }

    fn set_stance(&mut self, proposal: Uuid, author: Uuid, signed_weight: f32, now_ms: i64) {
        let seq = self.next_seq;
        // A stance can arrive for a proposal we haven't seen a `propose` for yet
        // (out-of-order marks). Create a placeholder so the signal still counts.
        let entry = self
            .proposals
            .entry(proposal)
            .or_insert_with(|| ProposalState {
                proposer: author,
                seq,
                ..Default::default()
            });
        if entry.seq == seq {
            self.next_seq += 1;
        }
        // One stance per real agent, latest wins across the *whole* field: drop
        // any prior stance this author held on another proposal so its decayed
        // weight stops counting once the agent moves its support/inhibition.
        self.clear_author_elsewhere(author, proposal);
        let entry = self
            .proposals
            .get_mut(&proposal)
            .expect("proposal just inserted");
        entry.stances.insert(
            author,
            Stance {
                signed_weight,
                at_ms: now_ms,
            },
        );
    }

    /// Number of distinct proposals currently tracked.
    pub fn proposal_count(&self) -> usize {
        self.proposals.len()
    }

    /// Current credential-weighted, decayed tallies, sorted by net weight desc
    /// (ties broken by proposal order so output is deterministic).
    pub fn tallies(&self, now_ms: i64) -> Vec<ProposalTally> {
        let mut out: Vec<(u64, ProposalTally)> = self
            .proposals
            .iter()
            .map(|(id, st)| {
                (
                    st.seq,
                    ProposalTally {
                        proposal: *id,
                        net_weight: st.net_weight(now_ms, self.config.mark_ttl_ms),
                    },
                )
            })
            .collect();
        // Sort by net weight desc, then by insertion seq asc for determinism.
        out.sort_by(|a, b| {
            b.1.net_weight
                .partial_cmp(&a.1.net_weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        out.into_iter().map(|(_, t)| t).collect()
    }

    /// The current front-runner (highest net weight), if any proposal has
    /// strictly positive net weight.
    pub fn leader(&self, now_ms: i64) -> Option<Uuid> {
        let tallies = self.tallies(now_ms);
        tallies
            .into_iter()
            .find(|t| t.net_weight > 0.0)
            .map(|t| t.proposal)
    }

    /// Evaluate the current field. Returns `Converged` once (and stays
    /// converged), else `Pending` with live tallies + distance to quorum.
    pub fn evaluate(&mut self, now_ms: i64) -> QuorumOutcome {
        let tallies = self.tallies(now_ms);

        if let Some(decision) = self.converged {
            return QuorumOutcome::Converged { decision, tallies };
        }

        let leader_net = tallies.first().map(|t| t.net_weight).unwrap_or(0.0);
        let runner_up = tallies.get(1).map(|t| t.net_weight).unwrap_or(0.0);
        let leader_id = tallies.first().map(|t| t.proposal);

        let confidence = self.config.rule.confidence(leader_net);
        let margin_ok = (leader_net - runner_up) >= self.config.rule.margin();
        let crosses = self.config.rule.crosses(leader_net) && margin_ok;

        if crosses {
            if let Some(decision) = leader_id {
                self.converged = Some(decision);
                return QuorumOutcome::Converged { decision, tallies };
            }
        }

        QuorumOutcome::Pending {
            leader: leader_id.filter(|_| leader_net > 0.0),
            distance_to_quorum: confidence.clamp(0.0, 1.0),
            tallies,
        }
    }

    /// Has this topic already converged?
    pub fn is_converged(&self) -> bool {
        self.converged.is_some()
    }

    /// The author that originally proposed `proposal`, if tracked. Used by the
    /// convening flow to bind the firekeeper title to the winning proposer.
    pub fn proposer_of(&self, proposal: Uuid) -> Option<Uuid> {
        self.proposals.get(&proposal).map(|p| p.proposer)
    }

    /// Force resolution at a deadline (or budget ceiling). A clear leader
    /// (positive net weight, satisfying the margin) converges. A true tie —
    /// two+ leaders within the margin — is broken deterministically by the
    /// configured seed when `tie_break` is true, else it aborts with `Split`.
    ///
    /// Returns either the converged proposal id, or an [`AbortReason`].
    pub fn force_resolve(
        &mut self,
        now_ms: i64,
        reason: AbortReason,
        tie_break: bool,
    ) -> Result<Uuid, AbortReason> {
        if let Some(decision) = self.converged {
            return Ok(decision);
        }
        let tallies = self.tallies(now_ms);
        let Some(top) = tallies.first().cloned() else {
            // No proposals at all.
            return Err(reason);
        };
        if top.net_weight <= 0.0 {
            // Nothing has positive support.
            return Err(if reason == AbortReason::Timeout {
                AbortReason::Split
            } else {
                reason
            });
        }
        let runner_up = tallies.get(1).map(|t| t.net_weight).unwrap_or(0.0);
        let is_tie = (top.net_weight - runner_up).abs() < self.config.rule.margin();

        if !is_tie {
            self.converged = Some(top.proposal);
            return Ok(top.proposal);
        }

        // A genuine tie among the leaders.
        if !tie_break {
            return Err(AbortReason::Split);
        }

        // Deterministic seeded tie-break: pick among all proposals tied with the
        // top within the margin, choosing by a hash of (seed, proposal bytes).
        let leaders: Vec<Uuid> = tallies
            .iter()
            .filter(|t| (top.net_weight - t.net_weight).abs() < self.config.rule.margin())
            .map(|t| t.proposal)
            .collect();
        let winner = seeded_pick(&leaders, self.config.tie_break_seed);
        self.converged = Some(winner);
        Ok(winner)
    }
}

/// Deterministic pick from a non-empty slice using a tiny splitmix64-style hash
/// of the seed mixed with each candidate's bytes. Stable across runs/platforms.
fn seeded_pick(candidates: &[Uuid], seed: u64) -> Uuid {
    debug_assert!(!candidates.is_empty());
    let mut best = candidates[0];
    let mut best_score = u64::MAX;
    for c in candidates {
        let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
        for b in c.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01B3); // FNV-ish prime
            h ^= h >> 29;
        }
        if h < best_score {
            best_score = h;
            best = *c;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    // A proposal that accrues endorsements climbs to convergence.
    #[test]
    fn proposal_climbs_to_convergence() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 3.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let p = uid(1);
        let (a, b, c) = (uid(10), uid(11), uid(12));
        let t0 = 0;

        eng.propose(p, a, t0); // proposer endorses: net 1.0
        assert!(matches!(eng.evaluate(t0), QuorumOutcome::Pending { .. }));

        eng.endorse(p, b, None, t0); // net 2.0
        assert!(!eng.is_converged());

        eng.endorse(p, c, None, t0); // net 3.0 -> crosses cutoff, margin (no rival) ok
        match eng.evaluate(t0) {
            QuorumOutcome::Converged { decision, .. } => assert_eq!(decision, p),
            other => panic!("expected convergence, got {other:?}"),
        }
        // Stays converged.
        assert!(eng.is_converged());
        assert!(matches!(
            eng.evaluate(t0 + 1),
            QuorumOutcome::Converged { .. }
        ));
    }

    // Cross-inhibition keeps two rivals from converging: each suppresses the
    // other so neither clears the margin.
    #[test]
    fn cross_inhibition_blocks_two_rivals() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let (pa, pb) = (uid(1), uid(2));
        let (a1, a2, b1, b2) = (uid(10), uid(11), uid(20), uid(21));
        let t = 0;

        // Two symmetric camps endorse their own and inhibit the rival.
        eng.propose(pa, a1, t);
        eng.propose(pb, b1, t);
        eng.endorse(pa, a2, None, t); // A net = 2.0 (a1 + a2)
        eng.endorse(pb, b2, None, t); // B net = 2.0 (b1 + b2)

        // Cross-inhibition: each camp inhibits the rival.
        eng.inhibit(pb, a1, None, t); // B net = 2.0 - 1.0 = 1.0
        eng.inhibit(pb, a2, None, t); // B net = 2.0 - 2.0 = 0.0
        eng.inhibit(pa, b1, None, t); // A net = 2.0 - 1.0 = 1.0
        eng.inhibit(pa, b2, None, t); // A net = 2.0 - 2.0 = 0.0

        match eng.evaluate(t) {
            QuorumOutcome::Pending { .. } => {} // neither side wins
            other => panic!("rivals should not converge: {other:?}"),
        }
        assert!(!eng.is_converged());
    }

    // Decay reduces a stale lead: an old endorsement fades, dropping the leader
    // back below quorum over time.
    #[test]
    fn decay_reduces_stale_lead() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.5,
                margin: 0.5,
            },
            mark_ttl_ms: 1_000, // half-life 1s
            tie_break_seed: 1,
        });
        let p = uid(1);
        let (a, b, c) = (uid(10), uid(11), uid(12));
        let t0 = 0;

        eng.propose(p, a, t0);
        eng.endorse(p, b, None, t0);
        eng.endorse(p, c, None, t0);
        // Fresh: net = 3.0 >= cutoff 2.5 -> converged.
        assert!(matches!(eng.evaluate(t0), QuorumOutcome::Converged { .. }));

        // But evaluate a *fresh* engine at a later time to see decay (the first
        // engine latched converged, which is correct — once decided, decided).
        let mut eng2 = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.5,
                margin: 0.5,
            },
            mark_ttl_ms: 1_000,
            tie_break_seed: 1,
        });
        eng2.propose(p, a, t0);
        eng2.endorse(p, b, None, t0);
        eng2.endorse(p, c, None, t0);
        // After 2 half-lives (2s): each 1.0 stance -> ~0.25, net ~0.75 < cutoff.
        let later = t0 + 2_000;
        match eng2.evaluate(later) {
            QuorumOutcome::Pending {
                distance_to_quorum, ..
            } => {
                let net = eng2.tallies(later)[0].net_weight;
                assert!(net < 1.0, "stale net should have decayed, got {net}");
                assert!(distance_to_quorum < 0.5);
            }
            other => panic!("decayed lead should be pending, got {other:?}"),
        }
    }

    // A tie at the deadline aborts with Split when tie-break is off, and
    // resolves deterministically when tie-break is on.
    #[test]
    fn tie_aborts_then_tiebreaks() {
        let cfg = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 1.5,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 42,
        };
        let (pa, pb) = (uid(1), uid(2));
        let (a, b) = (uid(10), uid(20));

        let mut eng = QuorumEngine::new(cfg);
        eng.propose(pa, a, 0); // A net 1.0
        eng.propose(pb, b, 0); // B net 1.0 -> exact tie, within margin 1.0

        // No clear leader during normal evaluation.
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));

        // Deadline, no tie-break -> Split.
        let mut eng_split = eng.clone();
        assert_eq!(
            eng_split.force_resolve(0, AbortReason::Timeout, false),
            Err(AbortReason::Split)
        );

        // Deadline, with tie-break -> deterministic winner, repeatable.
        let mut eng_tb1 = eng.clone();
        let mut eng_tb2 = eng.clone();
        let w1 = eng_tb1
            .force_resolve(0, AbortReason::Timeout, true)
            .unwrap();
        let w2 = eng_tb2
            .force_resolve(0, AbortReason::Timeout, true)
            .unwrap();
        assert_eq!(w1, w2, "seeded tie-break must be deterministic");
        assert!(w1 == pa || w1 == pb);
    }

    // A chatty agent posting many endorsements still counts as ONE credential.
    #[test]
    fn credential_weighting_dedupes_chatty_agent() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 5.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let p = uid(1);
        let spammer = uid(99);
        let t = 0;
        eng.propose(p, uid(1), t); // proposer credential = 1.0

        // Spammer floods 100 endorsements; only its latest stance counts.
        for _ in 0..100 {
            eng.endorse(p, spammer, None, t);
        }
        let net = eng.tallies(t)[0].net_weight;
        assert!(
            (net - 2.0).abs() < 1e-3,
            "proposer(1) + one spammer credential(1) = 2.0, got {net}"
        );
        assert!(!eng.is_converged(), "two credentials < cutoff 5.0");
    }

    // Regression for the PR #33 review: an agent that endorses proposal A and
    // then later endorses/inhibits proposal B must be counted only ONCE across
    // the field — its prior stance on A must be cleared, not left contributing
    // its credential weight to both proposals. "One stance per real agent,
    // latest wins" applies field-wide, not per-proposal.
    #[test]
    fn agent_switching_proposals_drops_prior_stance() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let (pa, pb) = (uid(1), uid(2));
        // Distinct proposers so A and B each have a standing credential that is
        // NOT the switching worker, isolating the worker's single moving stance.
        let (pa_author, pb_author) = (uid(10), uid(20));
        let worker = uid(50);
        let t = 0;

        eng.propose(pa, pa_author, t); // A net = 1.0 (pa_author)
        eng.propose(pb, pb_author, t); // B net = 1.0 (pb_author)

        // Round 2: the worker endorses A. A should now be 2.0, B still 1.0.
        eng.endorse(pa, worker, None, t);
        {
            let tallies = eng.tallies(t);
            let net_a = tallies.iter().find(|x| x.proposal == pa).unwrap().net_weight;
            let net_b = tallies.iter().find(|x| x.proposal == pb).unwrap().net_weight;
            assert!((net_a - 2.0).abs() < 1e-3, "A should be 2.0 after worker endorse, got {net_a}");
            assert!((net_b - 1.0).abs() < 1e-3, "B should still be 1.0, got {net_b}");
        }

        // Later round: the same worker switches its support to B (endorse B).
        // The BUG: without clearing, the worker's stance on A would remain, so A
        // stays at 2.0 AND B climbs to 2.0 — the worker is credential-counted on
        // both proposals at once. Correct behaviour: A drops back to 1.0 (only
        // pa_author left) and B rises to 2.0 (pb_author + worker).
        eng.endorse(pb, worker, None, t);
        let tallies = eng.tallies(t);
        let net_a = tallies.iter().find(|x| x.proposal == pa).unwrap().net_weight;
        let net_b = tallies.iter().find(|x| x.proposal == pb).unwrap().net_weight;
        assert!(
            (net_a - 1.0).abs() < 1e-3,
            "A must drop to 1.0 once the worker leaves it; got {net_a} (double-counting bug)"
        );
        assert!(
            (net_b - 2.0).abs() < 1e-3,
            "B should be 2.0 (pb_author + worker), got {net_b}"
        );

        // Same must hold when the switch is an *inhibit* on the new proposal:
        // the worker's prior endorse on B must go, leaving B at pb_author - worker.
        eng.inhibit(pa, worker, None, t); // worker now inhibits A
        let tallies = eng.tallies(t);
        let net_a = tallies.iter().find(|x| x.proposal == pa).unwrap().net_weight;
        let net_b = tallies.iter().find(|x| x.proposal == pb).unwrap().net_weight;
        assert!(
            (net_a - 0.0).abs() < 1e-3,
            "A should be 0.0 (pa_author 1.0 - worker inhibit 1.0), got {net_a}"
        );
        assert!(
            (net_b - 1.0).abs() < 1e-3,
            "B must drop back to 1.0 once the worker leaves it; got {net_b} (double-counting bug)"
        );
    }

    // A clear leader at the deadline converges even without crossing quorum
    // mid-flight (the deadline forces resolution on the front-runner).
    #[test]
    fn deadline_converges_clear_leader() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 10.0, // unreachably high so it never auto-converges
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let (pa, pb) = (uid(1), uid(2));
        eng.propose(pa, uid(10), 0);
        eng.endorse(pa, uid(11), None, 0);
        eng.endorse(pa, uid(12), None, 0); // A net = 3.0
        eng.propose(pb, uid(20), 0); // B net = 1.0 -> A leads by 2.0 >= margin

        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));
        let winner = eng.force_resolve(0, AbortReason::Timeout, true).unwrap();
        assert_eq!(winner, pa);
    }

    #[test]
    fn sigmoid_rule_crosses_at_midpoint() {
        let mut eng = QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::Sigmoid {
                midpoint: 2.0,
                steepness: 4.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        });
        let p = uid(1);
        eng.propose(p, uid(10), 0); // net 1.0, below midpoint
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));
        eng.endorse(p, uid(11), None, 0); // net 2.0 == midpoint -> confidence 0.5
        eng.endorse(p, uid(12), None, 0); // net 3.0 > midpoint -> crosses
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Converged { .. }));
    }
}
