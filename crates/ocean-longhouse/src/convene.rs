//! The convening flow — a real, leaderless council driven by cheap LLM agents.
//!
//! This is the orchestration layer that sits *between* the LLM workers
//! ([`crate::agent`]) and the pure [`QuorumEngine`](crate::quorum::QuorumEngine).
//! It runs the two-round blackboard protocol from
//! `docs/LONGHOUSE_ORCHESTRATION.md`:
//!
//! 1. **Round 1 — propose.** Each worker gets the question and posts one
//!    `proposal` mark (a candidate answer).
//! 2. **Rounds 2..N — active review acquisition.** In sequential-evidence
//!    mode, a fresh [`crate::planner::ReviewPlanner`] decision chooses an
//!    independent sample, adversarial challenge, decay reassertion, or
//!    non-weight-bearing evidence request before every provider call. Legacy
//!    net-weight mode retains the static endorse/inhibit reassertion order.
//!
//! After **every** mark the daemon-side [`QuorumEngine`] re-tallies, and we emit
//! a `QuorumUpdated`. When the engine reports convergence (or we hit a stopping
//! bound/deadline) we emit the single binding `Converged` (or `Aborted`), then
//! `TopicClosed`. The engine — never an LLM — decides when quorum is met.
//!
//! Every step emits the **existing** `LonghouseEvent`s from `ocean-agent-sdk`,
//! so the deck renders a real council with zero deck changes.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ocean_agent_sdk::{
    AbortReason, AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark,
    MarkKind, ProposalTally,
};
// OCEAN-229: the unforgeable-token primitive shared with OCEAN-185 permission
// decision tokens. `mint_decision_token` draws ~244 bits from the OS CSPRNG;
// `decision_token_matches` compares in constant time. We reuse it verbatim so
// the firekeeper's proof-of-title is the same trust-boundary primitive as a
// permission approval — never a fresh, ad-hoc secret.
use ocean_core::{decision_token_matches, mint_decision_token};
use uuid::Uuid;

use crate::agent::ModelHandle;
use crate::evidence::{ConvergenceBasis, ReviewerCredential};
use crate::planner::{EscalationReason, PlanOutcome, ReviewAction, ReviewPlanner};
use crate::quorum::{QuorumConfig, QuorumEngine, QuorumOutcome, QuorumRule};
use crate::replay::{RecordedMark, RecordedMarkKind, RecordedReviewer, Recording};

/// A request to convene a council on a question.
#[derive(Debug, Clone)]
pub struct ConveneRequest {
    /// The question/task the council deliberates.
    pub question: String,
    /// Which department room hosts it.
    pub federation: Federation,
    /// Why it was convened (defaults to a user request).
    pub trigger: ConveneTrigger,
    /// Model aliases to staff the council with, one worker per alias. Mixed
    /// across providers so the council is genuinely multi-model. Resolved via
    /// the standard auth.json.
    pub models: Vec<String>,
    /// Quorum tuning. The default is correlation-aware sequential evidence.
    pub quorum: QuorumConfig,
    /// Hard deadline budget in ms from convening. Sequential evidence aborts if
    /// its stopping rule has not fired; legacy net-weight mode may force-resolve.
    pub deadline_ms_from_now: i64,
    /// Maximum deliberation rounds, including the proposal round. Round 1
    /// proposes. In sequential mode, the remaining rounds become a provider
    /// call budget of `(max_rounds - 1) * seated_workers`; legacy net-weight
    /// mode retains one static reassertion pass per remaining round.
    pub max_rounds: usize,
}

impl ConveneRequest {
    /// A council with the two cheap models the build targets, mixed.
    pub fn new(question: impl Into<String>, federation: Federation) -> Self {
        Self {
            question: question.into(),
            federation,
            trigger: ConveneTrigger::UserRequest,
            // Mix deepseek + kimi so it's genuinely multi-model (3 + 2).
            models: vec![
                "deepseek-v4-flash".into(),
                "kimi-k2.6".into(),
                "deepseek-v4-flash".into(),
            ],
            quorum: QuorumConfig::default(),
            deadline_ms_from_now: 120_000,
            max_rounds: 4,
        }
    }
}

/// The result of a convening — what the council decided and the final field.
#[derive(Debug, Clone)]
pub struct ConveneOutcome {
    pub topic_id: Uuid,
    pub board_id: Uuid,
    /// `Some(proposal_id)` if the council converged, `None` if it aborted.
    pub decision: Option<Uuid>,
    /// Auditable daemon stopping condition for `decision`.
    pub convergence_basis: Option<ConvergenceBasis>,
    /// Final tallies for logging / the decision record.
    pub tallies: Vec<ProposalTally>,
    /// The proposal text for each proposal id (so callers can show the answer).
    pub proposals: HashMap<Uuid, String>,
    /// The full ordered mark-stream this council fed the engine, captured so the
    /// run can be **replayed** through different [`QuorumConfig`]s offline (see
    /// [`crate::replay`]). Timestamps are relative to the council start, so the
    /// recording is portable. Serialize this to disk to build a tuning corpus.
    pub recording: Recording,
}

/// A clock so the convening flow is testable without the wall clock.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Wall-clock implementation used in production.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        ocean_protocol::now_ms()
    }
}

/// Narrow response-layer seam used by the real provider handle and scripted
/// integration tests. It exposes model identity plus one text turn only;
/// proposal registration, planner consultation, engine mutation, and event
/// emission remain inside the real convene control flow.
#[async_trait]
trait CouncilHandle: Clone + Send + Sync {
    fn model_id(&self) -> &str;
    fn correlation_group(&self) -> String;
    fn has_credential(&self) -> bool;
    async fn ask(&self, system: &str, user: &str) -> Option<String>;
}

#[async_trait]
impl CouncilHandle for ModelHandle {
    fn model_id(&self) -> &str {
        ModelHandle::model_id(self)
    }

    fn correlation_group(&self) -> String {
        ModelHandle::correlation_group(self)
    }

    fn has_credential(&self) -> bool {
        ModelHandle::has_credential(self)
    }

    async fn ask(&self, system: &str, user: &str) -> Option<String> {
        ModelHandle::ask(self, system, user).await
    }
}

/// One seated worker: a stable id + the model driving it + its role.
struct Worker<H> {
    agent_id: Uuid,
    handle: H,
    role: AgentRole,
    label: String,
}

/// The unforgeable firekeeper title minted at convene time (OCEAN-229).
///
/// Binding the firekeeper to the winning proposer by `Uuid` alone is *namable*
/// — any caller of [`claim_outcome`] can assert it holds the title by passing
/// that public id, exactly the way any client could name a `permission_id` off
/// the public SSE in OCEAN-185. The id is therefore a handle, not a credential.
///
/// This title pairs the public `agent_id` with a **secret** `token`: a fresh
/// high-entropy value minted *server-side* by [`mint_decision_token`] when the
/// council seats its firekeeper. The token is the proof-of-title. It is held
/// only by the convening flow (and, in the persisted-engine future, by whatever
/// daemon component is authorized to ratify) and is **never** placed on any
/// emitted `LonghouseEvent` — `RoleGranted` and `Converged` carry the `agent_id`
/// only. So an observer who sees the granted firekeeper id on the event stream
/// still cannot forge a claim: they lack the token, and [`claim_outcome`]
/// verifies it in constant time before honoring the claim.
///
/// This is the server-decided-capability discipline of OCEAN-220 applied to the
/// terminator: the *right* to ratify is decided and minted by the server at
/// convene time, not asserted by the claimant.
#[derive(Clone)]
pub struct FirekeeperTitle {
    /// The public agent id that holds the title (the winning proposer). Safe to
    /// surface on events — on its own it grants nothing.
    pub agent_id: Uuid,
    /// The secret proof-of-title. Minted server-side; never serialized onto the
    /// event stream. Possession of this — not merely naming `agent_id` — is what
    /// authorizes a [`claim_outcome`].
    token: String,
}

impl FirekeeperTitle {
    /// Mint a fresh title for `agent_id`, drawing an unforgeable token from the
    /// OS CSPRNG via the shared OCEAN-185 primitive.
    pub fn mint(agent_id: Uuid) -> Self {
        Self {
            agent_id,
            token: mint_decision_token(),
        }
    }

    /// The secret proof-of-title to present to [`claim_outcome`]. Exposed so the
    /// legitimate holder (the convening flow) can present it; a forger never
    /// receives this value because it never leaves the minting site on the wire.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Constant-time check that a presented `(agent_id, token)` pair matches this
    /// title. Both the id and the token must match: a correct id with a wrong
    /// token, or a correct token asserted under the wrong id, both fail. The
    /// token comparison is constant-time so a forger cannot recover it by timing.
    fn authorizes(&self, agent_id: Uuid, presented_token: Option<&str>) -> bool {
        // Note: `==` on Uuid is fine — the id is public and namable by design;
        // it is the token (the secret) that must be compared without leaking.
        self.agent_id == agent_id
            && decision_token_matches(Some(self.token.as_str()), presented_token)
    }
}

impl std::fmt::Debug for FirekeeperTitle {
    /// Never print the secret token (avoid leaking it into logs/snapshots).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirekeeperTitle")
            .field("agent_id", &self.agent_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Run a full convening. `emit` receives each `LonghouseEvent` as it happens —
/// the daemon passes a closure that publishes onto the agent event bus
/// (`bus.emit(ev.into_turn_event())`), so the deck animates a live council.
///
/// `clock` is injected for deterministic testing of the timing/quorum path.
pub async fn convene<F>(req: ConveneRequest, clock: &dyn Clock, emit: F) -> ConveneOutcome
where
    F: FnMut(LonghouseEvent),
{
    convene_with_resolver(req, clock, emit, ModelHandle::resolve).await
}

/// Generic only over handle resolution so integration tests can script the LLM
/// response boundary while exercising the exact production control flow.
async fn convene_with_resolver<F, R, H>(
    req: ConveneRequest,
    clock: &dyn Clock,
    mut emit: F,
    mut resolve: R,
) -> ConveneOutcome
where
    F: FnMut(LonghouseEvent),
    R: FnMut(&str) -> anyhow::Result<H>,
    H: CouncilHandle,
{
    let topic_id = Uuid::new_v4();
    let board_id = Uuid::new_v4();
    let started = clock.now_ms();
    let deadline_ms = started + req.deadline_ms_from_now;

    emit(LonghouseEvent::TopicConvened {
        topic_id,
        board_id,
        federation: req.federation,
        trigger: req.trigger,
        title: truncate(&req.question, 140),
        deadline_ms,
    });

    // Staff the council: resolve each model alias to a real handle. A model that
    // fails to resolve (no credential) is dropped with a warning — the council
    // proceeds with whoever resolved.
    let mut workers: Vec<Worker<H>> = Vec::new();
    for alias in &req.models {
        match resolve(alias) {
            Ok(handle) if handle.has_credential() => {
                workers.push(Worker {
                    agent_id: Uuid::new_v4(),
                    label: format!("{} · {}", federation_label(req.federation), alias),
                    handle,
                    role: AgentRole::Courier,
                });
            }
            Ok(_) => tracing::warn!(alias, "model resolved but has no credential; skipping"),
            Err(e) => tracing::warn!(alias, error = %e, "failed to resolve model; skipping"),
        }
    }

    let members: Vec<LonghouseMember> = workers
        .iter()
        .map(|w| LonghouseMember {
            agent_id: w.agent_id,
            federation: req.federation,
            role: w.role,
            model: w.handle.model_id().to_string(),
            label: Some(w.label.clone()),
        })
        .collect();
    emit(LonghouseEvent::Convened {
        topic_id,
        members: members.clone(),
    });

    let mut engine = QuorumEngine::new(req.quorum);
    let mut recorded_reviewers: Vec<RecordedReviewer> = Vec::new();
    let evidence_config = req.quorum.rule.evidence_config().unwrap_or_default();
    for worker in &workers {
        match ReviewerCredential::with_default_prior(
            worker.agent_id,
            worker.handle.correlation_group(),
            evidence_config,
        ) {
            Ok(credential) => {
                recorded_reviewers.push(RecordedReviewer::from(&credential));
                engine.register_reviewer(credential);
            }
            Err(error) => tracing::warn!(
                agent = %worker.agent_id,
                error = %error,
                "invalid Longhouse reviewer credential; using independent fallback"
            ),
        }
    }
    let mut proposals: HashMap<Uuid, String> = HashMap::new();
    // proposal_id -> author agent_id, so endorse/inhibit can target by proposal.
    let mut proposal_by_author: HashMap<Uuid, Uuid> = HashMap::new();
    // Capture every mark fed to the engine (timestamps relative to `started`)
    // so the whole run is replayable offline under different configs.
    let mut recorded: Vec<RecordedMark> = Vec::new();

    if workers.is_empty() {
        // Nothing resolved — abort cleanly rather than hang.
        emit(LonghouseEvent::Aborted {
            topic_id,
            reason: AbortReason::Timeout,
        });
        emit(LonghouseEvent::TopicClosed { topic_id });
        return ConveneOutcome {
            topic_id,
            board_id,
            decision: None,
            convergence_basis: None,
            tallies: vec![],
            proposals,
            recording: Recording {
                question: req.question.clone(),
                reviewers: recorded_reviewers.clone(),
                marks: recorded.clone(),
            },
        };
    }

    // ---- Round 1: proposals -------------------------------------------------
    let proposal_prompts: Vec<(usize, String)> = workers
        .iter()
        .enumerate()
        .map(|(idx, _)| (idx, round1_user(&req.question)))
        .collect();

    // Fire all proposal calls concurrently — they're independent.
    let proposal_results = run_round(&workers, proposal_prompts, ROUND1_SYSTEM).await;

    // TASK-7 pre-registration consolidation: semantically duplicate answers
    // must not enter the engine as rival hypotheses — a unanimous council that
    // registers N copies of one answer fragments its own evidence field and
    // can never converge (live finding, topics 0ffd8ae7/4139172f). A duplicate
    // is folded into an endorse of the first-REGISTERED matching proposal
    // (deterministic: registration order, never HashMap order) before that
    // duplicate answer mutates the engine. No merge operation exists; the
    // correlation cap guards echo chambers through the existing endorse math,
    // unchanged.
    let mut registration_order: Vec<Uuid> = Vec::new();
    for (idx, answer) in proposal_results {
        let Some(answer) = answer else { continue };
        let w = &workers[idx];
        let now = clock.now_ms();

        let duplicate_of = find_duplicate_canonical(&registration_order, &proposals, &answer);
        if let Some(canonical) = duplicate_of {
            engine.endorse(canonical, w.agent_id, None, now);
            recorded.push(RecordedMark {
                at_ms: now - started,
                author: w.agent_id,
                kind: RecordedMarkKind::Endorse {
                    proposal: canonical,
                },
            });
            emit(LonghouseEvent::MarkPosted {
                topic_id,
                mark: Mark {
                    mark_id: Uuid::new_v4(),
                    author: w.agent_id,
                    kind: MarkKind::Endorse,
                    target: Some(canonical),
                    summary: truncate(&answer, 160),
                },
            });
            emit_quorum(&mut engine, topic_id, clock.now_ms(), &mut emit);
            if engine.is_converged() {
                break;
            }
            continue;
        }

        let proposal_id = Uuid::new_v4();
        engine.propose(proposal_id, w.agent_id, now);
        recorded.push(RecordedMark {
            at_ms: now - started,
            author: w.agent_id,
            kind: RecordedMarkKind::Propose {
                proposal: proposal_id,
            },
        });
        proposals.insert(proposal_id, answer.clone());
        registration_order.push(proposal_id);
        proposal_by_author.insert(w.agent_id, proposal_id);

        emit(LonghouseEvent::MarkPosted {
            topic_id,
            mark: Mark {
                mark_id: proposal_id,
                author: w.agent_id,
                kind: MarkKind::Proposal,
                target: None,
                summary: truncate(&answer, 160),
            },
        });
        emit_quorum(&mut engine, topic_id, clock.now_ms(), &mut emit);

        // Early exit if a single proposal already crossed (unlikely with margin).
        if engine.is_converged() {
            break;
        }
    }

    // If nobody proposed anything, abort.
    if proposals.is_empty() {
        emit(LonghouseEvent::Aborted {
            topic_id,
            reason: AbortReason::Split,
        });
        emit(LonghouseEvent::TopicClosed { topic_id });
        return ConveneOutcome {
            topic_id,
            board_id,
            decision: None,
            convergence_basis: None,
            tallies: engine.tallies(clock.now_ms()),
            proposals,
            recording: Recording {
                question: req.question.clone(),
                reviewers: recorded_reviewers.clone(),
                marks: recorded.clone(),
            },
        };
    }

    // ---- Rounds 2..N: review acquisition --------------------------------------
    //
    // Legacy net-weight mode keeps the original static re-assertion rounds.
    // Sequential-evidence mode replaces them with the pure [`ReviewPlanner`]:
    // a FRESH assessment is rebuilt from the engine before every provider
    // call, uncertainty governs who is asked next, and every escalation goes
    // through [`route_escalation`] (pure, test-pinned). No planner outcome can
    // construct commitment or emit an abort; the single direct-abort path is
    // the exhausted lone-proposal escalation, which emits
    // `InsufficientAlternatives` here in the convene loop and NEVER enters
    // `force_resolve` — the `Timeout -> Split` remap is unreachable from it.
    let max_rounds = req.max_rounds.max(1);
    let mut direct_abort: Option<DirectAbort> = None;
    match req.quorum.rule {
        QuorumRule::NetWeight { .. } => {
            for round in 2..=max_rounds {
                if engine.is_converged() || clock.now_ms() >= deadline_ms {
                    break;
                }

                // Build a bounded projection of the proposals for the voters to see.
                let projection = projection_text(&proposals);
                let proposal_ids: Vec<Uuid> = proposals.keys().copied().collect();

                // Query one reviewer at a time so the evidence engine can stop before
                // the next provider call is spent. Distinct correlation groups go
                // first; replicas of a provider/model are deferred until the end.
                let vote_prompts: Vec<(usize, String)> = independence_first_order(&workers)
                    .into_iter()
                    .map(|idx| {
                        let w = &workers[idx];
                        let own = proposal_by_author.get(&w.agent_id).copied();
                        (
                            idx,
                            round2_user(&req.question, &projection, &proposal_ids, own, round),
                        )
                    })
                    .collect();

                let mut contributed = false;

                for (idx, prompt) in vote_prompts {
                    if engine.is_converged() || clock.now_ms() >= deadline_ms {
                        break;
                    }
                    let answer = workers[idx].handle.ask(ROUND2_SYSTEM, &prompt).await;
                    let Some(answer) = answer else { continue };
                    let now = clock.now_ms();
                    let Some(vote) = parse_vote(&answer, &proposal_ids) else {
                        continue;
                    };
                    contributed = true;
                    apply_vote(
                        &mut engine,
                        workers[idx].agent_id,
                        &vote,
                        now,
                        started,
                        topic_id,
                        &mut recorded,
                        &mut emit,
                    );
                }

                // If a whole re-assertion round produced no usable marks, another identical
                // prompt cycle is unlikely to help; terminate and let force_resolve decide.
                if !contributed {
                    break;
                }
            }
        }
        QuorumRule::SequentialEvidence(_) => {
            // The planner's roster is the daemon-owned credential set; the
            // review budget preserves the legacy spend ceiling of one call per
            // worker per re-assertion round.
            let planner_roster: Vec<ReviewerCredential> = engine.reviewers().cloned().collect();
            let mut budget_remaining = (max_rounds.saturating_sub(1) * workers.len().max(1)) as f64;
            let mut rivals_generated = false;
            // Non-weight-bearing rationale artifacts. Observable on the event
            // stream as `MarkKind::Evidence` and surfaced to later reviewers
            // as prompt context — never fed to the engine, never recorded in
            // the replay evidence mass, and requested at most once per
            // proposal per convene.
            let mut evidence_notes: Vec<String> = Vec::new();
            let mut evidence_requested: HashSet<Uuid> = HashSet::new();
            let mut review_seq: usize = 1;

            'acquire: loop {
                if engine.is_converged() || clock.now_ms() >= deadline_ms {
                    break;
                }
                let now = clock.now_ms();
                let Some(assessment) = engine.assessment(now) else {
                    break;
                };
                let action = match ReviewPlanner::plan(
                    &assessment,
                    now,
                    deadline_ms,
                    budget_remaining,
                    &planner_roster,
                ) {
                    PlanOutcome::Continue(action) => action,
                    PlanOutcome::NeedsEscalation(reason) => {
                        match route_escalation(reason, rivals_generated, budget_remaining) {
                            EscalationRoute::GenerateRivals => {
                                rivals_generated = true;
                                // One bounded pass: every worker without a live
                                // proposal is asked once for a genuine rival.
                                let projection = projection_text(&proposals);
                                let prompts: Vec<(usize, String)> = workers
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, w)| !proposal_by_author.contains_key(&w.agent_id))
                                    .take(budget_remaining.max(0.0) as usize)
                                    .map(|(idx, _)| (idx, rival_user(&req.question, &projection)))
                                    .collect();
                                budget_remaining -= prompts.len() as f64;
                                let results = run_round(&workers, prompts, RIVAL_SYSTEM).await;
                                let mut landed = false;
                                for (idx, answer) in results {
                                    let Some(answer) = answer else { continue };
                                    if !is_distinct_rival(&answer, &proposals) {
                                        continue;
                                    }
                                    let w = &workers[idx];
                                    let proposal_id = Uuid::new_v4();
                                    let now = clock.now_ms();
                                    engine.propose(proposal_id, w.agent_id, now);
                                    recorded.push(RecordedMark {
                                        at_ms: now - started,
                                        author: w.agent_id,
                                        kind: RecordedMarkKind::Propose {
                                            proposal: proposal_id,
                                        },
                                    });
                                    proposals.insert(proposal_id, answer.clone());
                                    proposal_by_author.insert(w.agent_id, proposal_id);
                                    emit(LonghouseEvent::MarkPosted {
                                        topic_id,
                                        mark: Mark {
                                            mark_id: proposal_id,
                                            author: w.agent_id,
                                            kind: MarkKind::Proposal,
                                            target: None,
                                            summary: truncate(&answer, 160),
                                        },
                                    });
                                    emit_quorum(&mut engine, topic_id, now, &mut emit);
                                    landed = true;
                                }
                                if landed {
                                    // A rival registered: rebuild the
                                    // assessment and resume planning.
                                    continue 'acquire;
                                }
                                direct_abort = Some(DirectAbort::InsufficientAlternatives);
                                break 'acquire;
                            }
                            EscalationRoute::AbortInsufficientAlternatives => {
                                direct_abort = Some(DirectAbort::InsufficientAlternatives);
                                break 'acquire;
                            }
                            // Tied/saturated/budget/deadline: hand control to
                            // the Resolve section below — early force_resolve
                            // pre-deadline, honest Timeout at the deadline.
                            // Escalation itself never aborts and never commits.
                            EscalationRoute::ResolveEarly => break 'acquire,
                        }
                    }
                };

                let mut projection = projection_text(&proposals);
                if !evidence_notes.is_empty() {
                    projection.push_str("\n\nEvidence on the table:\n");
                    projection.push_str(&evidence_notes.join("\n"));
                }
                let proposal_ids: Vec<Uuid> = proposals.keys().copied().collect();

                match action {
                    ReviewAction::SampleIndependent { reviewer, .. }
                    | ReviewAction::ReassertAfterDecay { reviewer, .. } => {
                        let Some(idx) = workers.iter().position(|w| w.agent_id == reviewer) else {
                            continue;
                        };
                        budget_remaining -= 1.0;
                        let round = review_seq;
                        review_seq += 1;
                        let own = proposal_by_author.get(&reviewer).copied();
                        let prompt =
                            round2_user(&req.question, &projection, &proposal_ids, own, round);
                        let Some(answer) = workers[idx].handle.ask(ROUND2_SYSTEM, &prompt).await
                        else {
                            continue;
                        };
                        let now = clock.now_ms();
                        let Some(vote) = parse_vote(&answer, &proposal_ids) else {
                            continue;
                        };
                        apply_vote(
                            &mut engine,
                            reviewer,
                            &vote,
                            now,
                            started,
                            topic_id,
                            &mut recorded,
                            &mut emit,
                        );
                    }
                    ReviewAction::ChallengeLeader {
                        leader,
                        runner_up,
                        reviewer,
                    } => {
                        let Some(idx) = workers.iter().position(|w| w.agent_id == reviewer) else {
                            continue;
                        };
                        budget_remaining -= 1.0;
                        review_seq += 1;
                        let prompt = challenge_user(
                            &req.question,
                            &projection,
                            &proposal_ids,
                            leader,
                            runner_up,
                        );
                        let Some(answer) = workers[idx].handle.ask(ROUND2_SYSTEM, &prompt).await
                        else {
                            continue;
                        };
                        let now = clock.now_ms();
                        let Some(vote) = parse_vote(&answer, &proposal_ids) else {
                            continue;
                        };
                        // An adversarial comparison must land on one of the two
                        // compared proposals; anything else is an abstention.
                        if vote.target != leader && vote.target != runner_up {
                            continue;
                        }
                        apply_vote(
                            &mut engine,
                            reviewer,
                            &vote,
                            now,
                            started,
                            topic_id,
                            &mut recorded,
                            &mut emit,
                        );
                    }
                    ReviewAction::RequestEvidence { proposal } => {
                        // Bounded no-repeat: with a saturated field the plan
                        // is stable, so an unanswered/answered request must not
                        // re-fire every tick. One request per proposal per
                        // convene; a repeat means the acquisition plane has
                        // nothing left to buy — hand control to Resolve.
                        if !claim_evidence_request(&mut evidence_requested, proposal) {
                            break 'acquire;
                        }
                        // Ask the proposal's author for a falsifiable rationale.
                        // The response carries NO quorum weight: it is emitted
                        // as an observable `Evidence` mark and becomes prompt
                        // context for later reviewers — never an
                        // endorse/inhibit, never replay evidence mass.
                        let author = proposal_by_author
                            .iter()
                            .find(|(_, owned)| **owned == proposal)
                            .map(|(author, _)| *author);
                        let Some(author) = author else { continue };
                        let Some(idx) = workers.iter().position(|w| w.agent_id == author) else {
                            continue;
                        };
                        budget_remaining -= 1.0;
                        review_seq += 1;
                        let text = proposals.get(&proposal).cloned().unwrap_or_default();
                        let prompt = evidence_user(&req.question, &text);
                        if let Some(answer) =
                            workers[idx].handle.ask(EVIDENCE_SYSTEM, &prompt).await
                        {
                            evidence_notes.push(emit_evidence(
                                topic_id, author, proposal, &answer, &mut emit,
                            ));
                        }
                    }
                }
            }
        }
    }

    // The exhausted lone-proposal escalation terminates here, directly:
    // by construction it never reaches force_resolve or any ConvergenceBasis.
    if let Some(abort) = direct_abort {
        emit(abort.event(topic_id));
        emit(LonghouseEvent::TopicClosed { topic_id });
        let now = clock.now_ms();
        return ConveneOutcome {
            topic_id,
            board_id,
            decision: None,
            convergence_basis: None,
            tallies: engine.tallies(now),
            proposals,
            recording: Recording {
                question: req.question.clone(),
                reviewers: recorded_reviewers,
                marks: recorded,
            },
        };
    }

    // ---- Resolve ------------------------------------------------------------
    let now = clock.now_ms();
    let decision = match resolve_engine(&mut engine, now, deadline_ms) {
        ResolutionOutcome::Committed(decision) => Some(decision),
        ResolutionOutcome::Aborted(reason) => {
            emit(LonghouseEvent::Aborted { topic_id, reason });
            None
        }
    };

    if let Some(decision) = decision {
        // Bind a firekeeper to the winning proposal's author and have it ratify —
        // exactly the design's "single signed terminator". The engine tells us
        // who proposed the winning proposal; that proposer holds the firekeeper
        // title for this topic.
        let firekeeper_id = engine
            .proposer_of(decision)
            .or_else(|| workers.first().map(|w| w.agent_id))
            .unwrap_or_else(Uuid::new_v4);

        // OCEAN-229: mint the title server-side here. The secret token is the
        // unforgeable proof-of-title; it stays in this stack frame and is handed
        // straight to `claim_outcome`. It is NEVER emitted on any event — the
        // `RoleGranted`/`Converged` below carry only the public `agent_id`. A
        // forged firekeeper that merely names this id off the event stream has
        // no token and is rejected by the gate.
        let title = FirekeeperTitle::mint(firekeeper_id);

        // The accountability brake: the firekeeper may only emit the single
        // binding `Converged` if (a) it presents the unforgeable title token
        // minted above, AND (b) the daemon's own quorum state already agrees the
        // topic is converged on this exact decision (or was force-resolved at the
        // deadline). This is the load-bearing gate — a firekeeper can never ratify
        // a decision the quorum engine doesn't back, and a forged firekeeper can
        // never ratify at all. See [`claim_outcome`].
        match claim_outcome(
            &mut engine,
            &title,
            firekeeper_id,
            Some(title.token()),
            decision,
            now,
        ) {
            Ok(()) => {
                emit(LonghouseEvent::RoleGranted {
                    topic_id,
                    agent_id: firekeeper_id,
                    role: AgentRole::Firekeeper,
                });
                emit(LonghouseEvent::Converged {
                    topic_id,
                    decision,
                    by: firekeeper_id,
                });
            }
            Err(err) => {
                // The firekeeper claim was refused — either it did not present a
                // valid title token (forged firekeeper) or it tried to ratify a
                // decision the quorum engine does not back. Refuse and abort the
                // topic rather than emit an unaccountable `Converged`.
                tracing::warn!(
                    topic = %topic_id,
                    firekeeper = %firekeeper_id,
                    error = %err,
                    "firekeeper claim_outcome rejected; aborting topic"
                );
                emit(LonghouseEvent::Aborted {
                    topic_id,
                    reason: AbortReason::Split,
                });
            }
        }
    }

    emit(LonghouseEvent::TopicClosed { topic_id });

    ConveneOutcome {
        topic_id,
        board_id,
        decision,
        convergence_basis: engine.convergence_basis(),
        tallies: engine.tallies(now),
        proposals,
        recording: Recording {
            question: req.question.clone(),
            reviewers: recorded_reviewers,
            marks: recorded,
        },
    }
}

/// Why a firekeeper's claimed `Converged` was refused. A claim must clear two
/// independent boundaries: it must come from the *real* firekeeper (proven by an
/// unforgeable token, OCEAN-229), and it must agree with the daemon's quorum
/// state (the firekeeper *ratifies* a decision the engine already owns; it can
/// never manufacture one). Each variant is a distinct way the claim failed one
/// of those boundaries.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimError {
    /// OCEAN-229: the claimant did not prove it holds the firekeeper title —
    /// the presented `(agent_id, token)` pair did not match the title minted at
    /// convene time (wrong/absent token, or the token asserted under a different
    /// id). This is the unforgeability boundary: it is checked **first**, before
    /// any quorum state is consulted, so a forged firekeeper is rejected even
    /// when the quorum genuinely converged. A forger who only learned the public
    /// firekeeper `agent_id` from the event stream lands here.
    ForgedFirekeeper,
    /// The engine has not converged and is not at/over its deadline — there is
    /// no resolved decision for the firekeeper to ratify yet. This is the core
    /// accountability brake: a premature `Converged` claim lands here.
    NotConverged,
    /// The engine *has* converged, but on a *different* proposal than the one
    /// the firekeeper tried to ratify. The firekeeper may only sign the
    /// engine's own decision, never substitute its own.
    WrongDecision {
        /// The proposal the engine actually converged on.
        engine_decision: Uuid,
        /// The proposal the firekeeper tried to ratify.
        claimed: Uuid,
    },
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::ForgedFirekeeper => write!(
                f,
                "claimant did not prove the firekeeper title; claim refused as forged"
            ),
            ClaimError::NotConverged => write!(
                f,
                "quorum has not converged; firekeeper may not emit Converged"
            ),
            ClaimError::WrongDecision {
                engine_decision,
                claimed,
            } => write!(
                f,
                "firekeeper claimed {claimed} but quorum converged on {engine_decision}"
            ),
        }
    }
}

impl std::error::Error for ClaimError {}

/// The unforgeable accountability gate on the single binding `Converged`.
///
/// This gate guards **two** independent trust boundaries, and a claim must clear
/// both:
///
/// 1. **Unforgeable identity (OCEAN-229).** The claimant must prove it holds the
///    firekeeper `title` minted at convene time by presenting the matching
///    `(firekeeper_id, presented_token)`. The token is verified in constant time
///    via the shared OCEAN-185 primitive ([`decision_token_matches`]). This is
///    checked **first**, *before* the engine is even consulted: a claimant that
///    cannot prove the title is rejected with [`ClaimError::ForgedFirekeeper`]
///    regardless of the quorum state. This is the property OCEAN-229 adds — the
///    firekeeper's *right* to claim is verifiable, not merely asserted by passing
///    a public `Uuid`. It mirrors OCEAN-185 (a `permission_id` is public on the
///    SSE, but only the holder of the secret decision token may act on it) and
///    OCEAN-220 (the capability/right is decided + minted by the server, not by
///    the claimant).
///
/// 2. **Engine agreement (the original brake).** A firekeeper does not *decide* —
///    the daemon-side [`QuorumEngine`] does. Even a legitimately-titled firekeeper
///    may only *ratify* a decision the engine already owns. So after the identity
///    check passes, this returns `Ok(())` only when the engine reports
///    [`QuorumOutcome::Converged`] for exactly `claimed` at `now_ms`. A premature
///    claim (engine still `Pending`) gets [`ClaimError::NotConverged`]; a claim of
///    the wrong proposal gets [`ClaimError::WrongDecision`]. The deadline path is
///    covered because [`QuorumEngine::force_resolve`] latches `converged` before
///    the firekeeper is bound (see [`convene`]), so a deadline-forced resolution
///    already reads back as `Converged` here.
///
/// Checking identity before convergence is deliberate: a forged firekeeper must
/// be refused identically whether or not the quorum happened to converge, so that
/// the rejection reason never leaks engine state to an unauthorized caller.
pub fn claim_outcome(
    engine: &mut QuorumEngine,
    title: &FirekeeperTitle,
    firekeeper_id: Uuid,
    presented_token: Option<&str>,
    claimed: Uuid,
    now_ms: i64,
) -> Result<(), ClaimError> {
    // Boundary 1 — unforgeable identity. Reject a claimant that cannot prove the
    // title BEFORE consulting the engine, so a forged firekeeper is refused even
    // when the quorum genuinely converged (and the refusal leaks no engine state).
    if !title.authorizes(firekeeper_id, presented_token) {
        return Err(ClaimError::ForgedFirekeeper);
    }

    // Boundary 2 — the firekeeper may only ratify the engine's own decision.
    match engine.evaluate(now_ms) {
        QuorumOutcome::Converged { decision, .. } => {
            if decision == claimed {
                Ok(())
            } else {
                Err(ClaimError::WrongDecision {
                    engine_decision: decision,
                    claimed,
                })
            }
        }
        QuorumOutcome::Pending { .. } => Err(ClaimError::NotConverged),
    }
}

/// Recompute quorum and emit a `QuorumUpdated` reflecting the current field.
fn emit_quorum<F>(engine: &mut QuorumEngine, topic_id: Uuid, now_ms: i64, emit: &mut F)
where
    F: FnMut(LonghouseEvent),
{
    let outcome = engine.evaluate(now_ms);
    let (tallies, leader, distance) = match outcome {
        QuorumOutcome::Pending {
            tallies,
            leader,
            distance_to_quorum,
        } => (tallies, leader, distance_to_quorum),
        QuorumOutcome::Converged {
            decision, tallies, ..
        } => (tallies, Some(decision), 1.0),
    };
    emit(LonghouseEvent::QuorumUpdated {
        topic_id,
        tallies,
        leader,
        distance_to_quorum: distance,
    });
}

/// Run the proposal round concurrently. Candidate generation precedes
/// sequential evidence evaluation, so there is no stopping decision to save
/// these calls. A `None` answer means that worker did not contribute.
async fn run_round<H: CouncilHandle>(
    workers: &[Worker<H>],
    prompts: Vec<(usize, String)>,
    system: &str,
) -> Vec<(usize, Option<String>)> {
    let futures = prompts.into_iter().map(|(idx, user)| {
        let handle = workers[idx].handle.clone();
        let system = system.to_string();
        async move {
            let answer = handle.ask(&system, &user).await;
            (idx, answer)
        }
    });
    futures::future::join_all(futures).await
}

/// Stable reviewer order with one representative from every distinct
/// provider/model group before any correlated replica. This does not pretend
/// to estimate full information gain or that different groups are perfectly
/// independent; it is a deterministic proxy that avoids asking an exact
/// replica before every distinct group has had the same opportunity.
fn independence_first_order<H: CouncilHandle>(workers: &[Worker<H>]) -> Vec<usize> {
    let mut seen = HashSet::new();
    let mut independent = Vec::with_capacity(workers.len());
    let mut replicas = Vec::new();
    for (index, worker) in workers.iter().enumerate() {
        if seen.insert(worker.handle.correlation_group()) {
            independent.push(index);
        } else {
            replicas.push(index);
        }
    }
    independent.extend(replicas);
    independent
}

/// Where the convene loop routes a planner escalation. Pure and test-pinned:
/// this function IS the code path, so the tests on it constrain the loop.
///
/// The invariants it encodes: `LoneProposal` is the only escalation that may
/// spend more resources (one bounded rival-generation pass per convene), its
/// exhaustion is the only direct abort (emitted in the loop, never through
/// `force_resolve`), and every other reason resolves through the honest
/// Resolve section — early `force_resolve` pre-deadline or `Timeout` at the
/// deadline. No route constructs commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscalationRoute {
    /// Ask non-proposing workers for a genuine rival, once per convene.
    GenerateRivals,
    /// Terminate directly with `AbortReason::InsufficientAlternatives`.
    AbortInsufficientAlternatives,
    /// Hand control to the Resolve section (force_resolve / deadline).
    ResolveEarly,
}

/// The only planner-driven direct terminal path. Deadline and split exits are
/// intentionally unrepresentable here; they belong to [`ResolutionOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectAbort {
    InsufficientAlternatives,
}

impl DirectAbort {
    fn event(self, topic_id: Uuid) -> LonghouseEvent {
        let reason = match self {
            Self::InsufficientAlternatives => AbortReason::InsufficientAlternatives,
        };
        LonghouseEvent::Aborted { topic_id, reason }
    }
}

/// Mutually exclusive terminal outcomes after acquisition stops. The typed
/// split keeps an honest sequential abort distinct from a daemon-latched
/// commitment all the way to event emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolutionOutcome {
    Committed(Uuid),
    Aborted(AbortReason),
}

fn resolve_engine(engine: &mut QuorumEngine, now_ms: i64, deadline_ms: i64) -> ResolutionOutcome {
    match engine.evaluate(now_ms) {
        QuorumOutcome::Converged { decision, .. } => ResolutionOutcome::Committed(decision),
        QuorumOutcome::Pending { .. } => {
            // Only legacy net-weight mode may manufacture a deadline winner.
            // Sequential evidence passes the honest reason through unchanged.
            let reason = if now_ms >= deadline_ms {
                AbortReason::Timeout
            } else {
                AbortReason::Split
            };
            match engine.force_resolve(now_ms, reason, true) {
                Ok(decision) => ResolutionOutcome::Committed(decision),
                Err(reason) => ResolutionOutcome::Aborted(reason),
            }
        }
    }
}

fn route_escalation(
    reason: EscalationReason,
    rivals_generated: bool,
    budget_remaining: f64,
) -> EscalationRoute {
    match reason {
        EscalationReason::LoneProposal if !rivals_generated && budget_remaining >= 1.0 => {
            EscalationRoute::GenerateRivals
        }
        EscalationReason::LoneProposal => EscalationRoute::AbortInsufficientAlternatives,
        EscalationReason::TiedField
        | EscalationReason::BudgetExhausted
        | EscalationReason::DeadlineReached
        | EscalationReason::NoEligibleReviewers => EscalationRoute::ResolveEarly,
    }
}

/// Publish one non-weight-bearing artifact and return its bounded prompt form.
/// The engine is deliberately absent from this function's parameters, so an
/// evidence request cannot mutate the quorum field by construction.
fn emit_evidence<F>(
    topic_id: Uuid,
    author: Uuid,
    proposal: Uuid,
    answer: &str,
    emit: &mut F,
) -> String
where
    F: FnMut(LonghouseEvent),
{
    emit(LonghouseEvent::MarkPosted {
        topic_id,
        mark: Mark {
            mark_id: Uuid::new_v4(),
            author,
            kind: MarkKind::Evidence,
            target: Some(proposal),
            summary: truncate(answer, 160),
        },
    });
    format!("- {}", truncate(answer, 220))
}

/// Reserve the sole artifact request allowed for a proposal in one convene.
fn claim_evidence_request(requested: &mut HashSet<Uuid>, proposal: Uuid) -> bool {
    requested.insert(proposal)
}

/// Feed one parsed vote to the engine and mirror it to the recording and the
/// event stream. Shared by the legacy static rounds and the planner loop so
/// the two paths cannot drift in how a mark lands.
#[allow(clippy::too_many_arguments)]
fn apply_vote<F>(
    engine: &mut QuorumEngine,
    author: Uuid,
    vote: &Vote,
    now: i64,
    started: i64,
    topic_id: Uuid,
    recorded: &mut Vec<RecordedMark>,
    emit: &mut F,
) where
    F: FnMut(LonghouseEvent),
{
    match vote.kind {
        VoteKind::Endorse => engine.endorse(vote.target, author, None, now),
        VoteKind::Inhibit => engine.inhibit(vote.target, author, None, now),
    }
    recorded.push(RecordedMark {
        at_ms: now - started,
        author,
        kind: match vote.kind {
            VoteKind::Endorse => RecordedMarkKind::Endorse {
                proposal: vote.target,
            },
            VoteKind::Inhibit => RecordedMarkKind::Inhibit {
                proposal: vote.target,
            },
        },
    });
    emit(LonghouseEvent::MarkPosted {
        topic_id,
        mark: Mark {
            mark_id: Uuid::new_v4(),
            author,
            kind: match vote.kind {
                VoteKind::Endorse => MarkKind::Endorse,
                VoteKind::Inhibit => MarkKind::Inhibit,
            },
            target: Some(vote.target),
            summary: truncate(&vote.rationale, 160),
        },
    });
    emit_quorum(engine, topic_id, now, emit);
}

const ROUND1_SYSTEM: &str =
    "You are one member of a small council answering a question. Give your single best \
     answer in ONE short paragraph (max 3 sentences). Be concrete and decisive. Do not \
     hedge or list multiple options — commit to one answer. If the best answer is the \
     obvious one others will also give, state it plainly anyway: matching answers are \
     merged into shared support, never penalized.";

fn round1_user(question: &str) -> String {
    format!("Question for the council:\n\n{question}\n\nYour proposed answer:")
}

const ROUND2_SYSTEM: &str =
    "You are one member of a council reviewing proposed answers. Pick the ONE proposal you \
     most support, or the ONE you most oppose. Reply with EXACTLY one line in this format:\n\
     ENDORSE <number>: <one short reason>\n\
     or\n\
     INHIBIT <number>: <one short reason>\n\
     Use the proposal numbers shown. Do not add anything else.";

const RIVAL_SYSTEM: &str =
    "You are one member of a small council. The board currently holds only ONE proposed \
     answer, and a one-hypothesis field cannot be decided. Propose a GENUINELY DIFFERENT \
     alternative answer in ONE short paragraph (max 3 sentences). If you truly cannot \
     offer a distinct alternative, reply with exactly: PASS";

fn rival_user(question: &str, projection: &str) -> String {
    format!(
        "Question for the council:\n\n{question}\n\nThe only proposal so far:\n{projection}\n\n\
         Your genuinely different alternative (or PASS):"
    )
}

/// Reject the explicit abstention vocabulary and exact restatements before a
/// generated answer can enter the one-hypothesis field as a rival proposal.
fn is_distinct_rival(answer: &str, proposals: &HashMap<Uuid, String>) -> bool {
    let answer = answer.trim();
    let abstention = answer.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    !answer.is_empty()
        && !abstention.eq_ignore_ascii_case("PASS")
        && !proposals
            .values()
            .any(|existing| answers_are_duplicates(existing, answer))
}

/// Conservative Jaccard threshold above which two answers are treated as the
/// same hypothesis. Deliberately high: a false merge silently deletes a rival
/// (bad), while a missed merge only leaves the pre-TASK-7 fragmentation (the
/// engine stays honest and aborts). Tune only with measurements.
const DUPLICATE_JACCARD: f64 = 0.6;

/// Deterministic duplicate check shared by round-1 consolidation and rival
/// filtering (TASK-7): one similarity definition, one code path, no LLM.
///
/// Two answers are duplicates when the Jaccard similarity of their normalized
/// content-token sets reaches [`DUPLICATE_JACCARD`]. Exact restatements score
/// 1.0, so this strictly widens the old case-insensitive equality check.
/// Below this many content tokens an answer is too short for similarity to
/// mean anything ("Proposal A" vs "Proposal B" share every content token), so
/// the check falls back to exact-match equality — never merging what it
/// cannot judge.
const DUPLICATE_MIN_TOKENS: usize = 4;

fn answers_are_duplicates(a: &str, b: &str) -> bool {
    // NEGATION VETO (codex's review blocker, strengthened twice): "should use
    // X" vs "should NOT use X" must never merge — that false merge deletes a
    // directly contradictory rival. Preserving negation words as tokens is
    // NOT sufficient (one token in a long answer clears any Jaccard
    // threshold), and neither is boolean presence ("avoid X" vs "do not avoid
    // X" both 'contain negation' yet point opposite ways). So the veto
    // compares normalized negation SIGNATURES — the multiset of negation
    // words, with n't contractions recognized before tokenization — and any
    // difference vetoes before similarity is consulted. False-splitting two
    // differently phrased negatives is acceptable under the conservative
    // contract; false-merging opposites is not.
    if negation_signature(a) != negation_signature(b) {
        return false;
    }
    let ta = content_tokens(a);
    let tb = content_tokens(b);
    if ta.len() < DUPLICATE_MIN_TOKENS || tb.len() < DUPLICATE_MIN_TOKENS {
        return a.trim().eq_ignore_ascii_case(b.trim());
    }
    let intersection = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    intersection / union >= DUPLICATE_JACCARD
}

/// Normalized negation signature: how many times each negation word occurs,
/// with ASCII and curly `n't` contractions rewritten to ` not` BEFORE
/// tokenization (a bare alphanumeric split turns "can't" into ["can","t"] and
/// the negation silently vanishes). Any difference between two answers'
/// signatures — including "avoid X" vs "do not avoid X" — is a hard
/// anti-merge veto; a matching signature merely allows similarity to be
/// consulted.
fn negation_signature(text: &str) -> std::collections::BTreeMap<&'static str, usize> {
    const NEGATIONS: &[&str] = &["not", "no", "never", "cannot", "avoid", "against", "reject"];
    let normalized = text
        .to_lowercase()
        .replace("n\u{2019}t", " not")
        .replace("n't", " not");
    let mut signature = std::collections::BTreeMap::new();
    for token in normalized.split(|c: char| !c.is_ascii_alphanumeric()) {
        if let Some(word) = NEGATIONS.iter().find(|word| **word == token) {
            *signature.entry(*word).or_insert(0usize) += 1;
        }
    }
    signature
}

/// Lowercased alphanumeric tokens minus high-frequency English glue words, so
/// similarity is carried by content ("indexeddb", "transactional", "eviction")
/// rather than prose scaffolding shared by every answer. Negation words are
/// deliberately NOT stopwords — they are content (and additionally a hard
/// veto, see [`negation_signature`]).
fn content_tokens(text: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "because", "but", "by", "can", "for", "from",
        "has", "have", "if", "in", "is", "it", "its", "more", "most", "of", "on", "or", "should",
        "so", "than", "that", "the", "their", "them", "these", "this", "to", "use", "we", "when",
        "which", "while", "will", "with", "would", "you", "your",
    ];
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| token.len() > 1 && !STOPWORDS.contains(token))
        .map(str::to_owned)
        .collect()
}

/// First registered proposal the answer duplicates, in REGISTRATION order —
/// a `HashMap` scan would pick a process-randomized canonical whenever an
/// answer clears the threshold against more than one hypothesis.
fn find_duplicate_canonical(
    registration_order: &[Uuid],
    proposals: &HashMap<Uuid, String>,
    answer: &str,
) -> Option<Uuid> {
    registration_order
        .iter()
        .find(|id| {
            proposals
                .get(id)
                .is_some_and(|existing| answers_are_duplicates(existing, answer))
        })
        .copied()
}

const EVIDENCE_SYSTEM: &str =
    "You are one member of a council. Provide the strongest CONCRETE evidence for the \
     proposal you authored: a source, a test, a falsifiable claim, or a worked example. \
     Max 3 sentences. Do NOT vote and do NOT restate the proposal.";

fn evidence_user(question: &str, proposal: &str) -> String {
    format!(
        "Question:\n{question}\n\nYour proposal on the board:\n{proposal}\n\n\
         Your strongest concrete evidence for it:"
    )
}

/// Adversarial leader/runner-up comparison. Numbering matches
/// [`projection_text`]'s stable sorted order, and the reply is parsed with the
/// same [`parse_vote`] grammar — the caller rejects any target that is not one
/// of the two compared proposals.
fn challenge_user(
    question: &str,
    projection: &str,
    ids: &[Uuid],
    leader: Uuid,
    runner_up: Uuid,
) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort();
    let number_of = |id: Uuid| sorted.iter().position(|x| *x == id).map_or(0, |p| p + 1);
    let leader_no = number_of(leader);
    let runner_no = number_of(runner_up);
    format!(
        "Question:\n{question}\n\nProposals on the blackboard:\n{projection}\n\n\
         Adversarial comparison: weigh proposal {leader_no} (current leader) DIRECTLY \
         against proposal {runner_no} (runner-up). Which is stronger, and which — if \
         either — is actively weaker or harmful? Vote ONLY on proposal {leader_no} or \
         {runner_no}.\n\n\
         Your vote (ENDORSE <n>: reason  OR  INHIBIT <n>: reason):"
    )
}

fn round2_user(
    question: &str,
    projection: &str,
    _ids: &[Uuid],
    _own: Option<Uuid>,
    round: usize,
) -> String {
    format!(
        "Question:\n{question}\n\nDeliberation round: {round}\n\nProposals on the blackboard:\n{projection}\n\n\
         Re-assert your current stance. If the field already has a strong answer, ENDORSE it; \
         if one proposal is actively harmful or weaker than a rival, INHIBIT it.\n\n\
         Your vote (ENDORSE <n>: reason  OR  INHIBIT <n>: reason):"
    )
}

/// A bounded, numbered projection of the proposals (the stigmergic board view
/// workers receive — never the raw transcript).
fn projection_text(proposals: &HashMap<Uuid, String>) -> String {
    // Stable numbering by sorted proposal id so prompts are reproducible.
    let mut entries: Vec<(&Uuid, &String)> = proposals.iter().collect();
    entries.sort_by_key(|(id, _)| **id);
    entries
        .iter()
        .enumerate()
        .map(|(i, (_, text))| format!("{}. {}", i + 1, truncate(text, 220)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum VoteKind {
    Endorse,
    Inhibit,
}

struct Vote {
    kind: VoteKind,
    target: Uuid,
    rationale: String,
}

/// Parse a worker's `ENDORSE n: reason` / `INHIBIT n: reason` line into a vote
/// against a concrete proposal id. The number indexes the same sorted order as
/// [`projection_text`]. Returns `None` if it can't be parsed (worker abstains).
fn parse_vote(answer: &str, ids: &[Uuid]) -> Option<Vote> {
    // Reconstruct the same sorted id order the projection used.
    let mut sorted = ids.to_vec();
    sorted.sort();

    let line = answer.lines().find(|l| {
        let u = l.trim().to_uppercase();
        u.starts_with("ENDORSE") || u.starts_with("INHIBIT")
    })?;
    let trimmed = line.trim();
    let upper = trimmed.to_uppercase();
    let kind = if upper.starts_with("ENDORSE") {
        VoteKind::Endorse
    } else {
        VoteKind::Inhibit
    };
    // Everything after the keyword.
    let rest = &trimmed[7..];
    // Find the first integer in the rest.
    let num: usize = rest
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())?;
    if num == 0 || num > sorted.len() {
        return None;
    }
    let target = sorted[num - 1];
    let rationale = rest
        .split_once(':')
        .map(|(_, r)| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| match kind {
            VoteKind::Endorse => "endorses".into(),
            VoteKind::Inhibit => "inhibits".into(),
        });
    Some(Vote {
        kind,
        target,
        rationale,
    })
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn federation_label(f: Federation) -> &'static str {
    match f {
        Federation::Dev => "Dev",
        Federation::Sales => "Sales",
        Federation::Content => "Content",
        Federation::Campaign => "Campaign",
        Federation::Commons => "Commons",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::evidence::SequentialEvidenceConfig;

    #[derive(Debug, Clone)]
    enum ScriptedReply {
        Text(&'static str),
        Endorse(&'static str),
        Inhibit(&'static str),
        None,
    }

    impl ScriptedReply {
        fn render(self, user: &str) -> Option<String> {
            match self {
                Self::Text(text) => Some(text.to_owned()),
                Self::Endorse(proposal) => Some(format!(
                    "ENDORSE {}: scripted support",
                    proposal_number(user, proposal).unwrap_or_else(|| panic!(
                        "proposal {proposal:?} absent from prompt:\n{user}"
                    ))
                )),
                Self::Inhibit(proposal) => Some(format!(
                    "INHIBIT {}: scripted challenge",
                    proposal_number(user, proposal).unwrap_or_else(|| panic!(
                        "proposal {proposal:?} absent from prompt:\n{user}"
                    ))
                )),
                Self::None => None,
            }
        }
    }

    fn proposal_number(prompt: &str, proposal: &str) -> Option<usize> {
        prompt.lines().find_map(|line| {
            let (number, text) = line.split_once(". ")?;
            (text.trim() == proposal)
                .then(|| number.trim().parse::<usize>().ok())
                .flatten()
        })
    }

    #[derive(Debug, Clone)]
    struct ScriptedCall {
        alias: String,
        system: String,
        user: String,
    }

    #[derive(Clone)]
    struct ScriptedHandle {
        alias: String,
        group: String,
        replies: Arc<Mutex<VecDeque<ScriptedReply>>>,
        calls: Arc<Mutex<Vec<ScriptedCall>>>,
    }

    impl ScriptedHandle {
        fn new(
            alias: &str,
            group: &str,
            replies: impl IntoIterator<Item = ScriptedReply>,
            calls: &Arc<Mutex<Vec<ScriptedCall>>>,
        ) -> Self {
            Self {
                alias: alias.to_owned(),
                group: group.to_owned(),
                replies: Arc::new(Mutex::new(replies.into_iter().collect())),
                calls: Arc::clone(calls),
            }
        }
    }

    #[async_trait]
    impl CouncilHandle for ScriptedHandle {
        fn model_id(&self) -> &str {
            &self.alias
        }

        fn correlation_group(&self) -> String {
            self.group.clone()
        }

        fn has_credential(&self) -> bool {
            true
        }

        async fn ask(&self, system: &str, user: &str) -> Option<String> {
            self.calls.lock().unwrap().push(ScriptedCall {
                alias: self.alias.clone(),
                system: system.to_owned(),
                user: user.to_owned(),
            });
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ScriptedReply::None)
                .render(user)
        }
    }

    #[derive(Debug)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    #[derive(Debug)]
    struct StepClock {
        next: AtomicI64,
        step: i64,
    }

    impl StepClock {
        fn new(start: i64, step: i64) -> Self {
            Self {
                next: AtomicI64::new(start),
                step,
            }
        }
    }

    impl Clock for StepClock {
        fn now_ms(&self) -> i64 {
            self.next.fetch_add(self.step, Ordering::SeqCst)
        }
    }

    fn scripted_resolver(
        handles: Vec<ScriptedHandle>,
    ) -> impl FnMut(&str) -> anyhow::Result<ScriptedHandle> {
        let handles: HashMap<String, ScriptedHandle> = handles
            .into_iter()
            .map(|handle| (handle.alias.clone(), handle))
            .collect();
        move |alias| {
            handles
                .get(alias)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown scripted model {alias}"))
        }
    }

    fn sequential_request(
        models: &[&str],
        evidence: SequentialEvidenceConfig,
        max_rounds: usize,
        deadline_ms_from_now: i64,
    ) -> ConveneRequest {
        ConveneRequest {
            question: "Which proposal should the council adopt?".to_owned(),
            federation: Federation::Commons,
            trigger: ConveneTrigger::UserRequest,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            quorum: QuorumConfig {
                rule: QuorumRule::SequentialEvidence(evidence),
                mark_ttl_ms: 60_000,
                tie_break_seed: 11,
            },
            deadline_ms_from_now,
            max_rounds,
        }
    }

    fn has_abort(events: &[LonghouseEvent], expected: AbortReason) -> bool {
        events.iter().any(
            |event| matches!(event, LonghouseEvent::Aborted { reason, .. } if *reason == expected),
        )
    }

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    fn sequential_engine() -> QuorumEngine {
        QuorumEngine::new(QuorumConfig {
            rule: QuorumRule::SequentialEvidence(SequentialEvidenceConfig::default()),
            mark_ttl_ms: 60_000,
            tie_break_seed: 11,
        })
    }

    // ---- TASK-5: full convene acquire-loop integration ---------------------

    #[tokio::test]
    async fn lone_proposal_generates_rival_resumes_and_converges() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new("a", "group-a", [ScriptedReply::Text("Proposal A")], &calls),
            ScriptedHandle::new(
                "b",
                "group-b",
                [ScriptedReply::None, ScriptedReply::Text("Proposal B")],
                &calls,
            ),
            ScriptedHandle::new(
                "c",
                "group-c",
                [
                    ScriptedReply::None,
                    ScriptedReply::Text("PASS"),
                    ScriptedReply::Endorse("Proposal A"),
                ],
                &calls,
            ),
        ];
        let default = SequentialEvidenceConfig::default();
        let evidence = SequentialEvidenceConfig::new(
            default.target_error(),
            default.default_reliability(),
            default.correlation_cap(),
            1.0,
            default.decision_loss(),
        )
        .unwrap();
        let req = sequential_request(&["a", "b", "c"], evidence, 3, 10_000);
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &FixedClock(0),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert!(outcome.decision.is_some());
        assert_eq!(outcome.convergence_basis, Some(ConvergenceBasis::CostBound));
        assert_eq!(outcome.proposals.len(), 2);
        assert_eq!(
            outcome
                .decision
                .and_then(|decision| outcome.proposals.get(&decision))
                .map(String::as_str),
            Some("Proposal A")
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, LonghouseEvent::Converged { .. })));
        assert!(!has_abort(&events, AbortReason::InsufficientAlternatives));
        let proposal_marks = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    LonghouseEvent::MarkPosted {
                        mark: Mark {
                            kind: MarkKind::Proposal,
                            ..
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(proposal_marks, 2, "the rival must land on the real board");
        let calls = calls.lock().unwrap();
        let rival_call = calls
            .iter()
            .position(|call| call.system == RIVAL_SYSTEM)
            .expect("the lone field must enter bounded rival generation");
        let resumed_review = calls
            .iter()
            .position(|call| call.alias == "c" && call.system == ROUND2_SYSTEM)
            .expect("the landed rival must resume normal review planning");
        assert!(rival_call < resumed_review);
    }

    #[tokio::test]
    async fn zero_review_budget_aborts_insufficient_without_extra_calls() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new("a", "group-a", [ScriptedReply::Text("Proposal A")], &calls),
            ScriptedHandle::new("b", "group-b", [ScriptedReply::None], &calls),
            ScriptedHandle::new("c", "group-c", [ScriptedReply::None], &calls),
        ];
        let req = sequential_request(
            &["a", "b", "c"],
            SequentialEvidenceConfig::default(),
            1,
            10_000,
        );
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &FixedClock(0),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert_eq!(outcome.decision, None);
        assert_eq!(outcome.convergence_basis, None);
        assert!(has_abort(&events, AbortReason::InsufficientAlternatives));
        assert!(!has_abort(&events, AbortReason::Timeout));
        assert!(!events
            .iter()
            .any(|event| matches!(event, LonghouseEvent::Converged { .. })));
        let terminal: Vec<AbortReason> = events
            .iter()
            .filter_map(|event| match event {
                LonghouseEvent::Aborted { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect();
        assert_eq!(terminal, vec![AbortReason::InsufficientAlternatives]);
        assert!(matches!(
            events.as_slice(),
            [
                ..,
                LonghouseEvent::Aborted {
                    reason: AbortReason::InsufficientAlternatives,
                    ..
                },
                LonghouseEvent::TopicClosed { .. }
            ]
        ));
        assert_eq!(outcome.proposals.len(), 1);
        assert_eq!(outcome.recording.marks.len(), 1);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "only the concurrent proposal round may run");
        assert!(calls.iter().all(|call| call.system == ROUND1_SYSTEM));
    }

    #[tokio::test]
    async fn deadline_still_aborts_as_timeout() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new("a", "group-a", [ScriptedReply::Text("Proposal A")], &calls),
            ScriptedHandle::new("b", "group-b", [ScriptedReply::Text("Proposal B")], &calls),
        ];
        let req = sequential_request(&["a", "b"], SequentialEvidenceConfig::default(), 4, 0);
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &FixedClock(0),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert_eq!(outcome.decision, None);
        assert_eq!(outcome.convergence_basis, None);
        assert!(has_abort(&events, AbortReason::Timeout));
        assert!(!has_abort(&events, AbortReason::InsufficientAlternatives));
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn request_evidence_runs_in_convene_and_remains_non_weight_bearing() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new(
                "a",
                "group-a",
                [
                    ScriptedReply::Text("Proposal A"),
                    ScriptedReply::Text("A concrete falsifiable artifact"),
                ],
                &calls,
            ),
            ScriptedHandle::new("b", "group-b", [ScriptedReply::Text("Proposal B")], &calls),
            ScriptedHandle::new(
                "c",
                "group-c",
                [ScriptedReply::None, ScriptedReply::Endorse("Proposal A")],
                &calls,
            ),
        ];
        let default = SequentialEvidenceConfig::default();
        let evidence = SequentialEvidenceConfig::new(
            default.target_error(),
            default.default_reliability(),
            default.correlation_cap(),
            0.0,
            default.decision_loss(),
        )
        .unwrap();
        let req = sequential_request(&["a", "b", "c"], evidence, 3, 10_000);
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &FixedClock(0),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert_eq!(outcome.decision, None);
        assert_eq!(outcome.recording.marks.len(), 3);
        let evidence_marks: Vec<&Mark> = events
            .iter()
            .filter_map(|event| match event {
                LonghouseEvent::MarkPosted { mark, .. } if mark.kind == MarkKind::Evidence => {
                    Some(mark)
                }
                _ => None,
            })
            .collect();
        assert_eq!(evidence_marks.len(), 1);
        assert!(evidence_marks[0].target.is_some());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.system == EVIDENCE_SYSTEM)
                .count(),
            1,
            "the same stable plan cannot re-request evidence"
        );
        assert!(calls
            .iter()
            .any(|call| call.user.contains("strongest concrete evidence")));
        let evidence_event = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    LonghouseEvent::MarkPosted {
                        mark: Mark {
                            kind: MarkKind::Evidence,
                            ..
                        },
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            events[evidence_event + 1..]
                .iter()
                .all(|event| !matches!(event, LonghouseEvent::QuorumUpdated { .. })),
            "a rationale artifact must not trigger an evidence-field update"
        );
    }

    #[tokio::test]
    async fn acquire_loop_challenges_with_a_headroom_bearing_reviewer() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new("a", "group-a", [ScriptedReply::Text("Proposal A")], &calls),
            ScriptedHandle::new("b", "group-b", [ScriptedReply::Text("Proposal B")], &calls),
            ScriptedHandle::new(
                "c",
                "group-a",
                [ScriptedReply::None, ScriptedReply::Inhibit("Proposal B")],
                &calls,
            ),
        ];
        let default = SequentialEvidenceConfig::default();
        let evidence = SequentialEvidenceConfig::new(
            0.01,
            default.default_reliability(),
            default.correlation_cap(),
            0.0,
            default.decision_loss(),
        )
        .unwrap();
        let mut req = sequential_request(&["a", "b", "c"], evidence, 2, 2_000);
        // Both proposal groups retain positive mass at the deadline, while
        // normal decay has opened cap headroom for correlated reviewer c.
        req.quorum.mark_ttl_ms = 10_000;
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &StepClock::new(0, 100),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert_eq!(outcome.decision, None);
        let calls = calls.lock().unwrap();
        let challenge = calls
            .iter()
            .find(|call| call.alias == "c" && call.user.contains("Adversarial comparison"))
            .expect("the planner must route c through ChallengeLeader");
        assert_eq!(challenge.system, ROUND2_SYSTEM);
        assert!(outcome.recording.marks.iter().any(|mark| {
            matches!(
                mark.kind,
                RecordedMarkKind::Inhibit { proposal }
                    if outcome.proposals.get(&proposal).map(String::as_str) == Some("Proposal B")
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                LonghouseEvent::MarkPosted {
                    mark: Mark {
                        kind: MarkKind::Inhibit,
                        summary,
                        ..
                    },
                    ..
                } if summary == "scripted challenge"
            )
        }));
    }

    #[tokio::test]
    async fn projected_lead_flip_reasserts_the_oldest_supporter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new(
                "a",
                "group-a",
                [
                    ScriptedReply::Text("Proposal A"),
                    ScriptedReply::Endorse("Proposal A"),
                ],
                &calls,
            ),
            ScriptedHandle::new("b", "group-b", [ScriptedReply::Text("Proposal B")], &calls),
            ScriptedHandle::new(
                "c",
                "group-c",
                [ScriptedReply::None, ScriptedReply::Endorse("Proposal A")],
                &calls,
            ),
            ScriptedHandle::new(
                "d",
                "group-b",
                [ScriptedReply::None, ScriptedReply::Endorse("Proposal B")],
                &calls,
            ),
        ];
        let default = SequentialEvidenceConfig::default();
        let evidence = SequentialEvidenceConfig::new(
            0.01,
            default.default_reliability(),
            default.correlation_cap(),
            0.0,
            default.decision_loss(),
        )
        .unwrap();
        let mut req = sequential_request(&["a", "b", "c", "d"], evidence, 2, 1_250);
        req.quorum.mark_ttl_ms = 1_000;
        let quorum = req.quorum;
        let mut events = Vec::new();

        let outcome = convene_with_resolver(
            req,
            &StepClock::new(0, 50),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        assert_eq!(outcome.decision, None);
        let calls = calls.lock().unwrap();
        let review_aliases: Vec<&str> = calls
            .iter()
            .filter(|call| call.system == ROUND2_SYSTEM)
            .map(|call| call.alias.as_str())
            .collect();
        assert!(
            review_aliases.starts_with(&["c", "d", "a"]),
            "after independent sample c and correlated challenge d, a cap-transition A-to-B lead flip must re-poll oldest A supporter a; got {review_aliases:?}"
        );
        let challenge = calls
            .iter()
            .find(|call| call.alias == "d" && call.user.contains("Adversarial comparison"))
            .expect("correlated reviewer d must challenge through the real loop");
        assert!(challenge.user.contains("current leader"));
        let reassert = calls
            .iter()
            .find(|call| call.alias == "a" && call.system == ROUND2_SYSTEM)
            .expect("oldest leader supporter a must be reasserted");
        assert!(
            reassert.user.contains("Re-assert your current stance"),
            "expected reassertion prompt, got: {}",
            reassert.user
        );
        assert!(outcome.recording.marks.iter().any(|mark| {
            matches!(
                mark.kind,
                RecordedMarkKind::Endorse { proposal }
                    if outcome.proposals.get(&proposal).map(String::as_str) == Some("Proposal A")
                        && mark.at_ms >= 650
            )
        }));

        // Rebuild the exact pre-reassertion field from the real run. At the
        // planner capture instant A is the unique leader; at the deadline the
        // cap-aware trajectory flips to B because B's correlated group stays
        // capped while A's independent mass decays. This prevents the test
        // from passing on an arbitrary or prompt-only re-poll.
        let proposal_a = outcome
            .proposals
            .iter()
            .find_map(|(id, text)| (text == "Proposal A").then_some(*id))
            .unwrap();
        let proposal_b = outcome
            .proposals
            .iter()
            .find_map(|(id, text)| (text == "Proposal B").then_some(*id))
            .unwrap();
        let author_a = outcome
            .recording
            .marks
            .iter()
            .find_map(|mark| {
                matches!(
                    mark.kind,
                    RecordedMarkKind::Propose { proposal } if proposal == proposal_a
                )
                .then_some(mark.author)
            })
            .unwrap();
        let mut pre_reassert = QuorumEngine::new(quorum);
        for reviewer in &outcome.recording.reviewers {
            pre_reassert.register_reviewer(
                ReviewerCredential::new(
                    reviewer.agent_id,
                    reviewer.correlation_group.clone(),
                    reviewer.reliability_prior,
                )
                .unwrap(),
            );
        }
        for mark in outcome
            .recording
            .marks
            .iter()
            .filter(|mark| mark.at_ms < 650)
        {
            match mark.kind {
                RecordedMarkKind::Propose { proposal } => {
                    pre_reassert.propose(proposal, mark.author, mark.at_ms)
                }
                RecordedMarkKind::Endorse { proposal } => {
                    pre_reassert.endorse(proposal, mark.author, None, mark.at_ms)
                }
                RecordedMarkKind::Inhibit { proposal } => {
                    pre_reassert.inhibit(proposal, mark.author, None, mark.at_ms)
                }
            }
        }
        let assessment = pre_reassert.assessment(600).unwrap();
        assert_eq!(
            assessment
                .snapshot()
                .unique_leader()
                .map(|item| item.proposal),
            Some(proposal_a)
        );
        assert_eq!(
            assessment
                .trajectory()
                .snapshot_at(1_250)
                .unique_leader()
                .map(|item| item.proposal),
            Some(proposal_b)
        );
        let runner_cap_exit = assessment
            .trajectory()
            .cap_transition_times()
            .into_iter()
            .find(|transition| {
                matches!(
                    &transition.group,
                    crate::evidence::GroupId::Registered(group) if group == "group-b"
                )
            })
            .expect("the correlated runner must be capped at capture");
        assert!(
            runner_cap_exit.at_ms > 1_250.0,
            "the projected flip must occur while the runner is still capped"
        );
        assert!(matches!(
            ReviewPlanner::plan(
                &assessment,
                600,
                1_250,
                2.0,
                &pre_reassert.reviewers().cloned().collect::<Vec<_>>()
            ),
            PlanOutcome::Continue(ReviewAction::ReassertAfterDecay {
                proposal,
                reviewer,
                ..
            }) if proposal == proposal_a && reviewer == author_a
        ));
    }

    // ---- TASK-7: pre-registration proposal consolidation --------------------

    /// One duplicate definition powers both round-1 consolidation and rival
    /// filtering. Conservative: paraphrases of one answer merge; answers
    /// naming different technologies never do.
    #[test]
    fn duplicate_check_merges_paraphrases_and_keeps_rivals() {
        let canonical = "IndexedDB transactional writes handle transcript persistence with \
                         indexed queries and eviction control";
        let paraphrase = "IndexedDB handles transcript persistence: transactional writes, \
                          indexed queries, eviction control";
        let rival = "Cache API pairs with service workers for response storage and a \
                     simpler offline retrieval model";
        assert!(answers_are_duplicates(canonical, paraphrase));
        assert!(answers_are_duplicates(paraphrase, canonical), "symmetric");
        assert!(
            answers_are_duplicates(canonical, canonical),
            "exact restatement"
        );
        assert!(!answers_are_duplicates(canonical, rival));

        // Token floor: answers too short for similarity to mean anything fall
        // back to exact equality — "Proposal A" and "Proposal B" share every
        // content token but must never merge.
        assert!(!answers_are_duplicates("Proposal A", "Proposal B"));
        assert!(answers_are_duplicates("Proposal A", "proposal a"));

        // Negation veto (codex's blocker): a directly contradictory rival
        // differing only by negation must NEVER merge, no matter how long the
        // shared remainder is — one negation token inside a long answer would
        // clear any Jaccard threshold, so presence-of-negation is a hard veto,
        // not a similarity input.
        let affirm = "the council should adopt IndexedDB for transcript persistence \
                      because transactional writes handle incremental updates";
        let negate = "the council should not adopt IndexedDB for transcript persistence \
                      because transactional writes handle incremental updates poorly";
        assert!(!answers_are_duplicates(affirm, negate));
        assert!(!answers_are_duplicates(negate, affirm), "veto is symmetric");

        // Contractions must register as negations: a bare alphanumeric split
        // turns "can't" into ["can","t"] and the veto silently vanishes. Both
        // ASCII and curly apostrophes are normalized before tokenization.
        let can = "the council can adopt IndexedDB for transcript persistence safely";
        let cant_ascii = "the council can't adopt IndexedDB for transcript persistence safely";
        let cant_curly =
            "the council can\u{2019}t adopt IndexedDB for transcript persistence safely";
        assert!(!answers_are_duplicates(can, cant_ascii));
        assert!(!answers_are_duplicates(can, cant_curly));

        // Boolean negation-presence is not enough: "avoid X" and "do not
        // avoid X" both contain negation words yet point opposite directions.
        // The SIGNATURE ({avoid:1} vs {avoid:1, not:1}) differs, so they veto.
        let avoid = "avoid IndexedDB for transcript persistence because eviction \
                     behavior is unpredictable across browsers";
        let do_not_avoid = "do not avoid IndexedDB for transcript persistence because eviction \
                            behavior is unpredictable across browsers";
        assert!(!answers_are_duplicates(avoid, do_not_avoid));
        assert!(
            !answers_are_duplicates(do_not_avoid, avoid),
            "veto is symmetric"
        );

        // is_distinct_rival now rejects near-duplicates, not just exact ones —
        // the same definition, so late rivals can't re-fragment the field.
        let mut proposals = HashMap::new();
        proposals.insert(Uuid::new_v4(), canonical.to_owned());
        assert!(!is_distinct_rival(paraphrase, &proposals));
        assert!(is_distinct_rival(rival, &proposals));
    }

    /// Ambiguous-match determinism (codex's blocker): when an answer clears
    /// the duplicate threshold against MORE THAN ONE registered hypothesis,
    /// the canonical must be the first-REGISTERED match — a HashMap scan would
    /// pick a process-randomized winner.
    #[test]
    fn ambiguous_duplicate_folds_into_first_registered_canonical() {
        // Token design: a = t1..t10, b = t5..t14 (distinct from a: 6/14 ≈
        // 0.43 < 0.6), c = t3..t12 (duplicates BOTH: 8/12 ≈ 0.67 ≥ 0.6).
        let words: Vec<&str> = vec![
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima", "mike", "november",
        ];
        let text = |range: std::ops::Range<usize>| words[range].join(" ");
        let first = text(0..10);
        let second = text(4..14);
        let ambiguous = text(2..12);
        assert!(!answers_are_duplicates(&first, &second));
        assert!(answers_are_duplicates(&first, &ambiguous));
        assert!(answers_are_duplicates(&second, &ambiguous));

        let id_first = Uuid::new_v4();
        let id_second = Uuid::new_v4();
        let mut proposals = HashMap::new();
        proposals.insert(id_first, first);
        proposals.insert(id_second, second);
        // Registration order decides, whichever way the map happens to hash.
        assert_eq!(
            find_duplicate_canonical(&[id_first, id_second], &proposals, &ambiguous),
            Some(id_first)
        );
        assert_eq!(
            find_duplicate_canonical(&[id_second, id_first], &proposals, &ambiguous),
            Some(id_second)
        );
    }

    /// d1 — the live finding, fixed: a unanimous distinct-group council must
    /// CONVERGE. Consolidation folds the duplicates into endorses of one
    /// canonical proposal; the resulting lone field escalates through rival
    /// generation (identifiability requires an alternative); once a genuine
    /// rival lands, the unanimous evidence crosses EvidenceBound.
    #[tokio::test]
    async fn unanimous_distinct_group_council_converges() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let canonical = "IndexedDB transactional writes handle transcript persistence with \
                         indexed queries and eviction control";
        let handles = vec![
            ScriptedHandle::new("a", "group-a", [ScriptedReply::Text(canonical)], &calls),
            ScriptedHandle::new(
                "b",
                "group-b",
                [
                    ScriptedReply::Text(
                        "IndexedDB handles transcript persistence: transactional writes, \
                         indexed queries, eviction control",
                    ),
                    ScriptedReply::Text(
                        "Cache API pairs with service workers for response storage and a \
                         simpler offline retrieval model",
                    ),
                ],
                &calls,
            ),
            ScriptedHandle::new(
                "c",
                "group-c",
                [
                    ScriptedReply::Text(
                        "transcript persistence belongs in IndexedDB: transactional writes \
                         plus indexed queries and eviction control",
                    ),
                    ScriptedReply::Text("PASS"),
                ],
                &calls,
            ),
            ScriptedHandle::new(
                "d",
                "group-d",
                [
                    ScriptedReply::Text(
                        "IndexedDB, for transcript persistence — transactional writes, \
                         indexed queries, eviction control",
                    ),
                    ScriptedReply::Text("PASS"),
                ],
                &calls,
            ),
        ];
        let req = sequential_request(
            &["a", "b", "c", "d"],
            SequentialEvidenceConfig::default(),
            2,
            60_000,
        );
        let mut events = Vec::new();
        let outcome = convene_with_resolver(
            req,
            &StepClock::new(0, 50),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        // Consolidation registered ONE canonical proposal from the unanimous
        // four, plus the solicited rival.
        assert_eq!(outcome.proposals.len(), 2);
        let canonical_id = outcome
            .proposals
            .iter()
            .find_map(|(id, text)| (text == canonical).then_some(*id))
            .expect("canonical proposal registered");
        let consolidated_endorses = outcome
            .recording
            .marks
            .iter()
            .filter(|mark| {
                matches!(mark.kind, RecordedMarkKind::Endorse { proposal } if proposal == canonical_id)
            })
            .count();
        assert_eq!(consolidated_endorses, 3, "b, c, d fold into endorses");

        // The unanimous answer converges on the evidence bound once the field
        // is identifiable — never a coin flip, never a timeout.
        assert_eq!(outcome.decision, Some(canonical_id));
        assert_eq!(
            outcome.convergence_basis,
            Some(ConvergenceBasis::EvidenceBound)
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, LonghouseEvent::Converged { decision, .. } if *decision == canonical_id)));
    }

    /// d2 — the anti-echo-chamber proof: a SAME-GROUP unanimous council must
    /// NOT converge. Consolidation turns the echo into capped same-group
    /// endorses, so total group evidence stays cap-bound and the field ends
    /// honestly open (budget-exhausted Split), no matter how loudly one
    /// correlation group agrees with itself.
    #[tokio::test]
    async fn same_group_unanimous_council_stays_unconverged() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handles = vec![
            ScriptedHandle::new(
                "a",
                "same-model",
                [
                    ScriptedReply::Text(
                        "IndexedDB transactional writes handle transcript persistence with \
                         indexed queries and eviction control",
                    ),
                    ScriptedReply::Text("evidence: internal benchmark, 2ms per write"),
                ],
                &calls,
            ),
            ScriptedHandle::new(
                "b",
                "same-model",
                [
                    ScriptedReply::Text(
                        "IndexedDB handles transcript persistence: transactional writes, \
                         indexed queries, eviction control",
                    ),
                    ScriptedReply::Text("PASS"),
                ],
                &calls,
            ),
            ScriptedHandle::new(
                "c",
                "same-model",
                [
                    ScriptedReply::Text(
                        "transcript persistence belongs in IndexedDB: transactional writes \
                         plus indexed queries and eviction control",
                    ),
                    ScriptedReply::Text(
                        "Cache API pairs with service workers for response storage and a \
                         simpler offline retrieval model",
                    ),
                ],
                &calls,
            ),
        ];
        let req = sequential_request(
            &["a", "b", "c"],
            SequentialEvidenceConfig::default(),
            2,
            60_000,
        );
        let mut events = Vec::new();
        let outcome = convene_with_resolver(
            req,
            &StepClock::new(0, 50),
            |event| events.push(event),
            scripted_resolver(handles),
        )
        .await;

        // The echo chamber consolidated (one canonical + capped endorses) and a
        // rival landed — but one correlation group's agreement is one unit of
        // evidence, so nothing may converge.
        assert_eq!(outcome.decision, None);
        assert_eq!(outcome.convergence_basis, None);
        assert!(!events
            .iter()
            .any(|event| matches!(event, LonghouseEvent::Converged { .. })));
        assert!(
            has_abort(&events, AbortReason::Split),
            "honest pre-deadline split once review budget is spent"
        );
    }

    // ---- TASK-4: escalation routing + abort taxonomy -----------------------

    /// The routing table IS the loop's escalation behavior: LoneProposal is
    /// the only escalation that spends more resources, its exhaustion is the
    /// only direct abort, and everything else resolves through the honest
    /// Resolve section. `EscalationRoute` has no commitment variant, so no
    /// escalation can be converted into `Converged`.
    #[test]
    fn escalation_routing_is_exhaustive_and_never_commits() {
        assert_eq!(
            route_escalation(EscalationReason::LoneProposal, false, 1.0),
            EscalationRoute::GenerateRivals
        );
        assert_eq!(
            route_escalation(EscalationReason::LoneProposal, true, 1.0),
            EscalationRoute::AbortInsufficientAlternatives
        );
        assert_eq!(
            route_escalation(EscalationReason::LoneProposal, false, 0.0),
            EscalationRoute::AbortInsufficientAlternatives,
            "a zero review budget cannot be overspent on rival generation"
        );
        for reason in [
            EscalationReason::TiedField,
            EscalationReason::BudgetExhausted,
            EscalationReason::DeadlineReached,
            EscalationReason::NoEligibleReviewers,
        ] {
            for spent in [false, true] {
                assert_eq!(
                    route_escalation(reason, spent, 1.0),
                    EscalationRoute::ResolveEarly
                );
            }
        }
    }

    /// `InsufficientAlternatives` is unreachable through `force_resolve`:
    /// sequential mode returns the caller's reason verbatim as `Err` and never
    /// latches a basis, and convene's Resolve callsite only ever passes
    /// `Timeout` or `Split`. The direct-abort path is the ONLY emitter.
    #[test]
    fn insufficient_alternatives_never_flows_through_force_resolve() {
        let topic_id = uid(99);
        let event = DirectAbort::InsufficientAlternatives.event(topic_id);
        assert!(matches!(
            event,
            LonghouseEvent::Aborted {
                topic_id: emitted_topic,
                reason: AbortReason::InsufficientAlternatives,
            } if emitted_topic == topic_id
        ));
        assert_eq!(
            serde_json::to_value(AbortReason::InsufficientAlternatives).unwrap(),
            serde_json::json!("insufficient_alternatives")
        );

        for reason in [AbortReason::Timeout, AbortReason::Split] {
            let mut engine = sequential_engine();
            engine.propose(uid(1), uid(10), 0);
            let result = engine.force_resolve(1_000, reason, true);
            assert_eq!(result, Err(reason), "sequential passes reason through");
            assert!(!engine.is_converged(), "no forced basis may latch");
            assert_eq!(engine.convergence_basis(), None);
        }
    }

    #[test]
    fn timeout_split_and_commitment_remain_distinct_terminal_routes() {
        let mut deadline_pending = sequential_engine();
        deadline_pending.propose(uid(1), uid(10), 0);
        deadline_pending.propose(uid(2), uid(20), 0);
        assert_eq!(
            resolve_engine(&mut deadline_pending, 1_000, 1_000),
            ResolutionOutcome::Aborted(AbortReason::Timeout)
        );
        assert_eq!(deadline_pending.convergence_basis(), None);

        let mut early_pending = sequential_engine();
        early_pending.propose(uid(1), uid(10), 0);
        early_pending.propose(uid(2), uid(20), 0);
        assert_eq!(
            resolve_engine(&mut early_pending, 500, 1_000),
            ResolutionOutcome::Aborted(AbortReason::Split)
        );
        assert_eq!(early_pending.convergence_basis(), None);

        let mut committed = sequential_engine();
        committed.propose(uid(1), uid(10), 0);
        committed.propose(uid(2), uid(20), 0);
        committed.endorse(uid(1), uid(30), None, 0);
        committed.endorse(uid(1), uid(40), None, 0);
        assert_eq!(
            resolve_engine(&mut committed, 0, 1_000),
            ResolutionOutcome::Committed(uid(1))
        );
        assert!(committed.convergence_basis().is_some());
    }

    #[test]
    fn rival_generation_rejects_pass_blank_and_exact_restatements() {
        let mut proposals = HashMap::new();
        proposals.insert(uid(1), "Existing answer".to_string());

        assert!(!is_distinct_rival("PASS", &proposals));
        assert!(!is_distinct_rival("  pass  ", &proposals));
        assert!(!is_distinct_rival("`PASS.`", &proposals));
        assert!(!is_distinct_rival("   ", &proposals));
        assert!(!is_distinct_rival(" existing answer ", &proposals));
        assert!(is_distinct_rival(
            "A genuinely different answer",
            &proposals
        ));
    }

    /// LoneProposal escalation, rival registration, resume: the planner
    /// escalates on a one-hypothesis field; after a rival proposal registers,
    /// a FRESH assessment resumes normal planning with a review action.
    #[test]
    fn rival_registration_resumes_planning() {
        let config = SequentialEvidenceConfig::default();
        let mut engine = sequential_engine();
        let roster: Vec<ReviewerCredential> = [(uid(10), "a"), (uid(20), "b"), (uid(30), "c")]
            .into_iter()
            .map(|(reviewer, group)| {
                ReviewerCredential::with_default_prior(reviewer, group, config).unwrap()
            })
            .collect();
        for credential in &roster {
            engine.register_reviewer(credential.clone());
        }
        engine.propose(uid(1), uid(10), 0);

        let lone = engine.assessment(0).unwrap();
        assert_eq!(
            ReviewPlanner::plan(&lone, 0, 10_000, 5.0, &roster),
            PlanOutcome::NeedsEscalation(EscalationReason::LoneProposal)
        );
        assert_eq!(
            route_escalation(EscalationReason::LoneProposal, false, 1.0),
            EscalationRoute::GenerateRivals
        );

        // A rival lands (what the bounded generation pass does on success).
        engine.propose(uid(2), uid(20), 1);
        let fresh = engine.assessment(1).unwrap();
        assert!(
            matches!(
                ReviewPlanner::plan(&fresh, 1, 10_000, 5.0, &roster),
                PlanOutcome::Continue(_)
            ),
            "a two-proposal field must resume review planning"
        );
    }

    /// Every accepted stance is followed by a new assessment in the loop. This
    /// composition test pins the observable consequence: the capture instant,
    /// canonical snapshot, and next action all update after the stance lands.
    #[test]
    fn accepted_stance_rebuilds_assessment_before_next_plan() {
        let config = SequentialEvidenceConfig::default();
        let mut engine = sequential_engine();
        let roster: Vec<ReviewerCredential> = [(uid(10), "a"), (uid(20), "b"), (uid(30), "c")]
            .into_iter()
            .map(|(reviewer, group)| {
                ReviewerCredential::with_default_prior(reviewer, group, config).unwrap()
            })
            .collect();
        for credential in &roster {
            engine.register_reviewer(credential.clone());
        }
        engine.propose(uid(1), uid(10), 0);
        engine.propose(uid(2), uid(20), 0);

        let before = engine.assessment(0).unwrap();
        assert!(matches!(
            ReviewPlanner::plan(&before, 0, 10_000, 5.0, &roster),
            PlanOutcome::Continue(ReviewAction::SampleIndependent { reviewer, .. })
                if reviewer == uid(30)
        ));

        engine.endorse(uid(1), uid(30), None, 1);
        let fresh = engine.assessment(1).unwrap();
        assert_eq!(fresh.trajectory().captured_at_ms(), 1);
        assert_eq!(
            fresh.snapshot(),
            &fresh.trajectory().snapshot_at(1),
            "the rebuilt assessment must share the commitment evaluator"
        );
        assert_ne!(before.snapshot(), fresh.snapshot());
        assert!(!matches!(
            ReviewPlanner::plan(&fresh, 1, 10_000, 4.0, &roster),
            PlanOutcome::Continue(ReviewAction::SampleIndependent { reviewer, .. })
                if reviewer == uid(30)
        ));
    }

    #[test]
    fn evidence_mark_is_observable_non_weight_bearing_and_bounded() {
        let mut engine = sequential_engine();
        engine.propose(uid(1), uid(10), 0);
        engine.propose(uid(2), uid(20), 0);
        let before = engine.assessment(0).unwrap().snapshot().clone();
        let mut events = Vec::new();

        let note = emit_evidence(
            uid(90),
            uid(10),
            uid(1),
            "A falsifiable test result",
            &mut |event| events.push(event),
        );

        assert_eq!(before, *engine.assessment(0).unwrap().snapshot());
        assert_eq!(note, "- A falsifiable test result");
        assert!(matches!(
            events.as_slice(),
            [LonghouseEvent::MarkPosted {
                topic_id,
                mark: Mark {
                    author,
                    kind: MarkKind::Evidence,
                    target: Some(target),
                    ..
                },
            }] if *topic_id == uid(90) && *author == uid(10) && *target == uid(1)
        ));

        let mut requested = HashSet::new();
        assert!(claim_evidence_request(&mut requested, uid(1)));
        assert!(!claim_evidence_request(&mut requested, uid(1)));
    }

    /// The challenge prompt numbers proposals exactly like `projection_text`
    /// (stable sorted-id order), so a parsed vote lands on the intended
    /// proposal.
    #[test]
    fn challenge_prompt_numbering_matches_projection_order() {
        let ids = vec![uid(9), uid(3)];
        let prompt = challenge_user("q", "projection", &ids, uid(9), uid(3));
        // Sorted order: uid(3) is proposal 1, uid(9) is proposal 2.
        assert!(prompt.contains("proposal 2 (current leader)"));
        assert!(prompt.contains("proposal 1 (runner-up)"));
    }

    #[test]
    fn parse_vote_endorse() {
        let ids = vec![uid(5), uid(1), uid(9)]; // sorted -> [1,5,9]
        let v = parse_vote("ENDORSE 2: strongest evidence", &ids).unwrap();
        assert_eq!(v.kind, VoteKind::Endorse);
        assert_eq!(v.target, uid(5)); // index 2 in sorted order
        assert_eq!(v.rationale, "strongest evidence");
    }

    #[test]
    fn parse_vote_inhibit_with_noise() {
        let ids = vec![uid(1), uid(2)];
        let v = parse_vote("blah\nINHIBIT 1: too risky\nthanks", &ids).unwrap();
        assert_eq!(v.kind, VoteKind::Inhibit);
        assert_eq!(v.target, uid(1));
        assert_eq!(v.rationale, "too risky");
    }

    #[test]
    fn parse_vote_rejects_out_of_range() {
        let ids = vec![uid(1)];
        assert!(parse_vote("ENDORSE 5: nope", &ids).is_none());
        assert!(parse_vote("no vote here", &ids).is_none());
    }

    #[test]
    fn projection_is_numbered_and_stable() {
        let mut p = HashMap::new();
        p.insert(uid(9), "answer nine".to_string());
        p.insert(uid(1), "answer one".to_string());
        let text = projection_text(&p);
        // Sorted by id -> uid(1) first.
        assert!(text.starts_with("1. answer one"));
        assert!(text.contains("2. answer nine"));
    }

    // --- OCEAN-59: the firekeeper accountability brake -----------------------

    use crate::quorum::{QuorumConfig, QuorumEngine, QuorumRule};

    fn fast_quorum() -> QuorumConfig {
        QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        }
    }

    // The core OCEAN-59 guarantee: a firekeeper that tries to ratify `Converged`
    // while the quorum engine is still `Pending` is REFUSED. A premature
    // Converged claim must never be accepted — even with a valid title token.
    #[test]
    fn claim_outcome_rejects_premature_converged() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let proposer = uid(10);
        // Legitimately-titled firekeeper presenting the real token: the identity
        // boundary passes, so this isolates the convergence brake.
        let title = FirekeeperTitle::mint(proposer);
        let t = 0;

        // Only the proposer's implicit endorse: net 1.0 < cutoff 2.0 -> Pending.
        eng.propose(proposal, proposer, t);
        assert!(
            !eng.is_converged(),
            "engine must still be pending before the gate"
        );

        // The firekeeper jumps the gun and tries to ratify Converged anyway.
        let result = claim_outcome(&mut eng, &title, proposer, Some(title.token()), proposal, t);
        assert_eq!(
            result,
            Err(ClaimError::NotConverged),
            "a premature Converged claim must be rejected"
        );
        // And the rejected claim must not have latched convergence as a side effect.
        assert!(!eng.is_converged());
    }

    // Once the quorum engine genuinely converges, the firekeeper may ratify the
    // engine's decision — the gate opens — provided it proves its title.
    #[test]
    fn claim_outcome_accepts_when_quorum_converged() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let (a, b) = (uid(10), uid(11));
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        eng.propose(proposal, a, t); // net 1.0
        eng.endorse(proposal, b, None, t); // net 2.0 -> crosses cutoff, no rival
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        // The firekeeper ratifies the engine's own decision with its token: accepted.
        assert_eq!(
            claim_outcome(&mut eng, &title, a, Some(title.token()), proposal, t),
            Ok(())
        );
    }

    // A firekeeper may only sign the engine's decision, not substitute its own:
    // claiming a *different* proposal than the converged one is refused (even
    // with a valid title token).
    #[test]
    fn claim_outcome_rejects_wrong_decision() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let (winner, other) = (uid(1), uid(2));
        let (a, b) = (uid(10), uid(11));
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        eng.propose(winner, a, t);
        eng.endorse(winner, b, None, t); // winner net 2.0 -> converges on `winner`
        eng.propose(other, uid(20), t); // a rival proposal exists but didn't win
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        let result = claim_outcome(&mut eng, &title, a, Some(title.token()), other, t);
        assert_eq!(
            result,
            Err(ClaimError::WrongDecision {
                engine_decision: winner,
                claimed: other,
            }),
            "firekeeper may not ratify a proposal the engine did not choose"
        );
    }

    // The deadline path: a force-resolved topic latches `converged`, so a
    // firekeeper bound after force_resolve can ratify it — the gate respects the
    // timeout resolution exactly as the convene() flow relies on.
    #[test]
    fn claim_outcome_accepts_after_force_resolve_timeout() {
        let cfg = QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 10.0, // unreachably high; only the deadline resolves it
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        };
        let mut eng = QuorumEngine::new(cfg);
        let (winner, runner) = (uid(1), uid(2));
        let title = FirekeeperTitle::mint(uid(10));
        eng.propose(winner, uid(10), 0);
        eng.endorse(winner, uid(11), None, 0); // winner net 2.0
        eng.propose(runner, uid(20), 0); // runner net 1.0 -> winner leads by margin

        // Still pending mid-flight (cutoff unreachable).
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));
        // A firekeeper claim here would be premature.
        assert_eq!(
            claim_outcome(&mut eng, &title, uid(10), Some(title.token()), winner, 0),
            Err(ClaimError::NotConverged)
        );

        // Deadline forces resolution on the clear leader, latching converged.
        let forced = eng
            .force_resolve(0, AbortReason::Timeout, true)
            .expect("clear leader force-resolves");
        assert_eq!(forced, winner);

        // Now the firekeeper may ratify the timeout-forced decision.
        assert_eq!(
            claim_outcome(&mut eng, &title, uid(10), Some(title.token()), winner, 0),
            Ok(())
        );
    }

    // --- OCEAN-229: the unforgeable firekeeper identity gate -----------------

    // THE CORE OCEAN-229 GUARANTEE. A forged firekeeper — one that learned the
    // public firekeeper `agent_id` (e.g. from the `RoleGranted`/`Converged`
    // event stream) but holds NO title token — is REFUSED, even though the quorum
    // genuinely converged on exactly the proposal it claims. Before this gate, an
    // attacker who could name the id could emit an outcome the quorum backed under
    // a firekeeper identity it never legitimately held; now it cannot.
    #[test]
    fn claim_outcome_rejects_forged_firekeeper_no_token() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let (a, b) = (uid(10), uid(11));
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        // Quorum genuinely converges on `proposal`.
        eng.propose(proposal, a, t);
        eng.endorse(proposal, b, None, t);
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        // The forger names the real firekeeper id `a` (public, off the events)
        // but presents NO token. Refused as forged — convergence is irrelevant.
        let result = claim_outcome(&mut eng, &title, a, None, proposal, t);
        assert_eq!(
            result,
            Err(ClaimError::ForgedFirekeeper),
            "a firekeeper with no title token must be rejected even when quorum converged"
        );
    }

    // A claimant presenting a WRONG token (e.g. a guessed/stolen-but-stale value)
    // under the correct firekeeper id is refused. The token — not the id — is the
    // credential, and it is verified in constant time.
    #[test]
    fn claim_outcome_rejects_forged_firekeeper_wrong_token() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let (a, b) = (uid(10), uid(11));
        let title = FirekeeperTitle::mint(a);
        // A different, independently-minted token — what an attacker could produce
        // on their own. It must not authorize against this title.
        let attacker_token = mint_decision_token();
        assert_ne!(title.token(), attacker_token.as_str());
        let t = 0;

        eng.propose(proposal, a, t);
        eng.endorse(proposal, b, None, t);
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        let result = claim_outcome(
            &mut eng,
            &title,
            a,
            Some(attacker_token.as_str()),
            proposal,
            t,
        );
        assert_eq!(
            result,
            Err(ClaimError::ForgedFirekeeper),
            "a wrong title token must be rejected"
        );
    }

    // The real token asserted under a DIFFERENT agent id is refused: the title
    // binds a specific (id, token) pair, so a second agent cannot ride the real
    // firekeeper's leaked token under its own identity.
    #[test]
    fn claim_outcome_rejects_real_token_wrong_id() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let (a, b) = (uid(10), uid(11));
        let imposter = uid(99);
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        eng.propose(proposal, a, t);
        eng.endorse(proposal, b, None, t);
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        // Imposter id `99` presents the *real* token but is not the titled agent.
        let result = claim_outcome(&mut eng, &title, imposter, Some(title.token()), proposal, t);
        assert_eq!(
            result,
            Err(ClaimError::ForgedFirekeeper),
            "the real token under the wrong agent id must be rejected"
        );
    }

    // The identity boundary is checked FIRST: a forged firekeeper is refused with
    // `ForgedFirekeeper` (not `NotConverged`) even while the engine is still
    // pending, so the rejection reason never leaks engine state to an
    // unauthorized caller.
    #[test]
    fn claim_outcome_forgery_checked_before_convergence() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let a = uid(10);
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        // Engine is Pending (only the implicit self-endorse, net 1.0 < 2.0).
        eng.propose(proposal, a, t);
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Pending { .. }));

        // Forged claim while pending -> ForgedFirekeeper, NOT NotConverged.
        let result = claim_outcome(&mut eng, &title, a, None, proposal, t);
        assert_eq!(
            result,
            Err(ClaimError::ForgedFirekeeper),
            "identity must be checked before convergence, leaking no engine state"
        );
    }

    // The legitimate, fully-authorized path end-to-end: correct id + correct
    // token + genuine convergence on the claimed proposal -> Ok.
    #[test]
    fn claim_outcome_accepts_legit_firekeeper_with_token() {
        let mut eng = QuorumEngine::new(fast_quorum());
        let proposal = uid(1);
        let (a, b) = (uid(10), uid(11));
        let title = FirekeeperTitle::mint(a);
        let t = 0;

        eng.propose(proposal, a, t);
        eng.endorse(proposal, b, None, t);
        assert!(matches!(eng.evaluate(t), QuorumOutcome::Converged { .. }));

        assert_eq!(
            claim_outcome(&mut eng, &title, a, Some(title.token()), proposal, t),
            Ok(()),
            "the legitimate firekeeper with the right token must be able to ratify"
        );
    }

    // The title never leaks its secret token through Debug (logs/snapshots).
    #[test]
    fn firekeeper_title_debug_redacts_token() {
        let title = FirekeeperTitle::mint(uid(7));
        let shown = format!("{title:?}");
        assert!(shown.contains("<redacted>"), "Debug must redact the token");
        assert!(
            !shown.contains(title.token()),
            "Debug must never print the real token"
        );
    }
}
