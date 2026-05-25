use std::{collections::HashMap, convert::Infallible, env, net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::Method,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use ocean_agent::AgentRuntime;
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse, PermissionDecision,
    PermissionDecisionRequest, PermissionId, PromptRequest, RequestControlResponse, RequestId,
    RequestState, RequestStatus, SessionId,
};
use serde_json::json;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::{
    wrappers::{errors::BroadcastStreamRecvError, BroadcastStream},
    Stream, StreamExt,
};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    events: EventBus,
    requests: RequestRegistry,
}

type RequestRegistry = Arc<RwLock<HashMap<RequestId, RequestStatus>>>;

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
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/prompt", post(prompt))
        .route("/v1/requests/:id/cancel", post(cancel_request))
        .route("/v1/permissions/:id/decision", post(permission_decision))
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
            "GET /v1/events",
            "POST /v1/prompt",
            "POST /v1/requests/:id/cancel",
            "POST /v1/permissions/:id/decision",
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
    let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
    req.request_id = Some(request_id);

    upsert_request(
        &state.requests,
        RequestStatus {
            request_id,
            session_id: req.session_id,
            state: RequestState::Running,
            permission_id: None,
            message: Some("prompt running".into()),
            started_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            finished_at: None,
        },
    )
    .await;

    emit(
        &state.events,
        req.session_id,
        Some(request_id),
        None,
        OceanEvent::UserMessage {
            text: req.prompt.clone(),
        },
    );

    let res = state.runtime.prompt(req).await;
    let session_id = res.session_id;
    let request_id = res.request_id.or(Some(request_id));

    if !res.stdout.trim().is_empty() {
        emit(
            &state.events,
            session_id,
            request_id,
            None,
            OceanEvent::AssistantDelta {
                text: res.stdout.clone(),
            },
        );
    }

    if res.ok {
        update_request_finished(
            &state.requests,
            request_id.unwrap_or_else(RequestId::new_v4),
            session_id,
            RequestState::Completed,
            "prompt completed".into(),
        )
        .await;
        emit(
            &state.events,
            session_id,
            request_id,
            None,
            OceanEvent::TurnFinished {
                ok: true,
                wall_ms: res.wall_ms,
            },
        );
    } else {
        update_request_finished(
            &state.requests,
            request_id.unwrap_or_else(RequestId::new_v4),
            session_id,
            RequestState::Errored,
            res.stderr.clone(),
        )
        .await;
        emit(
            &state.events,
            session_id,
            request_id,
            None,
            OceanEvent::Error {
                message: res.stderr.clone(),
            },
        );
    }

    Json(res)
}

async fn cancel_request(
    State(state): State<AppState>,
    Path(request_id): Path<RequestId>,
) -> Json<RequestControlResponse> {
    let mut requests = state.requests.write().await;
    let Some(status) = requests.get_mut(&request_id) else {
        return Json(RequestControlResponse {
            ok: false,
            request_id,
            state: RequestState::Errored,
            message: "request not found".into(),
        });
    };

    status.state = RequestState::Cancelled;
    status.message = Some("cancel requested; cooperative cancellation hook pending".into());
    status.updated_at = Some(Utc::now());
    status.finished_at = Some(Utc::now());
    let session_id = status.session_id;
    drop(requests);

    emit(
        &state.events,
        session_id,
        Some(request_id),
        None,
        OceanEvent::Cancelled {
            reason: Some("cancel requested".into()),
        },
    );

    Json(RequestControlResponse {
        ok: true,
        request_id,
        state: RequestState::Cancelled,
        message: "cancel recorded; runtime-level cancellation token is next".into(),
    })
}

async fn permission_decision(
    State(state): State<AppState>,
    Path(permission_id): Path<PermissionId>,
    Json(decision): Json<PermissionDecisionRequest>,
) -> Json<PermissionControlResponse> {
    let (allowed, reason) = match decision.decision {
        PermissionDecision::Allow => (true, None),
        PermissionDecision::Deny { reason } => (false, reason),
    };

    emit(
        &state.events,
        None,
        None,
        Some(permission_id),
        OceanEvent::PermissionDecision { allowed, reason },
    );

    Json(PermissionControlResponse {
        ok: decision.permission_id == permission_id,
        permission_id,
        message: if decision.permission_id == permission_id {
            "permission decision recorded; runtime permission wait hook is next".into()
        } else {
            "permission id mismatch between path and body".into()
        },
    })
}

async fn sessions(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state.runtime.list_sessions() {
        Ok(sessions) => Json(json!({"ok": true, "sessions": sessions})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn upsert_request(requests: &RequestRegistry, status: RequestStatus) {
    requests.write().await.insert(status.request_id, status);
}

async fn update_request_finished(
    requests: &RequestRegistry,
    request_id: RequestId,
    session_id: Option<SessionId>,
    state: RequestState,
    message: String,
) {
    let mut requests = requests.write().await;
    if let Some(status) = requests.get_mut(&request_id) {
        status.session_id = session_id;
        status.state = state;
        status.message = Some(message);
        status.updated_at = Some(Utc::now());
        status.finished_at = Some(Utc::now());
    }
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
