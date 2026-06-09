//! The convening flow — a real, leaderless council driven by cheap LLM agents.
//!
//! This is the orchestration layer that sits *between* the LLM workers
//! ([`crate::agent`]) and the pure [`QuorumEngine`](crate::quorum::QuorumEngine).
//! It runs the two-round blackboard protocol from
//! `docs/LONGHOUSE_ORCHESTRATION.md`:
//!
//! 1. **Round 1 — propose.** Each worker gets the question and posts one
//!    `proposal` mark (a candidate answer).
//! 2. **Rounds 2..N — endorse / inhibit re-assertion.** Each worker sees the
//!    current bounded proposal projection and posts an `endorse` or `inhibit`
//!    mark. The loop stops on quorum, `max_rounds`, or deadline.
//!
//! After **every** mark the daemon-side [`QuorumEngine`] re-tallies, and we emit
//! a `QuorumUpdated`. When the engine reports convergence (or we hit the
//! deadline) we emit the single binding `Converged` (or `Aborted`), then
//! `TopicClosed`. The engine — never an LLM — decides when quorum is met.
//!
//! Every step emits the **existing** `LonghouseEvent`s from `ocean-agent-sdk`,
//! so the deck renders a real council with zero deck changes.

use std::collections::HashMap;

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
use crate::quorum::{QuorumConfig, QuorumEngine, QuorumOutcome};
use crate::replay::{RecordedMark, RecordedMarkKind, Recording};

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
    /// Quorum tuning. Sensible default is a low, fast-resolving quorum.
    pub quorum: QuorumConfig,
    /// Hard deadline budget in ms from convening. On expiry the engine force-
    /// resolves (clear leader → converge; tie → seeded tie-break).
    pub deadline_ms_from_now: i64,
    /// Maximum deliberation rounds, including the proposal round. Round 1 proposes;
    /// rounds 2..N are endorse/inhibit re-assertion rounds. This bounds open-ended
    /// deliberation until token-budget accounting lands.
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

/// One seated worker: a stable id + the model driving it + its role.
struct Worker {
    agent_id: Uuid,
    handle: ModelHandle,
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
pub async fn convene<F>(req: ConveneRequest, clock: &dyn Clock, mut emit: F) -> ConveneOutcome
where
    F: FnMut(LonghouseEvent),
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
    let mut workers: Vec<Worker> = Vec::new();
    for (i, alias) in req.models.iter().enumerate() {
        match ModelHandle::resolve(alias) {
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
        let _ = i;
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
            tallies: vec![],
            proposals,
            recording: Recording {
                question: req.question.clone(),
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

    for (idx, answer) in proposal_results {
        let Some(answer) = answer else { continue };
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
            tallies: engine.tallies(clock.now_ms()),
            proposals,
            recording: Recording {
                question: req.question.clone(),
                marks: recorded.clone(),
            },
        };
    }

    // ---- Rounds 2..N: endorse / inhibit re-assertion --------------------------
    let max_rounds = req.max_rounds.max(1);
    for round in 2..=max_rounds {
        if engine.is_converged() || clock.now_ms() >= deadline_ms {
            break;
        }

        // Build a bounded projection of the proposals for the voters to see.
        let projection = projection_text(&proposals);
        let proposal_ids: Vec<Uuid> = proposals.keys().copied().collect();

        let vote_prompts: Vec<(usize, String)> = workers
            .iter()
            .enumerate()
            .map(|(idx, w)| {
                let own = proposal_by_author.get(&w.agent_id).copied();
                (
                    idx,
                    round2_user(&req.question, &projection, &proposal_ids, own, round),
                )
            })
            .collect();

        let vote_results = run_round(&workers, vote_prompts, ROUND2_SYSTEM).await;
        let mut contributed = false;

        for (idx, answer) in vote_results {
            if engine.is_converged() || clock.now_ms() >= deadline_ms {
                break;
            }
            let Some(answer) = answer else { continue };
            let w = &workers[idx];
            let now = clock.now_ms();
            let Some(vote) = parse_vote(&answer, &proposal_ids) else {
                continue;
            };
            contributed = true;
            match vote.kind {
                VoteKind::Endorse => engine.endorse(vote.target, w.agent_id, None, now),
                VoteKind::Inhibit => engine.inhibit(vote.target, w.agent_id, None, now),
            }
            recorded.push(RecordedMark {
                at_ms: now - started,
                author: w.agent_id,
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
                    author: w.agent_id,
                    kind: match vote.kind {
                        VoteKind::Endorse => MarkKind::Endorse,
                        VoteKind::Inhibit => MarkKind::Inhibit,
                    },
                    target: Some(vote.target),
                    summary: truncate(&vote.rationale, 160),
                },
            });
            emit_quorum(&mut engine, topic_id, clock.now_ms(), &mut emit);
        }

        // If a whole re-assertion round produced no usable marks, another identical
        // prompt cycle is unlikely to help; terminate and let force_resolve decide.
        if !contributed {
            break;
        }
    }

    // ---- Resolve ------------------------------------------------------------
    let now = clock.now_ms();
    let decision = match engine.evaluate(now) {
        QuorumOutcome::Converged { decision, .. } => Some(decision),
        QuorumOutcome::Pending { .. } => {
            // Deadline behavior: if we've blown the deadline, force-resolve;
            // otherwise still force a resolution now (the council had its rounds)
            // with a seeded tie-break so a topic always terminates.
            let reason = if now >= deadline_ms {
                AbortReason::Timeout
            } else {
                AbortReason::Split
            };
            match engine.force_resolve(now, reason, true) {
                Ok(winner) => Some(winner),
                Err(abort) => {
                    emit(LonghouseEvent::Aborted {
                        topic_id,
                        reason: abort,
                    });
                    None
                }
            }
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
        tallies: engine.tallies(now),
        proposals,
        recording: Recording {
            question: req.question.clone(),
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
        QuorumOutcome::Converged { decision, tallies } => (tallies, Some(decision), 1.0),
    };
    emit(LonghouseEvent::QuorumUpdated {
        topic_id,
        tallies,
        leader,
        distance_to_quorum: distance,
    });
}

/// Run a round of independent LLM calls concurrently, returning each worker's
/// (index, optional answer). A `None` answer means that worker didn't
/// contribute (error/timeout) — the council carries on without it.
async fn run_round(
    workers: &[Worker],
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

const ROUND1_SYSTEM: &str =
    "You are one member of a small council answering a question. Give your single best \
     answer in ONE short paragraph (max 3 sentences). Be concrete and decisive. Do not \
     hedge or list multiple options — commit to one answer.";

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

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
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
        let result = claim_outcome(
            &mut eng,
            &title,
            proposer,
            Some(title.token()),
            proposal,
            t,
        );
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
        assert!(matches!(
            eng.evaluate(t),
            QuorumOutcome::Converged { .. }
        ));

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
        assert!(matches!(
            eng.evaluate(t),
            QuorumOutcome::Converged { .. }
        ));

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
        let result = claim_outcome(
            &mut eng,
            &title,
            imposter,
            Some(title.token()),
            proposal,
            t,
        );
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
