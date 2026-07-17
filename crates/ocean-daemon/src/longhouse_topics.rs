//! Scripted Longhouse topic production and read-only projection adapters.
//!
//! Route composition, shared-state assembly, real convene, and title/control
//! authority remain in the parent binary composition root.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use ocean_agent_sdk::{
    AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark, MarkKind,
    ProposalTally,
};
use serde_json::json;
use uuid::Uuid;

use super::AppState;

/// Emit a scripted-but-real Longhouse deliberation onto the agent event bus so
/// the Living Deck (the underwater-building UI) can render an actual council
/// flow before the full convening engine exists. Returns immediately; the flow
/// streams over `/v1/agent/events` as `Extension { extension: "longhouse" }`
/// events. This is a development harness, not the production convening path.
pub(super) async fn longhouse_demo(State(state): State<AppState>) -> Json<serde_json::Value> {
    let bus = state.agent_events.clone();
    let registry = state.longhouse.clone();
    let topic_id = Uuid::new_v4();
    let board_id = Uuid::new_v4();

    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};
        // Tee every demo event into the read-side registry before publishing to
        // the bus — identical to the `longhouse_convene` path (OCEAN-58 / Codex).
        // Without this, a demo council's TopicConvened/TopicClosed stream renders
        // live but never lands in the topic store, so GET /v1/longhouse/topics
        // stays empty and GET /v1/longhouse/topics/{id} 404s for the demo's id.
        // The std Mutex guard is dropped before any await (the closure is fully
        // synchronous), so it never blocks the scheduler.
        let emit = |ev: LonghouseEvent| {
            if let Ok(mut reg) = registry.lock() {
                reg.ingest(&ev);
            }
            bus.emit(ev.into_turn_event());
        };

        // 1. A user asks the Sales room a question → the room lights up.
        emit(LonghouseEvent::TopicConvened {
            topic_id,
            board_id,
            federation: Federation::Sales,
            trigger: ConveneTrigger::UserRequest,
            title: "Which 5 creators should we pitch for the Warner Q3 push?".into(),
            deadline_ms: 1_700_000_000_000,
        });
        sleep(Duration::from_millis(600)).await;

        // 2. Four members swim in — mixed models, mostly couriers + a steward.
        let opus = Uuid::new_v4();
        let kimi = Uuid::new_v4();
        let deepseek = Uuid::new_v4();
        let steward = Uuid::new_v4();
        let member = |id: Uuid, role: AgentRole, model: &str, label: &str| LonghouseMember {
            agent_id: id,
            federation: Federation::Sales,
            role,
            model: model.into(),
            label: Some(label.into()),
        };
        emit(LonghouseEvent::Convened {
            topic_id,
            members: vec![
                member(
                    opus,
                    AgentRole::Courier,
                    "claude-opus-4-7",
                    "Sales Courier · Opus",
                ),
                member(
                    kimi,
                    AgentRole::Courier,
                    "kimi-k2.6",
                    "Sales Courier · Kimi",
                ),
                member(
                    deepseek,
                    AgentRole::Courier,
                    "deepseek-v4-pro",
                    "Sales Courier · DeepSeek",
                ),
                member(
                    steward,
                    AgentRole::Steward,
                    "claude-opus-4-7",
                    "Sales Steward",
                ),
            ],
        });
        sleep(Duration::from_millis(700)).await;

        // 3. Two proposals land on the blackboard.
        let prop_a = Uuid::new_v4();
        let prop_b = Uuid::new_v4();
        emit(LonghouseEvent::MarkPosted {
            topic_id,
            mark: Mark {
                mark_id: Uuid::new_v4(),
                author: opus,
                kind: MarkKind::Proposal,
                target: None,
                summary: "Plan A: 5 mid-tier dance creators w/ proven Warner sound lift".into(),
            },
        });
        // give prop_a its identity by re-using mark_id as proposal id in tallies
        sleep(Duration::from_millis(500)).await;
        emit(LonghouseEvent::MarkPosted {
            topic_id,
            mark: Mark {
                mark_id: Uuid::new_v4(),
                author: kimi,
                kind: MarkKind::Proposal,
                target: None,
                summary: "Plan B: 3 macro creators + 2 emerging, higher reach, higher risk".into(),
            },
        });
        sleep(Duration::from_millis(600)).await;

        // 4. Evidence + endorsements + an inhibit — the deliberation moves.
        emit(LonghouseEvent::MarkPosted {
            topic_id,
            mark: Mark {
                mark_id: Uuid::new_v4(),
                author: deepseek,
                kind: MarkKind::Evidence,
                target: Some(prop_a),
                summary: "Campaign Hub: Plan A creators avg 2.3x save-rate on prior Warner sounds"
                    .into(),
            },
        });
        sleep(Duration::from_millis(500)).await;
        for (author, target) in [(opus, prop_a), (deepseek, prop_a), (steward, prop_a)] {
            emit(LonghouseEvent::MarkPosted {
                topic_id,
                mark: Mark {
                    mark_id: Uuid::new_v4(),
                    author,
                    kind: MarkKind::Endorse,
                    target: Some(target),
                    summary: "endorses Plan A".into(),
                },
            });
            emit(LonghouseEvent::QuorumUpdated {
                topic_id,
                tallies: vec![
                    ProposalTally {
                        proposal: prop_a,
                        net_weight: 1.0,
                    },
                    ProposalTally {
                        proposal: prop_b,
                        net_weight: 0.4,
                    },
                ],
                leader: Some(prop_a),
                distance_to_quorum: 0.5,
            });
            sleep(Duration::from_millis(450)).await;
        }
        emit(LonghouseEvent::MarkPosted {
            topic_id,
            mark: Mark {
                mark_id: Uuid::new_v4(),
                author: kimi,
                kind: MarkKind::Inhibit,
                target: Some(prop_a),
                summary: "flags Plan A reach ceiling — but concedes save-rate".into(),
            },
        });
        sleep(Duration::from_millis(500)).await;

        // 5. A firekeeper title is granted; quorum crosses.
        emit(LonghouseEvent::RoleGranted {
            topic_id,
            agent_id: steward,
            role: AgentRole::Firekeeper,
        });
        emit(LonghouseEvent::QuorumUpdated {
            topic_id,
            tallies: vec![
                ProposalTally {
                    proposal: prop_a,
                    net_weight: 2.6,
                },
                ProposalTally {
                    proposal: prop_b,
                    net_weight: 0.4,
                },
            ],
            leader: Some(prop_a),
            distance_to_quorum: 1.0,
        });
        sleep(Duration::from_millis(600)).await;

        // 6. The firekeeper ratifies — the room floods with light.
        emit(LonghouseEvent::Converged {
            topic_id,
            decision: prop_a,
            by: steward,
        });
        sleep(Duration::from_millis(400)).await;
        emit(LonghouseEvent::TopicClosed { topic_id });

        // 7. A steward heartbeat about the Sales automations (deck shows health).
        emit(LonghouseEvent::RunHealth {
            federation: Federation::Sales,
            runs_total: 7,
            runs_healthy: 7,
            note: Some("nightly outreach sync green".into()),
        });
    });

    Json(json!({ "ok": true, "topic_id": topic_id, "streaming_on": "/v1/agent/events" }))
}

/// `GET /v1/longhouse/topics` — list every tracked longhouse topic with its full
/// observable state (members, marks, tallies, leader, deadline, firekeeper,
/// decision, state). Read-only mirror of the per-council quorum engine, folded
/// from the event stream so the quorum observability deck survives a refresh
/// (OCEAN-58).
pub(super) async fn longhouse_topics(State(state): State<AppState>) -> Json<serde_json::Value> {
    let topics = match state.longhouse.lock() {
        Ok(reg) => reg.topics(),
        Err(poisoned) => poisoned.into_inner().topics(),
    };
    Json(json!({ "ok": true, "topics": topics }))
}

/// `GET /v1/longhouse/topics/{topic_id}` — one topic's full observable state by
/// id. 404 if the topic id is unknown, 400 if it isn't a valid UUID. Mirrors the
/// client-facing API shape: a typed error body, never a panic.
pub(super) async fn longhouse_topic(
    State(state): State<AppState>,
    Path(topic_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = match Uuid::parse_str(topic_id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("invalid topic id '{topic_id}'; expected a UUID"),
                })),
            );
        }
    };

    let snapshot = match state.longhouse.lock() {
        Ok(reg) => reg.topic(&id),
        Err(poisoned) => poisoned.into_inner().topic(&id),
    };

    match snapshot {
        Some(topic) => (StatusCode::OK, Json(json!({ "ok": true, "topic": topic }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": format!("no longhouse topic with id '{id}'"),
            })),
        ),
    }
}
