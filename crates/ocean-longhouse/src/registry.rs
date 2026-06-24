//! Observable topic state — the read-side projection of a council.
//!
//! A live council ([`crate::convene`]) streams `LonghouseEvent`s onto the agent
//! bus and then disappears: the `convene()` future owns the only `QuorumEngine`,
//! and once it returns nothing remembers the topic. That is fine for the deck
//! *while* it is connected, but a refresh — or a second surface attaching late —
//! has no way to recover the current field, so the quorum observability deck
//! wipes on reload.
//!
//! This module fixes that with a tiny, pure, daemon-owned **registry**: a
//! `topic_id -> TopicState` map that folds each emitted `LonghouseEvent` into a
//! durable snapshot. The daemon holds one [`LonghouseRegistry`] in shared state,
//! tees every event the council emits into it ([`LonghouseRegistry::ingest`]),
//! and serves `GET /v1/longhouse/topics` + `GET /v1/longhouse/topics/{id}` off
//! it. Convergence is still decided *only* by the [`QuorumEngine`]; this is the
//! read-only mirror of what the engine already published.
//!
//! It is deliberately event-sourced rather than wired into the engine: the
//! `LonghouseEvent` stream is the public, already-tested vocabulary, so the
//! registry stays decoupled from the engine internals and trivially testable.
//!
//! [`QuorumEngine`]: crate::quorum::QuorumEngine

use std::collections::HashMap;

use ocean_agent_sdk::{
    AbortReason, AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark,
    ProposalTally,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The lifecycle state of a topic, as observed from its event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicState {
    /// Open and deliberating — members seated, marks landing, quorum climbing.
    Convened,
    /// Resolved on a single proposal (the firekeeper emitted `Converged`).
    Converged,
    /// Closed without a converged answer (deadline/split/budget/recall).
    Aborted,
}

/// A durable, read-only snapshot of one topic's deliberation field.
///
/// This is exactly what the quorum observability deck needs to re-render after a
/// refresh: who is seated, what marks landed, the current tallies, the leader,
/// the deadline, the firekeeper (once granted), and the lifecycle state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicSnapshot {
    pub topic_id: Uuid,
    pub board_id: Uuid,
    pub federation: Federation,
    pub trigger: ConveneTrigger,
    /// Short human-facing description of the question/task.
    pub title: String,
    /// Hard deadline (epoch ms) after which the daemon forces resolution.
    pub deadline_ms: i64,
    /// Members seated in this topic (sprites on the deck).
    pub members: Vec<LonghouseMember>,
    /// Every mark posted to the blackboard, in arrival order.
    pub marks: Vec<Mark>,
    /// Latest daemon-computed tallies (the "pheromone levels").
    pub tallies: Vec<ProposalTally>,
    /// The current front-runner proposal, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<Uuid>,
    /// 0.0–1.0: how close the leader is to crossing the quorum threshold.
    pub distance_to_quorum: f32,
    /// The agent holding the firekeeper title for this topic, once granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firekeeper: Option<Uuid>,
    /// The winning proposal, once the topic converged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Uuid>,
    /// Why the topic aborted, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<AbortReason>,
    /// Lifecycle state: convened / converged / aborted.
    pub state: TopicState,
}

impl TopicSnapshot {
    fn new(
        topic_id: Uuid,
        board_id: Uuid,
        federation: Federation,
        trigger: ConveneTrigger,
        title: String,
        deadline_ms: i64,
    ) -> Self {
        Self {
            topic_id,
            board_id,
            federation,
            trigger,
            title,
            deadline_ms,
            members: Vec::new(),
            marks: Vec::new(),
            tallies: Vec::new(),
            leader: None,
            distance_to_quorum: 0.0,
            firekeeper: None,
            decision: None,
            abort_reason: None,
            state: TopicState::Convened,
        }
    }
}

/// A daemon-owned, in-memory map of live + recently-closed topics, rebuilt by
/// folding the council's `LonghouseEvent` stream. Read-only with respect to
/// convergence: the engine decides, this only mirrors what it published.
///
/// Cheap to clone the snapshots out for a response; the registry itself is held
/// behind the daemon's shared lock.
/// Upper bound on retained topic snapshots. Closed councils survive a refresh
/// (that's the point of the registry), but only the most recent `MAX_TOPICS` —
/// past that, the oldest closed ones are evicted so a long-lived daemon stays
/// bounded. Generous: a busy deck rarely holds more than a handful live at once.
const MAX_TOPICS: usize = 256;

#[derive(Debug, Default)]
pub struct LonghouseRegistry {
    topics: HashMap<Uuid, TopicSnapshot>,
}

impl LonghouseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one emitted `LonghouseEvent` into topic state. Idempotent-ish: the
    /// daemon calls this for every event the council emits, in order. Events for
    /// a topic that was never `TopicConvened` (shouldn't happen in the real
    /// flow) are ignored rather than fabricating a half-topic, except the
    /// opening `TopicConvened` which creates it.
    pub fn ingest(&mut self, event: &LonghouseEvent) {
        match event {
            LonghouseEvent::TopicConvened {
                topic_id,
                board_id,
                federation,
                trigger,
                title,
                deadline_ms,
            } => {
                self.topics.insert(
                    *topic_id,
                    TopicSnapshot::new(
                        *topic_id,
                        *board_id,
                        *federation,
                        *trigger,
                        title.clone(),
                        *deadline_ms,
                    ),
                );
            }
            LonghouseEvent::Convened { topic_id, members } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    t.members = members.clone();
                }
            }
            LonghouseEvent::MarkPosted { topic_id, mark } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    t.marks.push(mark.clone());
                }
            }
            LonghouseEvent::QuorumUpdated {
                topic_id,
                tallies,
                leader,
                distance_to_quorum,
            } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    t.tallies = tallies.clone();
                    t.leader = *leader;
                    t.distance_to_quorum = *distance_to_quorum;
                }
            }
            LonghouseEvent::RoleGranted {
                topic_id,
                agent_id,
                role,
            } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    if *role == AgentRole::Firekeeper {
                        t.firekeeper = Some(*agent_id);
                    }
                }
            }
            LonghouseEvent::Converged {
                topic_id,
                decision,
                by,
            } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    t.decision = Some(*decision);
                    t.firekeeper = Some(*by);
                    t.leader = Some(*decision);
                    t.state = TopicState::Converged;
                }
            }
            LonghouseEvent::Aborted { topic_id, reason } => {
                if let Some(t) = self.topics.get_mut(topic_id) {
                    t.abort_reason = Some(*reason);
                    t.state = TopicState::Aborted;
                }
            }
            // `TopicClosed` returns titles to escrow / dims the room, but the
            // snapshot must SURVIVE close so a refresh after the council ends
            // still shows the final field. Leave the snapshot in place.
            LonghouseEvent::TopicClosed { .. } => {}
            // Non-topic-scoped or render-only events don't alter topic state.
            LonghouseEvent::RoleRevoked { .. }
            | LonghouseEvent::Warned { .. }
            | LonghouseEvent::RunHealth { .. } => {}
        }
        self.evict_over_cap();
    }

    /// Bound the registry so a long-running daemon doesn't grow without limit.
    /// Closed snapshots intentionally survive their `TopicClosed` (a refresh
    /// after the council ends must still show the final field) — but unbounded,
    /// that's a leak: every other shared daemon map is TTL-reaped, this one was
    /// not. Cap the map and drop the OLDEST CLOSED topics first (by deadline_ms,
    /// the same recency order `topics()` presents). Live (`Convened`) topics are
    /// never evicted, so an in-flight council is never dropped even past the cap.
    fn evict_over_cap(&mut self) {
        while self.topics.len() > MAX_TOPICS {
            let victim = self
                .topics
                .values()
                .filter(|t| !matches!(t.state, TopicState::Convened))
                .min_by_key(|t| t.deadline_ms)
                .map(|t| t.topic_id);
            match victim {
                Some(id) => {
                    self.topics.remove(&id);
                }
                // Everything left is live; never evict a deliberating council.
                None => break,
            }
        }
    }

    /// All known topic snapshots, newest deadline first (a stable, useful order
    /// for the deck's topic list). Cloned out for the response.
    pub fn topics(&self) -> Vec<TopicSnapshot> {
        let mut out: Vec<TopicSnapshot> = self.topics.values().cloned().collect();
        // Most-recently-convened (largest deadline) first; tie-break by id so the
        // ordering is deterministic.
        out.sort_by(|a, b| {
            b.deadline_ms
                .cmp(&a.deadline_ms)
                .then(a.topic_id.cmp(&b.topic_id))
        });
        out
    }

    /// One topic's snapshot by id, if known.
    pub fn topic(&self, topic_id: &Uuid) -> Option<TopicSnapshot> {
        self.topics.get(topic_id).cloned()
    }

    /// How many topics are tracked.
    pub fn len(&self) -> usize {
        self.topics.len()
    }

    pub fn is_empty(&self) -> bool {
        self.topics.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::MarkKind;

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    fn convened(topic: Uuid, deadline_ms: i64) -> LonghouseEvent {
        LonghouseEvent::TopicConvened {
            topic_id: topic,
            board_id: uid(200),
            federation: Federation::Dev,
            trigger: ConveneTrigger::UserRequest,
            title: "test topic".into(),
            deadline_ms,
        }
    }

    // The whole point of OCEAN-58: state accumulated from events survives, so a
    // refresh after the council emitted its stream can re-render the field.
    #[test]
    fn folds_event_stream_into_durable_snapshot() {
        let mut reg = LonghouseRegistry::new();
        let topic = uid(1);
        let (a, b) = (uid(10), uid(11));
        let proposal = uid(50);

        reg.ingest(&convened(topic, 120_000));
        reg.ingest(&LonghouseEvent::Convened {
            topic_id: topic,
            members: vec![LonghouseMember {
                agent_id: a,
                federation: Federation::Dev,
                role: AgentRole::Courier,
                model: "deepseek-v4-flash".into(),
                label: Some("Dev Courier".into()),
            }],
        });
        reg.ingest(&LonghouseEvent::MarkPosted {
            topic_id: topic,
            mark: Mark {
                mark_id: proposal,
                author: a,
                kind: MarkKind::Proposal,
                target: None,
                summary: "do the thing".into(),
            },
        });
        reg.ingest(&LonghouseEvent::QuorumUpdated {
            topic_id: topic,
            tallies: vec![ProposalTally {
                proposal,
                net_weight: 2.0,
            }],
            leader: Some(proposal),
            distance_to_quorum: 0.8,
        });

        // Even after the topic CLOSES, the snapshot must survive for refreshes.
        reg.ingest(&LonghouseEvent::RoleGranted {
            topic_id: topic,
            agent_id: a,
            role: AgentRole::Firekeeper,
        });
        reg.ingest(&LonghouseEvent::Converged {
            topic_id: topic,
            decision: proposal,
            by: a,
        });
        reg.ingest(&LonghouseEvent::TopicClosed { topic_id: topic });

        let snap = reg.topic(&topic).expect("topic survives close");
        assert_eq!(snap.state, TopicState::Converged);
        assert_eq!(snap.decision, Some(proposal));
        assert_eq!(snap.firekeeper, Some(a));
        assert_eq!(snap.leader, Some(proposal));
        assert_eq!(snap.members.len(), 1);
        assert_eq!(snap.marks.len(), 1);
        assert_eq!(snap.tallies.len(), 1);
        assert!((snap.distance_to_quorum - 0.8).abs() < 1e-6);
        let _ = b;

        // And it's listed.
        assert_eq!(reg.topics().len(), 1);
        assert_eq!(reg.topics()[0].topic_id, topic);
    }

    #[test]
    fn registry_is_bounded_and_never_evicts_a_live_topic() {
        let mut reg = LonghouseRegistry::new();

        // One live (Convened, never closed) topic with the OLDEST deadline — it
        // must survive eviction even though it's the oldest. (`uid` only spans a
        // u8, so build ids past 255 directly.)
        let live = Uuid::from_u128(9_999);
        reg.ingest(&convened(live, 1));

        // Flood with closed topics well past the cap.
        for i in 0..(MAX_TOPICS as u128 + 50) {
            let t = Uuid::from_u128(1_000 + i);
            reg.ingest(&convened(t, 10_000 + i as i64));
            reg.ingest(&LonghouseEvent::Converged {
                topic_id: t,
                decision: uid(1),
                by: uid(2),
            });
        }

        // Bounded.
        assert!(
            reg.len() <= MAX_TOPICS,
            "registry must stay bounded, got {}",
            reg.len()
        );
        // The live topic was never evicted despite being the oldest.
        assert!(
            reg.topic(&live).is_some(),
            "a deliberating (Convened) topic must never be evicted"
        );
        assert_eq!(reg.topic(&live).unwrap().state, TopicState::Convened);
    }

    #[test]
    fn aborted_topic_records_reason_and_state() {
        let mut reg = LonghouseRegistry::new();
        let topic = uid(2);
        reg.ingest(&convened(topic, 1_000));
        reg.ingest(&LonghouseEvent::Aborted {
            topic_id: topic,
            reason: AbortReason::Split,
        });
        reg.ingest(&LonghouseEvent::TopicClosed { topic_id: topic });

        let snap = reg.topic(&topic).unwrap();
        assert_eq!(snap.state, TopicState::Aborted);
        assert_eq!(snap.abort_reason, Some(AbortReason::Split));
        assert_eq!(snap.decision, None);
    }

    #[test]
    fn topics_sorted_by_deadline_desc() {
        let mut reg = LonghouseRegistry::new();
        let (t1, t2, t3) = (uid(1), uid(2), uid(3));
        reg.ingest(&convened(t1, 100));
        reg.ingest(&convened(t2, 300));
        reg.ingest(&convened(t3, 200));
        let ids: Vec<Uuid> = reg.topics().into_iter().map(|t| t.topic_id).collect();
        assert_eq!(ids, vec![t2, t3, t1]);
    }

    #[test]
    fn events_for_unknown_topic_are_ignored() {
        let mut reg = LonghouseRegistry::new();
        // No TopicConvened first — a stray mark must not fabricate a topic.
        reg.ingest(&LonghouseEvent::MarkPosted {
            topic_id: uid(9),
            mark: Mark {
                mark_id: uid(50),
                author: uid(10),
                kind: MarkKind::Proposal,
                target: None,
                summary: "orphan".into(),
            },
        });
        assert!(reg.is_empty());
        assert!(reg.topic(&uid(9)).is_none());
    }
}
