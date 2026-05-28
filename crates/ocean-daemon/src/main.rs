use std::{collections::HashMap, convert::Infallible, env, net::SocketAddr, sync::Arc};

use uuid::Uuid;

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
use ocean_agent::{AgentRuntime, PromptControl};
use ocean_agent_sdk::{
    AgentSessionId, AgentSessionResponse, AgentSessionSummary, AgentSessionsResponse,
    AgentTurnEvent, AgentTurnId, AgentTurnRequest, AgentTurnResponse, AgentTurnStatus, ToolCall,
    ToolCallId, ToolResult,
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse,
    PermissionDecision as PermissionDecisionBody, PermissionDecisionRequest, PermissionId,
    PermissionStatus, PermissionsResponse, PromptRequest, RequestControlResponse,
    RequestCreateResponse, RequestId, RequestState, RequestStatus, RequestsResponse, SessionDetail,
    SessionId, SessionResponse, SessionRunState,
};
use ocean_runtime::{
    AgentEvent, PermissionDecision as AgentPermissionDecision, PermissionPolicy,
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
}

impl EventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    fn emit(&self, event: EventEnvelope) {
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
    let state = AppState {
        runtime: Arc::new(AgentRuntime::from_env()?),
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
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", get(session))
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
    let session_id = req.session_id.unwrap_or_else(AgentSessionId::new_v4);
    let turn_id = AgentTurnId::new_v4();
    let event_prefix = turn_id.0.to_string()[..8].to_string();

    // If new session, emit session_created first
    if req.session_id.is_none() {
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::SessionCreated {
                session_id,
                title: req.prompt.chars().take(60).collect(),
                cwd: req.cwd.clone(),
            },
        );
    }

    // Emit turn_started
    emit_agent(
        &state.events,
        &state.agent_events,
        session_id,
        AgentTurnEvent::TurnStarted {
            turn_id,
            session_id,
        },
    );

    // Map to PromptRequest; yolo=true for V0 foreground-allow
    let prompt_req = PromptRequest {
        prompt: req.prompt,
        request_id: None,
        session_id: Some(core_sid(session_id)),
        max_turns: None,
        yolo: true,
        cwd: req.cwd.clone(),
    };

    // Wire up the runtime → bus streaming bridge. Every TextDelta /
    // ThinkingDelta / ToolExecution* event the agent emits gets forwarded
    // onto the AgentEventBus in real time so SSE clients render as it streams.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let bridge_bus = state.agent_events.clone();
    let bridge_turn_id = turn_id;
    let bridge = tokio::spawn(async move {
        let mut tool_call_ids: HashMap<String, ToolCallId> = HashMap::new();
        while let Some(ev) = event_rx.recv().await {
            match ev {
                AgentEvent::TextDelta { delta } => {
                    if delta.is_empty() {
                        continue;
                    }
                    bridge_bus.emit(AgentTurnEvent::AssistantTextDelta {
                        turn_id: bridge_turn_id,
                        delta,
                    });
                }
                AgentEvent::ThinkingDelta { delta } => {
                    if delta.is_empty() {
                        continue;
                    }
                    bridge_bus.emit(AgentTurnEvent::ThinkingDelta {
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
                        turn_id: bridge_turn_id,
                        call_id,
                        result: ToolResult {
                            ok: false,
                            output: format!("permission denied for {tool_name}: {reason}"),
                            metadata_json: None,
                        },
                    });
                }
                _ => {}
            }
        }
    });

    let control = PromptControl::yolo(true).with_event_sink(event_tx);
    let res = state.runtime.prompt(prompt_req, control).await;
    // Wait for the bridge to drain (the sender has been dropped by now).
    let _ = bridge.await;
    let output_tokens = estimate_visible_tokens(&res.stdout);
    let tokens_per_second = if res.wall_ms > 0 {
        Some((output_tokens as f64) / (res.wall_ms as f64 / 1000.0))
    } else {
        None
    };
    tracing::info!(
        turn_id = %turn_id,
        session_id = %session_id,
        ok = res.ok,
        wall_ms = res.wall_ms,
        output_tokens,
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
                turn_id,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: Some(res.wall_ms),
                output_tokens: Some(output_tokens),
                tokens_per_second,
            },
        );
    } else {
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::TurnFinished {
                turn_id,
                status: AgentTurnStatus::Failed,
                error: Some(res.stderr.clone()),
                wall_ms: Some(res.wall_ms),
                output_tokens: Some(output_tokens),
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

async fn agent_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream =
        BroadcastStream::new(state.agent_events.subscribe()).filter_map(|event| match event {
            Ok(envelope) => {
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
        } => None,
        AgentTurnEvent::AssistantTextDelta { turn_id: _, delta } => {
            Some(OceanEvent::AssistantDelta { text: delta })
        }
        AgentTurnEvent::ThinkingDelta { .. } => None,
        AgentTurnEvent::ToolCallStarted { turn_id: _, call } => Some(OceanEvent::ToolStarted {
            tool: call.name,
            args: call.args_json,
        }),
        AgentTurnEvent::ToolCallFinished {
            turn_id: _,
            call_id: _,
            result,
        } => Some(OceanEvent::ToolEnded {
            tool: "tool".into(),
            is_error: !result.ok,
        }),
        AgentTurnEvent::TurnFinished {
            turn_id: _,
            status,
            error: _,
            wall_ms,
            output_tokens: _,
            tokens_per_second: _,
        } => Some(OceanEvent::TurnFinished {
            ok: matches!(status, AgentTurnStatus::Completed),
            wall_ms: wall_ms.unwrap_or(0),
        }),
        AgentTurnEvent::ToolCallChunk {
            turn_id: _,
            call_id: _,
            chunk,
        } => Some(OceanEvent::ToolOutput {
            tool: "tool".into(),
            text: chunk,
            is_error: false,
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
    }
}

fn ocean_to_agent_event(event: OceanEvent) -> Option<AgentTurnEvent> {
    match event {
        OceanEvent::AssistantDelta { text } => Some(AgentTurnEvent::AssistantTextDelta {
            turn_id: AgentTurnId(Uuid::new_v4()),
            delta: text,
        }),
        OceanEvent::ToolStarted { tool, args } => Some(AgentTurnEvent::ToolCallStarted {
            turn_id: AgentTurnId(Uuid::new_v4()),
            call: ToolCall {
                id: ToolCallId(Uuid::new_v4()),
                name: tool,
                args_json: args,
            },
        }),
        OceanEvent::ToolOutput {
            tool: _,
            text,
            is_error,
        } => Some(AgentTurnEvent::ToolCallFinished {
            turn_id: AgentTurnId(Uuid::new_v4()),
            call_id: ToolCallId(Uuid::new_v4()),
            result: ToolResult {
                ok: !is_error,
                output: text,
                metadata_json: None,
            },
        }),
        OceanEvent::TurnFinished { ok, wall_ms, .. } => Some(AgentTurnEvent::TurnFinished {
            turn_id: AgentTurnId(Uuid::new_v4()),
            status: if ok {
                AgentTurnStatus::Completed
            } else {
                AgentTurnStatus::Failed
            },
            error: None,
            wall_ms: Some(wall_ms),
            output_tokens: None,
            tokens_per_second: None,
        }),
        OceanEvent::Cancelled { reason } => Some(AgentTurnEvent::TurnFinished {
            turn_id: AgentTurnId(Uuid::new_v4()),
            status: AgentTurnStatus::Cancelled,
            error: reason,
            wall_ms: None,
            output_tokens: None,
            tokens_per_second: None,
        }),
        OceanEvent::Error { message } => Some(AgentTurnEvent::TurnFinished {
            turn_id: AgentTurnId(Uuid::new_v4()),
            status: AgentTurnStatus::Failed,
            error: Some(message),
            wall_ms: None,
            output_tokens: None,
            tokens_per_second: None,
        }),
        _ => None,
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
            Content::ToolCall { name, arguments, .. } => {
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
}
