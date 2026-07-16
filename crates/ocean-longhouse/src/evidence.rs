//! Correlation-aware sequential evidence for Longhouse convergence.
//!
//! The quorum engine receives only observable stances and daemon-owned reviewer
//! metadata. A reviewer reliability prior is converted to a log-likelihood
//! ratio (LLR), correlated reviewers share a bounded evidence budget, and the
//! resulting proposal scores are normalized into a posterior distribution.
//! No model-reported confidence enters this calculation.
//!
//! Stopping has two explicit, auditable paths:
//!
//! * [`ConvergenceBasis::EvidenceBound`] — the posterior error probability is
//!   at or below the configured target; or
//! * [`ConvergenceBasis::CostBound`] — the expected value of perfect additional
//!   information is no greater than the configured cost of another query.
//!
//! The latter is deliberately conservative: perfect information is an upper
//! bound on what one more real reviewer call can provide. If even that upper
//! bound is not worth its cost, continuing cannot be economically justified by
//! the configured loss model.
//!
//! # Canonical evaluator seam
//!
//! [`evaluate_field_full`] is the ONE evaluator for the sequential field. Every
//! surface that needs the field at a time `t` — the live engine, the decay
//! trajectory, replay — must reach it through the same path: decay each stance
//! with `Stance::effective` (an `f32` function), apply the existing `as f64`
//! cast per contribution, and hand the result here. Do not reimplement the
//! decay exponential in either precision; the trajectory/engine round-trip
//! invariant requires bit-identical evaluation, not merely equivalent math.
//!
//! Inputs are canonicalized internally (proposals by sequence, contributions by
//! `(proposal_seq, proposal, author)`) so identical evidence produces an
//! identical snapshot regardless of caller iteration order. Floating-point
//! accumulation is order-sensitive, and callers feed us `HashMap` iteration
//! order; without this, event-sourced replay could rank a near-tied field
//! differently across process restarts.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use uuid::Uuid;

/// LLR contributed by the default 0.75 reliability prior: `ln(0.75 / 0.25)`.
/// Using the same value as the default group cap means exact replicas together
/// have precisely the evidence budget of one default independent reviewer.
const DEFAULT_REVIEWER_LLR: f64 = 1.098_612_288_668_109_8;

/// Why the daemon latched a proposal as the Longhouse decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergenceBasis {
    /// Compatibility rule: a raw net-weight cutoff and margin were crossed.
    NetWeight,
    /// The posterior error probability reached the configured target.
    EvidenceBound,
    /// Perfect additional information was worth no more than another query.
    CostBound,
    /// Compatibility rule: a clear leader was selected at the hard deadline.
    ForcedDeadline,
}

impl ConvergenceBasis {
    /// Stable machine-readable label for logs and response bodies.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetWeight => "net_weight",
            Self::EvidenceBound => "evidence_bound",
            Self::CostBound => "cost_bound",
            Self::ForcedDeadline => "forced_deadline",
        }
    }
}

/// Invalid public configuration for the evidence engine.
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceConfigError {
    /// Reviewer groups are durable correlation identities and cannot be blank.
    EmptyCorrelationGroup,
    /// A numeric parameter was non-finite or outside its documented interval.
    InvalidParameter {
        name: &'static str,
        requirement: &'static str,
        value: f64,
    },
}

impl fmt::Display for EvidenceConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCorrelationGroup => write!(f, "correlation group cannot be empty"),
            Self::InvalidParameter {
                name,
                requirement,
                value,
            } => write!(f, "{name} must be {requirement}, got {value}"),
        }
    }
}

impl Error for EvidenceConfigError {}

/// Validated tuning for correlation-aware sequential evidence.
///
/// All monetary/utility values are expressed in the same caller-defined unit.
/// For example, `decision_loss = 1.0` and `query_cost = 0.02` says one more
/// reviewer call costs two percent of making the wrong decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SequentialEvidenceConfig {
    target_error: f64,
    default_reliability: f64,
    correlation_cap: f64,
    query_cost: f64,
    decision_loss: f64,
}

impl SequentialEvidenceConfig {
    /// Construct a validated evidence policy.
    ///
    /// * `target_error` must be in `(0, 0.5)`.
    /// * `default_reliability` must be in `(0.5, 1.0)`.
    /// * `correlation_cap` is a positive LLR budget shared by a group.
    /// * `query_cost` is finite and non-negative.
    /// * `decision_loss` is finite and positive.
    pub fn new(
        target_error: f64,
        default_reliability: f64,
        correlation_cap: f64,
        query_cost: f64,
        decision_loss: f64,
    ) -> Result<Self, EvidenceConfigError> {
        validate_range("target_error", target_error, 0.0, 0.5)?;
        validate_range("default_reliability", default_reliability, 0.5, 1.0)?;
        validate_positive("correlation_cap", correlation_cap)?;
        if !query_cost.is_finite() || query_cost < 0.0 {
            return Err(EvidenceConfigError::InvalidParameter {
                name: "query_cost",
                requirement: "finite and >= 0",
                value: query_cost,
            });
        }
        validate_positive("decision_loss", decision_loss)?;
        Ok(Self {
            target_error,
            default_reliability,
            correlation_cap,
            query_cost,
            decision_loss,
        })
    }

    pub fn target_error(self) -> f64 {
        self.target_error
    }

    pub fn default_reliability(self) -> f64 {
        self.default_reliability
    }

    pub fn correlation_cap(self) -> f64 {
        self.correlation_cap
    }

    pub fn query_cost(self) -> f64 {
        self.query_cost
    }

    pub fn decision_loss(self) -> f64 {
        self.decision_loss
    }
}

impl Default for SequentialEvidenceConfig {
    fn default() -> Self {
        // A 0.75 reliability prior contributes ln(3) LLR. The matching cap
        // grants every correlated family exactly one default reviewer's budget.
        Self {
            target_error: 0.20,
            default_reliability: 0.75,
            correlation_cap: DEFAULT_REVIEWER_LLR,
            query_cost: 0.02,
            decision_loss: 1.0,
        }
    }
}

/// Daemon-owned evidence credential for one seated reviewer.
///
/// `reliability_prior` is an externally supplied prior, not a confidence value
/// reported by the model. Reviewers with the same `correlation_group` share one
/// capped evidence budget so replicas cannot manufacture independence.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewerCredential {
    agent_id: Uuid,
    correlation_group: String,
    reliability_prior: f64,
}

impl ReviewerCredential {
    pub fn new(
        agent_id: Uuid,
        correlation_group: impl Into<String>,
        reliability_prior: f64,
    ) -> Result<Self, EvidenceConfigError> {
        let correlation_group = correlation_group.into();
        let correlation_group = correlation_group.trim();
        if correlation_group.is_empty() {
            return Err(EvidenceConfigError::EmptyCorrelationGroup);
        }
        validate_range("reliability_prior", reliability_prior, 0.5, 1.0)?;
        Ok(Self {
            agent_id,
            correlation_group: correlation_group.to_owned(),
            reliability_prior,
        })
    }

    /// Construct a credential from the policy's default reliability prior.
    pub fn with_default_prior(
        agent_id: Uuid,
        correlation_group: impl Into<String>,
        config: SequentialEvidenceConfig,
    ) -> Result<Self, EvidenceConfigError> {
        Self::new(agent_id, correlation_group, config.default_reliability)
    }

    pub fn agent_id(&self) -> Uuid {
        self.agent_id
    }

    pub fn correlation_group(&self) -> &str {
        &self.correlation_group
    }

    pub fn reliability_prior(&self) -> f64 {
        self.reliability_prior
    }
}

/// One proposal's normalized state in an [`EvidenceSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProposalEvidence {
    pub proposal: Uuid,
    /// Correlation-capped accumulated log evidence.
    pub log_evidence: f64,
    /// Softmax-normalized probability under the configured reviewer priors.
    pub posterior: f64,
}

/// Auditable state used by sequential stopping.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceSnapshot {
    ranked: Vec<ProposalEvidence>,
    convergence_basis: Option<ConvergenceBasis>,
    posterior_error: f64,
    evpi_upper_bound: f64,
    progress: f64,
}

impl EvidenceSnapshot {
    /// Proposals ranked by log evidence, with stable proposal-order tie breaks.
    pub fn ranked(&self) -> &[ProposalEvidence] {
        &self.ranked
    }

    pub fn leader(&self) -> Option<ProposalEvidence> {
        self.ranked.first().copied()
    }

    /// The front-runner only when its evidence strictly exceeds the runner-up.
    pub fn unique_leader(&self) -> Option<ProposalEvidence> {
        let leader = self.ranked.first().copied()?;
        let runner = self.ranked.get(1)?;
        (leader.log_evidence > runner.log_evidence).then_some(leader)
    }

    pub fn convergence_basis(&self) -> Option<ConvergenceBasis> {
        self.convergence_basis
    }

    /// Posterior probability that the current leader is wrong.
    pub fn posterior_error(&self) -> f64 {
        self.posterior_error
    }

    /// Conservative value-of-information ceiling: `decision_loss * error`.
    pub fn evpi_upper_bound(&self) -> f64 {
        self.evpi_upper_bound
    }

    /// Normalized progress from an uninformative uniform field to the configured
    /// evidence threshold. This is explanatory UI state, not a second gate.
    pub fn progress(&self) -> f64 {
        self.progress
    }
}

/// Effective stance passed from the time-decaying quorum field into the pure
/// evidence calculation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EvidenceContribution {
    pub proposal: Uuid,
    pub author: Uuid,
    pub signed_weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GroupKey<'a> {
    Registered(&'a str),
    /// Old recordings without reviewer metadata remain replayable. Treating
    /// each missing author as independent is explicit backward compatibility,
    /// while live convening always registers real provider/model groups.
    Independent(Uuid),
}

impl GroupKey<'_> {
    fn to_group_id(self) -> GroupId {
        match self {
            Self::Registered(group) => GroupId::Registered(group.to_owned()),
            Self::Independent(author) => GroupId::Independent(author),
        }
    }
}

/// Owned, public identity of one correlation budget. The internal borrowed
/// [`GroupKey`] never crosses the module boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupId {
    /// A registered correlation group (provider/model family).
    Registered(String),
    /// A synthetic per-author group for contributions without a credential
    /// (backward compatibility with old recordings).
    Independent(Uuid),
}

/// One correlation group's evidence budget at the evaluated instant.
///
/// `used` is the RAW decayed LLR mass the group has contributed — deliberately
/// not clamped to `cap`, so a planner can see saturation depth. `used >= cap`
/// means the group's total evidence budget is saturated; additional correlated
/// mass is rescaled within that fixed budget, and exact replicas cannot
/// increase total group influence. A new or flipped stance from a saturated
/// group can still REDISTRIBUTE that budget among proposals and move the
/// field. Because every stance decays, `used` is time-dependent: a group over
/// its cap now can fall back under it later.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupHeadroom {
    pub group: GroupId,
    pub used: f64,
    pub cap: f64,
}

/// The full result of one field evaluation: the auditable stopping snapshot
/// plus the per-group budget state the snapshot was computed from. Headroom is
/// the same `group_mass` the cap scaling used — computed once, never re-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldEvaluation {
    pub snapshot: EvidenceSnapshot,
    /// Every correlation group, sorted by [`GroupId`]: groups with live mass
    /// plus registered-but-silent groups synthesized at `used = 0.0` so an
    /// unused-group inventory is derivable without consulting the engine.
    pub headroom: Vec<GroupHeadroom>,
}

/// Evaluate one field under a validated evidence policy.
///
/// Thin compatibility wrapper over [`evaluate_field_full`] for callers that
/// only need the stopping snapshot.
pub(crate) fn evaluate_field(
    config: SequentialEvidenceConfig,
    proposals: &[(Uuid, u64)],
    contributions: &[EvidenceContribution],
    reviewers: &HashMap<Uuid, ReviewerCredential>,
) -> EvidenceSnapshot {
    evaluate_field_full(config, proposals, contributions, reviewers).snapshot
}

/// Evaluate one field and report per-group budget state alongside the
/// snapshot. This is the canonical evaluator (see the module docs): inputs are
/// canonicalized so identical evidence yields a bit-identical result
/// regardless of caller iteration order.
pub(crate) fn evaluate_field_full(
    config: SequentialEvidenceConfig,
    proposals: &[(Uuid, u64)],
    contributions: &[EvidenceContribution],
    reviewers: &HashMap<Uuid, ReviewerCredential>,
) -> FieldEvaluation {
    if proposals.is_empty() {
        return FieldEvaluation {
            snapshot: EvidenceSnapshot {
                ranked: Vec::new(),
                convergence_basis: None,
                posterior_error: 1.0,
                evpi_upper_bound: config.decision_loss,
                progress: 0.0,
            },
            headroom: silent_headroom(config, reviewers, &HashMap::new()),
        };
    }

    // Canonical order: proposals by sequence, contributions by
    // (proposal_seq, proposal, author). Float accumulation below follows this
    // order, which is what makes the evaluation reproducible across HashMap
    // layouts and process restarts.
    let seq_of: HashMap<Uuid, u64> = proposals.iter().copied().collect();
    let mut proposals: Vec<(Uuid, u64)> = proposals.to_vec();
    proposals.sort_by_key(|(id, seq)| (*seq, *id));
    let mut contributions: Vec<EvidenceContribution> = contributions.to_vec();
    contributions.sort_by_key(|c| {
        (
            seq_of.get(&c.proposal).copied().unwrap_or(u64::MAX),
            c.proposal,
            c.author,
        )
    });

    let mut group_mass: HashMap<GroupKey<'_>, f64> = HashMap::new();
    for contribution in &contributions {
        let (group, reliability) = reviewer_profile(config, reviewers, contribution.author);
        let raw = contribution.signed_weight * log_odds(reliability);
        *group_mass.entry(group).or_default() += raw.abs();
    }

    let mut scores: HashMap<Uuid, f64> = proposals.iter().map(|(id, _)| (*id, 0.0)).collect();
    for contribution in &contributions {
        let (group, reliability) = reviewer_profile(config, reviewers, contribution.author);
        let raw = contribution.signed_weight * log_odds(reliability);
        let total = group_mass.get(&group).copied().unwrap_or_default();
        let scale = if total > config.correlation_cap {
            config.correlation_cap / total
        } else {
            1.0
        };
        *scores.entry(contribution.proposal).or_default() += raw * scale;
    }

    let max_score = scores.values().copied().fold(f64::NEG_INFINITY, f64::max);
    // Sum in canonical proposal order, not HashMap order: the normalizer is a
    // float accumulation too.
    let normalizer: f64 = proposals
        .iter()
        .map(|(id, _)| (scores.get(id).copied().unwrap_or_default() - max_score).exp())
        .sum();

    let mut ranked: Vec<(u64, ProposalEvidence)> = proposals
        .iter()
        .map(|(proposal, seq)| {
            let score = scores.get(proposal).copied().unwrap_or_default();
            (
                *seq,
                ProposalEvidence {
                    proposal: *proposal,
                    log_evidence: score,
                    posterior: (score - max_score).exp() / normalizer,
                },
            )
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.log_evidence
            .total_cmp(&a.1.log_evidence)
            .then(a.0.cmp(&b.0))
    });
    let ranked: Vec<ProposalEvidence> = ranked.into_iter().map(|(_, item)| item).collect();

    let leader_posterior = ranked.first().map(|item| item.posterior).unwrap_or(0.0);
    let posterior_error = 1.0 - leader_posterior;
    let evpi_upper_bound = config.decision_loss * posterior_error;
    let unique_leader = ranked
        .get(1)
        .map(|runner| ranked[0].log_evidence > runner.log_evidence)
        .unwrap_or(false);
    let can_stop = ranked.len() >= 2 && unique_leader;
    let convergence_basis = if can_stop && posterior_error <= config.target_error {
        Some(ConvergenceBasis::EvidenceBound)
    } else if can_stop && evpi_upper_bound <= config.query_cost {
        Some(ConvergenceBasis::CostBound)
    } else {
        None
    };

    let uniform = 1.0 / ranked.len() as f64;
    let target = 1.0 - config.target_error;
    let progress = if target > uniform {
        ((leader_posterior - uniform) / (target - uniform)).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let mut headroom: Vec<GroupHeadroom> = group_mass
        .iter()
        .map(|(key, used)| GroupHeadroom {
            group: key.to_group_id(),
            used: *used,
            cap: config.correlation_cap,
        })
        .collect();
    headroom.extend(silent_headroom(config, reviewers, &group_mass));
    headroom.sort_by(|a, b| a.group.cmp(&b.group));

    FieldEvaluation {
        snapshot: EvidenceSnapshot {
            ranked,
            convergence_basis,
            posterior_error,
            evpi_upper_bound,
            progress,
        },
        headroom,
    }
}

/// Headroom entries for registered correlation groups with no live mass.
/// `group_mass` only contains groups that contributed, so the unused-group
/// inventory must be synthesized from the reviewer registry.
fn silent_headroom(
    config: SequentialEvidenceConfig,
    reviewers: &HashMap<Uuid, ReviewerCredential>,
    group_mass: &HashMap<GroupKey<'_>, f64>,
) -> Vec<GroupHeadroom> {
    let silent: std::collections::BTreeSet<&str> = reviewers
        .values()
        .map(|credential| credential.correlation_group())
        .filter(|group| !group_mass.contains_key(&GroupKey::Registered(group)))
        .collect();
    silent
        .into_iter()
        .map(|group| GroupHeadroom {
            group: GroupId::Registered(group.to_owned()),
            used: 0.0,
            cap: config.correlation_cap,
        })
        .collect()
}

fn reviewer_profile<'a>(
    config: SequentialEvidenceConfig,
    reviewers: &'a HashMap<Uuid, ReviewerCredential>,
    author: Uuid,
) -> (GroupKey<'a>, f64) {
    match reviewers.get(&author) {
        Some(credential) => (
            GroupKey::Registered(credential.correlation_group()),
            credential.reliability_prior(),
        ),
        None => (GroupKey::Independent(author), config.default_reliability),
    }
}

fn log_odds(probability: f64) -> f64 {
    (probability / (1.0 - probability)).ln()
}

fn validate_range(
    name: &'static str,
    value: f64,
    lower_exclusive: f64,
    upper_exclusive: f64,
) -> Result<(), EvidenceConfigError> {
    if value.is_finite() && value > lower_exclusive && value < upper_exclusive {
        Ok(())
    } else {
        Err(EvidenceConfigError::InvalidParameter {
            name,
            requirement: "finite and inside the documented open interval",
            value,
        })
    }
}

fn validate_positive(name: &'static str, value: f64) -> Result<(), EvidenceConfigError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(EvidenceConfigError::InvalidParameter {
            name,
            requirement: "finite and > 0",
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(n: u8) -> Uuid {
        let mut bytes = [0; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    fn proposals() -> Vec<(Uuid, u64)> {
        vec![(uid(1), 0), (uid(2), 1)]
    }

    fn contribution(proposal: u8, author: u8) -> EvidenceContribution {
        EvidenceContribution {
            proposal: uid(proposal),
            author: uid(author),
            signed_weight: 1.0,
        }
    }

    #[test]
    fn duplicate_reviewers_share_one_group_budget() {
        let config = SequentialEvidenceConfig::default();
        let mut reviewers = HashMap::new();
        for author in [10, 11] {
            reviewers.insert(
                uid(author),
                ReviewerCredential::with_default_prior(uid(author), "same-model", config).unwrap(),
            );
        }
        reviewers.insert(
            uid(20),
            ReviewerCredential::with_default_prior(uid(20), "independent", config).unwrap(),
        );

        let one = evaluate_field(
            config,
            &proposals(),
            &[contribution(1, 10), contribution(2, 20)],
            &reviewers,
        );
        let duplicate = evaluate_field(
            config,
            &proposals(),
            &[
                contribution(1, 10),
                contribution(1, 11),
                contribution(2, 20),
            ],
            &reviewers,
        );

        assert!((one.ranked()[0].posterior - duplicate.ranked()[0].posterior).abs() < 1e-12);
        assert!((one.ranked()[0].log_evidence - duplicate.ranked()[0].log_evidence).abs() < 1e-12);
    }

    #[test]
    fn independent_groups_accumulate_to_error_bound() {
        let config = SequentialEvidenceConfig::default();
        let mut reviewers = HashMap::new();
        for author in [10, 11, 12, 20] {
            reviewers.insert(
                uid(author),
                ReviewerCredential::with_default_prior(
                    uid(author),
                    format!("group-{author}"),
                    config,
                )
                .unwrap(),
            );
        }
        let snapshot = evaluate_field(
            config,
            &proposals(),
            &[
                contribution(1, 10),
                contribution(1, 11),
                contribution(1, 12),
                contribution(2, 20),
            ],
            &reviewers,
        );

        assert_eq!(
            snapshot.convergence_basis(),
            Some(ConvergenceBasis::EvidenceBound)
        );
        assert!(snapshot.posterior_error() < 0.20);
    }

    #[test]
    fn two_independent_groups_can_resolve_three_default_proposals() {
        let config = SequentialEvidenceConfig::default();
        let proposals = vec![(uid(1), 0), (uid(2), 1), (uid(3), 2)];
        let reviewers = [10, 11]
            .into_iter()
            .map(|author| {
                (
                    uid(author),
                    ReviewerCredential::with_default_prior(
                        uid(author),
                        format!("independent-{author}"),
                        config,
                    )
                    .unwrap(),
                )
            })
            .collect();
        let snapshot = evaluate_field(
            config,
            &proposals,
            &[contribution(1, 10), contribution(1, 11)],
            &reviewers,
        );

        assert_eq!(
            snapshot.convergence_basis(),
            Some(ConvergenceBasis::EvidenceBound)
        );
        assert!(snapshot.posterior_error() < config.target_error());
    }

    #[test]
    fn cost_bound_can_stop_before_stricter_error_target() {
        let config = SequentialEvidenceConfig::new(0.05, 0.75, 1.10, 0.11, 1.0).unwrap();
        let contributions = [
            contribution(1, 10),
            contribution(1, 11),
            contribution(1, 12),
            contribution(2, 20),
        ];
        let snapshot = evaluate_field(config, &proposals(), &contributions, &HashMap::new());

        assert!(snapshot.posterior_error() > config.target_error());
        assert!(snapshot.evpi_upper_bound() <= config.query_cost());
        assert_eq!(
            snapshot.convergence_basis(),
            Some(ConvergenceBasis::CostBound)
        );
    }

    #[test]
    fn uniform_field_never_chooses_an_arbitrary_leader() {
        let config = SequentialEvidenceConfig::new(0.05, 0.75, 1.10, 1.0, 1.0).unwrap();
        let snapshot = evaluate_field(config, &proposals(), &[], &HashMap::new());
        assert_eq!(snapshot.convergence_basis(), None);
        assert_eq!(snapshot.progress(), 0.0);
    }

    #[test]
    fn invalid_probability_is_rejected_at_the_boundary() {
        let error = SequentialEvidenceConfig::new(0.0, 0.75, 1.10, 0.02, 1.0).unwrap_err();
        assert!(matches!(
            error,
            EvidenceConfigError::InvalidParameter {
                name: "target_error",
                ..
            }
        ));
    }

    /// Engine-hardening proof: identical evidence must produce a bit-identical
    /// snapshot regardless of input iteration order. Callers feed HashMap
    /// order, float accumulation is order-sensitive, and event-sourced replay
    /// depends on identical evidence reproducing identical rankings.
    #[test]
    fn shuffled_inputs_produce_identical_snapshot() {
        let config = SequentialEvidenceConfig::default();
        let mut reviewers = HashMap::new();
        for author in [10, 11, 12, 20, 21] {
            reviewers.insert(
                uid(author),
                ReviewerCredential::with_default_prior(
                    uid(author),
                    format!("group-{}", author / 10),
                    config,
                )
                .unwrap(),
            );
        }
        let proposals = vec![(uid(1), 0), (uid(2), 1), (uid(3), 2)];
        let contributions = vec![
            contribution(1, 10),
            contribution(1, 11),
            contribution(2, 12),
            contribution(2, 20),
            contribution(3, 21),
        ];

        let baseline = evaluate_field_full(config, &proposals, &contributions, &reviewers);

        // Deterministic permutations standing in for arbitrary HashMap orders.
        let mut proposals_rev = proposals.clone();
        proposals_rev.reverse();
        let mut contributions_rev = contributions.clone();
        contributions_rev.reverse();
        let mut contributions_rot = contributions.clone();
        contributions_rot.rotate_left(2);

        for (props, contribs) in [
            (&proposals_rev, &contributions_rev),
            (&proposals, &contributions_rot),
            (&proposals_rev, &contributions),
        ] {
            let shuffled = evaluate_field_full(config, props, contribs, &reviewers);
            assert_eq!(baseline, shuffled);
        }
    }

    #[test]
    fn capped_group_reports_raw_used_above_cap() {
        let config = SequentialEvidenceConfig::default();
        let mut reviewers = HashMap::new();
        for author in [10, 11] {
            reviewers.insert(
                uid(author),
                ReviewerCredential::with_default_prior(uid(author), "same-model", config).unwrap(),
            );
        }
        let evaluation = evaluate_field_full(
            config,
            &proposals(),
            &[contribution(1, 10), contribution(1, 11)],
            &reviewers,
        );

        let saturated = evaluation
            .headroom
            .iter()
            .find(|h| h.group == GroupId::Registered("same-model".into()))
            .expect("contributing group must appear in headroom");
        // Two default-prior stances contribute 2*ln(3) raw mass against a
        // ln(3) cap: `used` reports the raw decayed mass, not the clamp.
        assert!(saturated.used > saturated.cap);
        assert!((saturated.used - 2.0 * DEFAULT_REVIEWER_LLR).abs() < 1e-12);
        assert_eq!(saturated.cap, config.correlation_cap());
    }

    #[test]
    fn silent_registered_group_is_synthesized_with_zero_used() {
        let config = SequentialEvidenceConfig::default();
        let mut reviewers = HashMap::new();
        reviewers.insert(
            uid(10),
            ReviewerCredential::with_default_prior(uid(10), "vocal", config).unwrap(),
        );
        reviewers.insert(
            uid(30),
            ReviewerCredential::with_default_prior(uid(30), "quiet", config).unwrap(),
        );
        // uid(99) is deliberately NOT registered: it must form a synthetic
        // per-author independent group rather than borrow anyone's budget.
        let evaluation = evaluate_field_full(
            config,
            &proposals(),
            &[contribution(1, 10), contribution(2, 99)],
            &reviewers,
        );

        let silent = evaluation
            .headroom
            .iter()
            .find(|h| h.group == GroupId::Registered("quiet".into()))
            .expect("registered-but-silent group must be synthesized");
        assert_eq!(silent.used, 0.0);
        assert_eq!(silent.cap, config.correlation_cap());
        // The credentialed contributor's registered group carries live mass.
        let vocal = evaluation
            .headroom
            .iter()
            .find(|h| h.group == GroupId::Registered("vocal".into()))
            .expect("contributing group present");
        assert!(vocal.used > 0.0);
        // The uncredentialed author appears as its own independent group with
        // live mass — never merged into a registered group.
        let independent = evaluation
            .headroom
            .iter()
            .find(|h| h.group == GroupId::Independent(uid(99)))
            .expect("uncredentialed author forms a synthetic independent group");
        assert!(independent.used > 0.0);
        // Headroom is sorted by GroupId for deterministic output.
        let mut sorted = evaluation.headroom.clone();
        sorted.sort_by(|a, b| a.group.cmp(&b.group));
        assert_eq!(evaluation.headroom, sorted);
    }
}
