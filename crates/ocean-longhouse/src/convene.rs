//! The convening flow — a real, leaderless council driven by cheap LLM agents.
//!
//! This is the orchestration layer that sits *between* the LLM workers
//! ([`crate::agent`]) and the pure [`QuorumEngine`](crate::quorum::QuorumEngine).
//! It runs the two-round blackboard protocol from
//! `docs/LONGHOUSE_ORCHESTRATION.md`:
//!
//! 1. **Round 1 — propose.** Each worker gets the question and posts one
//!    `proposal` mark (a candidate answer).
//! 2. **Round 2 — endorse / inhibit.** Each worker sees the *other* proposals
//!    (a bounded projection) and posts an `endorse` or `inhibit` mark.
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
use uuid::Uuid;

use crate::agent::ModelHandle;
use crate::quorum::{QuorumConfig, QuorumEngine, QuorumOutcome};

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

/// Run a full convening. `emit` receives each `LonghouseEvent` as it happens —
/// the daemon passes a closure that publishes onto the agent event bus
/// (`bus.emit(ev.into_turn_event())`), so the deck animates a live council.
///
/// `clock` is injected for deterministic testing of the timing/quorum path.
pub async fn convene<F>(
    req: ConveneRequest,
    clock: &dyn Clock,
    mut emit: F,
) -> ConveneOutcome
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
        };
    }

    // ---- Round 2: endorse / inhibit ----------------------------------------
    if !engine.is_converged() {
        // Build a bounded projection of the proposals for the voters to see.
        let projection = projection_text(&proposals);
        let proposal_ids: Vec<Uuid> = proposals.keys().copied().collect();

        let vote_prompts: Vec<(usize, String)> = workers
            .iter()
            .enumerate()
            .map(|(idx, w)| {
                let own = proposal_by_author.get(&w.agent_id).copied();
                (idx, round2_user(&req.question, &projection, &proposal_ids, own))
            })
            .collect();

        let vote_results = run_round(&workers, vote_prompts, ROUND2_SYSTEM).await;

        for (idx, answer) in vote_results {
            let Some(answer) = answer else { continue };
            let w = &workers[idx];
            let now = clock.now_ms();
            let Some(vote) = parse_vote(&answer, &proposal_ids) else {
                continue;
            };
            match vote.kind {
                VoteKind::Endorse => engine.endorse(vote.target, w.agent_id, None, now),
                VoteKind::Inhibit => engine.inhibit(vote.target, w.agent_id, None, now),
            }
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
            if engine.is_converged() {
                break;
            }
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
        let firekeeper = engine
            .proposer_of(decision)
            .or_else(|| workers.first().map(|w| w.agent_id))
            .unwrap_or_else(Uuid::new_v4);

        emit(LonghouseEvent::RoleGranted {
            topic_id,
            agent_id: firekeeper,
            role: AgentRole::Firekeeper,
        });
        emit(LonghouseEvent::Converged {
            topic_id,
            decision,
            by: firekeeper,
        });
    }

    emit(LonghouseEvent::TopicClosed { topic_id });

    ConveneOutcome {
        topic_id,
        board_id,
        decision,
        tallies: engine.tallies(now),
        proposals,
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
) -> String {
    format!(
        "Question:\n{question}\n\nProposals on the blackboard:\n{projection}\n\n\
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
}
