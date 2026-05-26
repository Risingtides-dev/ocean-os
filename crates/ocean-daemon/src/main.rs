use std::{collections::HashMap, convert::Infallible, env, net::SocketAddr, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::Method,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use ocean_agent::{AgentRuntime, PromptControl};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse,
    PermissionDecision as PermissionDecisionBody, PermissionDecisionRequest, PermissionId,
    PromptRequest, RequestControlResponse, RequestCreateResponse, RequestId, RequestState,
    RequestStatus, RequestsResponse, SessionId,
};
use pi_agent::{PermissionDecision as AgentPermissionDecision, PermissionPolicy};
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
    request_id: RequestId,
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
        .route("/v1/events", get(events))
        .route("/v1/prompt", post(prompt))
        .route("/v1/requests", get(requests).post(create_request))
        .route("/v1/requests/{id}/cancel", post(cancel_request))
        .route("/v1/permissions/{id}/decision", post(permission_decision))
        .route("/v1/sessions", get(sessions))
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
            "POST /v1/permissions/{id}/decision",
            "GET /v1/sessions"
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
        waiter.request_id,
        permission_id,
        agent_decision.clone(),
    )
    .await;

    emit(
        &state.events,
        None,
        Some(waiter.request_id),
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
                    request_id: self.request_id,
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
                tool: tool_name.to_string(),
                reason: format!("permission required for {tool_name}"),
                args: args.clone(),
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

async fn sessions(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state.runtime.list_sessions() {
        Ok(sessions) => Json(json!({"ok": true, "sessions": sessions})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
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
        if waiter.request_id != request_id {
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
                request_id,
                sender: Some(tx),
            },
        )])));

        cancel_permission_waiter(&permissions, permission_id, request_id).await;

        let decision = rx.await.unwrap();
        assert!(matches!(decision, AgentPermissionDecision::Deny { .. }));
        assert!(permissions.read().await.get(&permission_id).is_none());
    }
}
