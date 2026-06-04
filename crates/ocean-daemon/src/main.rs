use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    env,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use async_trait::async_trait;
use axum::{
    extract::{Path, Query, State},
    http::{Method, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use ocean_agent::{room_guidance, AgentRuntime, PromptControl};
use ocean_agent_sdk::{
    AgentRole, AgentSessionId, AgentSessionResponse, AgentSessionSummary, AgentSessionsResponse,
    AgentTurnEvent, AgentTurnId, AgentTurnRequest, AgentTurnResponse, AgentTurnStatus,
    ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark, MarkKind, ProposalTally,
    ToolCall, ToolCallId, ToolResult,
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse,
    PermissionDecision as PermissionDecisionBody, PermissionDecisionRequest, PermissionId,
    PermissionStatus, PermissionsResponse, Project, ProjectConfig, ProjectId, ProjectResponse,
    ProjectsResponse, PromptRequest, RequestControlResponse,
    RequestCreateResponse, RequestId, RequestState, RequestStatus, RequestsResponse, RoomId,
    RoomPanelSnapshot, RoomSnapshot, RoomsResponse, SessionDetail, SessionId, SessionResponse,
    SessionRunState, SessionSummary,
};
use ocean_runtime::{
    tools::component::COMPONENT_WAIT_REGISTRY, AgentEvent,
    PermissionDecision as AgentPermissionDecision, PermissionPolicy,
};
use serde_json::{json, Value};
use tokio::{
    sync::{broadcast, oneshot, RwLock},
    task::JoinHandle,
};
use tokio_stream::{
    wrappers::{errors::BroadcastStreamRecvError, BroadcastStream},
    Stream, StreamExt,
};
use tokio_util::sync::CancellationToken;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    events: EventBus,
    agent_events: AgentEventBus,
    requests: RequestRegistry,
    permissions: PermissionRegistry,
}

type RequestRegistry = Arc<RwLock<HashMap<RequestId, RequestControl>>>;
type PermissionRegistry = Arc<RwLock<HashMap<PermissionId, PermissionWaiter>>>;

struct RequestControl {
    status: RequestStatus,
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

struct PermissionWaiter {
    status: PermissionStatus,
    sender: Option<oneshot::Sender<AgentPermissionDecision>>,
}

struct DaemonPermissionPolicy {
    allow_mutating: bool,
    request_id: RequestId,
    session_id: Option<SessionId>,
    events: EventBus,
    permissions: PermissionRegistry,
    requests: RequestRegistry,
    cancel: CancellationToken,
}

#[derive(Clone)]
struct EventBus {
    tx: broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<EventEnvelope>>>,
    history_limit: usize,
}

impl EventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            history: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(128)))),
            history_limit: capacity.clamp(1, 256),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    fn recent(&self, limit: usize) -> Vec<EventEnvelope> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        history.iter().rev().take(limit).cloned().collect()
    }

    fn emit(&self, event: EventEnvelope) {
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            history.push_back(event.clone());
            while history.len() > self.history_limit {
                history.pop_front();
            }
        }

        let _ = self.tx.send(event);
    }
}

/// Parallel broadcast bus that carries `AgentTurnEvent`s with full fidelity
/// (turn_id, call_id, thinking deltas, tool chunks). The legacy `OceanEvent`
/// bus still ships, but `/v1/agent/events` subscribes here so the TUI can
/// render real-time streaming output without the lossy round-trip.
#[derive(Clone)]
struct AgentEventBus {
    tx: broadcast::Sender<AgentEventEnvelope>,
}

#[derive(Clone)]
struct AgentEventEnvelope {
    id: Uuid,
    event: AgentTurnEvent,
}

impl AgentEventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentEventEnvelope> {
        self.tx.subscribe()
    }

    fn emit(&self, event: AgentTurnEvent) {
        let _ = self.tx.send(AgentEventEnvelope {
            id: Uuid::new_v4(),
            event,
        });
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ocean_daemon=info".parse()?),
        )
        .init();

    let bind = env::var("OCEAN_BIND").unwrap_or_else(|_| "127.0.0.1:4780".to_string());
    // Built-ins first, then connect any configured MCP servers (non-fatally)
    // and fold their tools into the capability registry before sharing it.
    let runtime = Arc::new(AgentRuntime::from_env()?.with_extensions().await);
    let state = AppState {
        runtime,
        events: EventBus::new(1024),
        agent_events: AgentEventBus::new(1024),
        requests: Arc::new(RwLock::new(HashMap::new())),
        permissions: Arc::new(RwLock::new(HashMap::new())),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/agent/turns", post(agent_turn))
        .route("/v1/agent/events", get(agent_events))
        .route("/v1/agent/sessions", get(agent_sessions))
        .route("/v1/agent/sessions/{id}", get(agent_session))
        .route("/v1/events", get(events))
        .route("/v1/prompt", post(prompt))
        .route("/v1/requests", get(requests).post(create_request))
        .route("/v1/requests/{id}/cancel", post(cancel_request))
        .route("/v1/permissions", get(permissions))
        .route("/v1/permissions/{id}/decision", post(permission_decision))
        .route("/v1/rooms", get(rooms))
        .route("/v1/rooms/{room_id}", get(room))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", get(session))
        .route("/v1/projects", get(projects_list).post(project_create))
        .route(
            "/v1/projects/{id}",
            get(project_get).patch(project_patch).delete(project_delete),
        )
        .route("/v1/model", get(model_get).post(model_set))
        .route("/v1/models", get(models_list))
        .route("/v1/component/event", post(component_event))
        .route("/v1/longhouse/demo", post(longhouse_demo))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = bind.parse().context("invalid OCEAN_BIND")?;
    tracing::info!(%addr, "ocean-daemon listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "service": "ocean-daemon",
        "routes": [
            "GET /health",
            "GET /ready",
            "GET /v1/events",
            "POST /v1/prompt",
            "GET /v1/requests",
            "POST /v1/requests",
            "POST /v1/requests/{id}/cancel",
            "GET /v1/permissions",
            "POST /v1/permissions/{id}/decision",
            "GET /v1/sessions",
            "GET /v1/sessions/{id}"
        ]
    }))
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        service: "ocean-daemon".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        backend: state.runtime.backend_name().to_string(),
    })
}

async fn ready(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(state.runtime.provider_readiness()).unwrap_or_else(|err| {
            json!({
                "ok": false,
                "error": {
                    "code": "READINESS_SERIALIZE_ERROR",
                    "message": err.to_string()
                }
            })
        }),
    )
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|event| match event {
        Ok(envelope) => {
            let event_type = event_type_name(&envelope.event);
            let id = envelope.id.to_string();
            let data = serde_json::to_string(&envelope).unwrap_or_else(|err| {
                json!({
                    "id": envelope.id,
                    "at": envelope.at,
                    "type": "error",
                    "message": format!("serialize event: {err}")
                })
                .to_string()
            });
            Some(Ok(Event::default().id(id).event(event_type).data(data)))
        }
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            let data = json!({
                "type": "error",
                "message": format!("event stream lagged by {skipped} events")
            })
            .to_string();
            Some(Ok(Event::default().event("error").data(data)))
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn prompt(
    State(state): State<AppState>,
    Json(mut req): Json<PromptRequest>,
) -> Json<ocean_core::PromptResponse> {
    let (request_id, cancel) =
        register_running_request(&state, &mut req, "prompt running", RequestState::Running).await;
    emit_user_message(&state.events, &req, request_id);

    let control = build_prompt_control(&state, request_id, req.session_id, req.yolo, cancel);
    let res = state.runtime.prompt(req, control).await;
    record_prompt_result(&state, request_id, &res).await;

    Json(res)
}

async fn create_request(
    State(state): State<AppState>,
    Json(mut req): Json<PromptRequest>,
) -> Json<RequestCreateResponse> {
    let (request_id, cancel) = register_running_request(
        &state,
        &mut req,
        "request accepted; prompt running",
        RequestState::Running,
    )
    .await;
    let session_id = req.session_id;
    emit_user_message(&state.events, &req, request_id);

    let control = build_prompt_control(&state, request_id, session_id, req.yolo, cancel);
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        let res = task_state.runtime.prompt(req, control).await;
        record_prompt_result(&task_state, request_id, &res).await;
    });
    attach_request_handle(&state, request_id, handle).await;

    Json(RequestCreateResponse {
        ok: true,
        request_id,
        session_id,
        state: RequestState::Running,
        message: "request accepted; daemon owns async execution".into(),
    })
}

async fn cancel_request(
    State(state): State<AppState>,
    Path(request_id): Path<RequestId>,
) -> Json<RequestControlResponse> {
    let mut requests = state.requests.write().await;
    let Some(control) = requests.get_mut(&request_id) else {
        return Json(RequestControlResponse {
            ok: false,
            request_id,
            state: RequestState::Errored,
            message: "request not found".into(),
        });
    };

    if !control.status.state.is_cancellable() {
        return Json(RequestControlResponse {
            ok: false,
            request_id,
            state: control.status.state,
            message: format!(
                "request is already terminal ({:?}); cancel ignored",
                control.status.state
            ),
        });
    }

    control.status.state = RequestState::Cancelling;
    control.status.message = Some("cancel requested; cancellation token sent".into());
    control.status.updated_at = Some(Utc::now());
    let session_id = control.status.session_id;
    let permission_id = control.status.permission_id;
    control.cancel.cancel();
    drop(requests);

    if let Some(permission_id) = permission_id {
        cancel_permission_waiter(&state.permissions, permission_id, request_id).await;
    }

    emit(
        &state.events,
        session_id,
        Some(request_id),
        None,
        OceanEvent::Cancelled {
            reason: Some("cancel requested; runtime cancellation token signalled".into()),
        },
    );

    Json(RequestControlResponse {
        ok: true,
        request_id,
        state: RequestState::Cancelling,
        message: "cancel requested; runtime cancellation token signalled".into(),
    })
}

async fn permission_decision(
    State(state): State<AppState>,
    Path(permission_id): Path<PermissionId>,
    Json(decision): Json<PermissionDecisionRequest>,
) -> Json<PermissionControlResponse> {
    if decision.permission_id != permission_id {
        return Json(PermissionControlResponse {
            ok: false,
            permission_id,
            message: "permission id mismatch between path and body".into(),
        });
    }

    let waiter = {
        let mut permissions = state.permissions.write().await;
        permissions.remove(&permission_id)
    };

    let Some(mut waiter) = waiter else {
        return Json(PermissionControlResponse {
            ok: false,
            permission_id,
            message: "permission request not found or already handled".into(),
        });
    };

    let agent_decision = match decision.decision {
        PermissionDecisionBody::Allow => AgentPermissionDecision::Allow,
        PermissionDecisionBody::Deny { reason } => AgentPermissionDecision::Deny {
            reason: reason.unwrap_or_else(|| "permission denied by operator".into()),
        },
    };

    if let Some(sender) = waiter.sender.take() {
        let _ = sender.send(agent_decision.clone());
    }

    update_request_permission_result(
        &state.requests,
        waiter.status.request_id,
        permission_id,
        agent_decision.clone(),
    )
    .await;

    emit(
        &state.events,
        waiter.status.session_id,
        Some(waiter.status.request_id),
        Some(permission_id),
        OceanEvent::PermissionDecision {
            allowed: matches!(agent_decision, AgentPermissionDecision::Allow),
            reason: match &agent_decision {
                AgentPermissionDecision::Allow => None,
                AgentPermissionDecision::AllowSession => Some("allow_session".into()),
                AgentPermissionDecision::Deny { reason } => Some(reason.clone()),
            },
        },
    );

    Json(PermissionControlResponse {
        ok: true,
        permission_id,
        message: "permission decision recorded and waiter released".into(),
    })
}

fn build_prompt_control(
    state: &AppState,
    request_id: RequestId,
    session_id: Option<SessionId>,
    allow_mutating: bool,
    cancel: CancellationToken,
) -> PromptControl {
    let control: Arc<dyn PermissionPolicy> = if allow_mutating {
        Arc::new(DaemonPermissionPolicy {
            allow_mutating: true,
            request_id,
            session_id,
            events: state.events.clone(),
            permissions: state.permissions.clone(),
            requests: state.requests.clone(),
            cancel: cancel.clone(),
        })
    } else {
        Arc::new(DaemonPermissionPolicy {
            allow_mutating: false,
            request_id,
            session_id,
            events: state.events.clone(),
            permissions: state.permissions.clone(),
            requests: state.requests.clone(),
            cancel: cancel.clone(),
        })
    };

    PromptControl::new(control).with_cancel(cancel)
}

#[async_trait]
impl PermissionPolicy for DaemonPermissionPolicy {
    async fn check(&self, tool_name: &str, args: &Value) -> AgentPermissionDecision {
        if self.allow_mutating {
            return AgentPermissionDecision::Allow;
        }

        let permission_id = PermissionId::new_v4();
        let reason = format!("permission required for {tool_name}");
        let status = PermissionStatus {
            permission_id,
            request_id: self.request_id,
            session_id: self.session_id,
            tool: tool_name.to_string(),
            reason: reason.clone(),
            args: args.clone(),
            created_at: Utc::now(),
        };
        let (tx, rx) = oneshot::channel();

        {
            let mut requests = self.requests.write().await;
            if let Some(control) = requests.get_mut(&self.request_id) {
                control.status.state = RequestState::WaitingForPermission;
                control.status.permission_id = Some(permission_id);
                control.status.message = Some(format!("waiting on permission for {tool_name}"));
                control.status.updated_at = Some(Utc::now());
            }
        }

        {
            let mut permissions = self.permissions.write().await;
            permissions.insert(
                permission_id,
                PermissionWaiter {
                    status: status.clone(),
                    sender: Some(tx),
                },
            );
        }

        emit(
            &self.events,
            self.session_id,
            Some(self.request_id),
            Some(permission_id),
            OceanEvent::PermissionRequest {
                tool: status.tool.clone(),
                reason,
                args: status.args.clone(),
            },
        );

        let decision = tokio::select! {
            decision = rx => match decision {
                Ok(decision) => decision,
                Err(_) => AgentPermissionDecision::Deny {
                    reason: "permission decision channel closed".into(),
                },
            },
            _ = self.cancel.cancelled() => AgentPermissionDecision::Deny {
                reason: "request cancelled while waiting for permission".into(),
            },
        };

        {
            let mut permissions = self.permissions.write().await;
            permissions.remove(&permission_id);
        }

        if matches!(decision, AgentPermissionDecision::Deny { .. }) && self.cancel.is_cancelled() {
            let mut requests = self.requests.write().await;
            if let Some(control) = requests.get_mut(&self.request_id) {
                if !control.status.state.is_terminal() {
                    control.status.state = RequestState::Cancelling;
                    control.status.message =
                        Some("cancel requested while waiting for permission".into());
                    control.status.updated_at = Some(Utc::now());
                }
            }
        }

        decision
    }
}

#[derive(Debug, serde::Deserialize, Default)]
struct SessionListQuery {
    /// Optional workspace path filter. When provided, returns only sessions
    /// bound to that exact workspace_root. When `?cwd=` is provided instead,
    /// the daemon resolves it to a workspace root (git toplevel) first.
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// `?all=1` short-circuits the filter and returns every bucket.
    #[serde(default)]
    all: Option<String>,
}

impl SessionListQuery {
    fn workspace_filter(&self, runtime: &AgentRuntime) -> Option<String> {
        if self.all.as_deref().is_some_and(|v| v == "1" || v == "true") {
            return None;
        }
        if let Some(ws) = self.workspace.as_deref() {
            return Some(ws.to_string());
        }
        if let Some(cwd) = self.cwd.as_deref() {
            return Some(
                runtime
                    .workspace_root_for(std::path::Path::new(cwd))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        None
    }
}

async fn sessions(
    State(state): State<AppState>,
    Query(q): Query<SessionListQuery>,
) -> Json<serde_json::Value> {
    let scope = q.workspace_filter(&state.runtime);
    match state.runtime.list_sessions(scope.as_deref()) {
        Ok(sessions) => Json(json!({"ok": true, "sessions": sessions, "workspace": scope})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ModelSetRequest {
    model: String,
}

async fn model_get(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (provider, model) = state.runtime.current_model();
    Json(json!({"ok": true, "provider": provider, "model": model}))
}

/// List the models the daemon can route to, plus the currently selected one,
/// for a client model picker.
async fn models_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (provider, model) = state.runtime.current_model();
    Json(json!({
        "ok": true,
        "current": { "provider": provider, "model": model },
        "models": ocean_agent::known_models(),
    }))
}

/// Emit a scripted-but-real Longhouse deliberation onto the agent event bus so
/// the Living Deck (the underwater-building UI) can render an actual council
/// flow before the full convening engine exists. Returns immediately; the flow
/// streams over `/v1/agent/events` as `Extension { extension: "longhouse" }`
/// events. This is a development harness, not the production convening path.
async fn longhouse_demo(State(state): State<AppState>) -> Json<serde_json::Value> {
    let bus = state.agent_events.clone();
    let topic_id = Uuid::new_v4();
    let board_id = Uuid::new_v4();

    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};
        let emit = |ev: LonghouseEvent| bus.emit(ev.into_turn_event());

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
                member(opus, AgentRole::Courier, "claude-opus-4-7", "Sales Courier · Opus"),
                member(kimi, AgentRole::Courier, "kimi-k2.6", "Sales Courier · Kimi"),
                member(deepseek, AgentRole::Courier, "deepseek-v4-pro", "Sales Courier · DeepSeek"),
                member(steward, AgentRole::Steward, "claude-opus-4-7", "Sales Steward"),
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
                summary: "Campaign Hub: Plan A creators avg 2.3x save-rate on prior Warner sounds".into(),
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
                    ProposalTally { proposal: prop_a, net_weight: 1.0 },
                    ProposalTally { proposal: prop_b, net_weight: 0.4 },
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
                ProposalTally { proposal: prop_a, net_weight: 2.6 },
                ProposalTally { proposal: prop_b, net_weight: 0.4 },
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

async fn model_set(
    State(state): State<AppState>,
    Json(req): Json<ModelSetRequest>,
) -> Json<serde_json::Value> {
    match state.runtime.set_model(&req.model) {
        Ok((provider, model)) => {
            tracing::info!(provider, model, "model swapped");
            Json(json!({"ok": true, "provider": provider, "model": model}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---- Projects --------------------------------------------------------------

#[derive(serde::Deserialize)]
struct CreateProjectRequest {
    name: String,
    workspace_root: String,
    #[serde(default)]
    config: ProjectConfig,
}

#[derive(serde::Deserialize)]
struct PatchProjectRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    config: Option<ProjectConfig>,
}

/// `GET /v1/projects` — list all registered projects.
async fn projects_list(State(state): State<AppState>) -> Json<ProjectsResponse> {
    match state.runtime.list_projects() {
        Ok(projects) => Json(ProjectsResponse { ok: true, projects, error: None }),
        Err(e) => Json(ProjectsResponse {
            ok: false,
            projects: vec![],
            error: Some(e.to_string()),
        }),
    }
}

/// `POST /v1/projects` — create a project bound to a directory.
async fn project_create(
    State(state): State<AppState>,
    Json(req): Json<CreateProjectRequest>,
) -> (StatusCode, Json<ProjectResponse>) {
    let now = Utc::now().timestamp_millis();
    let project = Project {
        id: uuid::Uuid::new_v4(),
        name: req.name,
        workspace_root: req.workspace_root,
        config: req.config,
        created_ms: now,
        updated_ms: now,
    };
    match state.runtime.upsert_project(project, now) {
        Ok(project) => (
            StatusCode::CREATED,
            Json(ProjectResponse { ok: true, project: Some(project), error: None }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse { ok: false, project: None, error: Some(e.to_string()) }),
        ),
    }
}

/// `GET /v1/projects/{id}` — one project plus its sessions (the sessions in the
/// project's `workspace_root` bucket).
async fn project_get(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.runtime.find_project(id) {
        Ok(Some(project)) => {
            let sessions = state
                .runtime
                .list_sessions(Some(&project.workspace_root))
                .unwrap_or_default();
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "project": project, "sessions": sessions })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("unknown project {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// `PATCH /v1/projects/{id}` — update name and/or config (partial).
async fn project_patch(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(req): Json<PatchProjectRequest>,
) -> (StatusCode, Json<ProjectResponse>) {
    let now = Utc::now().timestamp_millis();
    let existing = match state.runtime.find_project(id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ProjectResponse {
                    ok: false,
                    project: None,
                    error: Some(format!("unknown project {id}")),
                }),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProjectResponse { ok: false, project: None, error: Some(e.to_string()) }),
            )
        }
    };
    let updated = Project {
        name: req.name.unwrap_or(existing.name),
        config: req.config.unwrap_or(existing.config),
        ..existing
    };
    match state.runtime.upsert_project(updated, now) {
        Ok(project) => (
            StatusCode::OK,
            Json(ProjectResponse { ok: true, project: Some(project), error: None }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse { ok: false, project: None, error: Some(e.to_string()) }),
        ),
    }
}

/// `DELETE /v1/projects/{id}` — remove a project. Its sessions are NOT deleted;
/// they keep their workspace bucket and simply become project-less.
async fn project_delete(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.runtime.delete_project(id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("unknown project {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

async fn session(
    State(state): State<AppState>,
    Path(session_id): Path<SessionId>,
) -> (StatusCode, Json<SessionResponse>) {
    match state.runtime.session_detail(session_id) {
        Ok(mut session) => {
            enrich_session_detail(&state, &mut session).await;
            (
                StatusCode::OK,
                Json(SessionResponse {
                    ok: true,
                    session: Some(session),
                    error: None,
                }),
            )
        }
        Err(error) if is_not_found(&error) => (
            StatusCode::NOT_FOUND,
            Json(SessionResponse {
                ok: false,
                session: None,
                error: Some("session not found".into()),
            }),
        ),
        Err(error) => {
            tracing::warn!(%session_id, error = %error, "failed to read session detail");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SessionResponse {
                    ok: false,
                    session: None,
                    error: Some("session could not be read".into()),
                }),
            )
        }
    }
}

async fn rooms(State(state): State<AppState>) -> Json<RoomsResponse> {
    let input = room_projection_input(&state).await;
    Json(RoomsResponse {
        ok: true,
        rooms: build_room_snapshots(&input),
        error: None,
    })
}

async fn room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> (StatusCode, Json<RoomsResponse>) {
    let room_id = match parse_room_id(&room_id) {
        Ok(room_id) => room_id,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(RoomsResponse {
                    ok: false,
                    rooms: vec![],
                    error: Some(error),
                }),
            );
        }
    };

    let input = room_projection_input(&state).await;
    let room = build_room_snapshot(&input, room_id);
    (
        StatusCode::OK,
        Json(RoomsResponse {
            ok: true,
            rooms: vec![room],
            error: None,
        }),
    )
}

fn parse_room_id(room_id: &str) -> Result<RoomId, String> {
    RoomId::parse(room_id).ok_or_else(|| {
        format!("invalid room id '{room_id}'; expected pm, writers, orch_mesh, or review")
    })
}

struct RoomProjectionInput {
    runtime_status: String,
    sessions: Vec<SessionSummary>,
    requests: Vec<RequestStatus>,
    permissions: Vec<PermissionStatus>,
    events: Vec<EventEnvelope>,
}

async fn room_projection_input(state: &AppState) -> RoomProjectionInput {
    let sessions = state.runtime.list_sessions(None).unwrap_or_default();
    let requests = state
        .requests
        .read()
        .await
        .values()
        .map(|control| control.status.clone())
        .collect::<Vec<_>>();
    let permissions = pending_permissions_snapshot(&state.permissions).await;
    let events = state.events.recent(32);

    RoomProjectionInput {
        runtime_status: runtime_status_line(state.runtime.as_ref()),
        sessions,
        requests,
        permissions,
        events,
    }
}

fn build_room_snapshots(input: &RoomProjectionInput) -> Vec<RoomSnapshot> {
    [
        RoomId::Pm,
        RoomId::Writers,
        RoomId::OrchMesh,
        RoomId::Review,
    ]
    .into_iter()
    .map(|room_id| build_room_snapshot(input, room_id))
    .collect()
}

fn build_room_snapshot(input: &RoomProjectionInput, room_id: RoomId) -> RoomSnapshot {
    let updated_ms = input
        .events
        .first()
        .map(|event| event.at.timestamp_millis())
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    let panels = match room_id {
        RoomId::Pm => vec![
            panel(
                "Prompt rail",
                "event feed",
                if input.events.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_events(&input.events, 4),
            ),
            panel(
                "Sessions",
                "session list",
                if input.sessions.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_sessions(&input.sessions, 4),
            ),
            panel(
                "Runtime",
                "daemon state",
                "status",
                vec![
                    input.runtime_status.clone(),
                    format!("requests: {}", input.requests.len()),
                ],
            ),
        ],
        RoomId::Writers => vec![
            panel(
                "Drafts",
                "session list",
                if input.sessions.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_sessions(&input.sessions, 5),
            ),
            panel(
                "Transcript cues",
                "event feed",
                if input.events.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_events(&input.events, 5),
            ),
            panel(
                "Runtime",
                "daemon state",
                "status",
                vec![
                    input.runtime_status.clone(),
                    format!("sessions: {}", input.sessions.len()),
                ],
            ),
        ],
        RoomId::OrchMesh => vec![
            panel(
                "Board",
                "request rail",
                if input.requests.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_requests(&input.requests, 5),
            ),
            panel(
                "Permissions",
                "approval rail",
                if input.permissions.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_permissions(&input.permissions, 5),
            ),
            panel(
                "Events",
                "control feed",
                if input.events.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_events(&input.events, 6),
            ),
        ],
        RoomId::Review => vec![
            panel(
                "Review queue",
                "request rail",
                if input.requests.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_requests(&input.requests, 4),
            ),
            panel(
                "Evidence",
                "event rail",
                if input.events.is_empty() {
                    "empty"
                } else {
                    "active"
                },
                summarize_events(&input.events, 4),
            ),
            panel(
                "Gate",
                "release status",
                "status",
                vec![
                    input.runtime_status.clone(),
                    format!("pending permissions: {}", input.permissions.len()),
                ],
            ),
        ],
    };

    RoomSnapshot {
        room_id,
        title: room_id.title().into(),
        summary: room_id.summary().into(),
        status: input.runtime_status.clone(),
        updated_ms,
        panels,
    }
}

fn panel(title: &str, kind: &str, status: &str, mut lines: Vec<String>) -> RoomPanelSnapshot {
    if lines.is_empty() {
        lines.push("no live data yet".into());
    }

    RoomPanelSnapshot {
        title: title.into(),
        kind: kind.into(),
        status: status.into(),
        lines,
    }
}

fn runtime_status_line(runtime: &AgentRuntime) -> String {
    let readiness = runtime.provider_readiness();
    let mut status = format!(
        "{} · {} · {} · {}",
        runtime.backend_name(),
        readiness.provider.as_str(),
        readiness.model,
        if readiness.ok { "ready" } else { "degraded" }
    );

    if let Some(error) = readiness.error.as_ref() {
        status.push_str(" · ");
        status.push_str(&error.to_string());
    }

    status
}

fn summarize_sessions(sessions: &[SessionSummary], limit: usize) -> Vec<String> {
    let mut lines = sessions
        .iter()
        .take(limit)
        .map(|session| {
            format!(
                "{} · {} turns · {}",
                short_id(session.id),
                session.turns,
                session.title
            )
        })
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push("no sessions yet".into());
    }

    lines
}

fn summarize_requests(requests: &[RequestStatus], limit: usize) -> Vec<String> {
    let mut lines = requests
        .iter()
        .take(limit)
        .map(|status| {
            let mut line = format!(
                "{} · {}",
                short_id(status.request_id),
                request_state_label(status.state)
            );
            if let Some(message) = status.message.as_deref() {
                line.push_str(" · ");
                line.push_str(message);
            }
            line
        })
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push("no requests yet".into());
    }

    lines
}

fn summarize_permissions(permissions: &[PermissionStatus], limit: usize) -> Vec<String> {
    let mut lines = permissions
        .iter()
        .take(limit)
        .map(|permission| {
            format!(
                "{} · {} · {}",
                short_id(permission.request_id),
                permission.tool,
                permission.reason
            )
        })
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push("no pending permissions".into());
    }

    lines
}

fn summarize_events(events: &[EventEnvelope], limit: usize) -> Vec<String> {
    let mut lines = events
        .iter()
        .take(limit)
        .map(event_summary)
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push("no recent events".into());
    }

    lines
}

fn event_summary(event: &EventEnvelope) -> String {
    let actor = event
        .request_id
        .map(short_id)
        .or_else(|| event.session_id.map(short_id))
        .unwrap_or_else(|| short_id(event.id));

    match &event.event {
        OceanEvent::SessionCreated => format!("{actor} · session created"),
        OceanEvent::UserMessage { text } => format!("{actor} · user: {}", trim_line(text, 48)),
        OceanEvent::AssistantDelta { text } => {
            format!("{actor} · assistant: {}", trim_line(text, 48))
        }
        OceanEvent::ToolStarted { tool, .. } => format!("{actor} · tool start: {tool}"),
        OceanEvent::ToolOutput {
            tool,
            text,
            is_error,
        } => format!(
            "{actor} · tool {}: {}",
            tool,
            if *is_error {
                trim_line(text, 42)
            } else {
                trim_line(text, 48)
            }
        ),
        OceanEvent::ToolEnded { tool, is_error } => format!(
            "{actor} · tool end: {tool}{}",
            if *is_error { " (error)" } else { "" }
        ),
        OceanEvent::PermissionRequest { tool, reason, .. } => {
            format!("{actor} · perm: {tool} · {reason}")
        }
        OceanEvent::PermissionDecision { allowed, reason } => format!(
            "{actor} · permission {}{}",
            if *allowed { "allowed" } else { "denied" },
            reason
                .as_deref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        ),
        OceanEvent::TurnFinished { ok, wall_ms } => {
            format!(
                "{actor} · turn {} · {wall_ms}ms",
                if *ok { "ok" } else { "failed" }
            )
        }
        OceanEvent::Cancelled { reason } => format!(
            "{actor} · cancelled{}",
            reason
                .as_deref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        ),
        OceanEvent::Error { message } => format!("{actor} · error: {}", trim_line(message, 48)),
    }
}

fn request_state_label(state: RequestState) -> &'static str {
    match state {
        RequestState::Queued => "queued",
        RequestState::Running => "running",
        RequestState::WaitingForPermission => "waiting-permission",
        RequestState::Cancelling => "cancelling",
        RequestState::Cancelled => "cancelled",
        RequestState::Completed => "completed",
        RequestState::Errored => "errored",
    }
}

fn short_id(id: uuid::Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn trim_line(text: &str, max_chars: usize) -> String {
    let trimmed = text.lines().next().unwrap_or_default().trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }

    let mut clipped = trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    clipped.push('…');
    clipped
}

async fn enrich_session_detail(state: &AppState, session: &mut SessionDetail) {
    let requests = state.requests.read().await;
    let mut matching = requests
        .values()
        .filter(|control| control.status.session_id == Some(session.id))
        .map(|control| control.status.clone())
        .collect::<Vec<_>>();
    matching.sort_by_key(|status| status.updated_at.or(status.started_at));
    matching.reverse();

    session.active_requests = matching
        .iter()
        .filter(|status| !status.state.is_terminal())
        .map(|status| status.request_id)
        .collect();
    session.pending_permissions = matching
        .iter()
        .filter_map(|status| status.permission_id)
        .collect();

    if let Some(active) = matching.iter().find(|status| !status.state.is_terminal()) {
        session.state = session_run_state(active.state);
        session.resumable = false;
    } else if let Some(latest) = matching.first() {
        session.state = session_run_state(latest.state);
        session.resumable = true;
    }
}

fn session_run_state(state: RequestState) -> SessionRunState {
    match state {
        RequestState::Queued | RequestState::Running => SessionRunState::Running,
        RequestState::WaitingForPermission => SessionRunState::WaitingForPermission,
        RequestState::Cancelling => SessionRunState::Cancelling,
        RequestState::Cancelled => SessionRunState::Cancelled,
        RequestState::Completed => SessionRunState::Completed,
        RequestState::Errored => SessionRunState::Errored,
    }
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

async fn permissions(State(state): State<AppState>) -> Json<PermissionsResponse> {
    Json(PermissionsResponse {
        ok: true,
        permissions: pending_permissions_snapshot(&state.permissions).await,
        error: None,
    })
}

async fn requests(State(state): State<AppState>) -> Json<RequestsResponse> {
    let mut requests = state
        .requests
        .read()
        .await
        .values()
        .map(|control| control.status.clone())
        .collect::<Vec<_>>();
    requests.sort_by_key(|status| status.started_at);
    requests.reverse();
    Json(RequestsResponse {
        ok: true,
        requests,
        error: None,
    })
}

async fn pending_permissions_snapshot(permissions: &PermissionRegistry) -> Vec<PermissionStatus> {
    let mut pending = permissions
        .read()
        .await
        .values()
        .map(|waiter| waiter.status.clone())
        .collect::<Vec<_>>();
    pending.sort_by_key(|status| status.created_at);
    pending.reverse();
    pending
}

async fn register_running_request(
    state: &AppState,
    req: &mut PromptRequest,
    message: impl Into<String>,
    state_value: RequestState,
) -> (RequestId, CancellationToken) {
    let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
    req.request_id = Some(request_id);
    let cancel = CancellationToken::new();
    let now = Utc::now();

    state.requests.write().await.insert(
        request_id,
        RequestControl {
            status: RequestStatus {
                request_id,
                session_id: req.session_id,
                state: state_value,
                permission_id: None,
                message: Some(message.into()),
                started_at: Some(now),
                updated_at: Some(now),
                finished_at: None,
            },
            cancel: cancel.clone(),
            handle: None,
        },
    );

    (request_id, cancel)
}

async fn attach_request_handle(state: &AppState, request_id: RequestId, handle: JoinHandle<()>) {
    let mut requests = state.requests.write().await;
    if let Some(control) = requests.get_mut(&request_id) {
        control.handle = Some(handle);
    }
}

async fn cancel_permission_waiter(
    permissions: &PermissionRegistry,
    permission_id: PermissionId,
    request_id: RequestId,
) {
    let waiter = {
        let mut permissions = permissions.write().await;
        permissions.remove(&permission_id)
    };

    if let Some(mut waiter) = waiter {
        if waiter.status.request_id != request_id {
            return;
        }
        if let Some(sender) = waiter.sender.take() {
            let _ = sender.send(AgentPermissionDecision::Deny {
                reason: "request cancelled while waiting for permission".into(),
            });
        }
    }
}

async fn update_request_permission_result(
    requests: &RequestRegistry,
    request_id: RequestId,
    permission_id: PermissionId,
    decision: AgentPermissionDecision,
) {
    let mut requests = requests.write().await;
    let Some(control) = requests.get_mut(&request_id) else {
        return;
    };

    if control.status.state.is_terminal()
        || matches!(control.status.state, RequestState::Cancelling)
    {
        return;
    }

    control.status.state = RequestState::Running;
    control.status.permission_id = None;
    control.status.message = Some(match decision {
        AgentPermissionDecision::Allow => format!("permission {permission_id} allowed"),
        AgentPermissionDecision::AllowSession => {
            format!("permission {permission_id} allowed for session")
        }
        AgentPermissionDecision::Deny { ref reason } => {
            format!("permission {permission_id} denied: {reason}")
        }
    });
    control.status.updated_at = Some(Utc::now());
}

fn emit_user_message(events: &EventBus, req: &PromptRequest, request_id: RequestId) {
    emit(
        events,
        req.session_id,
        Some(request_id),
        None,
        OceanEvent::UserMessage {
            text: req.prompt.clone(),
        },
    );
}

async fn record_prompt_result(
    state: &AppState,
    request_id: RequestId,
    res: &ocean_core::PromptResponse,
) {
    let desired_state = if res.ok {
        RequestState::Completed
    } else {
        RequestState::Errored
    };
    let message = if res.ok {
        "prompt completed".to_string()
    } else {
        res.stderr.clone()
    };

    let final_state = update_request_finished(
        &state.requests,
        request_id,
        res.session_id,
        desired_state,
        message,
    )
    .await;

    match final_state {
        Some(RequestState::Completed) => {
            if !res.stdout.trim().is_empty() {
                emit(
                    &state.events,
                    res.session_id,
                    Some(request_id),
                    None,
                    OceanEvent::AssistantDelta {
                        text: res.stdout.clone(),
                    },
                );
            }
            emit(
                &state.events,
                res.session_id,
                Some(request_id),
                None,
                OceanEvent::TurnFinished {
                    ok: true,
                    wall_ms: res.wall_ms,
                },
            );
        }
        Some(RequestState::Errored) => {
            emit(
                &state.events,
                res.session_id,
                Some(request_id),
                None,
                OceanEvent::Error {
                    message: res.stderr.clone(),
                },
            );
        }
        Some(RequestState::Cancelled) => {
            emit(
                &state.events,
                res.session_id,
                Some(request_id),
                None,
                OceanEvent::Cancelled {
                    reason: Some("request marked cancelled after runtime returned".into()),
                },
            );
        }
        _ => {}
    }
}

async fn update_request_finished(
    requests: &RequestRegistry,
    request_id: RequestId,
    session_id: Option<SessionId>,
    desired_state: RequestState,
    message: String,
) -> Option<RequestState> {
    let mut requests = requests.write().await;
    let control = requests.get_mut(&request_id)?;
    let status = &mut control.status;

    if matches!(
        status.state,
        RequestState::Cancelling | RequestState::Cancelled
    ) {
        status.session_id = session_id.or(status.session_id);
        status.state = RequestState::Cancelled;
        status.message = Some(
            "cancel requested; runtime completed after cancellation request and output was ignored"
                .into(),
        );
        status.updated_at = Some(Utc::now());
        status.finished_at = Some(Utc::now());
        let _ = control.handle.take();
        return Some(RequestState::Cancelled);
    }

    if status.state.is_terminal() {
        let _ = control.handle.take();
        return Some(status.state);
    }

    status.session_id = session_id.or(status.session_id);
    status.state = desired_state;
    status.message = Some(message);
    status.updated_at = Some(Utc::now());
    status.finished_at = Some(Utc::now());
    let _ = control.handle.take();
    Some(desired_state)
}

// ---------------------------------------------------------------------------
// /v1/agent/* handlers — product-shaped agent-turn API
// ---------------------------------------------------------------------------

async fn agent_turn(
    State(state): State<AppState>,
    Json(req): Json<AgentTurnRequest>,
) -> (StatusCode, Json<AgentTurnResponse>) {
    let AgentTurnRequest {
        session_id,
        prompt,
        cwd,
        guidance: _,
        room_id,
        project_id,
        client_type,
    } = req;

    // Resolve the working directory: a non-empty cwd wins; else the project's
    // workspace_root; else an explicit error — never the daemon's own launch
    // dir. This is the fix for "every session reverts to ocean-os".
    let cwd = match state.runtime.resolve_cwd_for_turn(project_id, &cwd) {
        Ok(resolved) => resolved,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AgentTurnResponse {
                    ok: false,
                    turn_id: AgentTurnId::new_v4(),
                    session_id: session_id.unwrap_or_else(AgentSessionId::new_v4),
                    status: AgentTurnStatus::Failed,
                    event_id_prefix: String::new(),
                    error: Some(error.to_string()),
                }),
            );
        }
    };

    let is_new_session = session_id.is_none();
    let session_id = session_id.unwrap_or_else(AgentSessionId::new_v4);
    let turn_id = AgentTurnId::new_v4();
    let request_id = turn_id.0;
    let event_prefix = request_id.to_string()[..8].to_string();

    if is_new_session {
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::SessionCreated {
                session_id,
                title: prompt.chars().take(60).collect(),
                cwd: cwd.clone(),
            },
        );
    }

    // Parse room_id for F1-F4 guidance injection (new in Track-0 room migration).
    let room_id = match parse_agent_turn_room(room_id.as_deref()) {
        Ok(room_id) => room_id,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AgentTurnResponse {
                    ok: false,
                    turn_id,
                    session_id,
                    status: AgentTurnStatus::Failed,
                    event_id_prefix: event_prefix,
                    error: Some(error),
                }),
            );
        }
    };

    // Emit turn_started, tagged with the model driving this turn so clients
    // can show it live and reflect a mid-session swap.
    let (_provider, current_model) = state.runtime.current_model();
    emit_agent(
        &state.events,
        &state.agent_events,
        session_id,
        AgentTurnEvent::TurnStarted {
            turn_id,
            session_id,
            model: Some(current_model),
        },
    );

    let guided_prompt = apply_room_guidance(room_id, &prompt);
    let mut prompt_req = PromptRequest {
        prompt: guided_prompt,
        request_id: Some(request_id),
        session_id: Some(core_sid(session_id)),
        // New session → allow creating under the freshly-minted id. Resume
        // (client supplied the id) → strict: error if that session is gone,
        // rather than silently forking a fresh transcript under the same id.
        create_if_missing: is_new_session,
        max_turns: None,
        yolo: true,
        cwd,
        project_id,
        client_type,
    };

    // Register the turn in the request map so it's cancellable via
    // POST /v1/requests/{turn_id}/cancel (the turn_id IS the request_id). The
    // returned token is threaded into PromptControl below; the agent loop polls
    // it, so a halt from the client actually stops the turn mid-flight.
    let (_request_id, cancel) =
        register_running_request(&state, &mut prompt_req, "agent turn running", RequestState::Running)
            .await;

    // Wire up the runtime → bus streaming bridge. Every TextDelta /
    // ThinkingDelta / ToolExecution* event the agent emits gets forwarded
    // onto the AgentEventBus in real time so SSE clients render as it streams.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let bridge_bus = state.agent_events.clone();
    let bridge_turn_id = turn_id;
    let bridge_session_id = session_id;
    let bridge = tokio::spawn(async move {
        let mut tool_call_ids: HashMap<String, ToolCallId> = HashMap::new();
        while let Some(ev) = event_rx.recv().await {
            match ev {
                AgentEvent::TextDelta { delta } => {
                    if delta.is_empty() {
                        continue;
                    }
                    bridge_bus.emit(AgentTurnEvent::AssistantTextDelta {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        delta,
                    });
                }
                AgentEvent::ThinkingDelta { delta } => {
                    if delta.is_empty() {
                        continue;
                    }
                    bridge_bus.emit(AgentTurnEvent::ThinkingDelta {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        delta,
                    });
                }
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => {
                    let call_id = ToolCallId(Uuid::new_v4());
                    tool_call_ids.insert(tool_call_id, call_id.clone());
                    bridge_bus.emit(AgentTurnEvent::ToolCallStarted {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        call: ToolCall {
                            id: call_id,
                            name: tool_name,
                            args_json: args,
                        },
                    });
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name: _,
                    is_error,
                    content,
                } => {
                    let call_id = tool_call_ids
                        .remove(&tool_call_id)
                        .unwrap_or_else(|| ToolCallId(Uuid::new_v4()));
                    let output = render_tool_output(&content);
                    bridge_bus.emit(AgentTurnEvent::ToolCallFinished {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        call_id,
                        result: ToolResult {
                            ok: !is_error,
                            output,
                            metadata_json: None,
                        },
                    });
                }
                AgentEvent::PermissionDenied { tool_name, reason } => {
                    let call_id = ToolCallId(Uuid::new_v4());
                    bridge_bus.emit(AgentTurnEvent::ToolCallFinished {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        call_id,
                        result: ToolResult {
                            ok: false,
                            output: format!("permission denied for {tool_name}: {reason}"),
                            metadata_json: None,
                        },
                    });
                }
                AgentEvent::Render {
                    id,
                    kind,
                    props,
                    replace,
                } => {
                    bridge_bus.emit(AgentTurnEvent::ComponentRender {
                        session_id: bridge_session_id,
                        component_id: id,
                        kind,
                        props,
                        replace,
                    });
                }
                AgentEvent::Unmount { id } => {
                    bridge_bus.emit(AgentTurnEvent::ComponentUnmount {
                        session_id: bridge_session_id,
                        component_id: id,
                    });
                }
                AgentEvent::BrowserActivity { active } => {
                    bridge_bus.emit(AgentTurnEvent::BrowserActivity {
                        session_id: bridge_session_id,
                        active,
                    });
                }
                _ => {}
            }
        }
    });

    let control = build_prompt_control(&state, request_id, Some(core_sid(session_id)), true, cancel)
        .with_event_sink(event_tx);
    let res = state.runtime.prompt(prompt_req, control).await;
    // Wait for the bridge to drain (the sender has been dropped by now).
    let _ = bridge.await;
    // Prefer real provider usage; fall back to a visible-text estimate only
    // when the provider reported no output tokens.
    let output_tokens = if res.usage.output > 0 {
        res.usage.output
    } else {
        estimate_visible_tokens(&res.stdout)
    };
    let input_tokens = (res.usage.input > 0).then_some(res.usage.input);
    let cache_read_tokens = (res.usage.cache_read > 0).then_some(res.usage.cache_read);
    let tokens_per_second = if res.wall_ms > 0 {
        Some((output_tokens as f64) / (res.wall_ms as f64 / 1000.0))
    } else {
        None
    };
    record_prompt_result(&state, request_id, &res).await;

    tracing::info!(
        turn_id = %turn_id,
        request_id = %request_id,
        session_id = %session_id,
        ok = res.ok,
        wall_ms = res.wall_ms,
        input_tokens = res.usage.input,
        output_tokens,
        cache_read = res.usage.cache_read,
        total_tokens = res.usage.total_tokens,
        tokens_per_second,
        "agent turn finished"
    );

    // NOTE: assistant text already streamed delta-by-delta through the bridge,
    // so we do NOT re-emit res.stdout here. Just close out the turn.
    if res.ok {
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: Some(res.wall_ms),
                output_tokens: Some(output_tokens),
                input_tokens,
                cache_read_tokens,
                tokens_per_second,
            },
        );
    } else {
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status: AgentTurnStatus::Failed,
                error: Some(res.stderr.clone()),
                wall_ms: Some(res.wall_ms),
                output_tokens: Some(output_tokens),
                input_tokens,
                cache_read_tokens,
                tokens_per_second,
            },
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(AgentTurnResponse {
            ok: true,
            turn_id,
            session_id,
            status: if res.ok {
                AgentTurnStatus::Completed
            } else {
                AgentTurnStatus::Failed
            },
            event_id_prefix: event_prefix,
            error: if res.ok { None } else { Some(res.stderr) },
        }),
    )
}


fn parse_agent_turn_room(room_id: Option<&str>) -> Result<Option<RoomId>, String> {
    let Some(room_id) = room_id.map(str::trim).filter(|room_id| !room_id.is_empty()) else {
        return Ok(None);
    };

    RoomId::parse(room_id).map(Some).ok_or_else(|| {
        format!("unsupported room_id '{room_id}'; supported rooms: pm, writers, orch_mesh, review")
    })
}

fn apply_room_guidance(room_id: Option<RoomId>, prompt: &str) -> String {
    match room_id {
        Some(room_id) => format!("{}\n\n{}", room_guidance(room_id), prompt),
        None => prompt.to_string(),
    }
}

/// Receive a user interaction event for a rendered component and deliver it
/// to the waiting `component_wait` tool call.
///
/// Request body:
/// ```json
/// {
///   "session_id": "uuid-of-session",
///   "component_id": "agent-chosen-id",
///   "event": { "type": "submit", "data": { ... } }
/// }
/// ```
///
/// Returns 200 if the event was delivered, 404 if nobody is waiting on that
/// component, 400 on missing fields.
async fn component_event(
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let session_id = match body.get("session_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'session_id'" })),
            );
        }
    };
    let component_id = match body.get("component_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'component_id'" })),
            );
        }
    };
    let event = body.get("event").cloned().unwrap_or(json!({}));

    let sender = {
        let mut pending = match COMPONENT_WAIT_REGISTRY.pending.lock() {
            Ok(g) => g,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("registry lock: {e}") })),
                );
            }
        };
        pending.remove(&(session_id.clone(), component_id.clone()))
    };

    match sender {
        Some(tx) => {
            if tx.send(event).is_err() {
                // Receiver dropped (timeout or cancellation) — not an error for the caller.
                (
                    StatusCode::GONE,
                    Json(json!({ "status": "nobody waiting" })),
                )
            } else {
                (
                    StatusCode::OK,
                    Json(json!({ "status": "delivered" })),
                )
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no pending wait for component", "session_id": session_id, "component_id": component_id })),
        ),
    }
}

#[derive(Debug, serde::Deserialize, Default)]
struct AgentEventsQuery {
    /// When set, the SSE stream only delivers events for this session. Without
    /// it the stream is the legacy global firehose (every session's events).
    /// This is the server-side floor for the cross-surface bleed: the GPUI app
    /// and the Chrome extension subscribe scoped to their own session id and no
    /// longer interleave each other's transcript.
    #[serde(default)]
    session_id: Option<AgentSessionId>,
}

async fn agent_events(
    State(state): State<AppState>,
    Query(q): Query<AgentEventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let want = q.session_id;
    let stream =
        BroadcastStream::new(state.agent_events.subscribe()).filter_map(move |event| match event {
            Ok(envelope) => {
                // Scope to the requested session when one was given. Events that
                // carry no session_id (e.g. Extension) always pass through —
                // they aren't session-scoped. Every session-bearing event is
                // dropped unless it matches; SessionCreated/TurnStarted carry
                // their own id, so a client that already knows its session id
                // still receives its own adoption events.
                if let Some(want) = want {
                    if let Some(sid) = envelope.event.session_id() {
                        if sid != want {
                            return None;
                        }
                    }
                }
                let id = envelope.id.to_string();
                let event_type = agent_event_type_name(&envelope.event);
                let data = serde_json::to_string(&envelope.event).unwrap_or_else(|_| {
                    r#"{"type":"error","message":"serialize failed"}"#.to_string()
                });
                Some(Ok(Event::default().id(id).event(event_type).data(data)))
            }
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                let data =
                    json!({ "type": "error", "message": format!("stream lagged by {skipped}") })
                        .to_string();
                Some(Ok(Event::default().event("error").data(data)))
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn agent_sessions(
    State(state): State<AppState>,
    Query(q): Query<SessionListQuery>,
) -> Json<AgentSessionsResponse> {
    let scope = q.workspace_filter(&state.runtime);
    let core_sessions = state
        .runtime
        .list_sessions(scope.as_deref())
        .unwrap_or_default();
    let summaries: Vec<AgentSessionSummary> = core_sessions
        .into_iter()
        .map(|s| AgentSessionSummary {
            id: sdk_sid(s.id),
            title: s.title,
            cwd: s.workspace_root.clone().unwrap_or_default(),
            updated_at: chrono::Utc::now(),
            active_turn: None,
            turn_count: s.turns,
        })
        .collect();
    Json(AgentSessionsResponse {
        ok: true,
        sessions: summaries,
        error: None,
    })
}

async fn agent_session(
    State(state): State<AppState>,
    Path(session_id): Path<AgentSessionId>,
) -> (StatusCode, Json<AgentSessionResponse>) {
    let core_id = core_sid(session_id);
    match state.runtime.session_detail(core_id) {
        Ok(session) => (
            StatusCode::OK,
            Json(AgentSessionResponse {
                ok: true,
                session: Some(ocean_agent_sdk::AgentSession {
                    id: session_id,
                    title: session.title.clone(),
                    cwd: String::new(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    active_turn: None,
                }),
                turns: vec![],
                error: None,
            }),
        ),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(AgentSessionResponse {
                ok: false,
                session: None,
                turns: vec![],
                error: Some("session not found".into()),
            }),
        ),
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers — bridge between core SessionId/Uuid and SDK wrappers
// ---------------------------------------------------------------------------

/// Wrap a raw Uuid in a SessionId type alias.
fn core_sid(sdk_id: AgentSessionId) -> SessionId {
    sdk_id.inner()
}

/// Wrap a raw Uuid in an AgentSessionId wrapper.
fn sdk_sid(core_id: SessionId) -> AgentSessionId {
    AgentSessionId(core_id)
}

fn estimate_visible_tokens(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

fn emit_agent(
    events: &EventBus,
    agent_events: &AgentEventBus,
    session_id: AgentSessionId,
    event: AgentTurnEvent,
) {
    // Always publish on the AgentEventBus (full fidelity for SSE consumers).
    agent_events.emit(event.clone());
    // Legacy OceanEvent bus mirror for any subscriber still on it.
    if let Some(inner) = agent_to_ocean_event(event) {
        let mut env = EventEnvelope::new(inner);
        env.session_id = Some(core_sid(session_id));
        events.emit(env);
    }
}

fn agent_to_ocean_event(event: AgentTurnEvent) -> Option<OceanEvent> {
    match event {
        AgentTurnEvent::TurnStarted {
            turn_id: _,
            session_id: _,
            ..
        } => None,
        AgentTurnEvent::AssistantTextDelta { turn_id: _, delta, .. } => {
            Some(OceanEvent::AssistantDelta { text: delta })
        }
        AgentTurnEvent::ThinkingDelta { .. } => None,
        AgentTurnEvent::ToolCallStarted { turn_id: _, call, .. } => Some(OceanEvent::ToolStarted {
            tool: call.name,
            args: call.args_json,
        }),
        AgentTurnEvent::TurnFinished {
            turn_id: _,
            status,
            error: _,
            wall_ms,
            output_tokens: _,
            tokens_per_second: _,
            ..
        } => Some(OceanEvent::TurnFinished {
            ok: matches!(status, AgentTurnStatus::Completed),
            wall_ms: wall_ms.unwrap_or(0),
        }),
        AgentTurnEvent::ToolCallChunk {
            turn_id: _,
            call_id: _,
            chunk,
            ..
        } => Some(OceanEvent::ToolOutput {
            tool: "tool".into(),
            text: chunk,
            is_error: false,
        }),
        AgentTurnEvent::ToolCallFinished {
            turn_id: _,
            call_id: _,
            result,
            ..
        } => Some(OceanEvent::ToolEnded {
            tool: "tool".into(),
            is_error: !result.ok,
        }),
        AgentTurnEvent::SessionCreated {
            session_id: _,
            title: _,
            cwd: _,
        } => Some(OceanEvent::SessionCreated),
        AgentTurnEvent::Extension {
            extension: _,
            payload: _,
        } => None,
        AgentTurnEvent::ComponentRender { .. } => None,
        AgentTurnEvent::ComponentUnmount { .. } => None,
        AgentTurnEvent::BrowserActivity { .. } => None,
    }
}

fn agent_event_type_name(event: &AgentTurnEvent) -> &'static str {
    match event {
        AgentTurnEvent::TurnStarted { .. } => "turn_started",
        AgentTurnEvent::AssistantTextDelta { .. } => "assistant_text_delta",
        AgentTurnEvent::ThinkingDelta { .. } => "thinking_delta",
        AgentTurnEvent::ToolCallStarted { .. } => "tool_call_started",
        AgentTurnEvent::ToolCallChunk { .. } => "tool_call_chunk",
        AgentTurnEvent::ToolCallFinished { .. } => "tool_call_finished",
        AgentTurnEvent::TurnFinished { .. } => "turn_finished",
        AgentTurnEvent::SessionCreated { .. } => "session_created",
        AgentTurnEvent::Extension {
            extension: _,
            payload: _,
        } => "extension",
        AgentTurnEvent::ComponentRender { .. } => "component_render",
        AgentTurnEvent::ComponentUnmount { .. } => "component_unmount",
        AgentTurnEvent::BrowserActivity { .. } => "browser_activity",
    }
}

fn render_tool_output(content: &[ocean_protocol::Content]) -> String {
    use ocean_protocol::Content;
    let mut out = String::new();
    for c in content {
        match c {
            Content::Text { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            Content::Thinking { thinking, .. } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(thinking);
            }
            Content::Image { .. } => {}
            Content::ToolCall {
                name, arguments, ..
            } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("→ {name}({arguments})"));
            }
        }
    }
    out
}

fn emit(
    events: &EventBus,
    session_id: Option<ocean_core::SessionId>,
    request_id: Option<ocean_core::RequestId>,
    permission_id: Option<ocean_core::PermissionId>,
    event: OceanEvent,
) {
    let mut envelope = EventEnvelope::new(event);
    envelope.session_id = session_id;
    envelope.request_id = request_id;
    envelope.permission_id = permission_id;
    events.emit(envelope);
}

fn event_type_name(event: &OceanEvent) -> &'static str {
    match event {
        OceanEvent::SessionCreated => "session_created",
        OceanEvent::UserMessage { .. } => "user_message",
        OceanEvent::AssistantDelta { .. } => "assistant_delta",
        OceanEvent::ToolStarted { .. } => "tool_started",
        OceanEvent::ToolOutput { .. } => "tool_output",
        OceanEvent::ToolEnded { .. } => "tool_ended",
        OceanEvent::PermissionRequest { .. } => "permission_request",
        OceanEvent::PermissionDecision { .. } => "permission_decision",
        OceanEvent::TurnFinished { .. } => "turn_finished",
        OceanEvent::Cancelled { .. } => "cancelled",
        OceanEvent::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(request_id: RequestId, state: RequestState) -> RequestControl {
        RequestControl {
            status: RequestStatus {
                request_id,
                session_id: None,
                state,
                permission_id: None,
                message: None,
                started_at: Some(Utc::now()),
                updated_at: Some(Utc::now()),
                finished_at: None,
            },
            cancel: CancellationToken::new(),
            handle: None,
        }
    }

    fn permission_status(permission_id: PermissionId, request_id: RequestId) -> PermissionStatus {
        PermissionStatus {
            permission_id,
            request_id,
            session_id: None,
            tool: "write".into(),
            reason: "permission required for write".into(),
            args: json!({"path": "src/lib.rs"}),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn finish_does_not_overwrite_terminal_state() {
        let request_id = RequestId::new_v4();
        let requests = Arc::new(RwLock::new(HashMap::from([(
            request_id,
            status(request_id, RequestState::Completed),
        )])));

        let state = update_request_finished(
            &requests,
            request_id,
            None,
            RequestState::Errored,
            "late error".into(),
        )
        .await;

        assert_eq!(state, Some(RequestState::Completed));
        let requests = requests.read().await;
        let status = requests.get(&request_id).unwrap();
        assert_eq!(status.status.state, RequestState::Completed);
        assert_eq!(status.status.message, None);
    }

    #[tokio::test]
    async fn finish_converts_cancelling_to_cancelled() {
        let request_id = RequestId::new_v4();
        let requests = Arc::new(RwLock::new(HashMap::from([(
            request_id,
            status(request_id, RequestState::Cancelling),
        )])));

        let state = update_request_finished(
            &requests,
            request_id,
            None,
            RequestState::Completed,
            "late completion".into(),
        )
        .await;

        assert_eq!(state, Some(RequestState::Cancelled));
        let requests = requests.read().await;
        let status = requests.get(&request_id).unwrap();
        assert_eq!(status.status.state, RequestState::Cancelled);
        assert!(status.status.finished_at.is_some());
        assert!(status
            .status
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("cancel requested"));
    }
    #[tokio::test]
    async fn permission_result_records_decision_on_waiting_request() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let mut control = status(request_id, RequestState::WaitingForPermission);
        control.status.permission_id = Some(permission_id);
        let requests = Arc::new(RwLock::new(HashMap::from([(request_id, control)])));

        update_request_permission_result(
            &requests,
            request_id,
            permission_id,
            AgentPermissionDecision::Deny {
                reason: "operator denied".into(),
            },
        )
        .await;

        let requests = requests.read().await;
        let status = requests.get(&request_id).unwrap();
        assert_eq!(status.status.state, RequestState::Running);
        assert_eq!(status.status.permission_id, None);
        assert!(status
            .status
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("operator denied"));
    }

    #[tokio::test]
    async fn pending_permissions_snapshot_exposes_request_context() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let permissions = Arc::new(RwLock::new(HashMap::from([(
            permission_id,
            PermissionWaiter {
                status: permission_status(permission_id, request_id),
                sender: None,
            },
        )])));

        let pending = pending_permissions_snapshot(&permissions).await;

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].permission_id, permission_id);
        assert_eq!(pending[0].request_id, request_id);
        assert_eq!(pending[0].tool, "write");
        assert_eq!(pending[0].args, json!({"path": "src/lib.rs"}));
    }

    #[tokio::test]
    async fn permission_events_are_observable_with_ids() {
        let events = EventBus::new(8);
        let mut rx = events.subscribe();
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();

        emit(
            &events,
            None,
            Some(request_id),
            Some(permission_id),
            OceanEvent::PermissionRequest {
                tool: "write".into(),
                reason: "permission required for write".into(),
                args: json!({"path": "src/lib.rs"}),
            },
        );
        emit(
            &events,
            None,
            Some(request_id),
            Some(permission_id),
            OceanEvent::PermissionDecision {
                allowed: false,
                reason: Some("operator denied".into()),
            },
        );

        let request_event = rx.recv().await.unwrap();
        assert_eq!(request_event.request_id, Some(request_id));
        assert_eq!(request_event.permission_id, Some(permission_id));
        assert!(matches!(
            request_event.event,
            OceanEvent::PermissionRequest { .. }
        ));

        let decision_event = rx.recv().await.unwrap();
        assert_eq!(decision_event.request_id, Some(request_id));
        assert_eq!(decision_event.permission_id, Some(permission_id));
        assert!(matches!(
            decision_event.event,
            OceanEvent::PermissionDecision { allowed: false, .. }
        ));
    }

    #[tokio::test]
    async fn permission_result_does_not_resume_cancelling_request() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let mut control = status(request_id, RequestState::Cancelling);
        control.status.permission_id = Some(permission_id);
        let requests = Arc::new(RwLock::new(HashMap::from([(request_id, control)])));

        update_request_permission_result(
            &requests,
            request_id,
            permission_id,
            AgentPermissionDecision::Allow,
        )
        .await;

        let requests = requests.read().await;
        let status = requests.get(&request_id).unwrap();
        assert_eq!(status.status.state, RequestState::Cancelling);
        assert_eq!(status.status.permission_id, Some(permission_id));
    }

    #[tokio::test]
    async fn cancel_permission_waiter_releases_waiter_with_deny() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let (tx, rx) = oneshot::channel();
        let permissions = Arc::new(RwLock::new(HashMap::from([(
            permission_id,
            PermissionWaiter {
                status: permission_status(permission_id, request_id),
                sender: Some(tx),
            },
        )])));

        cancel_permission_waiter(&permissions, permission_id, request_id).await;

        let decision = rx.await.unwrap();
        assert!(matches!(decision, AgentPermissionDecision::Deny { .. }));
        assert!(permissions.read().await.get(&permission_id).is_none());
    }

    #[test]
    fn parse_room_id_rejects_unknown_ids() {
        let error = parse_agent_turn_room(Some("bogus")).unwrap_err();
        assert!(error.contains("unsupported room_id 'bogus'"));
        assert!(error.contains("pm, writers, orch_mesh, review"));
    }

    #[test]
    fn room_guidance_is_optional_for_legacy_requests() {
        assert_eq!(parse_agent_turn_room(None).unwrap(), None);
        let guided = apply_room_guidance(None, "build the thing");
        assert_eq!(guided, "build the thing");
    }

    #[test]
    fn room_guidance_is_prepended_for_canonical_rooms() {
        for (wire, room_name) in [
            ("pm", "PM room"),
            ("writers", "Writers Room"),
            ("orch_mesh", "ORCH + MESH"),
            ("review", "Review Room"),
        ] {
            let room = parse_agent_turn_room(Some(wire)).unwrap().unwrap();
            let guided = apply_room_guidance(Some(room), "verify the diff");
            assert!(guided.contains(room_name));
            assert!(guided.ends_with("verify the diff"));
        }
    }

    #[test]
    fn room_projection_renders_empty_panels() {
        let input = RoomProjectionInput {
            runtime_status: "ocean-native-fake · fake · model · ready".into(),
            sessions: vec![],
            requests: vec![],
            permissions: vec![],
            events: vec![],
        };

        let room = build_room_snapshot(&input, RoomId::Pm);
        let rooms = build_room_snapshots(&input);

        assert_eq!(room.room_id, RoomId::Pm);
        assert_eq!(room.panels.len(), 3);
        assert_eq!(room.panels[0].lines, vec!["no recent events".to_string()]);
        assert!(room.status.contains("ocean-native-fake"));
        assert_eq!(rooms.len(), 4);
        assert_eq!(rooms[0].room_id, RoomId::Pm);
        assert_eq!(rooms[3].room_id, RoomId::Review);
    }

    #[tokio::test]
    async fn event_bus_records_recent_history() {
        let events = EventBus::new(4);
        let request_id = RequestId::new_v4();
        let session_id = SessionId::new_v4();

        emit(
            &events,
            Some(session_id),
            Some(request_id),
            None,
            OceanEvent::UserMessage {
                text: "hello".into(),
            },
        );
        emit(
            &events,
            Some(session_id),
            Some(request_id),
            None,
            OceanEvent::AssistantDelta {
                text: "world".into(),
            },
        );

        let recent = events.recent(2);
        assert_eq!(recent.len(), 2);
        assert!(matches!(recent[0].event, OceanEvent::AssistantDelta { .. }));
        assert!(matches!(recent[1].event, OceanEvent::UserMessage { .. }));
    }
}
