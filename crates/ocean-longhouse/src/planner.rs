//! Pure review planner for sequential Longhouse convergence.
//!
//! The planner allocates review compute over an EXISTING decision field. It
//! never authorizes correctness: [`PlanOutcome`] has no commitment or abort
//! variant, so no planner output can be converted into `Converged` or
//! `Aborted` — the type system enforces the contract split between the
//! commitment plane ([`QuorumEngine::evaluate`]) and the acquisition plane
//! (this module).
//!
//! Purity contract: every input is passed in per call and the planner holds no
//! state. Callers must request a fresh [`QuorumAssessment`] after every
//! accepted stance mutation and re-plan from it; no assessment may be cached
//! across reviews within one orchestration tick. Cap-transition times on the
//! trajectory are ADVISORY scheduling hints only — this planner decides from
//! the assessment's live correlation headroom, never from a projected
//! transition (see `cap_transition_times` docs on [`DecayTrajectory`]).
//!
//! [`QuorumEngine::evaluate`]: crate::quorum::QuorumEngine::evaluate
//! [`DecayTrajectory`]: crate::quorum::DecayTrajectory

use std::collections::HashSet;

use uuid::Uuid;

use crate::evidence::{GroupId, ReviewerCredential};
use crate::quorum::QuorumAssessment;

/// One concrete unit of review work the orchestrator may execute next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    /// Ask an eligible reviewer from an unused correlation group for its first
    /// endorse/inhibit stance on the existing field.
    SampleIndependent { group: GroupId, reviewer: Uuid },
    /// Ask for a rationale, source, test, or falsifiable artifact about the
    /// current leader. The response carries NO quorum weight by itself; only a
    /// later credentialed stance over that material affects the snapshot.
    RequestEvidence { proposal: Uuid },
    /// Ask an eligible reviewer to compare the leader directly with the
    /// runner-up and return one adversarial stance, still subject to its
    /// group's correlation cap.
    ChallengeLeader {
        leader: Uuid,
        runner_up: Uuid,
        reviewer: Uuid,
    },
    /// Re-poll the credential whose existing stance is projected to become
    /// stale before the deadline. This is an UNCERTAIN action, never an
    /// assumed improvement: a withheld response leaves the old stance
    /// decaying, and a flip replaces it under latest-wins — either can degrade
    /// the projected field.
    ReassertAfterDecay {
        proposal: Uuid,
        reviewer: Uuid,
        before_ms: i64,
    },
}

/// Why the planner is handing control back instead of spending review compute.
///
/// Escalation is NON-TERMINAL orchestration control: the caller may add
/// reviewers or budget, wait, or explicitly invoke an early `force_resolve`.
/// It can never itself construct commitment or emit an abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// Fewer than two proposals exist. Sampling more stances against a
    /// one-hypothesis field cannot make the sequential decision identifiable,
    /// so spending review budget there is invalid. Proposal creation belongs
    /// to preparation/convening, not to review planning.
    LoneProposal,
    /// The field is exactly tied and no unused independent group remains to
    /// break it. Only new proposals, reviewers, or an explicit early
    /// resolution can move a saturated tie.
    TiedField,
    /// The remaining review budget cannot fund another admissible review.
    BudgetExhausted,
    /// `now_ms` has reached the deadline. Deadline termination itself belongs
    /// to the convene loop's separate `Timeout` branch; this reason only
    /// surfaces if the planner is (incorrectly) consulted at or past it.
    DeadlineReached,
    /// No roster reviewer can currently perform any admissible action.
    NoEligibleReviewers,
}

/// The planner's verdict for one tick. Deliberately closed over exactly two
/// shapes: keep reviewing, or hand control back. Neither converts into
/// commitment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanOutcome {
    Continue(ReviewAction),
    NeedsEscalation(EscalationReason),
}

/// Pure, stateless review planner. See the module docs for the contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewPlanner;

impl ReviewPlanner {
    /// Choose the next review action for an OPEN sequential field.
    ///
    /// Deterministic: identical inputs produce an identical outcome. Priority
    /// is independence first (an unused group is the only source of genuinely
    /// new evidence mass), then reassertion when the projected field
    /// deteriorates before the deadline, then adversarial challenge, then a
    /// non-weight-bearing evidence request once group diversity is saturated.
    pub fn plan(
        assessment: &QuorumAssessment,
        now_ms: i64,
        deadline_ms: i64,
        budget_remaining: f64,
        roster: &[ReviewerCredential],
    ) -> PlanOutcome {
        let snapshot = assessment.snapshot();
        let ranked = snapshot.ranked();

        if ranked.len() < 2 {
            return PlanOutcome::NeedsEscalation(EscalationReason::LoneProposal);
        }
        if now_ms >= deadline_ms {
            return PlanOutcome::NeedsEscalation(EscalationReason::DeadlineReached);
        }
        if budget_remaining.is_nan() || budget_remaining <= 0.0 {
            return PlanOutcome::NeedsEscalation(EscalationReason::BudgetExhausted);
        }

        let live_authors: HashSet<Uuid> = assessment
            .trajectory()
            .stances()
            .iter()
            .map(|stance| stance.author())
            .collect();

        // Independence first: the only way to ADD evidence mass rather than
        // redistribute it. Unused groups come pre-sorted from the assessment;
        // reviewer choice inside a group is by ascending id for determinism.
        if let Some((group, reviewer)) =
            first_unused_group_candidate(assessment, roster, &live_authors)
        {
            return PlanOutcome::Continue(ReviewAction::SampleIndependent { group, reviewer });
        }

        let unique_leader = snapshot.unique_leader();
        let Some(leader) = unique_leader else {
            // Exactly tied with no unused group left to break it.
            return PlanOutcome::NeedsEscalation(EscalationReason::TiedField);
        };
        let leader = leader.proposal;
        let runner_up = ranked[1].proposal;

        // Projected deterioration: capped evidence holds constant while
        // uncapped evidence decays, so a lead can genuinely erode across the
        // deadline horizon. The signal is projected LOSS OF UNIQUE LEADERSHIP
        // (the leader at the deadline differs or dissolves) — not the ranked
        // gap, which is non-negative by construction and cannot express a
        // flip. Reassert the oldest roster-owned stance supporting the leader.
        // Uncertain action by contract — never assumed to help.
        let projected_leader = assessment
            .trajectory()
            .snapshot_at(deadline_ms)
            .unique_leader()
            .map(|item| item.proposal);
        if projected_leader != Some(leader) {
            if let Some(reviewer) = oldest_leader_supporter(assessment, roster, leader) {
                return PlanOutcome::Continue(ReviewAction::ReassertAfterDecay {
                    proposal: leader,
                    reviewer,
                    before_ms: deadline_ms,
                });
            }
        }

        // Adversarial challenge: a leader exists, diversity is exhausted, and
        // the engine's economics still say more review is worth buying (an
        // open field with a unique leader means neither stopping rule has
        // latched). Prefer reviewers without a live stance, then the group
        // with the most remaining headroom, then ascending id.
        if let Some(reviewer) = challenge_candidate(assessment, roster, &live_authors) {
            return PlanOutcome::Continue(ReviewAction::ChallengeLeader {
                leader,
                runner_up,
                reviewer,
            });
        }

        // Every roster group is saturated: correlated stances can only
        // redistribute the fixed budget. A non-weight-bearing artifact request
        // is the remaining admissible spend.
        if !roster.is_empty() {
            return PlanOutcome::Continue(ReviewAction::RequestEvidence { proposal: leader });
        }

        PlanOutcome::NeedsEscalation(EscalationReason::NoEligibleReviewers)
    }
}

/// First (group, reviewer) pair where the group has no live mass and the
/// reviewer has not yet posted a stance. `unused_groups` is already sorted by
/// `GroupId`, which makes the group choice deterministic.
fn first_unused_group_candidate(
    assessment: &QuorumAssessment,
    roster: &[ReviewerCredential],
    live_authors: &HashSet<Uuid>,
) -> Option<(GroupId, Uuid)> {
    for group in assessment.unused_groups() {
        let GroupId::Registered(group_name) = group else {
            continue;
        };
        let reviewer = roster
            .iter()
            .filter(|credential| {
                credential.correlation_group() == group_name
                    && !live_authors.contains(&credential.agent_id())
            })
            .map(ReviewerCredential::agent_id)
            .min();
        if let Some(reviewer) = reviewer {
            return Some((group.clone(), reviewer));
        }
    }
    None
}

/// Oldest roster-owned endorsing stance on `leader`, by (at_ms, author id).
fn oldest_leader_supporter(
    assessment: &QuorumAssessment,
    roster: &[ReviewerCredential],
    leader: Uuid,
) -> Option<Uuid> {
    let roster_ids: HashSet<Uuid> = roster.iter().map(ReviewerCredential::agent_id).collect();
    assessment
        .trajectory()
        .stances()
        .iter()
        .filter(|stance| {
            stance.proposal() == leader
                && stance.signed_weight() > 0.0
                && roster_ids.contains(&stance.author())
        })
        .min_by_key(|stance| (stance.at_ms(), stance.author()))
        .map(|stance| stance.author())
}

/// Reviewer for an adversarial leader/runner-up comparison: their group must
/// retain headroom (`used < cap`) so the stance can move the field rather than
/// merely redistribute a saturated budget. Prefer reviewers without a live
/// stance, then more group headroom, then ascending id.
fn challenge_candidate(
    assessment: &QuorumAssessment,
    roster: &[ReviewerCredential],
    live_authors: &HashSet<Uuid>,
) -> Option<Uuid> {
    let headroom_of = |group_name: &str| -> Option<f64> {
        assessment
            .correlation_headroom()
            .iter()
            .find(|entry| matches!(&entry.group, GroupId::Registered(name) if name == group_name))
            .map(|entry| entry.cap - entry.used)
    };
    roster
        .iter()
        .filter_map(|credential| {
            let headroom = headroom_of(credential.correlation_group())?;
            (headroom > 0.0).then(|| {
                (
                    live_authors.contains(&credential.agent_id()),
                    // total_cmp semantics via bits are overkill here; headroom
                    // is finite by construction, so an ordered key suffices.
                    std::cmp::Reverse(OrderedHeadroom(headroom)),
                    credential.agent_id(),
                )
            })
        })
        .min()
        .map(|(_, _, reviewer)| reviewer)
}

/// Finite-by-construction f64 ordering key (headroom = cap - used, both
/// validated finite), so a total order is safe.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrderedHeadroom(f64);

impl Eq for OrderedHeadroom {}

impl PartialOrd for OrderedHeadroom {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedHeadroom {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::evidence::SequentialEvidenceConfig;
    use crate::quorum::{QuorumConfig, QuorumEngine, QuorumRule};

    fn uid(n: u8) -> Uuid {
        let mut bytes = [0; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    fn engine(config: SequentialEvidenceConfig, ttl_ms: i64) -> QuorumEngine {
        QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::SequentialEvidence(config),
            mark_ttl_ms: ttl_ms,
            tie_break_seed: 5,
        })
    }

    fn credential(
        config: SequentialEvidenceConfig,
        reviewer: Uuid,
        group: &str,
    ) -> ReviewerCredential {
        ReviewerCredential::with_default_prior(reviewer, group, config).unwrap()
    }

    /// A lone field escalates immediately: no reviewer selection, no budget
    /// spend, no fifth action.
    #[test]
    fn lone_field_escalates_without_reviewer_call() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 60_000);
        let reviewer = credential(config, uid(10), "a");
        eng.register_reviewer(reviewer.clone());
        eng.propose(uid(1), uid(10), 0);

        let assessment = eng.assessment(0).unwrap();
        assert_eq!(
            ReviewPlanner::plan(&assessment, 0, 10_000, 5.0, &[reviewer]),
            PlanOutcome::NeedsEscalation(EscalationReason::LoneProposal)
        );
    }

    #[test]
    fn unused_independent_group_is_selected_first() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 60_000);
        let roster: Vec<ReviewerCredential> = [
            (uid(10), "a"),
            (uid(20), "b"),
            (uid(30), "fresh"),
            (uid(31), "fresh"),
        ]
        .into_iter()
        .map(|(reviewer, group)| credential(config, reviewer, group))
        .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);

        let assessment = eng.assessment(0).unwrap();
        assert_eq!(
            ReviewPlanner::plan(&assessment, 0, 10_000, 5.0, &roster),
            PlanOutcome::Continue(ReviewAction::SampleIndependent {
                group: GroupId::Registered("fresh".into()),
                // Two eligible reviewers in the unused group: ascending id.
                reviewer: uid(30),
            })
        );
    }

    /// An exact replica from an already-saturated group must not change the
    /// plan: capped mass rescales within a fixed budget, so the field — and
    /// therefore the deterministic plan — is unchanged.
    #[test]
    fn post_cap_replica_cannot_change_the_plan() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 60_000);
        let roster: Vec<ReviewerCredential> = [
            (uid(10), "same-model"),
            (uid(11), "same-model"),
            (uid(12), "same-model"),
            (uid(20), "b"),
        ]
        .into_iter()
        .map(|(reviewer, group)| credential(config, reviewer, group))
        .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);
        // Saturate "same-model" beyond its cap with a second endorsement.
        eng.endorse(uid(1), uid(11), None, 0);

        let before = ReviewPlanner::plan(&eng.assessment(0).unwrap(), 0, 10_000, 5.0, &roster);
        // Exact replica from the saturated group.
        eng.endorse(uid(1), uid(12), None, 0);
        let after = ReviewPlanner::plan(&eng.assessment(0).unwrap(), 0, 10_000, 5.0, &roster);
        assert_eq!(before, after);
    }

    /// Diversity exhausted + unique leader -> adversarial challenge from the
    /// group with remaining headroom, preferring reviewers without a stance.
    #[test]
    fn saturated_diversity_selects_leader_challenge() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 60_000);
        let roster: Vec<ReviewerCredential> = [
            (uid(10), "a"),
            (uid(20), "b"),
            (uid(21), "b"),
            (uid(30), "c"),
        ]
        .into_iter()
        .map(|(reviewer, group)| credential(config, reviewer, group))
        .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);
        // Break the tie so a unique leader exists; every group now has mass.
        eng.endorse(uid(1), uid(30), Some(0.5), 0);

        let assessment = eng.assessment(0).unwrap();
        let outcome = ReviewPlanner::plan(&assessment, 0, 10_000, 5.0, &roster);
        // Groups "a" and "b" sit exactly AT cap (one default stance = ln3 =
        // cap): zero headroom, their reviewers can only redistribute a fixed
        // budget. Group "c" (the 0.5-weight tie-breaker) retains headroom, so
        // its reviewer is the only one whose adversarial stance can move the
        // field — even though they already hold a stance.
        assert_eq!(
            outcome,
            PlanOutcome::Continue(ReviewAction::ChallengeLeader {
                leader: uid(1),
                runner_up: uid(2),
                reviewer: uid(30),
            })
        );
    }

    /// Capped runner-up evidence holds constant while the leader's uncapped
    /// evidence decays: the projected gap at the deadline flips, so the
    /// planner re-polls the oldest leader-supporting credential.
    #[test]
    fn projected_deterioration_selects_reassert() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 1_000);
        let roster: Vec<ReviewerCredential> = [
            (uid(10), "leader-a"),
            (uid(30), "leader-b"),
            (uid(20), "rival"),
            (uid(21), "rival"),
        ]
        .into_iter()
        .map(|(reviewer, group)| credential(config, reviewer, group))
        .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        // Runner-up pb: two same-group endorsements saturate "rival", so its
        // capped score holds constant while the group stays over cap. Leader
        // pa: two sub-cap stances (0.9 weight, distinct groups) total more now
        // but DECAY — the lead erodes and flips before the deadline. The 0.9
        // re-endorsements replace the proposer's implicit 1.0 stance
        // (latest-wins) to keep both leader groups strictly under cap.
        eng.propose(uid(2), uid(20), 0);
        eng.endorse(uid(2), uid(21), None, 0);
        eng.propose(uid(1), uid(10), 0);
        eng.endorse(uid(1), uid(10), Some(0.9), 0);
        eng.endorse(uid(1), uid(30), Some(0.9), 0);

        let assessment = eng.assessment(0).unwrap();
        let deadline_ms = 6_000;
        let now_leader = assessment.snapshot().unique_leader().unwrap().proposal;
        let projected_leader = assessment
            .trajectory()
            .snapshot_at(deadline_ms)
            .unique_leader()
            .map(|item| item.proposal);
        assert_eq!(now_leader, uid(1), "pa must lead at capture");
        assert_ne!(
            projected_leader,
            Some(uid(1)),
            "pa's lead must erode away by the deadline, got {projected_leader:?}"
        );

        let outcome = ReviewPlanner::plan(&assessment, 0, deadline_ms, 5.0, &roster);
        assert_eq!(
            outcome,
            PlanOutcome::Continue(ReviewAction::ReassertAfterDecay {
                proposal: uid(1),
                reviewer: uid(10),
                before_ms: deadline_ms,
            })
        );
    }

    /// The dead StopSampling composition: an exact tie whose stopping
    /// economics are satisfied, with no unused group left, must escalate —
    /// never commit, never abort.
    #[test]
    fn saturated_tie_escalates() {
        let config = SequentialEvidenceConfig::new(0.20, 0.75, 1.10, 0.60, 1.0).unwrap();
        let mut eng = engine(config, 60_000);
        let roster: Vec<ReviewerCredential> = [(uid(10), "a"), (uid(20), "b")]
            .into_iter()
            .map(|(reviewer, group)| credential(config, reviewer, group))
            .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);

        let assessment = eng.assessment(0).unwrap();
        assert!(assessment.snapshot().unique_leader().is_none());
        assert_eq!(
            ReviewPlanner::plan(&assessment, 0, 10_000, 5.0, &roster),
            PlanOutcome::NeedsEscalation(EscalationReason::TiedField)
        );
    }

    #[test]
    fn exhausted_budget_escalates() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 60_000);
        let roster = vec![
            credential(config, uid(10), "a"),
            credential(config, uid(20), "b"),
        ];
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);

        let assessment = eng.assessment(0).unwrap();
        assert_eq!(
            ReviewPlanner::plan(&assessment, 0, 10_000, 0.0, &roster),
            PlanOutcome::NeedsEscalation(EscalationReason::BudgetExhausted)
        );
    }

    /// Advisory rule: the plan is derived from the assessment's LIVE headroom,
    /// never from a trajectory's projected cap transitions. A group that a
    /// stale trajectory predicted would free up — but which is saturated in
    /// the fresh assessment — must not be selected.
    #[test]
    fn plan_ignores_advisory_transitions_in_favor_of_live_headroom() {
        let config = SequentialEvidenceConfig::default();
        let mut eng = engine(config, 1_000);
        let roster: Vec<ReviewerCredential> = [
            (uid(10), "same-model"),
            (uid(11), "same-model"),
            (uid(12), "same-model"),
            (uid(20), "b"),
        ]
        .into_iter()
        .map(|(reviewer, group)| credential(config, reviewer, group))
        .collect();
        for reviewer in &roster {
            eng.register_reviewer(reviewer.clone());
        }
        eng.propose(uid(1), uid(10), 0);
        eng.propose(uid(2), uid(20), 0);
        eng.endorse(uid(1), uid(11), None, 0);

        // The stale trajectory predicts "same-model" exits its cap at 1000ms.
        let stale = eng.assessment(0).unwrap();
        let transitions = stale.trajectory().cap_transition_times();
        assert_eq!(transitions.len(), 1);
        assert!((transitions[0].at_ms - 1_000.0).abs() < 1e-9);

        // But at 1500ms a new stance re-saturates the group before planning.
        eng.endorse(uid(1), uid(12), None, 1_500);
        let fresh = eng.assessment(1_500).unwrap();
        let saturated = fresh
            .correlation_headroom()
            .iter()
            .find(|entry| entry.group == GroupId::Registered("same-model".into()))
            .unwrap();
        assert!(saturated.used > saturated.cap);

        // The plan must not sample from the re-saturated group the stale
        // projection advertised; with no unused group and a unique leader it
        // challenges instead.
        let outcome = ReviewPlanner::plan(&fresh, 1_500, 10_000, 5.0, &roster);
        assert!(
            !matches!(
                &outcome,
                PlanOutcome::Continue(ReviewAction::SampleIndependent { group, .. })
                    if *group == GroupId::Registered("same-model".into())
            ),
            "planner trusted an advisory transition over live headroom: {outcome:?}"
        );
        assert!(matches!(
            outcome,
            PlanOutcome::Continue(ReviewAction::ChallengeLeader { .. })
        ));
    }
}
