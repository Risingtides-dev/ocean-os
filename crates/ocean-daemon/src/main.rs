use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    env,
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use async_trait::async_trait;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, TimeZone, Utc};
use ocean_agent::{room_guidance, AgentRuntime, PromptControl};
use ocean_agent_sdk::{
    AgentRole, AgentSessionCreateRequest, AgentSessionCreateResponse, AgentSessionId,
    AgentSessionResponse, AgentSessionSummary, AgentSessionsResponse, AgentTurn, AgentTurnEvent,
    AgentTurnId, AgentTurnRequest, AgentTurnResponse, AgentTurnStatus,
    ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark, MarkKind, ProposalTally,
    ToolCall, ToolCallId, ToolResult,
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse,
    PermissionDecision as PermissionDecisionBody, PermissionDecisionRequest, PermissionId,
    PermissionStatus, PermissionsResponse, Project, ProjectConfig, ProjectId, ProjectResponse,
    ProjectsResponse, PromptRequest, RequestControlResponse, RequestCreateResponse, RequestId,
    evaluate_trigger_policy, RequestState, RequestStatus, RequestsResponse, RoomId, RoomKey,
    RoomMessageKind, RoomPanelSnapshot, RoomParticipant, RoomParticipantKind, RoomSnapshot,
    RoomTriggerEvent, RoomTriggerPolicy, RoomsResponse, SessionDetail, SessionId, SessionResponse,
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
    /// Read-side projection of longhouse councils: a `topic_id -> TopicSnapshot`
    /// store folded from the events each council emits, so the quorum
    /// observability deck survives a refresh (OCEAN-58). Convergence is still
    /// decided only by the per-council `QuorumEngine`; this only mirrors it.
    longhouse: LonghouseRegistryHandle,
    /// Persistent Room lifecycle store (OCEAN-65): the durable `Room` entities
    /// (roster + transcript + trigger policy), distinct from the Track-0
    /// `RoomSnapshot` projection served by `GET /v1/rooms`. In-memory for now;
    /// SQLite persistence is a future ticket. Held behind a std `Mutex` like the
    /// longhouse registry — the guard is always dropped before any `await`.
    rooms: RoomRegistryHandle,
}

type LonghouseRegistryHandle = Arc<Mutex<ocean_longhouse::LonghouseRegistry>>;
type RoomRegistryHandle = Arc<Mutex<ocean_agent::RoomRegistry>>;

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

// --- Registry garbage collection (OCEAN-12) ---------------------------------
//
// `requests`/`permissions` are unbounded `HashMap`s that gain an entry per turn
// and per permission prompt. Without eviction a long-lived daemon leaks memory.
// A background task (spawned in `main`) calls `gc_registries` on this interval.

/// How often the background GC task sweeps the registries.
const REGISTRY_GC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Terminal entries older than this are eligible for eviction.
const REGISTRY_TERMINAL_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Hard cap on entries per registry. On overflow, oldest-terminal entries are
/// evicted first (then, if still over, oldest entries regardless of state).
const REGISTRY_MAX_ENTRIES: usize = 10_000;

impl RequestControl {
    /// Whether this request has reached a terminal lifecycle state.
    fn is_terminal(&self) -> bool {
        self.status.state.is_terminal()
    }

    /// Best-effort "when did this become final" timestamp for age comparison.
    fn terminal_at(&self) -> DateTime<Utc> {
        self.status
            .finished_at
            .or(self.status.updated_at)
            .or(self.status.started_at)
            .unwrap_or_else(Utc::now)
    }
}

impl PermissionWaiter {
    /// A waiter whose decision channel has been consumed is effectively done —
    /// it's normally removed on decision/cancel, so a lingering `None`-sender
    /// entry is a leak. Pending waiters (`Some`) are never reaped by age.
    fn is_terminal(&self) -> bool {
        self.sender.is_none()
    }

    fn terminal_at(&self) -> DateTime<Utc> {
        self.status.created_at
    }
}

/// One GC sweep: drop terminal entries older than [`REGISTRY_TERMINAL_TTL`],
/// then enforce [`REGISTRY_MAX_ENTRIES`] by evicting oldest-terminal first.
/// `now` is injected so the sweep is deterministic in tests.
async fn gc_registries(
    requests: &RequestRegistry,
    permissions: &PermissionRegistry,
    now: DateTime<Utc>,
) {
    let ttl = REGISTRY_TERMINAL_TTL;
    {
        let mut reqs = requests.write().await;
        reqs.retain(|_, ctl| !(ctl.is_terminal() && (now - ctl.terminal_at()) > ttl));
        if reqs.len() > REGISTRY_MAX_ENTRIES {
            evict_overflow(&mut reqs, |c| c.is_terminal(), |c| c.terminal_at());
        }
    }
    {
        let mut perms = permissions.write().await;
        perms.retain(|_, w| !(w.is_terminal() && (now - w.terminal_at()) > ttl));
        if perms.len() > REGISTRY_MAX_ENTRIES {
            evict_overflow(&mut perms, |w| w.is_terminal(), |w| w.terminal_at());
        }
    }
}

/// Trim `map` down to [`REGISTRY_MAX_ENTRIES`]. Removes oldest-terminal entries
/// first; if still over the cap (all remaining are live), removes the oldest
/// entries regardless of state. Generic over the registry value type.
fn evict_overflow<K, V, FTerm, FAt>(
    map: &mut HashMap<K, V>,
    is_terminal: FTerm,
    terminal_at: FAt,
) where
    K: std::hash::Hash + Eq + Clone,
    FTerm: Fn(&V) -> bool,
    FAt: Fn(&V) -> DateTime<Utc>,
{
    if map.len() <= REGISTRY_MAX_ENTRIES {
        return;
    }
    let overflow = map.len() - REGISTRY_MAX_ENTRIES;
    // Rank candidates: terminal entries before live ones, oldest first within
    // each group. Take exactly `overflow` keys to remove.
    let mut ranked: Vec<(K, bool, DateTime<Utc>)> = map
        .iter()
        .map(|(k, v)| (k.clone(), is_terminal(v), terminal_at(v)))
        .collect();
    ranked.sort_by(|a, b| {
        // terminal (true) should come first => reverse the bool ordering
        b.1.cmp(&a.1).then(a.2.cmp(&b.2))
    });
    for (key, _, _) in ranked.into_iter().take(overflow) {
        map.remove(&key);
    }
}

/// Stable hash of a tool call's args for permission deduplication. Serializes
/// the `Value` to canonical JSON (serde_json sorts object keys deterministically
/// for a given `Value` shape) and hashes the bytes, so equal args produce equal
/// keys within one turn. Falls back to hashing the `Debug` form if serialization
/// ever fails (it won't for a `serde_json::Value`).
fn permission_args_hash(args: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match serde_json::to_vec(args) {
        Ok(bytes) => bytes.hash(&mut hasher),
        Err(_) => format!("{args:?}").hash(&mut hasher),
    }
    hasher.finish()
}

struct DaemonPermissionPolicy {
    allow_mutating: bool,
    request_id: RequestId,
    session_id: Option<SessionId>,
    events: EventBus,
    permissions: PermissionRegistry,
    requests: RequestRegistry,
    cancel: CancellationToken,
    /// Dedupe map for identical (tool, args) pairs within this turn's scope.
    /// A `DaemonPermissionPolicy` instance is built once per turn in
    /// `build_prompt_control`, so a per-instance cache is per-turn. When the
    /// agent re-issues an identical tool+args call (e.g. retrying after a
    /// failure) we reuse the original `PermissionId` instead of minting a new
    /// one, so the same approval doesn't surface twice in the UI. Keyed on the
    /// tool name plus a stable hash of the canonical args JSON.
    seen_permissions: Arc<Mutex<HashMap<(String, u64), PermissionId>>>,
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
        longhouse: Arc::new(Mutex::new(ocean_longhouse::LonghouseRegistry::new())),
        rooms: Arc::new(Mutex::new(ocean_agent::RoomRegistry::new())),
    };

    // Background GC: the request/permission registries are otherwise unbounded
    // and accrete one entry per turn/permission for the daemon's whole lifetime.
    // This task reaps terminal entries on an interval so a long-lived daemon
    // doesn't leak memory. See `gc_registries`.
    {
        let requests = state.requests.clone();
        let permissions = state.permissions.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REGISTRY_GC_INTERVAL);
            // Skip the immediate first tick; first sweep happens one interval in.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                gc_registries(&requests, &permissions, Utc::now()).await;
            }
        });
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/agent/turns", post(agent_turn))
        .route("/v1/agent/voice", post(agent_voice))
        .route("/v1/agent/events", get(agent_events))
        .route(
            "/v1/agent/sessions",
            get(agent_sessions).post(agent_sessions_create),
        )
        .route("/v1/agent/sessions/{id}", get(agent_session))
        .route("/v1/events", get(events))
        .route("/v1/prompt", post(prompt))
        .route("/v1/requests", get(requests).post(create_request))
        .route("/v1/requests/{id}/cancel", post(cancel_request))
        .route("/v1/permissions", get(permissions))
        .route("/v1/permissions/{id}/decision", post(permission_decision))
        .route("/v1/rooms", get(rooms))
        // Persistent Room lifecycle (OCEAN-65). Namespaced under `/persistent`
        // so it never shadows the Track-0 projection route `/v1/rooms/{room_id}`
        // below: `persistent` is a reserved segment, real room keys live one
        // level deeper at `/v1/rooms/persistent/{key}`.
        .route(
            "/v1/rooms/persistent",
            get(rooms_list_persistent).post(room_create),
        )
        .route("/v1/rooms/persistent/{key}", get(room_get))
        .route(
            "/v1/rooms/persistent/{key}/participants",
            post(room_join),
        )
        .route(
            "/v1/rooms/persistent/{key}/participants/{participant_id}",
            axum::routing::delete(room_leave),
        )
        .route(
            "/v1/rooms/persistent/{key}/messages",
            post(room_post_message),
        )
        .route(
            "/v1/rooms/persistent/{key}/transcript",
            get(room_transcript),
        )
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
        .route("/v1/longhouse/convene", post(longhouse_convene))
        .route("/v1/longhouse/topics", get(longhouse_topics))
        .route("/v1/longhouse/topics/{topic_id}", get(longhouse_topic))
        .route("/v1/calls/demo", post(call_demo))
        .route("/v1/calls/place", post(call_place))
        .route("/v1/calls/webhook", post(call_webhook))
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
    // OCEAN-25: this list mirrors the `Router::route()` calls in `main()` exactly,
    // grouped by concern. Keep it in sync with both the router above and the
    // authoritative route table in docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md whenever a
    // route is added or removed.
    Json(json!({
        "ok": true,
        "service": "ocean-daemon",
        "routes": [
            "GET /",
            "GET /health",
            "GET /ready",
            "POST /v1/agent/turns",
            "POST /v1/agent/voice",
            "GET /v1/agent/events",
            "POST /v1/agent/sessions",
            "GET /v1/agent/sessions",
            "GET /v1/agent/sessions/{id}",
            "GET /v1/events",
            "POST /v1/prompt",
            "POST /v1/agent/sessions",
            "GET /v1/requests",
            "POST /v1/requests",
            "POST /v1/requests/{id}/cancel",
            "GET /v1/permissions",
            "POST /v1/permissions/{id}/decision",
            "GET /v1/rooms",
            "GET /v1/rooms/{room_id}",
            "GET /v1/rooms/persistent",
            "POST /v1/rooms/persistent",
            "GET /v1/rooms/persistent/{key}",
            "POST /v1/rooms/persistent/{key}/participants",
            "DELETE /v1/rooms/persistent/{key}/participants/{participant_id}",
            "POST /v1/rooms/persistent/{key}/messages",
            "GET /v1/rooms/persistent/{key}/transcript",
            "GET /v1/sessions",
            "GET /v1/sessions/{id}",
            "GET /v1/projects",
            "POST /v1/projects",
            "GET /v1/projects/{id}",
            "PATCH /v1/projects/{id}",
            "DELETE /v1/projects/{id}",
            "GET /v1/model",
            "POST /v1/model",
            "GET /v1/models",
            "POST /v1/component/event",
            "POST /v1/longhouse/demo",
            "POST /v1/longhouse/convene",
            "GET /v1/longhouse/topics",
            "GET /v1/longhouse/topics/{topic_id}",
            "POST /v1/calls/demo",
            "POST /v1/calls/place",
            "POST /v1/calls/webhook"
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
            seen_permissions: Arc::new(Mutex::new(HashMap::new())),
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
            seen_permissions: Arc::new(Mutex::new(HashMap::new())),
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

        // OCEAN-21: within one turn, an identical tool+args combination must map
        // to a single, stable `PermissionId`. Re-issuing the same call (e.g. the
        // agent retrying after a failure) otherwise mints a fresh id and shows a
        // duplicate approval in the UI. Reuse the original id for identical
        // tool+args; only the first occurrence allocates a new one.
        let dedupe_key = (tool_name.to_string(), permission_args_hash(args));
        let permission_id = {
            let mut seen = self
                .seen_permissions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *seen
                .entry(dedupe_key)
                .or_insert_with(PermissionId::new_v4)
        };
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

/// Request body for `POST /v1/longhouse/convene`.
#[derive(Debug, serde::Deserialize)]
struct LonghouseConveneRequest {
    /// The question / task the council deliberates.
    question: String,
    /// Which department room hosts it: dev | sales | content | campaign |
    /// commons. Defaults to `commons` if omitted or unrecognized.
    #[serde(default)]
    federation: Option<String>,
    /// Optional model alias override; one worker per alias. Defaults to a mixed
    /// deepseek + kimi council so it's genuinely multi-model.
    #[serde(default)]
    models: Option<Vec<String>>,
}

fn parse_federation(s: Option<&str>) -> Federation {
    match s.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("dev") => Federation::Dev,
        Some("sales") => Federation::Sales,
        Some("content") => Federation::Content,
        Some("campaign") => Federation::Campaign,
        _ => Federation::Commons,
    }
}

/// Convene a **real** longhouse council: spawn cheap-model LLM workers, run the
/// propose → endorse/inhibit rounds, let the daemon-side `QuorumEngine` decide
/// convergence, and stream the resulting `LonghouseEvent`s onto the existing
/// agent event bus — exactly like `longhouse_demo`, but driven by real agents
/// and a real quorum engine instead of a scripted timer. The deck renders it
/// with zero changes.
///
/// Returns immediately with the topic id; the council runs in a background task
/// and its events arrive on `/v1/agent/events`.
async fn longhouse_convene(
    State(state): State<AppState>,
    Json(req): Json<LonghouseConveneRequest>,
) -> Json<serde_json::Value> {
    let bus = state.agent_events.clone();
    let registry = state.longhouse.clone();
    let federation = parse_federation(req.federation.as_deref());

    let mut convene_req = ocean_longhouse::ConveneRequest::new(req.question.clone(), federation);
    if let Some(models) = req.models {
        if !models.is_empty() {
            convene_req.models = models;
        }
    }

    let topic_hint = convene_req.question.clone();
    tokio::spawn(async move {
        let clock = ocean_longhouse::SystemClock;
        // Emit each longhouse event onto the agent bus, exactly as the demo does
        // (`bus.emit(ev.into_turn_event())`), so existing SSE clients render it —
        // AND tee it into the read-side registry so the topic survives a refresh
        // (OCEAN-58). The registry is the durable mirror; the bus is the live feed.
        let outcome = ocean_longhouse::convene(convene_req, &clock, |ev| {
            // Fold into the observable topic store first, then publish to the bus.
            // A std Mutex is fine: the guard is dropped before any await (the
            // closure is fully synchronous), so it never blocks the scheduler.
            if let Ok(mut reg) = registry.lock() {
                reg.ingest(&ev);
            }
            bus.emit(ev.into_turn_event());
        })
        .await;
        tracing::info!(
            topic = %outcome.topic_id,
            converged = outcome.decision.is_some(),
            proposals = outcome.proposals.len(),
            "longhouse council finished"
        );
    });

    Json(json!({
        "ok": true,
        "question": topic_hint,
        "federation": format!("{federation:?}").to_lowercase(),
        "streaming_on": "/v1/agent/events",
    }))
}

/// Bridges ocean-call's orchestrator events onto the daemon EventBus, turning
/// each OceanEvent into an EventEnvelope on the real SSE rail.
struct BusSink {
    events: EventBus,
}

impl ocean_call::EventSink for BusSink {
    fn emit(&mut self, event: ocean_core::OceanEvent) {
        self.events.emit(ocean_core::EventEnvelope::new(event));
    }
}

/// Demo: run the ocean-call orchestrator over a scripted transcript and emit
/// the real call events (CallStarted/Transcript/Task/Summary/Ended) onto the
/// SSE rail. Proves the daemon→orchestrator→EventBus path end to end WITHOUT
/// any Twilio/LiveKit account — the live `place_call` path is gated on those.
async fn call_demo(State(state): State<AppState>) -> Json<serde_json::Value> {
    use ocean_call::{CallSession, Summarizer, SummaryPolicy, TranscriptSegment, WakeGate};

    let mut sink = BusSink {
        events: state.events.clone(),
    };
    let mut session = CallSession::new(
        format!("demo-{}", Uuid::new_v4()),
        Summarizer::new(SummaryPolicy {
            every_n_segments: 3,
            silence_ms: 15_000,
        }),
        WakeGate::new(false, 2_000),
    );

    session.start(&mut sink, "call:demo", vec!["sip:+17035081859".into()]);
    let script = [
        ("caller", "hey thanks for jumping on", 0u64),
        ("caller", "so for the Warner Q3 push", 2_000),
        ("caller", "I'll send the master to Atlantic tonight", 4_000),
        ("caller", "and we need to verify the toll-free number by Friday", 7_000),
        ("caller", "hey Ocean what did we just agree to", 10_000),
    ];
    for (speaker, text, ms) in script {
        session.on_segment(TranscriptSegment::final_(speaker, text, ms), ms, &mut sink);
    }
    session.end(&mut sink, 12_000);

    Json(json!({ "ok": true, "streaming_on": "/v1/events" }))
}

#[derive(serde::Deserialize)]
struct PlaceCallRequest {
    /// Number to dial; any common format (normalized to E.164 server-side).
    to: String,
}

/// Place a real outbound call. The operator's trigger: POST { "to": "..." }.
///
/// If the SIP/LiveKit env is configured (LIVEKIT_URL/_API_KEY/_API_SECRET +
/// OCEAN_CALL_OUTBOUND_TRUNK + OCEAN_CALL_CALLER_NUMBER), this mints a call
/// room and dials via the verified LiveKit SIP bridge. If not, it returns 503
/// naming exactly what's unset — so the only thing between here and a ringing
/// phone is John's Twilio upgrade + LiveKit Cloud account, and the error says so.
async fn call_place(
    State(state): State<AppState>,
    Json(req): Json<PlaceCallRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_call::{normalize_e164, CallBridge, LiveKitSipBridge, SipConfig};

    let Some(dialed) = normalize_e164(&req.to) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("not a valid phone number: {}", req.to) })),
        );
    };

    let config = match SipConfig::from_env() {
        Ok(c) => c,
        Err(missing) => {
            // Not a code failure — the account/creds aren't provisioned yet.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "blocked_on": "telephony not configured",
                    "missing": missing,
                    "needed_env": [
                        "LIVEKIT_URL", "LIVEKIT_API_KEY", "LIVEKIT_API_SECRET",
                        "OCEAN_CALL_OUTBOUND_TRUNK", "OCEAN_CALL_CALLER_NUMBER"
                    ],
                    "note": "Requires a LiveKit Cloud account + a Twilio SIP trunk (paid). Once set, this route dials for real."
                })),
            );
        }
    };

    let bridge = match LiveKitSipBridge::new(config) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "ok": false, "error": e })),
            );
        }
    };

    // Mint a call room and emit CallStarted so subscribers see the attempt.
    let room = format!("call:{}", Uuid::new_v4());
    state.events.emit(ocean_core::EventEnvelope::new(
        ocean_core::OceanEvent::CallStarted {
            call_id: room.clone(),
            room_id: room.clone(),
            participants: vec![format!("sip:{dialed}")],
        },
    ));

    match bridge.place_call(&dialed, &room).await {
        Ok(call) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "dialed": call.dialed,
                "room": call.room,
                "participant_id": call.participant_id,
                "streaming_on": "/v1/events"
            })),
        ),
        Err(e) => {
            // Balance the lifecycle: we already emitted CallStarted, so a failed
            // dial MUST emit CallEnded or subscribers (TUI/surface) are left with
            // a phantom call stuck "in progress" forever.
            state.events.emit(ocean_core::EventEnvelope::new(
                ocean_core::OceanEvent::CallEnded {
                    call_id: room.clone(),
                    duration_ms: 0,
                },
            ));
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("dial failed: {e}") })),
            )
        }
    }
}

/// LiveKit webhook receiver. LiveKit POSTs room lifecycle events here; we
/// verify the signature, and on a `room_started` for a `call_` room emit
/// CallStarted (and CallEnded on `room_finished`) onto the SSE rail. This is
/// the trigger that lets an INBOUND call (someone dialing Ocean's number, which
/// SIP-routes into a call_<caller>_<random> room) reach the pipeline.
///
/// The live room-audio tap (room_tap::live) needs the native `livekit-tap`
/// feature; this endpoint proves the reception + lifecycle path without it, so
/// a real call already produces call_started/call_ended on /v1/events.
async fn call_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let (Ok(api_key), Ok(api_secret)) = (
        std::env::var("LIVEKIT_API_KEY"),
        std::env::var("LIVEKIT_API_SECRET"),
    ) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": "LIVEKIT_API_KEY/SECRET not set" })),
        );
    };
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    match ocean_call::verify_and_decide(&api_key, &api_secret, &body, auth) {
        Ok(ocean_call::WebhookAction::JoinCall { room }) => {
            state.events.emit(ocean_core::EventEnvelope::new(
                ocean_core::OceanEvent::CallStarted {
                    call_id: room.clone(),
                    room_id: room.clone(),
                    participants: vec![],
                },
            ));
            tracing::info!(%room, "inbound call room started");
            (StatusCode::OK, Json(json!({ "ok": true, "action": "join", "room": room })))
        }
        Ok(ocean_call::WebhookAction::EndCall { room }) => {
            state.events.emit(ocean_core::EventEnvelope::new(
                ocean_core::OceanEvent::CallEnded {
                    call_id: room.clone(),
                    duration_ms: 0,
                },
            ));
            (StatusCode::OK, Json(json!({ "ok": true, "action": "end", "room": room })))
        }
        Ok(ocean_call::WebhookAction::Ignore) => {
            (StatusCode::OK, Json(json!({ "ok": true, "action": "ignore" })))
        }
        Err(e) => {
            // Verification failed — do NOT act. Log and 200 so LiveKit doesn't retry-storm.
            tracing::warn!(error = %e, "rejected livekit webhook");
            (StatusCode::OK, Json(json!({ "ok": false, "error": e.to_string() })))
        }
    }
}

/// `GET /v1/longhouse/topics` — list every tracked longhouse topic with its full
/// observable state (members, marks, tallies, leader, deadline, firekeeper,
/// decision, state). Read-only mirror of the per-council quorum engine, folded
/// from the event stream so the quorum observability deck survives a refresh
/// (OCEAN-58).
async fn longhouse_topics(State(state): State<AppState>) -> Json<serde_json::Value> {
    let topics = match state.longhouse.lock() {
        Ok(reg) => reg.topics(),
        Err(poisoned) => poisoned.into_inner().topics(),
    };
    Json(json!({ "ok": true, "topics": topics }))
}

/// `GET /v1/longhouse/topics/{topic_id}` — one topic's full observable state by
/// id. 404 if the topic id is unknown, 400 if it isn't a valid UUID. Mirrors the
/// `GET /v1/rooms/{room_id}` shape: a typed error body, never a panic.
async fn longhouse_topic(
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

// ---- Persistent Rooms (OCEAN-65) -------------------------------------------
//
// These routes serve the *persistent* `Room` lifecycle: create, fetch, roster
// join/leave, post message, read transcript. They are intentionally additive and
// fully separate from the Track-0 `RoomSnapshot` projection (`GET /v1/rooms`,
// `GET /v1/rooms/{room_id}`), which is untouched. They also live entirely apart
// from the `agent_turn` handler and its cwd/permission machinery, which is in
// flight on held security PRs — none of this code touches turn execution.
//
// Error shape mirrors `GET /v1/longhouse/topics/{topic_id}`: a typed `{ ok,
// error }` body, 400 on a bad key, 404 on an unknown room. The store maps to
// status codes in `room_store_error_status`.

/// Run a closure with the locked room registry, recovering a poisoned lock the
/// same way the longhouse handlers do (`into_inner`). Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock.
fn with_rooms<T>(state: &AppState, f: impl FnOnce(&mut ocean_agent::RoomRegistry) -> T) -> T {
    let mut guard = match state.rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Map a store error onto an HTTP status + typed JSON body.
fn room_store_error_response(
    err: ocean_agent::RoomStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_agent::RoomStoreError::*;
    let status = match &err {
        BadKey(_) => StatusCode::BAD_REQUEST,
        UnknownRoom(_) | UnknownParticipant { .. } => StatusCode::NOT_FOUND,
        AlreadyExists(_) => StatusCode::CONFLICT,
    };
    (
        status,
        Json(json!({ "ok": false, "error": err.to_string() })),
    )
}

#[derive(serde::Deserialize)]
struct RoomCreateRequest {
    /// Persistent room key, e.g. `"ocean-surface-map-fix"`. Must be non-empty.
    key: String,
    /// Human-readable room name.
    name: String,
    /// Optional trigger policy controlling auto-convene/notify behaviour.
    #[serde(default)]
    trigger_policy: Option<RoomTriggerPolicy>,
}

/// `POST /v1/rooms/persistent` — create a persistent room.
async fn room_create(
    State(state): State<AppState>,
    Json(req): Json<RoomCreateRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(req.key.trim());
    let result = with_rooms(&state, |reg| {
        reg.create(key, req.name, req.trigger_policy, Utc::now())
    });
    match result {
        Ok(rec) => (
            StatusCode::CREATED,
            Json(json!({ "ok": true, "room": rec.room })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent` — list all persistent rooms (no transcripts).
async fn rooms_list_persistent(State(state): State<AppState>) -> Json<serde_json::Value> {
    let rooms = with_rooms(&state, |reg| reg.list());
    Json(json!({ "ok": true, "rooms": rooms }))
}

/// `GET /v1/rooms/persistent/{key}` — one persistent room (with its transcript).
async fn room_get(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    match with_rooms(&state, |reg| reg.get(&key)) {
        Some(rec) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "room": rec.room, "transcript": rec.transcript })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no room with key '{key}'") })),
        ),
    }
}

#[derive(serde::Deserialize)]
struct RoomJoinRequest {
    /// Stable participant id, unique within the room.
    id: String,
    /// Display name shown in the roster and transcript.
    display_name: String,
    /// What kind of actor is joining. Defaults to `human`.
    #[serde(default = "default_participant_kind")]
    kind: RoomParticipantKind,
}

fn default_participant_kind() -> RoomParticipantKind {
    RoomParticipantKind::Human
}

/// `POST /v1/rooms/persistent/{key}/participants` — add a participant.
async fn room_join(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<RoomJoinRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let participant = RoomParticipant {
        id: req.id,
        kind: req.kind,
        display_name: req.display_name,
    };
    let result =
        with_rooms(&state, |reg| reg.add_participant(&key, participant, Utc::now()));
    match result {
        Ok(rec) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "room": rec.room })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `DELETE /v1/rooms/persistent/{key}/participants/{participant_id}` — remove a
/// participant from the roster.
async fn room_leave(
    State(state): State<AppState>,
    Path((key, participant_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |reg| {
        reg.remove_participant(&key, participant_id.trim(), Utc::now())
    });
    match result {
        Ok(rec) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "room": rec.room })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

#[derive(serde::Deserialize)]
struct RoomMessageRequest {
    /// Author participant id (or a synthetic id like `"system"`).
    author_id: String,
    /// Author kind for attribution. Defaults to `human`.
    #[serde(default = "default_participant_kind")]
    author_kind: RoomParticipantKind,
    /// Message body. `@id` mentions in the body drive trigger evaluation.
    body: String,
}

/// `POST /v1/rooms/persistent/{key}/messages` — append a chat message to the
/// transcript, then evaluate the room's trigger policy against any @-mentions in
/// the body. On a positive decision, emit a `room.trigger` notice onto the agent
/// event bus (the observable half of auto-convene).
async fn room_post_message(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<RoomMessageRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let append = with_rooms(&state, |reg| {
        reg.append_message(
            &key,
            req.author_id.clone(),
            req.author_kind,
            RoomMessageKind::Message,
            req.body.clone(),
            Utc::now(),
        )
        .map(|msg| (msg, reg.trigger_policy(&key)))
    });

    let (msg, policy) = match append {
        Ok(v) => v,
        Err(e) => return room_store_error_response(e),
    };

    // ---- Trigger policy evaluation wiring point (OCEAN-65) -----------------
    //
    // Parse @-mentions from the message body and evaluate each against the
    // room's trigger policy. For every positive decision we emit a notice event
    // and append a `System` transcript line so the convene is auditable in the
    // room itself.
    //
    // The notice event is the observable contract today. The ACTUAL
    // auto-convene — queuing an agent turn for `decision.target_participant` —
    // hooks in HERE: the daemon already spawns turns via `agent_turn` /
    // `state.runtime`; a follow-up wires that target id + the room transcript
    // (as read-before-answer context) into a queued `AgentTurnRequest`. That is
    // deliberately deferred so this PR stays out of the in-flight `agent_turn`
    // handler and its permission/cwd code on the held security PRs.
    let mut fired = Vec::new();
    for participant_id in parse_mentions(&req.body) {
        let decision = evaluate_trigger_policy(
            policy.as_ref(),
            &RoomTriggerEvent::Mention {
                participant_id: participant_id.clone(),
            },
        );
        if decision.should_convene {
            // Emit a notice onto the agent event bus so any subscriber sees the
            // would-be convene. Uses the generic Extension event so it never
            // collides with the Track-0/longhouse event scoping rules.
            state
                .agent_events
                .emit(AgentTurnEvent::Extension {
                    extension: "room_trigger".into(),
                    payload: json!({
                        "room": key.as_str(),
                        "target": decision.target_participant,
                        "reason": decision.reason,
                        "triggered_by_seq": msg.seq,
                    }),
                    // Room-wide, not session-scoped: reaches `?all=1` subscribers
                    // only, exactly like longhouse council events (Invariant 5
                    // exception). Keeps this out of any single session's stream.
                    scope: None,
                });
            // Audit line inside the room.
            let _ = with_rooms(&state, |reg| {
                reg.append_message(
                    &key,
                    "system",
                    RoomParticipantKind::System,
                    RoomMessageKind::System,
                    format!(
                        "auto-convene: {} ({})",
                        decision.target_participant.clone().unwrap_or_default(),
                        decision.reason
                    ),
                    Utc::now(),
                )
            });
            fired.push(decision);
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({ "ok": true, "message": msg, "triggers_fired": fired })),
    )
}

/// Extract `@id` mentions from a message body. A mention is `@` followed by a
/// run of id-safe characters (alphanumerics, `-`, `_`). Returns ids without the
/// leading `@`, de-duplicated in first-seen order.
fn parse_mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > start {
                let id = body[start..j].to_string();
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[derive(serde::Deserialize)]
struct TranscriptQuery {
    /// If set, return only entries with `seq > after_seq` (live-tail).
    #[serde(default)]
    after_seq: Option<u64>,
}

/// `GET /v1/rooms/persistent/{key}/transcript` — read a room's transcript,
/// optionally only entries after a given seq.
async fn room_transcript(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    match with_rooms(&state, |reg| reg.transcript(&key, q.after_seq)) {
        Ok(transcript) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "transcript": transcript })),
        ),
        Err(e) => room_store_error_response(e),
    }
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
        Ok(projects) => Json(ProjectsResponse {
            ok: true,
            projects,
            error: None,
        }),
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
            Json(ProjectResponse {
                ok: true,
                project: Some(project),
                error: None,
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse {
                ok: false,
                project: None,
                error: Some(e.to_string()),
            }),
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
                Json(ProjectResponse {
                    ok: false,
                    project: None,
                    error: Some(e.to_string()),
                }),
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
            Json(ProjectResponse {
                ok: true,
                project: Some(project),
                error: None,
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse {
                ok: false,
                project: None,
                error: Some(e.to_string()),
            }),
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
        OceanEvent::CallStarted { room_id, .. } => format!("{actor} · call started · {room_id}"),
        OceanEvent::CallTranscriptSegment { speaker, text, .. } => {
            format!("{actor} · {speaker}: {}", trim_line(text, 42))
        }
        OceanEvent::CallSummaryUpdated { summary, .. } => {
            format!("{actor} · call summary: {}", trim_line(summary, 42))
        }
        OceanEvent::CallTaskDetected { title, .. } => {
            format!("{actor} · task: {}", trim_line(title, 44))
        }
        OceanEvent::CallWakeTriggered { utterance } => {
            format!("{actor} · wake: {}", trim_line(utterance, 44))
        }
        OceanEvent::CallAgentSpoke { text } => {
            format!("{actor} · spoke: {}", trim_line(text, 44))
        }
        OceanEvent::CallEnded { duration_ms, .. } => {
            format!("{actor} · call ended · {duration_ms}ms")
        }
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

/// Voice turn input. STT happens client-side (or at the surface proxy), so the
/// daemon receives an already-transcribed utterance. This makes voice a
/// first-class daemon turn — any surface (web, TUI, a future always-on wake-word
/// listener) can speak to Ocean through the same session machinery instead of
/// voice being a web-UI-only feature.
#[derive(serde::Deserialize)]
struct AgentVoiceRequest {
    #[serde(default)]
    session_id: Option<AgentSessionId>,
    /// The transcribed utterance.
    transcript: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    project_id: Option<Uuid>,
}

/// POST /v1/agent/voice — accept a transcribed utterance and run it as a normal
/// agent turn tagged `client_type = "voice"`. Thin wrapper over `agent_turn` so
/// it inherits cwd resolution, per-session locking, cancellation, and SSE
/// streaming with zero duplication.
async fn agent_voice(
    State(state): State<AppState>,
    Json(req): Json<AgentVoiceRequest>,
) -> (StatusCode, Json<AgentTurnResponse>) {
    let turn = AgentTurnRequest {
        session_id: req.session_id,
        prompt: req.transcript,
        cwd: req.cwd,
        guidance: None,
        room_id: req.room_id,
        project_id: req.project_id,
        // Canonical voice client_type (see AgentTurnRequest::client_type docs).
        client_type: Some("leo-voice".to_string()),
        // Voice turns defer to the runtime's global reasoning/model selection.
        thinking_level: None,
        model_id: None,
    };
    agent_turn(State(state), Json(turn)).await
}

/// Outcome of binding a turn's requested cwd against the session it claims to
/// resume. See [`resolve_bound_cwd`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum CwdBindingError {
    /// The resumed session is bound to one workspace, but the turn supplied a
    /// cwd that resolves to a *different* workspace. A forged `session_id`
    /// pointed at an arbitrary cwd is a session-hijack attempt (OCEAN-52a):
    /// reject rather than relocate the session into the attacker's directory.
    WorkspaceMismatch {
        requested_workspace: String,
        session_workspace: String,
    },
    /// The requested cwd contains a parent-dir (`..`) traversal component, so it
    /// could escape its intended workspace into an arbitrary filesystem location
    /// (OCEAN-52b). Legit cwds are already-resolved absolute paths.
    PathTraversal { cwd: String },
}

impl CwdBindingError {
    fn message(&self) -> String {
        match self {
            CwdBindingError::WorkspaceMismatch {
                requested_workspace,
                session_workspace,
            } => format!(
                "session/workspace mismatch: this session is bound to workspace \
                 {session_workspace}, but the turn's cwd resolves to {requested_workspace}. \
                 A resumed turn cannot relocate its session to a different workspace."
            ),
            CwdBindingError::PathTraversal { cwd } => format!(
                "rejected cwd {cwd}: a working directory must be an absolute, \
                 already-resolved path with no parent-directory ('..') components."
            ),
        }
    }
}

/// True if `cwd` contains a parent-directory (`..`) component, which could let a
/// forged path escape its intended workspace boundary. We check lexically (not
/// via `canonicalize`) so the guard is deterministic and does not depend on the
/// path existing on disk — a resolved turn cwd should never contain `..`.
fn cwd_has_traversal(cwd: &str) -> bool {
    std::path::Path::new(cwd)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Resolve the working directory a turn will actually execute in, enforcing the
/// session↔workspace binding (OCEAN-52) and pinning resumed turns to their
/// session's bound workspace (OCEAN-55).
///
/// - `requested_cwd`: the cwd already resolved by `resolve_cwd_for_turn`
///   (non-empty: the client's cwd, or a project's workspace_root).
/// - `requested_workspace_root`: the workspace root `requested_cwd` maps to
///   (git toplevel, or the cwd itself), computed by the caller.
/// - `session_binding`: `Some((session_cwd, session_workspace_root))` when the
///   turn resumes an existing session that carries a bound workspace; `None` for
///   a brand-new session (implicit or explicit) or a legacy session with no
///   recorded workspace.
///
/// Returns the cwd to run in. For a NEW session this is `requested_cwd` (the
/// first turn legitimately sets the cwd). For a RESUMED session this is the
/// session's *bound* cwd — the turn is pinned to where the session was started,
/// never an arbitrary `requested_cwd`, after validating the two share a
/// workspace root.
fn resolve_bound_cwd(
    requested_cwd: &str,
    requested_workspace_root: &str,
    session_binding: Option<(&str, &str)>,
) -> Result<String, CwdBindingError> {
    // Path-traversal guard applies to every turn: the resolved cwd must not
    // contain `..` components that could escape into a parent / arbitrary dir.
    if cwd_has_traversal(requested_cwd) {
        return Err(CwdBindingError::PathTraversal {
            cwd: requested_cwd.to_string(),
        });
    }

    match session_binding {
        // RESUMED session: validate the binding, then pin to the session's
        // bound cwd. The requested cwd's workspace must match the session's
        // bound workspace, otherwise this is a cross-workspace hijack.
        Some((session_cwd, session_workspace)) => {
            if requested_workspace_root != session_workspace {
                return Err(CwdBindingError::WorkspaceMismatch {
                    requested_workspace: requested_workspace_root.to_string(),
                    session_workspace: session_workspace.to_string(),
                });
            }
            // The session's own bound cwd must also be traversal-free (it was
            // captured at creation, but validate defensively).
            if cwd_has_traversal(session_cwd) {
                return Err(CwdBindingError::PathTraversal {
                    cwd: session_cwd.to_string(),
                });
            }
            Ok(session_cwd.to_string())
        }
        // NEW session (or legacy with no bound workspace): the first turn sets
        // the cwd. Run in the requested cwd as before.
        None => Ok(requested_cwd.to_string()),
    }
}

/// Look up the workspace binding (`cwd`, `workspace_root`) of an existing
/// session, if it exists on disk and carries a recorded workspace. Returns
/// `None` for an unknown session (the strict resume-vs-create check downstream
/// in the agent loop turns that into the canonical "session not found" error)
/// or a legacy session with no bound workspace.
fn session_workspace_binding(
    runtime: &AgentRuntime,
    session_id: AgentSessionId,
) -> Option<(String, String)> {
    let detail = runtime.session_detail(core_sid(session_id)).ok()?;
    match (detail.cwd, detail.workspace_root) {
        (Some(cwd), Some(root)) => Some((cwd, root)),
        // Defensive: workspace_root and cwd are bound together, but if only one
        // is present, fall back to whichever we have for both fields so the
        // binding check still has a boundary to enforce.
        (Some(cwd), None) => Some((cwd.clone(), cwd)),
        (None, Some(root)) => Some((root.clone(), root)),
        (None, None) => None,
    }
}

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
        thinking_level,
        model_id,
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

    // Session↔workspace binding (OCEAN-52) + resume cwd pinning (OCEAN-55).
    //
    // A NEW session (`is_new_session`) legitimately sets its own cwd: the
    // path-traversal guard still applies, but there is no prior workspace to
    // bind against. A RESUMED session (client supplied a `session_id` that
    // exists on disk) is PINNED to the workspace it was started in — the turn
    // executes in the session's bound cwd, never an arbitrary `req.cwd` — after
    // validating that the requested cwd resolves to the *same* workspace root.
    // A mismatch is a cross-workspace hijack (forged session_id pointed at an
    // arbitrary cwd) and is rejected. An unknown `session_id` yields no binding
    // here; the strict resume check inside the agent loop surfaces it as the
    // canonical "session not found" error, preserving existing behaviour.
    let requested_workspace_root = state
        .runtime
        .workspace_root_for(std::path::Path::new(&cwd))
        .to_string_lossy()
        .into_owned();
    let session_binding = if is_new_session {
        None
    } else {
        session_workspace_binding(&state.runtime, session_id)
    };
    let cwd = match resolve_bound_cwd(
        &cwd,
        &requested_workspace_root,
        session_binding
            .as_ref()
            .map(|(c, r)| (c.as_str(), r.as_str())),
    ) {
        Ok(resolved) => resolved,
        Err(binding_error) => {
            tracing::warn!(
                %session_id,
                error = %binding_error.message(),
                "agent_turn: rejected by session/workspace binding guard"
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(AgentTurnResponse {
                    ok: false,
                    turn_id,
                    session_id,
                    status: AgentTurnStatus::Failed,
                    event_id_prefix: event_prefix,
                    error: Some(binding_error.message()),
                }),
            );
        }
    };

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
    // can show it live and reflect a mid-session swap. A per-turn `model_id`
    // override (OCEAN-36) wins over the global model for this readout, so the
    // client sees the model that actually drives the turn.
    let (_provider, global_model) = state.runtime.current_model();
    let turn_model = model_id.clone().unwrap_or(global_model);
    emit_agent(
        &state.events,
        &state.agent_events,
        session_id,
        AgentTurnEvent::TurnStarted {
            turn_id,
            session_id,
            model: Some(turn_model),
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
    let (_request_id, cancel) = register_running_request(
        &state,
        &mut prompt_req,
        "agent turn running",
        RequestState::Running,
    )
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
            // Every runtime AgentEvent now carries its own `session_id`
            // (OCEAN-54), stamped by the agent loop from AgentConfig. The bridge
            // still re-attaches `bridge_session_id` below, which is now
            // redundant — the native id equals the bridge id for this turn — but
            // kept so the SSE payload type (`SessionId` Uuid) is unchanged. The
            // debug_assert documents the invariant without affecting release.
            debug_assert!(
                ev.session_id().is_none()
                    || ev.session_id() == Some(bridge_session_id.to_string()).as_deref(),
                "runtime event session_id must match the bridge session id"
            );
            match ev {
                AgentEvent::TextDelta { delta, .. } => {
                    if delta.is_empty() {
                        continue;
                    }
                    bridge_bus.emit(AgentTurnEvent::AssistantTextDelta {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        delta,
                    });
                }
                AgentEvent::ThinkingDelta { delta, .. } => {
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
                    ..
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
                    ..
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
                AgentEvent::PermissionDenied {
                    tool_name, reason, ..
                } => {
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
                    ..
                } => {
                    bridge_bus.emit(AgentTurnEvent::ComponentRender {
                        session_id: bridge_session_id,
                        component_id: id,
                        kind,
                        props,
                        replace,
                    });
                }
                AgentEvent::Unmount { id, .. } => {
                    bridge_bus.emit(AgentTurnEvent::ComponentUnmount {
                        session_id: bridge_session_id,
                        component_id: id,
                    });
                }
                AgentEvent::BrowserActivity { active, .. } => {
                    bridge_bus.emit(AgentTurnEvent::BrowserActivity {
                        session_id: bridge_session_id,
                        active,
                    });
                }
                _ => {}
            }
        }
    });

    let control =
        build_prompt_control(&state, request_id, Some(core_sid(session_id)), true, cancel)
            .with_event_sink(event_tx)
            // Per-turn reasoning override (OCEAN-28/41): threads the optional
            // request `thinking_level` into this turn's config only, leaving the
            // runtime's global thinking_level untouched.
            .with_thinking_level(thinking_level)
            // Per-turn model override (OCEAN-36): threads the optional request
            // `model_id` into this turn's config only, leaving the runtime's
            // global model selection untouched.
            .with_model_id(model_id.clone());
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
    // A per-turn timeout (OCEAN-17) surfaces as code 408 from the runtime; map
    // it to HTTP 408 Request Timeout so callers can distinguish a hung-provider
    // abort from a normal failed turn. Every other outcome keeps the
    // fire-and-acknowledge 202 envelope (the turn ran; success/failure lives in
    // the body + streamed events).
    let http_status = if !res.ok && res.code == Some(408) {
        StatusCode::REQUEST_TIMEOUT
    } else {
        StatusCode::ACCEPTED
    };
    (
        http_status,
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
async fn component_event(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
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
                (StatusCode::OK, Json(json!({ "status": "delivered" })))
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "error": "no pending wait for component", "session_id": session_id, "component_id": component_id }),
            ),
        ),
    }
}

#[derive(Debug, serde::Deserialize, Default)]
struct AgentEventsQuery {
    /// When set, the SSE stream only delivers events for this session.
    ///
    /// Without it the stream deliberately omits session-bearing events. Older
    /// global subscribers may stay connected, but they cannot receive or adopt
    /// another surface's transcript. Operator diagnostics can opt into the old
    /// firehose explicitly with `?all=1`.
    #[serde(default)]
    session_id: Option<AgentSessionId>,
    #[serde(default)]
    all: Option<String>,
}

async fn agent_events(
    State(state): State<AppState>,
    Query(q): Query<AgentEventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let want = q.session_id;
    let all = q
        .all
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes" | "on"));
    let stream =
        BroadcastStream::new(state.agent_events.subscribe()).filter_map(move |event| match event {
            Ok(envelope) => {
                // Scope to the requested session when one was given. If no
                // session was requested, do not stream session-bearing events by
                // default; this prevents any stale/global first-party client
                // from adopting or rendering another surface's active session.
                if !should_emit_agent_event(want, all, &envelope.event) {
                    return None;
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

fn should_emit_agent_event(
    want: Option<AgentSessionId>,
    all: bool,
    event: &AgentTurnEvent,
) -> bool {
    match (want, event.session_id()) {
        // Session-scoped subscriber: deliver only its own session's events.
        // A session-less event (a council-wide `Extension`) is NOT this
        // session's event, so it is dropped here — the Invariant 5 exception:
        // global-by-design extension events never leak into a scoped stream.
        (Some(want), Some(sid)) => sid == want,
        (Some(_), None) => false,
        // No session requested: session-bearing events (and session-scoped
        // extension events) require the explicit `?all=1` firehose opt-in.
        (None, Some(_)) => all,
        // No session requested, session-less event (council-wide `Extension`):
        // global-by-design, delivered only to the `?all=1` firehose.
        (None, None) => all,
    }
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
            // Real per-session updated-at from metadata; fall back to now only
            // for legacy sessions that predate the timestamp field.
            updated_at: s
                .updated_ms
                .map(ms_to_datetime)
                .unwrap_or_else(Utc::now),
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

/// `POST /v1/agent/sessions` — explicit session creation before the first turn.
///
/// Per `OCEAN_ECOSYSTEM_CONTRACT`, a surface mints its session here, then posts
/// turns carrying that `session_id`. This binds the workspace and records the
/// client surface up front; it runs no agent loop. The implicit create-on-turn
/// path (a turn with no `session_id`) is unchanged and still works for clients
/// that don't call this endpoint.
async fn agent_sessions_create(
    State(state): State<AppState>,
    Json(req): Json<AgentSessionCreateRequest>,
) -> (StatusCode, Json<AgentSessionCreateResponse>) {
    let AgentSessionCreateRequest {
        workspace_root,
        project_id,
        client_type,
    } = req;

    // Resolve the working directory with the same precedence the turn path uses:
    // a non-empty workspace_root wins; else the project's workspace_root; else an
    // explicit error — never the daemon's own launch dir.
    let cwd = match state
        .runtime
        .resolve_cwd_for_turn(project_id, &workspace_root)
    {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(%error, "agent_sessions_create: cwd resolution failed");
            return (
                StatusCode::BAD_REQUEST,
                Json(AgentSessionCreateResponse {
                    session_id: AgentSessionId::new_v4(),
                    cwd: String::new(),
                    client_type: None,
                }),
            );
        }
    };

    match state.runtime.create_session(&cwd, client_type) {
        Ok((core_id, bound_cwd, stored_client_type)) => {
            let session_id = sdk_sid(core_id);
            // Announce the new session on the agent event bus so live consumers
            // (and the legacy mirror) see it the same way the turn path does.
            emit_agent(
                &state.events,
                &state.agent_events,
                session_id,
                AgentTurnEvent::SessionCreated {
                    session_id,
                    title: String::new(),
                    cwd: bound_cwd.clone(),
                },
            );
            (
                StatusCode::OK,
                Json(AgentSessionCreateResponse {
                    session_id,
                    cwd: bound_cwd,
                    client_type: stored_client_type,
                }),
            )
        }
        Err(error) => {
            tracing::warn!(%error, "agent_sessions_create: create_session failed");
            (
                StatusCode::BAD_REQUEST,
                Json(AgentSessionCreateResponse {
                    session_id: AgentSessionId::new_v4(),
                    cwd: String::new(),
                    client_type: None,
                }),
            )
        }
    }
}

async fn agent_session(
    State(state): State<AppState>,
    Path(session_id): Path<AgentSessionId>,
) -> (StatusCode, Json<AgentSessionResponse>) {
    let core_id = core_sid(session_id);
    match state.runtime.session_detail(core_id) {
        Ok(session) => {
            // Real workspace path: prefer the bound workspace root, fall back to
            // the recorded cwd; empty only for legacy pre-binding sessions.
            let cwd = session
                .workspace_root
                .clone()
                .or_else(|| session.cwd.clone())
                .unwrap_or_default();
            let turns = turns_from_detail(&session);
            // A still-running session surfaces its in-flight turn as active.
            let active_turn = turns.first().and_then(|t| {
                matches!(t.status, AgentTurnStatus::Running).then_some(t.id)
            });
            (
                StatusCode::OK,
                Json(AgentSessionResponse {
                    ok: true,
                    session: Some(ocean_agent_sdk::AgentSession {
                        id: session_id,
                        title: session.title.clone(),
                        cwd,
                        created_at: ms_to_datetime(session.created_ms),
                        updated_at: ms_to_datetime(session.updated_ms),
                        active_turn,
                        client_type: session.client_type.clone(),
                    }),
                    turns,
                    error: None,
                }),
            )
        }
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

/// Convert an epoch-millisecond timestamp into a `DateTime<Utc>`, falling back
/// to `Utc::now()` if the value is out of range (never expected for real data).
fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

/// Derive the SDK `AgentTurn` list from a persisted session transcript.
///
/// A "turn" is one operator instruction plus its execution. The transcript
/// records messages by role; each `user`-role entry begins a turn whose prompt
/// is that entry's text. The turn is considered completed once the session is
/// no longer running. Returned newest-first to match the documented contract.
fn turns_from_detail(session: &SessionDetail) -> Vec<AgentTurn> {
    let sid = sdk_sid(session.id);
    let session_running = matches!(
        session.state,
        SessionRunState::Running
            | SessionRunState::WaitingForPermission
            | SessionRunState::Cancelling
    );
    let user_entries: Vec<&ocean_core::SessionTranscriptEntry> = session
        .transcript
        .iter()
        .filter(|e| e.role == "user")
        .collect();
    let last_idx = user_entries.len().saturating_sub(1);
    let mut turns: Vec<AgentTurn> = user_entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let started_at = entry
                .timestamp_ms
                .map(ms_to_datetime)
                .unwrap_or_else(|| ms_to_datetime(session.created_ms));
            // Only the final turn can still be in-flight; earlier ones are done.
            let is_last = i == last_idx;
            let (status, finished_at) = if is_last && session_running {
                (AgentTurnStatus::Running, None)
            } else {
                (AgentTurnStatus::Completed, Some(started_at))
            };
            AgentTurn {
                id: AgentTurnId(Uuid::new_v4()),
                session_id: sid,
                prompt: entry.text.clone(),
                status,
                started_at,
                finished_at,
                error: None,
            }
        })
        .collect();
    // Newest first per the AgentSessionResponse contract.
    turns.reverse();
    turns
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
        AgentTurnEvent::AssistantTextDelta {
            turn_id: _, delta, ..
        } => Some(OceanEvent::AssistantDelta { text: delta }),
        AgentTurnEvent::ThinkingDelta { .. } => None,
        AgentTurnEvent::ToolCallStarted {
            turn_id: _, call, ..
        } => Some(OceanEvent::ToolStarted {
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
        AgentTurnEvent::Extension { .. } => None,
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
        AgentTurnEvent::Extension { .. } => "extension",
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
        OceanEvent::CallStarted { .. } => "call_started",
        OceanEvent::CallTranscriptSegment { .. } => "call_transcript_segment",
        OceanEvent::CallSummaryUpdated { .. } => "call_summary_updated",
        OceanEvent::CallTaskDetected { .. } => "call_task_detected",
        OceanEvent::CallWakeTriggered { .. } => "call_wake_triggered",
        OceanEvent::CallAgentSpoke { .. } => "call_agent_spoke",
        OceanEvent::CallEnded { .. } => "call_ended",
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
    fn agent_event_filter_requires_session_or_explicit_global_opt_in() {
        let session_a = AgentSessionId::new_v4();
        let session_b = AgentSessionId::new_v4();
        let event = AgentTurnEvent::TurnStarted {
            turn_id: AgentTurnId::new_v4(),
            session_id: session_a,
            model: Some("model-a".to_string()),
        };

        assert!(should_emit_agent_event(Some(session_a), false, &event));
        assert!(!should_emit_agent_event(Some(session_b), false, &event));
        assert!(!should_emit_agent_event(None, false, &event));
        assert!(should_emit_agent_event(None, true, &event));
    }

    // OCEAN-56: a council-wide (sessionless) Longhouse/Extension event must NOT
    // leak into a session-scoped subscriber's stream — that would violate
    // Invariant 5. It is global-by-design, so it reaches only the `?all=1`
    // firehose. A session-scoped Extension event is filtered like any
    // session-bearing event.
    #[test]
    fn council_wide_extension_event_is_global_opt_in_only() {
        let council = LonghouseEvent::TopicConvened {
            topic_id: Uuid::new_v4(),
            board_id: Uuid::new_v4(),
            federation: Federation::Sales,
            trigger: ConveneTrigger::Deliberation,
            title: "which creators for Warner Q3".into(),
            deadline_ms: 1_700_000_000_000,
        }
        .into_turn_event();

        // A subscriber scoped to ANY session must NOT receive it.
        let unrelated = AgentSessionId::new_v4();
        assert!(!should_emit_agent_event(Some(unrelated), false, &council));

        // A plain global subscriber (no `?all=1`) must NOT receive it either —
        // global-by-design events are opt-in, not default.
        assert!(!should_emit_agent_event(None, false, &council));

        // Only the explicit `?all=1` firehose receives it.
        assert!(should_emit_agent_event(None, true, &council));
    }

    #[test]
    fn session_scoped_extension_event_is_filtered_like_a_session_event() {
        let session_a = AgentSessionId::new_v4();
        let session_b = AgentSessionId::new_v4();
        let scoped = LonghouseEvent::TopicClosed {
            topic_id: Uuid::new_v4(),
        }
        .into_turn_event_scoped(session_a);

        // Matching session gets it; unrelated session does not.
        assert!(should_emit_agent_event(Some(session_a), false, &scoped));
        assert!(!should_emit_agent_event(Some(session_b), false, &scoped));
        // Plain global subscriber does not; `?all=1` does.
        assert!(!should_emit_agent_event(None, false, &scoped));
        assert!(should_emit_agent_event(None, true, &scoped));
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

    // --- OCEAN-13: session detail uses real data, not stubs -----------------

    fn transcript_entry(role: &str, text: &str, ts_ms: i64) -> ocean_core::SessionTranscriptEntry {
        ocean_core::SessionTranscriptEntry {
            role: role.into(),
            timestamp_ms: Some(ts_ms),
            text: text.into(),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
        }
    }

    fn detail_fixture(state: SessionRunState) -> SessionDetail {
        SessionDetail {
            id: SessionId::new_v4(),
            created_ms: 1_000,
            updated_ms: 5_000,
            model: "test-model".into(),
            provider: "test".into(),
            turns: 2,
            title: "fix the thing".into(),
            state,
            resumable: true,
            active_requests: vec![],
            pending_permissions: vec![],
            transcript: vec![
                transcript_entry("user", "first ask", 1_000),
                transcript_entry("assistant", "working on it", 2_000),
                transcript_entry("user", "second ask", 3_000),
                transcript_entry("assistant", "done", 4_000),
            ],
            tool_context: vec![],
            messages: vec![],
            workspace_root: Some("/work/repo".into()),
            cwd: Some("/work/repo/sub".into()),
            git_branch: None,
            git_commit: None,
            client_type: None,
        }
    }

    #[test]
    fn session_detail_yields_real_cwd_and_timestamps() {
        let detail = detail_fixture(SessionRunState::Completed);
        // cwd preference is workspace_root, not empty.
        let cwd = detail
            .workspace_root
            .clone()
            .or_else(|| detail.cwd.clone())
            .unwrap_or_default();
        assert_eq!(cwd, "/work/repo");
        assert!(!cwd.is_empty(), "cwd must not be the empty stub");
        assert_eq!(ms_to_datetime(detail.created_ms).timestamp_millis(), 1_000);
        assert_eq!(ms_to_datetime(detail.updated_ms).timestamp_millis(), 5_000);
    }

    #[test]
    fn turns_from_detail_maps_user_entries_newest_first() {
        let detail = detail_fixture(SessionRunState::Completed);
        let turns = turns_from_detail(&detail);
        assert_eq!(turns.len(), 2, "one turn per user transcript entry");
        // Newest first.
        assert_eq!(turns[0].prompt, "second ask");
        assert_eq!(turns[1].prompt, "first ask");
        // A completed session has all turns completed.
        assert!(turns
            .iter()
            .all(|t| t.status == AgentTurnStatus::Completed));
    }

    #[test]
    fn turns_from_detail_marks_running_last_turn() {
        let detail = detail_fixture(SessionRunState::Running);
        let turns = turns_from_detail(&detail);
        // The newest (running) turn is in-flight; the earlier one is done.
        assert_eq!(turns[0].status, AgentTurnStatus::Running);
        assert!(turns[0].finished_at.is_none());
        assert_eq!(turns[1].status, AgentTurnStatus::Completed);
    }

    // --- OCEAN-12: registry GC ----------------------------------------------

    fn terminal_status_at(
        request_id: RequestId,
        state: RequestState,
        finished_at: DateTime<Utc>,
    ) -> RequestControl {
        let mut ctl = status(request_id, state);
        ctl.status.finished_at = Some(finished_at);
        ctl
    }

    #[tokio::test]
    async fn gc_drops_old_terminal_requests_keeps_recent_and_live() {
        let now = Utc::now();
        let old_terminal = RequestId::new_v4();
        let fresh_terminal = RequestId::new_v4();
        let live = RequestId::new_v4();

        let requests = Arc::new(RwLock::new(HashMap::from([
            (
                old_terminal,
                terminal_status_at(
                    old_terminal,
                    RequestState::Completed,
                    now - chrono::Duration::hours(2),
                ),
            ),
            (
                fresh_terminal,
                terminal_status_at(
                    fresh_terminal,
                    RequestState::Completed,
                    now - chrono::Duration::minutes(5),
                ),
            ),
            (live, status(live, RequestState::Running)),
        ])));
        let permissions: PermissionRegistry = Arc::new(RwLock::new(HashMap::new()));

        gc_registries(&requests, &permissions, now).await;

        let reqs = requests.read().await;
        assert!(!reqs.contains_key(&old_terminal), "old terminal evicted");
        assert!(reqs.contains_key(&fresh_terminal), "recent terminal kept");
        assert!(reqs.contains_key(&live), "live request kept regardless of age");
    }

    #[tokio::test]
    async fn gc_drops_old_consumed_permission_waiters() {
        let now = Utc::now();
        let leaked = PermissionId::new_v4();
        let pending = PermissionId::new_v4();
        let req = RequestId::new_v4();

        let mut leaked_status = permission_status(leaked, req);
        leaked_status.created_at = now - chrono::Duration::hours(2);
        let mut pending_status = permission_status(pending, req);
        pending_status.created_at = now - chrono::Duration::hours(2);

        let (tx, _rx) = oneshot::channel();
        let permissions: PermissionRegistry = Arc::new(RwLock::new(HashMap::from([
            // sender consumed => terminal, and old => evicted
            (
                leaked,
                PermissionWaiter {
                    status: leaked_status,
                    sender: None,
                },
            ),
            // still pending (Some) => never reaped by age
            (
                pending,
                PermissionWaiter {
                    status: pending_status,
                    sender: Some(tx),
                },
            ),
        ])));
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));

        gc_registries(&requests, &permissions, now).await;

        let perms = permissions.read().await;
        assert!(!perms.contains_key(&leaked), "old consumed waiter evicted");
        assert!(perms.contains_key(&pending), "pending waiter kept");
    }

    #[tokio::test]
    async fn evict_overflow_trims_terminal_first() {
        let now = Utc::now();
        let mut map: HashMap<RequestId, RequestControl> = HashMap::new();
        // Two live + one terminal; cap-trim of 1 should drop the terminal one.
        let live_a = RequestId::new_v4();
        let live_b = RequestId::new_v4();
        let term = RequestId::new_v4();
        map.insert(live_a, status(live_a, RequestState::Running));
        map.insert(live_b, status(live_b, RequestState::Running));
        map.insert(
            term,
            terminal_status_at(term, RequestState::Completed, now),
        );

        // Directly exercise the ranking: remove 1 entry.
        // (REGISTRY_MAX_ENTRIES is 10k, so call the ranker via a manual trim.)
        let overflow = 1;
        let mut ranked: Vec<(RequestId, bool, DateTime<Utc>)> = map
            .iter()
            .map(|(k, v)| (*k, v.is_terminal(), v.terminal_at()))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        let first = ranked.into_iter().take(overflow).next().unwrap();
        assert!(first.1, "terminal entry ranked first for eviction");
        assert_eq!(first.0, term);
    }

    #[test]
    fn permission_args_hash_is_stable_for_equal_args() {
        let a = json!({"path": "src/lib.rs", "content": "x"});
        let b = json!({"path": "src/lib.rs", "content": "x"});
        assert_eq!(permission_args_hash(&a), permission_args_hash(&b));
    }

    #[test]
    fn permission_args_hash_differs_for_different_args() {
        let a = json!({"path": "src/lib.rs"});
        let b = json!({"path": "src/main.rs"});
        assert_ne!(permission_args_hash(&a), permission_args_hash(&b));
    }

    // OCEAN-21: identical (tool, args) within one turn must reuse the same
    // PermissionId; a different tool or different args must get a distinct one.
    // This exercises the same dedupe map / keying used in
    // `DaemonPermissionPolicy::check`.
    #[test]
    fn permission_dedupe_reuses_id_for_identical_tool_and_args() {
        let seen: Arc<Mutex<HashMap<(String, u64), PermissionId>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mint = |tool: &str, args: &Value| -> PermissionId {
            let key = (tool.to_string(), permission_args_hash(args));
            let mut guard = seen.lock().unwrap();
            *guard.entry(key).or_insert_with(PermissionId::new_v4)
        };

        let args = json!({"path": "src/lib.rs"});
        let first = mint("write", &args);
        let retry = mint("write", &json!({"path": "src/lib.rs"}));
        assert_eq!(first, retry, "retrying identical tool+args reuses the id");

        let other_args = mint("write", &json!({"path": "src/main.rs"}));
        assert_ne!(first, other_args, "different args mint a new id");

        let other_tool = mint("edit", &args);
        assert_ne!(first, other_tool, "different tool mints a new id");

        assert_eq!(seen.lock().unwrap().len(), 3, "three distinct keys cached");
    }

    // ---- Persistent Rooms (OCEAN-65) ---------------------------------------

    #[test]
    fn parse_mentions_extracts_dedup_in_order() {
        let ids = parse_mentions("@ocean fix it then @reviewer check, cc @ocean");
        assert_eq!(ids, vec!["ocean".to_string(), "reviewer".to_string()]);
    }

    #[test]
    fn parse_mentions_handles_no_mentions_and_punctuation() {
        assert!(parse_mentions("just a plain line").is_empty());
        // trailing punctuation terminates the id.
        assert_eq!(parse_mentions("ping @ocean!"), vec!["ocean".to_string()]);
        // bare @ is not a mention.
        assert!(parse_mentions("email me @ work").is_empty());
    }

    #[test]
    fn room_store_error_maps_to_expected_status() {
        use ocean_agent::RoomStoreError;
        let (s, _) = room_store_error_response(RoomStoreError::BadKey("".into()));
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) =
            room_store_error_response(RoomStoreError::UnknownRoom(RoomKey::new("x")));
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) =
            room_store_error_response(RoomStoreError::AlreadyExists(RoomKey::new("x")));
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _) = room_store_error_response(RoomStoreError::UnknownParticipant {
            room: RoomKey::new("x"),
            participant: "p".into(),
        });
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn mention_with_policy_produces_convene_decision() {
        // The exact inputs the message handler feeds the evaluator: a stored
        // room's policy + a Mention event parsed from the body. Proves the
        // trigger wiring point fires on a matching event.
        let mut reg = ocean_agent::RoomRegistry::new();
        let key = RoomKey::new("r1");
        reg.create(
            key.clone(),
            "R1",
            Some(RoomTriggerPolicy {
                on_mention: true,
                ..Default::default()
            }),
            Utc::now(),
        )
        .unwrap();

        let mentions = parse_mentions("@ocean please look");
        assert_eq!(mentions, vec!["ocean".to_string()]);
        let decision = evaluate_trigger_policy(
            reg.trigger_policy(&key).as_ref(),
            &RoomTriggerEvent::Mention {
                participant_id: mentions[0].clone(),
            },
        );
        assert!(decision.should_convene);
        assert_eq!(decision.target_participant.as_deref(), Some("ocean"));
    }

    // --- OCEAN-52/55: session↔workspace binding + traversal guard -----------

    #[test]
    fn new_session_runs_in_requested_cwd() {
        // A brand-new session (no prior binding) legitimately sets its own cwd.
        let out = resolve_bound_cwd("/work/repo/sub", "/work/repo", None)
            .expect("new session cwd should be accepted");
        assert_eq!(
            out, "/work/repo/sub",
            "a new session runs in exactly the requested cwd"
        );
    }

    #[test]
    fn resumed_turn_pinned_to_session_bound_cwd_not_req_cwd() {
        // The session was started in /work/repo/sub (workspace /work/repo). The
        // resumed turn supplies the same workspace via a *different* sub-dir,
        // but execution must pin to the session's bound cwd, not req.cwd.
        let out = resolve_bound_cwd(
            "/work/repo/another-sub",
            "/work/repo",
            Some(("/work/repo/sub", "/work/repo")),
        )
        .expect("matching workspace should be accepted");
        assert_eq!(
            out, "/work/repo/sub",
            "a resumed turn executes in the session's bound cwd, not req.cwd"
        );
    }

    #[test]
    fn resumed_turn_session_cwd_equal_to_req_cwd_is_pinned() {
        // The common case: the client re-sends the same cwd it started in.
        let out = resolve_bound_cwd(
            "/work/repo",
            "/work/repo",
            Some(("/work/repo", "/work/repo")),
        )
        .expect("identical workspace should be accepted");
        assert_eq!(out, "/work/repo");
    }

    #[test]
    fn session_cwd_mismatch_is_rejected() {
        // A forged session_id whose bound workspace differs from the cwd the
        // turn points at is a cross-workspace hijack — reject it.
        let err = resolve_bound_cwd(
            "/etc",                                  // attacker cwd
            "/etc",                                  // its workspace
            Some(("/work/repo/sub", "/work/repo")),  // session bound elsewhere
        )
        .expect_err("cross-workspace resume must be rejected");
        assert_eq!(
            err,
            CwdBindingError::WorkspaceMismatch {
                requested_workspace: "/etc".into(),
                session_workspace: "/work/repo".into(),
            }
        );
    }

    #[test]
    fn path_traversal_cwd_is_rejected_for_new_session() {
        let err = resolve_bound_cwd("/work/repo/../../etc", "/work/repo", None)
            .expect_err("traversal cwd must be rejected");
        assert!(matches!(err, CwdBindingError::PathTraversal { .. }));
    }

    #[test]
    fn path_traversal_cwd_is_rejected_for_resumed_session() {
        // Even when the (lexical) workspace strings would match, a `..` in the
        // requested cwd is rejected before any binding comparison.
        let err = resolve_bound_cwd(
            "/work/repo/../repo",
            "/work/repo",
            Some(("/work/repo", "/work/repo")),
        )
        .expect_err("traversal cwd must be rejected on resume too");
        assert!(matches!(err, CwdBindingError::PathTraversal { .. }));
    }

    #[test]
    fn cwd_has_traversal_detects_parent_components_only() {
        assert!(cwd_has_traversal("/a/../b"));
        assert!(cwd_has_traversal("../b"));
        assert!(!cwd_has_traversal("/a/b/c"));
        // A literal dir literally named "..something" is not a parent ref.
        assert!(!cwd_has_traversal("/a/..b/c"));
        assert!(!cwd_has_traversal("/work/repo"));
    }
}
