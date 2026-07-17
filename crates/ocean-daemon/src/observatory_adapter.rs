//! Bridge the daemon's `AgentTurnEvent` stream into redacted Observatory facts.
//!
//! One-way adapter: every runtime event is either translated into exactly one
//! metadata-only [`EventEnvelope`] or explicitly skipped when the variant
//! carries forbidden content (text/thinking deltas, tool chunks, component
//! props, canvas payloads, extension payloads). Tool arguments, tool output
//! bodies, free-text error strings, session titles, and working directories
//! never leave this module — the closed [`EventPayload`] enum plus these
//! mappings are the structural redaction boundary (Gate 1 manifest §6).
//!
//! Mapping table (real `AgentTurnEvent` names; the planner spec's aspirational
//! names in parentheses):
//!
//! - `SessionCreated` (`AgentStart`) → `ExecutionAdmitted { Admitted, ["session"] }`
//!   — title/cwd stripped (paths and free text are forbidden).
//! - `TurnStarted` (`TurnStart`) → `ExecutionAdmitted { Running, ["turn"] }`
//!   — admitted and running are the same fact at this seam.
//! - `ToolCallStarted` (`ToolExecutionStart`) → `ToolStarted` — tool name only;
//!   args stripped entirely. `model_alias` stays empty (spec: tool_name ONLY).
//! - `ToolCallFinished` (`ToolExecutionEnd`) → `ToolFinished` — outcome from
//!   `ok`, `byte_count` from output length; output body and metadata stripped.
//! - `TurnFinished` → `ExecutionFinished` — status mapped to a terminal phase
//!   with a fixed `error_classification` code; free-text error stripped.
//! - `ModelRerouted` → `ModelRerouted` — model aliases pass through; reason is
//!   classified to a fixed code (never credentials/free text).
//! - `AssistantTextDelta`, `ThinkingDelta`, `ToolCallChunk` (`TurnCheckpoint`),
//!   `BrowserActivity`, `ComponentRender`, `ComponentUnmount`, `SurfacePatch`,
//!   `SlackCanvas`, `Extension` → SKIP (forbidden or non-factual content).
//!
//! `AgentTurnEvent` carries no permission-request/resolution variants, so no
//! `PermissionWaiting`/`PermissionResolved` facts are emitted at this seam yet;
//! the payload variants stay reserved until the runtime grows that event.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use ocean_agent_sdk::{AgentTurnEvent, AgentTurnStatus};
use ocean_observatory::{
    Correlation, Cursor, EventEnvelope, EventKind, EventPayload, ExecutionPhase, ObservatoryStore,
    Producer, ProducerKind, ToolOutcome, Topology, TruthProvenance, Visibility,
};
use uuid::Uuid;

/// One execution label that is always safe: fixed enum, never user text.
const SESSION_LABEL: &str = "session";
const TURN_LABEL: &str = "turn";

/// Stateful adapter. Tracks per-call tool names and start times so a
/// `ToolCallFinished` (which carries no name) can be attributed correctly.
/// Maps are bounded by live call cardinality and entries are removed on
/// completion, so they cannot grow unboundedly.
pub(crate) struct ObservatoryAdapter {
    observatory_id: String,
    daemon_instance_id: String,
    tool_names: Mutex<HashMap<String, String>>,
    tool_started: Mutex<HashMap<String, Instant>>,
}

impl ObservatoryAdapter {
    pub(crate) fn new(observatory_id: String, daemon_instance_id: String) -> Self {
        Self {
            observatory_id,
            daemon_instance_id,
            tool_names: Mutex::new(HashMap::new()),
            tool_started: Mutex::new(HashMap::new()),
        }
    }

    /// Translate one runtime event. `None` means the variant is structurally
    /// forbidden content and must not reach the Observatory at all.
    pub(crate) fn adapt(&self, event: &AgentTurnEvent) -> Option<EventEnvelope> {
        match event {
            AgentTurnEvent::SessionCreated { session_id, .. } => {
                // title/cwd are a path and free text: stripped by construction.
                Some(self.envelope(
                    EventKind::ExecutionAdmitted,
                    EventPayload::ExecutionAdmitted {
                        phase: ExecutionPhase::Admitted,
                        labels: vec![SESSION_LABEL.to_owned()],
                    },
                    Topology {
                        execution_id: session_id.to_string(),
                        root_execution_id: session_id.to_string(),
                        parent_execution_id: None,
                        edge_id: None,
                        session_id: session_id.to_string(),
                        turn_id: String::new(),
                        request_id: String::new(),
                    },
                    no_correlation(),
                ))
            }
            AgentTurnEvent::TurnStarted {
                turn_id,
                session_id,
                ..
            } => Some(self.envelope(
                EventKind::ExecutionAdmitted,
                EventPayload::ExecutionAdmitted {
                    phase: ExecutionPhase::Running,
                    labels: vec![TURN_LABEL.to_owned()],
                },
                turn_topology(turn_id.to_string(), session_id.to_string()),
                no_correlation(),
            )),
            AgentTurnEvent::ToolCallStarted {
                session_id,
                turn_id,
                call,
            } => {
                let call_id = call.id.to_string();
                self.tool_names
                    .lock()
                    .expect("tool names")
                    .insert(call_id.clone(), call.name.clone());
                self.tool_started
                    .lock()
                    .expect("tool starts")
                    .insert(call_id.clone(), Instant::now());
                Some(self.envelope(
                    EventKind::ToolStarted,
                    EventPayload::ToolStarted {
                        tool_name: call.name.clone(),
                        model_alias: String::new(),
                    },
                    turn_topology(turn_id.to_string(), session_id.to_string()),
                    Correlation {
                        tool_call_id: Some(call_id),
                        permission_id: None,
                    },
                ))
            }
            AgentTurnEvent::ToolCallFinished {
                session_id,
                turn_id,
                call_id,
                result,
            } => {
                let call_id = call_id.to_string();
                let tool_name = self
                    .tool_names
                    .lock()
                    .expect("tool names")
                    .remove(&call_id)
                    .unwrap_or_else(|| "unknown".to_owned());
                let duration = self
                    .tool_started
                    .lock()
                    .expect("tool starts")
                    .remove(&call_id)
                    .map(|started| started.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                Some(self.envelope(
                    EventKind::ToolFinished,
                    EventPayload::ToolFinished {
                        tool_name,
                        duration_millis: duration,
                        outcome: if result.ok {
                            ToolOutcome::Success
                        } else {
                            ToolOutcome::Error
                        },
                        // Length is a safe metadata fact; the body is not.
                        byte_count: result.output.len() as u64,
                    },
                    turn_topology(turn_id.to_string(), session_id.to_string()),
                    Correlation {
                        tool_call_id: Some(call_id),
                        permission_id: None,
                    },
                ))
            }
            AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status,
                wall_ms,
                ..
            } => {
                let (phase, classification) = match status {
                    AgentTurnStatus::Completed => (ExecutionPhase::Finished, None),
                    // Fixed codes only; the free-text error is stripped.
                    AgentTurnStatus::Failed => {
                        (ExecutionPhase::Error, Some("turn_failed".to_owned()))
                    }
                    AgentTurnStatus::Cancelled => {
                        (ExecutionPhase::Canceled, Some("turn_cancelled".to_owned()))
                    }
                    AgentTurnStatus::Queued => {
                        (ExecutionPhase::Canceled, Some("turn_abandoned".to_owned()))
                    }
                    AgentTurnStatus::Running => (
                        ExecutionPhase::Finished,
                        Some("status_unreported".to_owned()),
                    ),
                };
                Some(self.envelope(
                    EventKind::ExecutionFinished,
                    EventPayload::ExecutionFinished {
                        phase,
                        duration_millis: wall_ms.unwrap_or(0),
                        error_classification: classification,
                    },
                    turn_topology(turn_id.to_string(), session_id.to_string()),
                    no_correlation(),
                ))
            }
            AgentTurnEvent::ModelRerouted {
                requested,
                effective,
                reason,
                session_id,
                turn_id,
            } => Some(self.envelope(
                EventKind::ModelRerouted,
                EventPayload::ModelRerouted {
                    from_model: requested.clone(),
                    to_model: effective.clone(),
                    reason: classify_reroute(reason),
                },
                turn_topology(turn_id.to_string(), session_id.to_string()),
                no_correlation(),
            )),
            // Forbidden content variants: text/thinking deltas, tool output
            // chunks, and arbitrary extension/component/canvas payloads never
            // cross this boundary.
            AgentTurnEvent::AssistantTextDelta { .. }
            | AgentTurnEvent::ThinkingDelta { .. }
            | AgentTurnEvent::ToolCallChunk { .. }
            | AgentTurnEvent::BrowserActivity { .. }
            | AgentTurnEvent::ComponentRender { .. }
            | AgentTurnEvent::ComponentUnmount { .. }
            | AgentTurnEvent::SurfacePatch { .. }
            | AgentTurnEvent::SlackCanvas { .. }
            | AgentTurnEvent::SessionConfigChanged { .. }
            | AgentTurnEvent::Extension { .. } => None,
        }
    }

    /// Daemon lifecycle fact: startup.
    pub(crate) fn daemon_started(&self, version: &str) -> EventEnvelope {
        self.envelope(
            EventKind::DaemonStarted,
            EventPayload::DaemonStarted {
                version: version.to_owned(),
            },
            daemon_topology(&self.daemon_instance_id),
            no_correlation(),
        )
    }

    /// Daemon lifecycle fact: graceful shutdown.
    pub(crate) fn daemon_stopping(&self, reason: Option<String>) -> EventEnvelope {
        self.envelope(
            EventKind::DaemonStopping,
            EventPayload::DaemonStopping { reason },
            daemon_topology(&self.daemon_instance_id),
            no_correlation(),
        )
    }

    /// Fixed interruption sweep for restart safety: every execution left in a
    /// nonterminal phase by the previous boot is closed as canceled. Returns
    /// the number of executions marked.
    pub(crate) fn mark_interrupted(&self, store: &ObservatoryStore) -> usize {
        let Ok(nonterminal) = store.nonterminal_executions() else {
            tracing::error!("observatory restart sweep failed to read nonterminal executions");
            return 0;
        };
        let mut marked = 0;
        for (execution_id, phase) in nonterminal {
            let from_phase = crate::observatory::phase_from_projection(&phase);
            let envelope = self.envelope(
                EventKind::ExecutionPhaseChanged,
                EventPayload::ExecutionPhaseChanged {
                    from_phase,
                    to_phase: ExecutionPhase::Canceled,
                },
                Topology {
                    root_execution_id: execution_id.clone(),
                    execution_id,
                    parent_execution_id: None,
                    edge_id: None,
                    session_id: String::new(),
                    turn_id: String::new(),
                    request_id: String::new(),
                },
                no_correlation(),
            );
            match store.append_event(envelope) {
                Ok(_) => marked += 1,
                Err(error) => {
                    tracing::error!(%error, "observatory restart sweep append failed");
                    break;
                }
            }
        }
        marked
    }

    fn envelope(
        &self,
        kind: EventKind,
        payload: EventPayload,
        topology: Topology,
        correlation: Correlation,
    ) -> EventEnvelope {
        let now = chrono::Utc::now().to_rfc3339();
        EventEnvelope {
            schema_version: 1,
            // The store assigns the authoritative cursor on append.
            cursor: Cursor::new(0),
            event_id: Uuid::new_v4().to_string(),
            observatory_id: self.observatory_id.clone(),
            daemon_instance_id: self.daemon_instance_id.clone(),
            occurred_at: now.clone(),
            recorded_at: now,
            kind,
            truth: TruthProvenance::HostObserved,
            producer: Producer {
                kind: ProducerKind::Daemon,
                id: "ocean-daemon".to_owned(),
            },
            topology,
            correlation,
            visibility: Visibility::Metadata,
            payload,
        }
    }
}

/// Empty correlation for facts that carry no tool-call/permission linkage.
fn no_correlation() -> Correlation {
    Correlation {
        tool_call_id: None,
        permission_id: None,
    }
}

/// Turn-scoped topology: the turn is the execution, parented to its session.
fn turn_topology(turn_id: String, session_id: String) -> Topology {
    Topology {
        execution_id: turn_id.clone(),
        root_execution_id: session_id.clone(),
        parent_execution_id: Some(session_id.clone()),
        edge_id: Some(format!("edge:{session_id}:{turn_id}")),
        session_id,
        turn_id,
        request_id: String::new(),
    }
}

/// Daemon-scoped topology for lifecycle facts (no session/turn exists).
fn daemon_topology(daemon_instance_id: &str) -> Topology {
    Topology {
        execution_id: daemon_instance_id.to_owned(),
        root_execution_id: daemon_instance_id.to_owned(),
        parent_execution_id: None,
        edge_id: None,
        session_id: String::new(),
        turn_id: String::new(),
        request_id: String::new(),
    }
}

/// Classify a free-text reroute reason to a fixed code. Credentials, request
/// bodies, and provider error text never pass through.
fn classify_reroute(reason: &str) -> String {
    let lowered = reason.to_lowercase();
    if lowered.contains("degrad") {
        "provider_degraded".to_owned()
    } else if lowered.contains("timeout") || lowered.contains("timed out") {
        "provider_timeout".to_owned()
    } else if lowered.contains("fail") || lowered.contains("error") {
        "provider_failure".to_owned()
    } else {
        "rerouted".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{AgentSessionId, AgentTurnId, ToolCall, ToolCallId, ToolResult};
    use ocean_observatory::RetentionPolicy;

    const OBS_ID: &str = "obs-test";
    const DAEMON_ID: &str = "daemon-test";

    fn adapter() -> ObservatoryAdapter {
        ObservatoryAdapter::new(OBS_ID.to_owned(), DAEMON_ID.to_owned())
    }

    fn session_id() -> AgentSessionId {
        AgentSessionId(Uuid::new_v4())
    }

    fn turn_id() -> AgentTurnId {
        AgentTurnId(Uuid::new_v4())
    }

    fn store() -> ObservatoryStore {
        let dir = tempfile::tempdir().expect("tempdir");
        ObservatoryStore::open(&dir.path().join("obs.db"), RetentionPolicy::default())
            .expect("open")
    }

    #[test]
    fn session_created_admits_root_execution_without_path_or_title() {
        let event = AgentTurnEvent::SessionCreated {
            session_id: session_id(),
            title: "PRIVATE TITLE MUST NOT LEAK".to_owned(),
            cwd: "/home/operator/secret-project".to_owned(),
        };
        let envelope = adapter().adapt(&event).expect("mapped");
        assert_eq!(envelope.kind, EventKind::ExecutionAdmitted);
        let serialized = serde_json::to_string(&envelope).expect("json");
        assert!(!serialized.contains("PRIVATE TITLE"), "{serialized}");
        assert!(!serialized.contains("secret-project"), "{serialized}");
        assert!(serialized.contains("\"session\""), "{serialized}");
    }

    #[test]
    fn turn_started_admits_running_turn_execution() {
        let event = AgentTurnEvent::TurnStarted {
            turn_id: turn_id(),
            session_id: session_id(),
            model: Some("kimi-k2.6".to_owned()),
        };
        let envelope = adapter().adapt(&event).expect("mapped");
        match envelope.payload {
            EventPayload::ExecutionAdmitted { phase, labels } => {
                assert_eq!(phase, ExecutionPhase::Running);
                assert_eq!(labels, vec![TURN_LABEL.to_owned()]);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(envelope.topology.parent_execution_id.is_some());
        assert!(envelope.topology.edge_id.is_some());
    }

    #[test]
    fn tool_calls_map_with_args_and_output_stripped() {
        let adapter = adapter();
        let session = session_id();
        let turn = turn_id();
        let call_id = ToolCallId(Uuid::new_v4());
        let call = ToolCall {
            id: ToolCallId(call_id.0),
            name: "bash".to_owned(),
            args_json: serde_json::json!({"command": "cat ~/.ssh/id_rsa"}),
        };
        let started = adapter
            .adapt(&AgentTurnEvent::ToolCallStarted {
                session_id: session,
                turn_id: turn,
                call,
            })
            .expect("started");
        let started_json = serde_json::to_string(&started).expect("json");
        assert!(!started_json.contains("id_rsa"), "{started_json}");
        assert!(
            started_json.contains("\"tool_name\":\"bash\""),
            "{started_json}"
        );

        let finished = adapter
            .adapt(&AgentTurnEvent::ToolCallFinished {
                session_id: session,
                turn_id: turn,
                call_id,
                result: ToolResult {
                    ok: false,
                    output: "SENSITIVE TOOL OUTPUT BODY".to_owned(),
                    metadata_json: Some(serde_json::json!({"exit_code": 1})),
                },
            })
            .expect("finished");
        match &finished.payload {
            EventPayload::ToolFinished {
                tool_name,
                outcome,
                byte_count,
                ..
            } => {
                assert_eq!(tool_name, "bash");
                assert!(matches!(outcome, ToolOutcome::Error));
                assert_eq!(*byte_count, "SENSITIVE TOOL OUTPUT BODY".len() as u64);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        let finished_json = serde_json::to_string(&finished).expect("json");
        assert!(!finished_json.contains("SENSITIVE"), "{finished_json}");
        assert!(!finished_json.contains("exit_code"), "{finished_json}");
    }

    #[test]
    fn turn_finished_maps_status_with_fixed_classification() {
        let adapter = adapter();
        for (status, phase, classification) in [
            (AgentTurnStatus::Completed, ExecutionPhase::Finished, None),
            (
                AgentTurnStatus::Failed,
                ExecutionPhase::Error,
                Some("turn_failed"),
            ),
            (
                AgentTurnStatus::Cancelled,
                ExecutionPhase::Canceled,
                Some("turn_cancelled"),
            ),
        ] {
            let event = AgentTurnEvent::TurnFinished {
                session_id: session_id(),
                turn_id: turn_id(),
                status,
                error: Some("provider said: sk-secret-key-value".to_owned()),
                wall_ms: Some(42),
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                context_usage: None,
            };
            let envelope = adapter.adapt(&event).expect("mapped");
            match &envelope.payload {
                EventPayload::ExecutionFinished {
                    phase: got_phase,
                    duration_millis,
                    error_classification,
                } => {
                    assert_eq!(*got_phase, phase);
                    assert_eq!(*duration_millis, 42);
                    assert_eq!(
                        error_classification.as_deref(),
                        classification,
                        "{status:?}"
                    );
                }
                other => panic!("unexpected payload: {other:?}"),
            }
            let json = serde_json::to_string(&envelope).expect("json");
            assert!(!json.contains("sk-secret"), "{json}");
        }
    }

    #[test]
    fn reroute_reason_is_classified_never_free_text() {
        let adapter = adapter();
        let event = AgentTurnEvent::ModelRerouted {
            session_id: session_id(),
            turn_id: turn_id(),
            requested: "kimi-k2.6".to_owned(),
            effective: "deepseek-v4-pro".to_owned(),
            reason: "provider kimi degraded: Authorization Bearer sk-live-key rejected".to_owned(),
        };
        let envelope = adapter.adapt(&event).expect("mapped");
        let json = serde_json::to_string(&envelope).expect("json");
        assert!(json.contains("provider_degraded"), "{json}");
        assert!(!json.contains("sk-live-key"), "{json}");
        assert!(json.contains("kimi-k2.6"), "{json}"); // model alias is safe
    }

    #[test]
    fn forbidden_variants_are_skipped() {
        let adapter = adapter();
        let session = session_id();
        let turn = turn_id();
        let skipped = [
            AgentTurnEvent::AssistantTextDelta {
                session_id: session,
                turn_id: turn,
                delta: "visible answer text".to_owned(),
            },
            AgentTurnEvent::ThinkingDelta {
                session_id: session,
                turn_id: turn,
                delta: "private reasoning".to_owned(),
            },
            AgentTurnEvent::ToolCallChunk {
                session_id: session,
                turn_id: turn,
                call_id: ToolCallId(Uuid::new_v4()),
                chunk: "streamed output".to_owned(),
            },
            AgentTurnEvent::BrowserActivity {
                session_id: session,
                active: true,
            },
            AgentTurnEvent::ComponentUnmount {
                session_id: session,
                component_id: "c-1".to_owned(),
            },
            AgentTurnEvent::Extension {
                extension: "council".to_owned(),
                payload: serde_json::json!({"raw": "anything"}),
                scope: None,
            },
        ];
        for event in skipped {
            assert!(adapter.adapt(&event).is_none(), "{event:?}");
        }
    }

    #[test]
    fn lifecycle_facts_carry_safe_fields() {
        let adapter = adapter();
        let started = adapter.daemon_started("0.1.0");
        assert_eq!(started.kind, EventKind::DaemonStarted);
        let stopping = adapter.daemon_stopping(Some("shutdown".to_owned()));
        let json = serde_json::to_string(&stopping).expect("json");
        assert!(json.contains("daemon_stopping"), "{json}");
    }

    #[test]
    fn restart_sweep_cancels_nonterminal_executions() {
        let adapter = adapter();
        let store = store();
        // One running turn from a "previous boot".
        store
            .append_event(
                adapter
                    .adapt(&AgentTurnEvent::TurnStarted {
                        turn_id: turn_id(),
                        session_id: session_id(),
                        model: None,
                    })
                    .expect("turn"),
            )
            .expect("append");
        let marked = adapter.mark_interrupted(&store);
        assert_eq!(marked, 1);
        let snapshot = store.snapshot_at(None).expect("snapshot");
        assert_eq!(snapshot.nodes.len(), 1);
        assert_eq!(snapshot.nodes[0].phase, "canceled");
        // Terminal stores sweep nothing.
        assert_eq!(adapter.mark_interrupted(&store), 0);
    }

    /// Spec e2e: agent event → adapter → store → SSE route → subscriber.
    #[tokio::test]
    async fn agent_event_reaches_sse_subscriber_through_store() {
        use axum::{body::Body, http::Request, routing::get, Router};
        use tower::ServiceExt;

        use crate::observatory::ObservatoryServices;
        use crate::observatory_auth::ObservatoryAuthState;
        use ocean_observatory::{ObserverScope, ObserverSecret, ObserverToken};

        let store = std::sync::Arc::new(store());
        let adapter = adapter();
        let secret = ObserverSecret::from_raw_key([0x5C; 32]);
        let claims = ObserverToken::issue(ObserverScope::Summary, DAEMON_ID, 1_800).expect("issue");
        let token = ocean_observatory::sign_token(&claims, &secret);

        let router = Router::new()
            .route("/v1/observatory/events", get(crate::observatory::events))
            .layer(axum::Extension(ObservatoryAuthState::for_test(
                secret, DAEMON_ID,
            )))
            .layer(axum::Extension(ObservatoryServices::for_test(
                std::sync::Arc::clone(&store),
                OBS_ID,
                DAEMON_ID,
            )));

        // The fact lands in the durable store BEFORE the subscriber attaches,
        // so the tail must replay it from history (never a live-only fact).
        store
            .append_event(
                adapter
                    .adapt(&AgentTurnEvent::TurnStarted {
                        turn_id: turn_id(),
                        session_id: session_id(),
                        model: None,
                    })
                    .expect("adapt"),
            )
            .expect("append");

        let request = Request::builder()
            .uri("/v1/observatory/events?after=0")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        use futures::StreamExt;
        let collect = http_body_util::BodyExt::into_data_stream(response.into_body())
            .take(1)
            .filter_map(|chunk| async move {
                chunk.ok().map(|b| String::from_utf8_lossy(&b).into_owned())
            })
            .collect::<Vec<String>>();
        let frames = tokio::time::timeout(std::time::Duration::from_secs(5), collect)
            .await
            .expect("frame within 5s");
        let text = frames.concat();
        assert!(text.contains("\"kind\":\"execution_admitted\""), "{text}");
        assert!(text.contains("id: 1"), "{text}");
    }

    #[test]
    fn end_to_end_turn_flow_projects_nodes_and_phases() {
        let adapter = adapter();
        let store = store();
        let session = session_id();
        let turn = turn_id();
        for event in [
            AgentTurnEvent::SessionCreated {
                session_id: session,
                title: "t".to_owned(),
                cwd: "/x".to_owned(),
            },
            AgentTurnEvent::TurnStarted {
                turn_id: turn,
                session_id: session,
                model: None,
            },
            AgentTurnEvent::TurnFinished {
                session_id: session,
                turn_id: turn,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: Some(7),
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                context_usage: None,
            },
        ] {
            let envelope = adapter.adapt(&event).expect("mapped");
            store.append_event(envelope).expect("append");
        }
        let snapshot = store.snapshot_at(None).expect("snapshot");
        assert_eq!(snapshot.nodes.len(), 2, "session + turn nodes");
        let turn_node = snapshot
            .nodes
            .iter()
            .find(|node| node.execution_id == turn.to_string())
            .expect("turn node");
        assert_eq!(turn_node.phase, "finished");
        let events = store.events_after(Cursor::new(0), None).expect("events");
        assert_eq!(events.len(), 3);
        // Cursors are assigned monotonically by the store on append.
        assert_eq!(events[2].cursor, Cursor::new(3));
    }
}
