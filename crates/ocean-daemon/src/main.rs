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
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
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
    PermissionStatus, PermissionsResponse, Project, ProjectConfig, ProjectId, ProjectRef,
    ProjectResponse, ProjectsResponse, PromptImage, PromptRequest, RequestControlResponse,
    RequestCreateResponse,
    RequestId,
    evaluate_trigger_policy, RequestState, RequestStatus, RequestsResponse, RoomId, RoomKey,
    RoomMessageKind, RoomPanelSnapshot, RoomParticipant, RoomParticipantKind, RoomSnapshot,
    RoomTriggerEvent, RoomTriggerPolicy, RoomsResponse, SessionDetail, SessionId, SessionResponse,
    SessionRunState, SessionSummary,
};
use ocean_runtime::{
    tools::component::COMPONENT_WAIT_REGISTRY, AgentEvent,
    PermissionDecision as AgentPermissionDecision, PermissionPolicy,
};
// Brings the `RoomStore` trait methods (create/get/list/append_message/…) into
// scope on `SqliteRoomStore` for the persistent-room handlers (OCEAN-107).
use ocean_store::RoomStore;
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
    cors::{AllowOrigin, CorsLayer},
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
    /// Persistent Room lifecycle store (OCEAN-65 / OCEAN-107): the durable `Room`
    /// entities (roster + transcript + trigger policy), distinct from the Track-0
    /// `RoomSnapshot` projection served by `GET /v1/rooms`. Backed by
    /// `ocean_store::SqliteRoomStore` (OCEAN-86) so rooms and transcripts survive
    /// daemon restarts. Held behind a std `Mutex` like the longhouse registry —
    /// the guard is always dropped before any `await`, and every store method is
    /// synchronous, so a std `Mutex` is correct and never blocks the scheduler.
    rooms: RoomStoreHandle,
}

type LonghouseRegistryHandle = Arc<Mutex<ocean_longhouse::LonghouseRegistry>>;
type RoomStoreHandle = Arc<Mutex<ocean_store::SqliteRoomStore>>;

type RequestRegistry = Arc<RwLock<HashMap<RequestId, RequestControl>>>;
type PermissionRegistry = Arc<RwLock<HashMap<PermissionId, PermissionWaiter>>>;

struct RequestControl {
    status: RequestStatus,
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
    /// Per-turn secret bound to the submitter (OCEAN-185, P0). Set from the
    /// turn's `decision_token`; the gating policy copies it into every
    /// `PermissionWaiter` this turn raises, and the decision POST must present
    /// it. `None` = the turn was submitted without binding (a legacy/internal
    /// turn). Never serialized onto the public `/v1/events` SSE. Held here so the
    /// turn record owns the secret; the enforcement read is on the waiter.
    #[allow(dead_code)]
    decision_token: Option<String>,
}

struct PermissionWaiter {
    status: PermissionStatus,
    sender: Option<oneshot::Sender<AgentPermissionDecision>>,
    /// The turn's `decision_token` (OCEAN-185), copied from the owning
    /// `RequestControl` when the waiter is registered. The decision handler
    /// constant-time-compares the POSTed token against this; a missing/wrong
    /// token is rejected 403. `None` = the gated turn was submitted unbound
    /// (legacy client). NEVER placed in `status` or any SSE payload.
    decision_token: Option<String>,
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
    /// Per-turn permission secret (OCEAN-185, P0). Copied from the submitting
    /// turn's `decision_token` into every `PermissionWaiter` this policy mints,
    /// so the decision POST can be bound to the original submitter. `None` = the
    /// turn was submitted without binding (legacy client). Never emitted on SSE.
    decision_token: Option<String>,
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

    // Used by the daemon unit tests; the live `/v1/events` handler now uses
    // `subscribe_with_replay` (OCEAN-129).
    #[cfg_attr(not(test), allow(dead_code))]
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// OCEAN-129: atomically subscribe to the live broadcast and snapshot the
    /// history buffer under the same lock so no event slips through the seam.
    /// When `last_event_id` is present and still buffered, returns the buffered
    /// envelopes strictly AFTER it (in emission order) to replay before the live
    /// stream attaches; otherwise the replay vec is empty (id aged out / no
    /// header) and the caller just attaches the live stream as before.
    fn subscribe_with_replay(
        &self,
        last_event_id: Option<Uuid>,
    ) -> (Vec<EventEnvelope>, broadcast::Receiver<EventEnvelope>) {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let rx = self.tx.subscribe();
        let replay = match last_event_id {
            Some(want) => match history.iter().position(|env| env.id == want) {
                Some(pos) => history.iter().skip(pos + 1).cloned().collect(),
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        (replay, rx)
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

        // `broadcast::send` errors only when there are zero live receivers; the
        // event is still buffered for late subscribers and also lives in
        // `history`, so this is expected (no SSE client connected) and logged at
        // debug — not a dropped event (OCEAN-87).
        if let Err(err) = self.tx.send(event) {
            tracing::debug!(?err, "EventBus: no active subscribers for event");
        }
    }
}

/// How many recent agent events the bus retains for `Last-Event-ID` replay
/// (OCEAN-129). Each envelope is a small enum value plus a UUID — well under a
/// few KB even for the largest variants (tool chunks / thinking deltas) — so
/// 2048 entries caps the buffer at a handful of MB while covering a generous
/// reconnect window (a full streaming turn is typically a few hundred events).
/// When the buffer overflows, the oldest entries are evicted; a client whose
/// `Last-Event-ID` has already aged out simply gets the live stream with no
/// replay (same as the pre-OCEAN-129 behavior), so memory stays bounded.
const AGENT_EVENT_REPLAY_BUFFER: usize = 2048;

/// Parallel broadcast bus that carries `AgentTurnEvent`s with full fidelity
/// (turn_id, call_id, thinking deltas, tool chunks). The legacy `OceanEvent`
/// bus still ships, but `/v1/agent/events` subscribes here so the TUI can
/// render real-time streaming output without the lossy round-trip.
///
/// OCEAN-129: the bus also keeps a bounded in-memory ring buffer of recent
/// envelopes keyed by id so a reconnecting SSE client carrying a
/// `Last-Event-ID` header can be replayed the events it missed while away,
/// before it attaches to the live broadcast.
#[derive(Clone)]
struct AgentEventBus {
    tx: broadcast::Sender<AgentEventEnvelope>,
    history: Arc<Mutex<VecDeque<AgentEventEnvelope>>>,
    history_limit: usize,
}

#[derive(Clone)]
struct AgentEventEnvelope {
    id: Uuid,
    event: AgentTurnEvent,
}

impl AgentEventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            history: Arc::new(Mutex::new(VecDeque::with_capacity(
                AGENT_EVENT_REPLAY_BUFFER.min(256),
            ))),
            history_limit: AGENT_EVENT_REPLAY_BUFFER,
        }
    }

    fn emit(&self, event: AgentTurnEvent) {
        let envelope = AgentEventEnvelope {
            id: Uuid::new_v4(),
            event,
        };

        // Record into the bounded replay ring BEFORE broadcasting so that a
        // client which subscribes (and snapshots the buffer) concurrently with
        // this emit can never observe the live event without also finding it in
        // the replay buffer — closing the gap/dupe seam (OCEAN-129).
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            history.push_back(envelope.clone());
            while history.len() > self.history_limit {
                history.pop_front();
            }
        }

        // `broadcast::send` errors only when there are no live receivers (no SSE
        // client subscribed to `/v1/agent/events`). That's expected during idle
        // periods, so debug — not warn. Per-subscriber *lag* (a slow client that
        // overflows the ring buffer) surfaces on the RECEIVE side as
        // `Lagged(n)`, which the SSE handlers log at warn (OCEAN-87).
        if self.tx.send(envelope).is_err() {
            tracing::debug!("AgentEventBus: no active subscribers for event");
        }
    }

    /// Atomically subscribe to the live broadcast and snapshot the replay
    /// buffer under the same lock, so no event can slip between the two. If
    /// `last_event_id` is present and still in the buffer, returns the buffered
    /// envelopes strictly AFTER it (in emission order) for replay; otherwise the
    /// replay vector is empty (the id aged out, or no header was sent), and the
    /// caller just attaches the live stream — matching pre-OCEAN-129 behavior.
    ///
    /// Holding the `history` lock across `self.tx.subscribe()` is the seam
    /// guarantee: `emit` takes the same lock before it sends, so every event is
    /// either already in the snapshot (and will be replayed) or will arrive on
    /// the freshly-created live receiver — never both, never neither. Replayed
    /// ids are still deduped against the live tail by the handler as a belt-and-
    /// suspenders measure.
    fn subscribe_with_replay(
        &self,
        last_event_id: Option<Uuid>,
    ) -> (Vec<AgentEventEnvelope>, broadcast::Receiver<AgentEventEnvelope>) {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let rx = self.tx.subscribe();
        let replay = match last_event_id {
            Some(want) => match history.iter().position(|env| env.id == want) {
                // Found: replay everything strictly after it.
                Some(pos) => history.iter().skip(pos + 1).cloned().collect(),
                // Not found (aged out / unknown id): no replay.
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        (replay, rx)
    }
}

/// Parse a `Last-Event-ID` SSE reconnect header (RFC: EventSource sets it to the
/// last `id:` it saw) into a `Uuid`. Returns `None` when absent or unparseable.
fn parse_last_event_id(headers: &HeaderMap) -> Option<Uuid> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok())
}

/// OCEAN-51: whether the daemon runs the product agent-turn path
/// (`POST /v1/agent/turns`, and the voice wrapper) in "yolo" mode — every tool
/// auto-approved, no per-tool permission gating.
///
/// Default is `false`: tool calls are gated by `DaemonPermissionPolicy` exactly
/// as the permission machinery was designed, and a mutating tool will emit a
/// `PermissionRequest` event and block until an operator decision arrives via
/// `POST /v1/permissions/{id}/decision`.
///
/// Set `OCEAN_YOLO=1` (or `true`/`yes`/`on`) to restore the previous
/// fire-and-forget behavior for trusted automation. This is the documented,
/// explicit operator opt-in — the bypass is NEVER the silent default.
///
/// Read fresh on each turn (not cached) so an operator can flip it by restarting
/// with a different env without code changes, and so tests can scope it.
///
/// This is ONLY the env layer. The effective per-turn posture is resolved by
/// [`effective_yolo`], which layers the persisted operator default (OCEAN-YOLO)
/// underneath the env — every live call site now uses `effective_yolo`, so this
/// remains as the focused env-layer assertion target for tests.
#[cfg(test)]
fn yolo_enabled() -> bool {
    yolo_env_pref().unwrap_or(false)
}

/// Parse the `OCEAN_YOLO` env var into an explicit preference: `Some(true)` /
/// `Some(false)` for a recognized spelling, `None` when unset or unrecognized
/// (so the caller falls through to the persisted setting). Recognizing the
/// "off" spellings explicitly (not just "absent") is what lets `OCEAN_YOLO=0`
/// OVERRIDE a persisted `true` for a session — the documented precedence.
fn yolo_env_pref() -> Option<bool> {
    match env::var("OCEAN_YOLO")
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolve the effective YOLO posture for a turn, in precedence order:
///
///   1. `OCEAN_YOLO` env, if set to a recognized value (operator/CI override),
///   2. the persisted operator default (OCEAN-YOLO — set once via
///      `POST /v1/settings/yolo`, survives restarts),
///   3. the built-in default: **off** (permission gating ON).
///
/// The per-request `req.yolo` flag (a client opting INTO yolo for one turn)
/// sits ABOVE this whole chain and is applied at the call site (`req.yolo ||
/// effective_yolo()`), so an explicit per-request opt-in always wins while
/// absence falls through to env → persisted → off.
///
/// Default-off is the safety invariant: nothing configured ⇒ gated. This
/// function only decides whether tools auto-approve; it does NOT touch the
/// permission decision-token binding (OCEAN-185), which stays orthogonal.
fn effective_yolo() -> bool {
    if let Some(env_pref) = yolo_env_pref() {
        return env_pref;
    }
    ocean_agent::load_yolo_pref(&ocean_agent::config_dir_from_env()).unwrap_or(false)
}

/// Resolve the YOLO posture for a turn arriving on a wire `PromptRequest`
/// (`POST /v1/prompt`, `POST /v1/requests`), DELIBERATELY IGNORING the
/// client-supplied `wire_yolo` flag (OCEAN-160, P0).
///
/// History: the legacy handlers used to compute `req.yolo || effective_yolo()`.
/// Because `PromptRequest.yolo` deserializes straight off the request JSON, any
/// client could POST `{"yolo": true, ...}` and force the bypass on — every tool
/// auto-approved, the entire `DaemonPermissionPolicy` gate skipped — even when
/// the operator had NOT opted in. That is an auth-bypass: a per-request wire
/// flag must never be able to escalate past the operator's policy.
///
/// The fix matches the modern product path (`POST /v1/agent/turns`, see
/// `agent_turn`), whose `AgentTurnRequest` carries no yolo field at all and
/// resolves the posture purely from `effective_yolo()` (OCEAN_YOLO env →
/// persisted operator default → off). It also matches the established
/// epic-E7 pattern: OCEAN-162 documented that "the daemon ignores the wire
/// `yolo` flag and gates mutating tools on its own `OCEAN_YOLO`" and patched
/// the CLI to stop sending it — this closes the daemon side of that contract so
/// the field is truly inert, regardless of which client sends it.
///
/// A legitimate operator who relies on the persisted/env yolo default is
/// unaffected: that path runs through `effective_yolo()` exactly as before. The
/// `wire_yolo` parameter is accepted (and ignored) only so the inert flag is
/// explicit at the call site and the security intent is greppable.
fn resolve_request_yolo(wire_yolo: bool) -> bool {
    // The wire flag is intentionally discarded; see the doc comment above.
    let _ = wire_yolo;
    effective_yolo()
}

/// HTTP methods advertised in the CORS preflight (`Access-Control-Allow-Methods`).
/// Must cover EVERY method the router actually serves, or the browser's OPTIONS
/// preflight fails and the real request never fires (OCEAN-87). The router serves
/// GET/POST plus `PATCH /v1/projects/{id}`, `DELETE /v1/projects/{id}`, and
/// `DELETE /v1/rooms/persistent/{key}/participants/{id}`; OPTIONS is the preflight
/// method itself. Keep this in sync with the `Router::route()` method set.
fn cors_allowed_methods() -> [Method; 5] {
    [
        Method::GET,
        Method::POST,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ]
}

/// Parse a comma-separated `OCEAN_ALLOWED_ORIGINS` list into normalized origins.
/// Whitespace is trimmed, empty entries dropped, and a trailing slash removed so
/// `https://app.example.com/` and `https://app.example.com` both match the
/// browser-sent `Origin` header (which never has a trailing slash).
fn parse_allowed_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// CORS gate (OCEAN-53). Returns true for origins the daemon trusts:
///
/// 1. Loopback web origins on any port: `http://localhost:*`,
///    `http://127.0.0.1:*`, `http://[::1]:*` (and their `https` forms). This
///    covers the browser PWA however it's served (`trunk serve` :8080, vite
///    :5173, the surface proxy :8790) without hardcoding ports.
/// 2. Any `chrome-extension://...` origin — the Ocean side-panel extension runs
///    from a per-install id we can't enumerate, and it already declares the
///    daemon in its MV3 `host_permissions`/CSP `connect-src`.
/// 3. Exact matches against operator-configured `OCEAN_ALLOWED_ORIGINS` (e.g. a
///    tunnel hostname for phone access).
///
/// Everything else (arbitrary public web pages) is rejected.
fn is_trusted_origin(origin: &HeaderValue, extra: &[String]) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    is_loopback_origin(origin)
        || origin.starts_with("chrome-extension://")
        || extra.iter().any(|allowed| allowed == origin)
}

/// True for `http(s)://localhost|127.0.0.1|[::1]` with any (or no) port. Matches
/// only the exact loopback hosts — `localhost.evil.com` and
/// `127.0.0.1.evil.com` do NOT match because the host segment must end right
/// after the loopback name (`:` for a port, or end-of-string).
fn is_loopback_origin(origin: &str) -> bool {
    let host = match origin.strip_prefix("http://") {
        Some(rest) => rest,
        None => match origin.strip_prefix("https://") {
            Some(rest) => rest,
            None => return false,
        },
    };
    // Strip the port (everything after the first ':'), if present. IPv6 `[::1]`
    // is handled explicitly below since it contains its own colons in brackets.
    if let Some(rest) = host.strip_prefix("[::1]") {
        return rest.is_empty() || rest.starts_with(':');
    }
    let host_only = host.split(':').next().unwrap_or(host);
    host_only == "localhost" || host_only == "127.0.0.1"
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

    // The Longhouse read-side topic registry. Built BEFORE the runtime so the
    // SAME handle is shared two ways (OCEAN-118): into the capability registry
    // (so agents' `longhouse__convene` / `longhouse__board_read` tools drive it)
    // AND onto AppState (so the operator's `/v1/longhouse/topics*` HTTP routes
    // serve off it). One observable board for both surfaces.
    let longhouse: LonghouseRegistryHandle =
        Arc::new(Mutex::new(ocean_longhouse::LonghouseRegistry::new()));

    // Built-ins first, then connect any configured MCP servers (non-fatally)
    // and fold their tools into the capability registry before sharing it.
    let runtime =
        Arc::new(AgentRuntime::from_env()?.with_extensions(Some(longhouse.clone())).await);

    // Persistent rooms (OCEAN-107): open the durable SQLite store at startup so
    // rooms + transcripts survive a daemon restart. The DB lives under the same
    // config dir the agent uses for sessions/projects (`OCEAN_CONFIG_DIR`,
    // `XDG_CONFIG_HOME/ocean-rs`, then `~/.config/ocean-rs`), as `rooms.db` —
    // overridable wholesale with `OCEAN_DB_PATH`. `open` runs migrations
    // idempotently, so this is safe on a fresh or an existing DB.
    let rooms_db_path = room_db_path();
    if let Some(parent) = rooms_db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating rooms DB directory {}", parent.display())
        })?;
    }
    let room_store = ocean_store::SqliteRoomStore::open(&rooms_db_path)
        .with_context(|| format!("opening rooms DB at {}", rooms_db_path.display()))?;
    tracing::info!(path = %rooms_db_path.display(), "persistent rooms store ready");

    let state = AppState {
        runtime,
        events: EventBus::new(1024),
        agent_events: AgentEventBus::new(1024),
        requests: Arc::new(RwLock::new(HashMap::new())),
        permissions: Arc::new(RwLock::new(HashMap::new())),
        longhouse,
        rooms: Arc::new(Mutex::new(room_store)),
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
                // Run each sweep on its own task so a panic inside
                // `gc_registries` (e.g. a poisoned lock surfacing) is caught as a
                // `JoinError` instead of killing this loop. A dead GC loop leaks
                // the request/permission registries unbounded for the daemon's
                // whole lifetime, silently — so we catch, log, and keep sweeping
                // (OCEAN-87).
                let reqs = requests.clone();
                let perms = permissions.clone();
                let sweep =
                    tokio::spawn(
                        async move { gc_registries(&reqs, &perms, Utc::now()).await },
                    );
                if let Err(join_err) = sweep.await {
                    tracing::error!(
                        error = %join_err,
                        "registry GC sweep panicked; skipping this cycle, loop continues"
                    );
                }
            }
        });
    }

    // OCEAN-53: the daemon is a local trust boundary; reflecting any origin
    // (`allow_origin(Any)`) let any web page in the operator's browser drive it
    // cross-origin. We restrict to a safe localhost set plus operator-configured
    // extras (`OCEAN_ALLOWED_ORIGINS`). See `cors_allowed_origins`.
    //
    // Why a predicate (and not a fixed list): the browser PWA can be served from
    // any loopback port (`trunk serve` → :8080, vite → :5173, the proxy → :8790)
    // and the Chrome side-panel runs from a per-install `chrome-extension://<id>`
    // origin we can't enumerate ahead of time. `is_trusted_origin` matches those
    // classes by shape, so we don't have to hardcode every port. The proxy and
    // native GPUI client are server-side/native HTTP callers and never send a
    // browser `Origin`, so CORS does not gate them at all.
    let extra_origins = env::var("OCEAN_ALLOWED_ORIGINS").unwrap_or_default();
    let extra_origins: Vec<String> = parse_allowed_origins(&extra_origins);
    if !extra_origins.is_empty() {
        tracing::info!(origins = ?extra_origins, "OCEAN_ALLOWED_ORIGINS: extra CORS origins");
    }
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _req| {
            is_trusted_origin(origin, &extra_origins)
        }))
        .allow_methods(cors_allowed_methods())
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

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
        // OCEAN-137: mint a LiveKit join token for a room. The proxy + web
        // surface already POST to this path; the daemon honors it here so
        // in-room voice/video connects on web instead of 404ing.
        .route(
            "/v1/rooms/{room_id}/livekit-token",
            post(room_livekit_token),
        )
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", get(session))
        .route("/v1/projects", get(projects_list).post(project_create))
        .route(
            "/v1/projects/{id}",
            get(project_get).patch(project_patch).delete(project_delete),
        )
        .route("/v1/model", get(model_get).post(model_set))
        .route("/v1/models", get(models_list))
        .route("/v1/settings/yolo", get(yolo_setting_get).post(yolo_setting_set))
        .route("/v1/component/event", post(component_event))
        // Longhouse + council convene routes (incl. the `/v1/council/convene`
        // alias and the read-only `/v1/longhouse/prepare` prep step) live in one
        // reusable group so the router here and the HTTP route test below
        // register exactly the same table (OCEAN-227, OCEAN-226).
        .merge(longhouse_routes())
        .route("/v1/calls/demo", post(call_demo))
        .route("/v1/calls/place", post(call_place))
        .route("/v1/calls/webhook", post(call_webhook))
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    // Drain the registry of in-flight turn tasks AFTER axum finishes draining
    // open connections (OCEAN-184). `with_graceful_shutdown` only waits for live
    // HTTP connections, but `create_request` returns immediately after
    // `tokio::spawn`-ing the actual turn and registering its `JoinHandle`, so the
    // turn keeps running in a detached task. Without the drain below those tasks
    // would be aborted the instant `main()` returns and the Tokio runtime drops.
    // Clone the registry handle BEFORE `state` is consumed by `with_state`.
    let drain_requests = state.requests.clone();
    let app = app.with_state(state);

    let addr: SocketAddr = bind.parse().context("invalid OCEAN_BIND")?;
    tracing::info!(%addr, "ocean-daemon listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    drain_request_tasks(&drain_requests, shutdown_grace()).await;
    Ok(())
}

/// Total wall-clock budget for draining in-flight turn tasks on shutdown.
/// Overridable via `OCEAN_SHUTDOWN_GRACE_SECS` (default 20s). A value of `0`
/// disables waiting entirely (exit as soon as connections are drained).
fn shutdown_grace() -> std::time::Duration {
    let secs = env::var("OCEAN_SHUTDOWN_GRACE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

/// After axum has drained open connections, wait (up to `grace`) for the
/// detached turn tasks registered in `requests` to finish on their own. We do
/// NOT abort them — the whole point is to let active turns complete rather than
/// die mid-stream. Only the timeout path gives up, logging a warning, so a stuck
/// turn can never hang shutdown forever. (OCEAN-184)
///
/// Lock discipline: the registry is a tokio `RwLock`, so taking the handles is
/// itself an `.await`. We take the write lock, `take()` every live `JoinHandle`
/// out of its `RequestControl`, then drop the guard BEFORE awaiting any handle —
/// the awaits never run while the lock is held.
async fn drain_request_tasks(requests: &RequestRegistry, grace: std::time::Duration) {
    let handles: Vec<JoinHandle<()>> = {
        let mut reqs = requests.write().await;
        reqs.values_mut()
            .filter_map(|ctl| ctl.handle.take())
            .collect()
    };

    if handles.is_empty() {
        return;
    }

    let count = handles.len();
    tracing::info!(
        in_flight = count,
        grace_secs = grace.as_secs(),
        "draining in-flight turn tasks before exit"
    );

    let drained = tokio::time::timeout(grace, async {
        for handle in handles {
            // A task that already finished resolves immediately; a JoinError
            // (panic/abort) is fine to ignore — we only care that it's no longer
            // running.
            let _ = handle.await;
        }
    })
    .await;

    match drained {
        Ok(()) => tracing::info!(in_flight = count, "in-flight turn tasks drained; exiting"),
        Err(_) => tracing::warn!(
            in_flight = count,
            grace_secs = grace.as_secs(),
            "shutdown grace elapsed with turns still running; exiting anyway"
        ),
    }
}

/// Completes when the process receives SIGTERM or SIGINT (Ctrl-C), letting axum
/// drain in-flight HTTP requests / agent turns before the daemon exits instead
/// of being hard-killed mid-stream. (OCEAN-184)
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl_c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received; draining in-flight requests");
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
            "POST /v1/rooms/{room_id}/livekit-token",
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
            "GET /v1/settings/yolo",
            "POST /v1/settings/yolo",
            "POST /v1/component/event",
            "POST /v1/longhouse/demo",
            "POST /v1/longhouse/convene",
            "POST /v1/council/convene",
            "POST /v1/longhouse/prepare",
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

fn legacy_event_to_sse(envelope: &EventEnvelope) -> Event {
    let event_type = event_type_name(&envelope.event);
    let id = envelope.id.to_string();
    let data = serde_json::to_string(envelope).unwrap_or_else(|err| {
        json!({
            "id": envelope.id,
            "at": envelope.at,
            "type": "error",
            "message": format!("serialize event: {err}")
        })
        .to_string()
    });
    Event::default().id(id).event(event_type).data(data)
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // OCEAN-129: honor `Last-Event-ID` on reconnect — replay buffered events
    // newer than the client's last-seen id, then attach the live broadcast.
    let last_event_id = parse_last_event_id(&headers);
    let (replay, live_rx) = state.events.subscribe_with_replay(last_event_id);

    let mut replayed_ids: std::collections::HashSet<Uuid> =
        std::collections::HashSet::with_capacity(replay.len());
    let replay_events: Vec<Result<Event, Infallible>> = replay
        .into_iter()
        .map(|envelope| {
            replayed_ids.insert(envelope.id);
            Ok(legacy_event_to_sse(&envelope))
        })
        .collect();

    let live = BroadcastStream::new(live_rx).filter_map(move |event| match event {
        Ok(envelope) => {
            // Seam dedupe: drop anything already replayed.
            if replayed_ids.remove(&envelope.id) {
                return None;
            }
            Some(Ok(legacy_event_to_sse(&envelope)))
        }
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            // A slow SSE consumer overflowed the 1024-slot ring and silently
            // lost `skipped` events. Surface it server-side at warn so dropped
            // events are visible in the daemon log, not just to the client
            // (OCEAN-87).
            tracing::warn!(skipped, "events SSE subscriber lagged; dropped events");
            let data = json!({
                "type": "error",
                "message": format!("event stream lagged by {skipped} events")
            })
            .to_string();
            Some(Ok(Event::default().event("error").data(data)))
        }
    });

    // Replay first (in emission order), then the live broadcast.
    let stream = tokio_stream::iter(replay_events).chain(live);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn prompt(
    State(state): State<AppState>,
    Json(mut req): Json<PromptRequest>,
) -> Json<ocean_core::PromptResponse> {
    let (request_id, cancel) =
        register_running_request(&state, &mut req, "prompt running", RequestState::Running).await;
    // OCEAN-160 (P0): do NOT trust the wire `yolo` flag to escalate. The posture
    // is resolved purely from operator policy (env → persisted default → off),
    // exactly like `POST /v1/agent/turns`; a client-supplied `yolo: true` is
    // inert and can no longer bypass the permission gate on its own.
    req.yolo = resolve_request_yolo(req.yolo);
    emit_user_message(&state.events, &req, request_id);

    let control = build_prompt_control(
        &state,
        request_id,
        req.session_id,
        req.yolo,
        cancel,
        req.decision_token.clone(),
    );
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
    // OCEAN-160 (P0): same wire-yolo bypass as `POST /v1/prompt` — this is its
    // async sibling on the same `PromptRequest` wire type. Resolve from operator
    // policy only (env → persisted default → off); the wire `yolo` flag is inert.
    req.yolo = resolve_request_yolo(req.yolo);
    emit_user_message(&state.events, &req, request_id);

    let control = build_prompt_control(
        &state,
        request_id,
        session_id,
        req.yolo,
        cancel,
        req.decision_token.clone(),
    );
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
) -> (StatusCode, Json<PermissionControlResponse>) {
    if decision.permission_id != permission_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(PermissionControlResponse {
                ok: false,
                permission_id,
                message: "permission id mismatch between path and body".into(),
            }),
        );
    }

    // OCEAN-185 (P0): verify the per-turn secret BEFORE consuming the waiter, so
    // an attacker who sniffed the broadcast `permission_id` off /v1/events but
    // doesn't hold the token can neither approve the tool nor burn the pending
    // waiter. We peek under a read lock, constant-time-compare, and only remove
    // the waiter once the token is proven. A waiter bound to a token
    // (`Some`, the safe default for any client-submitted turn) REQUIRES a
    // matching token; an unbound waiter (`None`, legacy/daemon-internal turn)
    // skips the check so existing internal flows keep working.
    {
        let permissions = state.permissions.read().await;
        match permissions.get(&permission_id) {
            None => {
                drop(permissions);
                return (
                    StatusCode::NOT_FOUND,
                    Json(PermissionControlResponse {
                        ok: false,
                        permission_id,
                        message: "permission request not found or already handled".into(),
                    }),
                );
            }
            Some(waiter) => {
                if let Some(expected) = waiter.decision_token.as_deref() {
                    if !ocean_core::decision_token_matches(
                        Some(expected),
                        decision.decision_token.as_deref(),
                    ) {
                        drop(permissions);
                        tracing::warn!(
                            %permission_id,
                            "rejected permission decision: missing/invalid decision_token (OCEAN-185)"
                        );
                        return (
                            StatusCode::FORBIDDEN,
                            Json(PermissionControlResponse {
                                ok: false,
                                permission_id,
                                message: "forbidden: missing or invalid decision token; this \
                                    decision was not authorized by the turn's submitter"
                                    .into(),
                            }),
                        );
                    }
                }
            }
        }
    }

    let waiter = {
        let mut permissions = state.permissions.write().await;
        permissions.remove(&permission_id)
    };

    let Some(mut waiter) = waiter else {
        // Lost a race: the waiter was resolved/cancelled between our verify and
        // our remove. Treat as already-handled.
        return (
            StatusCode::NOT_FOUND,
            Json(PermissionControlResponse {
                ok: false,
                permission_id,
                message: "permission request not found or already handled".into(),
            }),
        );
    };

    let agent_decision = match decision.decision {
        PermissionDecisionBody::Allow => AgentPermissionDecision::Allow,
        // "Allow for this session": the runtime records the tool in the agent
        // loop's per-run `session_allowed` set, so identical follow-up calls of
        // the same tool skip the permission gate for the rest of the run. Without
        // this arm the wire decision could not reach the runtime at all (the
        // wire enum previously had no `AllowSession` variant — OCEAN-74).
        PermissionDecisionBody::AllowSession => AgentPermissionDecision::AllowSession,
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
            // AllowSession is an allow (it permits the call to run) — only Deny
            // is a non-allow. Reporting AllowSession as `allowed: false` would
            // mislead clients into rendering an approved tool as blocked.
            allowed: matches!(
                agent_decision,
                AgentPermissionDecision::Allow | AgentPermissionDecision::AllowSession
            ),
            reason: match &agent_decision {
                AgentPermissionDecision::Allow => None,
                AgentPermissionDecision::AllowSession => Some("allow_session".into()),
                AgentPermissionDecision::Deny { reason } => Some(reason.clone()),
            },
        },
    );

    (
        StatusCode::OK,
        Json(PermissionControlResponse {
            ok: true,
            permission_id,
            message: "permission decision recorded and waiter released".into(),
        }),
    )
}

fn build_prompt_control(
    state: &AppState,
    request_id: RequestId,
    session_id: Option<SessionId>,
    allow_mutating: bool,
    cancel: CancellationToken,
    decision_token: Option<String>,
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
            decision_token,
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
            decision_token,
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
                    // OCEAN-185: bind this waiter to the submitter's per-turn
                    // secret. The decision POST must replay it (constant-time
                    // verify) or be rejected 403. NOT placed in `status`, so it
                    // never reaches the public /v1/events SSE below.
                    decision_token: self.decision_token.clone(),
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

/// Optional workspace scope for `GET /v1/agent/sessions/{id}`. The session LIST
/// path already scopes reads to a caller-declared workspace (OCEAN-52); the
/// DETAIL path did not, so a caller could read another workspace's session
/// transcript by id alone (OCEAN-74). When a scope is supplied here, the detail
/// handler rejects a cross-workspace read with the same 400 shape the turn path
/// uses. Absence of a scope preserves the legacy unscoped read for first-party
/// callers that don't declare a workspace.
#[derive(Debug, serde::Deserialize, Default)]
struct SessionDetailQuery {
    /// Exact workspace_root to scope the read to (wins over `cwd`).
    #[serde(default)]
    workspace: Option<String>,
    /// A cwd the daemon resolves to a workspace root (git toplevel) first.
    #[serde(default)]
    cwd: Option<String>,
}

impl SessionDetailQuery {
    /// The workspace root the caller is claiming to read from, if any. `None`
    /// means the caller declared no scope (legacy unscoped read).
    fn requested_workspace(&self, runtime: &AgentRuntime) -> Option<String> {
        if let Some(ws) = self.workspace.as_deref() {
            return Some(ws.to_string());
        }
        self.cwd.as_deref().map(|cwd| {
            runtime
                .workspace_root_for(std::path::Path::new(cwd))
                .to_string_lossy()
                .into_owned()
        })
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

// ---- YOLO setting (OCEAN-YOLO) ---------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct YoloSetRequest {
    /// The new persisted default. `true` opts into the permission-gating bypass
    /// (tools auto-approve); `false` restores gated/safe.
    enabled: bool,
}

/// `GET /v1/settings/yolo` — report the operator's persisted YOLO default and
/// the *effective* posture (after env override), so a client can show both
/// "your saved default" and "what's actually in force right now".
///
/// Mirrors `model_get`'s shape: `{ ok, persisted, effective, env_override }`.
/// `persisted` is the saved personal default (null on first run); `effective`
/// is what a turn would actually use via [`effective_yolo`]; `env_override`
/// flags when `OCEAN_YOLO` is masking the persisted value.
async fn yolo_setting_get() -> Json<serde_json::Value> {
    let persisted = ocean_agent::load_yolo_pref(&ocean_agent::config_dir_from_env());
    let env_override = yolo_env_pref();
    Json(json!({
        "ok": true,
        "persisted": persisted,
        "effective": effective_yolo(),
        "env_override": env_override,
    }))
}

/// `POST /v1/settings/yolo` — set + persist the operator's YOLO default. Writes
/// the preference under the config dir (same mechanism as the persisted model
/// selection) so it survives restarts. Mirrors `model_set`'s response shape and
/// returns the freshly resolved `effective` value so the caller sees whether an
/// env override is still masking their new default.
///
/// Persisting `enabled` does NOT weaken the permission decision-token binding
/// (OCEAN-185); it only sets the default for whether tools auto-approve.
async fn yolo_setting_set(Json(req): Json<YoloSetRequest>) -> Json<serde_json::Value> {
    let config_dir = ocean_agent::config_dir_from_env();
    ocean_agent::persist_yolo_pref(&config_dir, req.enabled);
    let env_override = yolo_env_pref();
    tracing::info!(persisted = req.enabled, ?env_override, "yolo default persisted");
    Json(json!({
        "ok": true,
        "persisted": req.enabled,
        "effective": effective_yolo(),
        "env_override": env_override,
    }))
}

/// Longhouse/council convene route group, factored out of `main()` so the live
/// router and the HTTP route test share one source of truth (OCEAN-227).
///
/// `POST /v1/council/convene` is a first-class **alias** of
/// `POST /v1/longhouse/convene`: "council" is the governance term for the same
/// convene/quorum flow, and `docs/LONGHOUSE.md` documents the council path as a
/// live route. Both names dispatch to the identical `longhouse_convene` handler
/// so a client following the canonical doc no longer 404s.
fn longhouse_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/longhouse/demo", post(longhouse_demo))
        .route("/v1/longhouse/convene", post(longhouse_convene))
        // Canonical-doc alias — same handler, governance-facing name (OCEAN-227).
        .route("/v1/council/convene", post(longhouse_convene))
        // Read-only pre-turn prep step — the "first safe integration slice"
        // (OCEAN-226). Advisory only; no gate, no side effect.
        .route("/v1/longhouse/prepare", post(longhouse_prepare))
        .route("/v1/longhouse/topics", get(longhouse_topics))
        .route("/v1/longhouse/topics/{topic_id}", get(longhouse_topic))
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

/// Request body for `POST /v1/longhouse/prepare`.
///
/// Mirrors the [`ocean_longhouse::TurnBrief`] the prep loop ranks against — the
/// daemon's own turn shape minus the heavy bits. Only `prompt` is required; the
/// rest scope the skill index (`cwd`) or are reserved for future SOP/workflow
/// selection. `top_n` overrides how many compact skill briefs come back.
#[derive(Debug, serde::Deserialize)]
struct LonghousePrepareRequest {
    /// The upcoming turn's prompt — the text Longhouse ranks skills against.
    prompt: String,
    /// Opaque daemon session id this turn belongs to (carried through to the
    /// brief; unused in v1 ranking).
    #[serde(default)]
    session_id: Option<String>,
    /// Working directory of the turn. When set, the skill index also scans the
    /// repo-local `./skills` dir under it (`SkillRoots::for_cwd`), on top of the
    /// documented home libraries.
    #[serde(default)]
    cwd: Option<String>,
    /// Which client is steering ("tui", "surface", "voice"). Reserved for future
    /// client-aware SOP reminders; unused in v1 ranking.
    #[serde(default)]
    client_type: Option<String>,
    /// Cap on how many compact skill briefs to return. Defaults to
    /// [`ocean_longhouse::DEFAULT_TOP_N`] when omitted.
    #[serde(default)]
    top_n: Option<usize>,
}

/// `POST /v1/longhouse/prepare` — the **read-only pre-turn preparation step**,
/// the "first safe integration slice" from `docs/LONGHOUSE.md` §"First safe
/// integration slice" (lines 101-115). This is the first real consumer of
/// [`ocean_longhouse::SkillIndex::prepare`] (OCEAN-226): the library capability
/// shipped in OCEAN-215 ph1 had no caller until now.
///
/// The daemon hands Longhouse a compact [`ocean_longhouse::TurnBrief`] (the
/// prompt + a little session context) and gets back a [`ocean_longhouse::TurnPrep`]:
/// the handful of skills (plus, in later phases, SOPs/workflows) most relevant to
/// that prompt, each as a *compact* brief — name + one-line when-to-use, never a
/// full body. A client may call this before submitting a turn and fold the briefs
/// into its own guidance.
///
/// **Advisory only — Longhouse recommends, it never acts (per the repo's Longhouse
/// rule + `docs/LONGHOUSE.md` line 115).** This endpoint performs no local side
/// effects, executes nothing, and touches no permission gate: it loads the skill
/// index off disk and ranks it. The returned `advisory: true` makes that contract
/// explicit on the wire. The main agent still routes every real action back
/// through the daemon's permission gates.
///
/// **Fail-open** (matches `prepare` itself): a missing/garbled skill library, an
/// empty index, or an irrelevant prompt yields `ok: true` with an empty `prep` —
/// it never errors, so consulting Longhouse can never block a would-be turn.
///
/// The disk scan runs on a blocking thread (`spawn_blocking`) so the index walk
/// never stalls the async scheduler; the cheap keyword ranking then runs inline.
async fn longhouse_prepare(
    Json(req): Json<LonghousePrepareRequest>,
) -> Json<serde_json::Value> {
    let brief = ocean_longhouse::TurnBrief {
        session_id: req.session_id.unwrap_or_default(),
        prompt: req.prompt,
        cwd: req.cwd.clone(),
        client_type: req.client_type,
    };
    let top_n = req.top_n;

    // Load the skill index + rank on a blocking thread: the loader walks
    // ~/.spawner/skills, ~/.codex/skills (+ repo-local ./skills when a cwd is
    // given), which is filesystem I/O we must not run on the async scheduler.
    // Both load and rank are fail-open, so a JoinError (the only way this can
    // fail) collapses to an empty prep — never a 500 — preserving the contract
    // that consulting Longhouse can't block a turn.
    let prep = tokio::task::spawn_blocking(move || {
        let roots = match brief.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => ocean_longhouse::SkillRoots::for_cwd(cwd),
            _ => ocean_longhouse::SkillRoots::default(),
        };
        let index = ocean_longhouse::SkillIndex::load_from(&roots);
        let skills_indexed = index.len();
        let prep = match top_n {
            Some(n) => index.prepare_top_n(&brief, n),
            None => index.prepare(&brief),
        };
        (prep, skills_indexed)
    })
    .await;

    let (prep, skills_indexed) = prep.unwrap_or_else(|err| {
        // spawn_blocking only errors if the closure panicked; the loader and
        // ranker don't panic, but stay fail-open here regardless.
        tracing::warn!(error = %err, "longhouse prepare task failed; returning empty prep");
        (ocean_longhouse::TurnPrep::default(), 0)
    });

    Json(json!({
        "ok": true,
        // Advisory contract: Longhouse only recommends. This endpoint executes
        // nothing and bypasses no permission gate.
        "advisory": true,
        // How many skills the index held this call (diagnostic: distinguishes
        // "no library on disk" from "library present, nothing matched").
        "skills_indexed": skills_indexed,
        "prep": prep,
    }))
}

/// Bridges ocean-call's orchestrator events onto the daemon EventBus, turning
/// each OceanEvent into an EventEnvelope on the real SSE rail — and, when given a
/// room store, *persisting* the call's transcript into a durable Room so it
/// survives daemon restarts and is queryable after the call ends (OCEAN-170).
///
/// The live SSE emit and the durable write are independent: a store failure is
/// logged but never blocks the bus emit, so a transient DB hiccup can't stall the
/// live feed the rail subscribers depend on. Persistence is opt-in via
/// [`BusSink::with_persistence`] so the demo path records a transcript while the
/// `place_call` lifecycle (which mints its own room separately) can use a
/// bus-only sink.
///
/// Cloneable so the per-call session task can own its own sink (OCEAN-CALL) — every
/// clone forwards to the same bus, and (since [`RoomStoreHandle`] is an
/// `Arc<Mutex<…>>`) shares the same durable room store behind the handle.
#[derive(Clone)]
struct BusSink {
    events: EventBus,
    /// When set, call events are mirrored into this durable room store under
    /// [`Self::room_key`]. `None` = bus-only (no persistence).
    rooms: Option<RoomStoreHandle>,
    /// The persistent room key (`call:<uuid>`) the transcript lands under. Filled
    /// from the first `CallStarted.room_id` so the sink writes to the same room the
    /// orchestrator announced. Empty until the call starts.
    room_key: String,
}

impl BusSink {
    /// A bus-only sink: forwards events onto the SSE rail, no persistence. Today
    /// only the tests exercise this (the live `place_call` path emits directly on
    /// `state.events`), so it's gated to test builds to keep the release binary
    /// warning-free; lift the gate when a production caller needs a non-persisting
    /// call sink.
    #[cfg(test)]
    fn bus_only(events: EventBus) -> Self {
        Self {
            events,
            rooms: None,
            room_key: String::new(),
        }
    }

    /// A sink that ALSO persists the call transcript into `rooms` (OCEAN-170).
    fn with_persistence(events: EventBus, rooms: RoomStoreHandle) -> Self {
        Self {
            events,
            rooms: Some(rooms),
            room_key: String::new(),
        }
    }

    /// Mirror a call event into the durable room store, if persistence is on.
    /// Best-effort: every failure is logged and swallowed so the live SSE emit is
    /// never blocked by the DB. Maps the call lifecycle onto room operations:
    ///   - `CallStarted`            → create the `call:<uuid>` room (key = room_id)
    ///   - `CallTranscriptSegment`  → append FINAL segments as chat messages
    ///                                 (author_id = speaker); interim segments are
    ///                                 skipped to avoid duplicate/revised noise
    ///   - `CallSummaryUpdated`     → append the rolling summary as a System message
    ///   - `CallEnded`              → close the room (freezes the transcript)
    fn persist(&mut self, event: &ocean_core::OceanEvent) {
        use ocean_core::OceanEvent::*;
        let Some(rooms) = self.rooms.clone() else {
            return;
        };
        let now = Utc::now();
        match event {
            CallStarted { room_id, .. } => {
                // Remember which room this call's transcript belongs to, then
                // create it. The orchestrator announces the room_id here; we mint
                // the durable Room under the same key so a later GET on
                // /v1/rooms/persistent/{room_id}/transcript reads it back.
                self.room_key = room_id.clone();
                let key = RoomKey::new(room_id.as_str());
                let res = with_rooms_handle(&rooms, |store| {
                    store.create(key, "Call transcript", None, now)
                });
                match res {
                    Ok(_) => {}
                    // A re-announced room (e.g. a webhook JoinCall after the demo
                    // already created it) is not an error for us — the transcript
                    // just keeps appending to the existing room.
                    Err(ocean_store::RoomStoreError::AlreadyExists(_)) => {}
                    Err(e) => tracing::warn!(room = %room_id, error = %e,
                        "call-transcript: failed to create persistent room"),
                }
            }
            CallTranscriptSegment {
                speaker,
                text,
                is_final,
                ..
            } => {
                // FINAL segments only: interim segments get revised by streaming
                // STT, so persisting them would write duplicate/contradictory rows.
                if !*is_final || self.room_key.is_empty() {
                    return;
                }
                let key = RoomKey::new(self.room_key.as_str());
                let res = with_rooms_handle(&rooms, |store| {
                    store.append_message(
                        &key,
                        speaker,
                        RoomParticipantKind::Human,
                        RoomMessageKind::Message,
                        text,
                        now,
                    )
                });
                if let Err(e) = res {
                    tracing::warn!(room = %self.room_key, error = %e,
                        "call-transcript: failed to append transcript segment");
                }
            }
            CallSummaryUpdated { summary, .. } => {
                if self.room_key.is_empty() {
                    return;
                }
                let key = RoomKey::new(self.room_key.as_str());
                let res = with_rooms_handle(&rooms, |store| {
                    store.append_message(
                        &key,
                        "ocean",
                        RoomParticipantKind::System,
                        RoomMessageKind::System,
                        summary,
                        now,
                    )
                });
                if let Err(e) = res {
                    tracing::warn!(room = %self.room_key, error = %e,
                        "call-transcript: failed to append summary");
                }
            }
            CallEnded { .. } => {
                if self.room_key.is_empty() {
                    return;
                }
                let key = RoomKey::new(self.room_key.as_str());
                let res = with_rooms_handle(&rooms, |store| store.close(&key));
                if let Err(e) = res {
                    tracing::warn!(room = %self.room_key, error = %e,
                        "call-transcript: failed to close room on call end");
                }
            }
            // Wake/spoke/task events are live-only signals, not transcript content.
            _ => {}
        }
    }
}

impl ocean_call::EventSink for BusSink {
    fn emit(&mut self, event: ocean_core::OceanEvent) {
        // Persist first (best-effort), then publish to the live rail. Ordering is
        // immaterial to correctness — the two paths are independent — but writing
        // before emit means a subscriber that immediately reads back the transcript
        // sees the row that triggered its notification.
        self.persist(&event);
        self.events.emit(ocean_core::EventEnvelope::new(event));
    }
}

/// Run a closure with a locked room store behind a [`RoomStoreHandle`], recovering
/// a poisoned lock the same way [`with_rooms`] does. Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock. Takes
/// the handle directly (rather than `&AppState`) so the call sink — which only
/// holds the `rooms` handle, not the whole state — can write through.
fn with_rooms_handle<T>(
    rooms: &RoomStoreHandle,
    f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T,
) -> T {
    let mut guard = match rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// The active lane's [`ocean_call::TurnRunner`]: runs one ephemeral agent turn
/// over a wake command and returns the assistant's reply text for TTS.
///
/// It drives the *same* `AgentRuntime` every other turn uses — so a call answer
/// is a real agent turn (tools, permissions, the operator's model) — but in its
/// own throwaway session per call (`call:<room>`), tagged `client_type =
/// "call-voice"`, so a call never pollutes a user's chat session. The reply is
/// `res.stdout`, the full assistant text the runtime already streamed.
///
/// Only constructed in the live (`livekit-tap`) build — the wake/answer lane is
/// part of the live audio loop — so the default build allows it as dead code.
#[cfg_attr(not(feature = "livekit-tap"), allow(dead_code))]
struct DaemonTurnRunner {
    state: AppState,
    /// Per-call session id, lazily created on the first answer so a call that
    /// never triggers the active lane never creates a session.
    session_id: Option<SessionId>,
    cwd: String,
    room_label: String,
}

#[cfg_attr(not(feature = "livekit-tap"), allow(dead_code))]
impl DaemonTurnRunner {
    fn new(state: AppState, room_label: String) -> Self {
        // Voice answers run in the daemon's own workspace — they read context
        // and talk, not edit a user's repo. `OCEAN_CALL_CWD` overrides.
        let cwd = std::env::var("OCEAN_CALL_CWD")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".to_string())
            });
        Self {
            state,
            session_id: None,
            cwd,
            room_label,
        }
    }
}

#[async_trait::async_trait]
impl ocean_call::TurnRunner for DaemonTurnRunner {
    async fn run(&mut self, command: &str) -> anyhow::Result<String> {
        let is_new = self.session_id.is_none();
        let session_id = *self.session_id.get_or_insert_with(SessionId::new_v4);
        let request_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        // Permission posture follows operator policy, same as any other turn.
        let yolo = effective_yolo();
        let control = build_prompt_control(
            &self.state,
            request_id,
            Some(session_id),
            yolo,
            cancel,
            None,
        );

        let prompt_req = PromptRequest {
            prompt: command.to_string(),
            images: None,
            request_id: Some(request_id),
            session_id: Some(session_id),
            create_if_missing: is_new,
            max_turns: None,
            yolo,
            cwd: self.cwd.clone(),
            project_id: None,
            client_type: Some("call-voice".to_string()),
            decision_token: None,
        };

        tracing::info!(
            room = %self.room_label,
            %session_id,
            "call active lane: running agent turn for wake answer"
        );
        let res = self.state.runtime.prompt(prompt_req, control).await;
        if res.ok {
            Ok(res.stdout)
        } else {
            anyhow::bail!("agent turn failed: {}", res.stderr)
        }
    }
}

/// Spawn the long-running per-call session task: audio tap → STT → orchestrator
/// → (wake) agent turn → TTS, with every event forwarded onto the SSE rail via
/// [`BusSink`] (so transcripts both stream and, once OCEAN-170 is in, persist).
///
/// This is the running wiring the call pipeline was missing. It is called from
/// [`call_place`] (outbound) and [`call_webhook`] (inbound) right after the room
/// exists. The heavy *live* leg — joining the LiveKit room and pumping native
/// WebRTC PCM — is compiled only under the `livekit-tap` feature; the default
/// daemon build keeps this function present but inert (it logs that the live tap
/// isn't compiled in), so `cargo build -p ocean-daemon` never needs native libs.
///
/// Activation in the live build is further gated on `LIVEKIT_URL` + a join token
/// (minted via the existing `ocean_call::token`) and `XAI_API_KEY` for STT; when
/// those aren't set it logs and returns without spawning, exactly like the dial
/// path returns 503 — the only thing between here and a live call is the creds.
fn spawn_call_session(state: &AppState, room: &str, participants: Vec<String>) {
    let _ = participants; // used by the live arm below
    #[cfg(not(feature = "livekit-tap"))]
    {
        let _ = (state, room);
        tracing::info!(
            room = %room,
            "call-session task not spawned: live LiveKit tap requires the \
             `livekit-tap` feature (native WebRTC). Lifecycle + scripted demo \
             paths still emit on /v1/events; build with --features livekit-tap \
             and set LIVEKIT_*/XAI_API_KEY to activate the live audio loop."
        );
    }

    #[cfg(feature = "livekit-tap")]
    {
        use ocean_call::session_task::live::{default_tts_synth, LiveKitFrameSource, LiveKitVoice};
        use ocean_call::{
            run_call_session, CallSession, Summarizer, SummaryPolicy, UtterancePolicy, WakeGate,
        };

        // Live activation needs LiveKit creds (to mint a tap token) + an STT key.
        let token_config = match ocean_call::LiveKitTokenConfig::from_env() {
            Ok(c) => c,
            Err(missing) => {
                tracing::info!(
                    room = %room,
                    missing = %missing,
                    "call-session task not spawned: LiveKit not configured"
                );
                return;
            }
        };
        let xai_key = match std::env::var("XAI_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k,
            _ => {
                tracing::info!(
                    room = %room,
                    "call-session task not spawned: XAI_API_KEY unset (no STT)"
                );
                return;
            }
        };

        // Mint a publish-capable join token for the server tap so the active
        // lane can also speak. The passive transcript lane only needs subscribe.
        let token_req = ocean_call::LiveKitTokenRequest {
            surface_id: ocean_call::room_tap::ROOM_TAP_IDENTITY.to_string(),
            participant_id: ocean_call::room_tap::ROOM_TAP_IDENTITY.to_string(),
            display_name: "Ocean".to_string(),
            can_publish: true,
            can_subscribe: true,
        };
        // This is an IN-PROCESS server lane (the daemon joining its own call room
        // to publish Ocean's TTS). It is entitled by construction — it never
        // crosses the HTTP trust boundary — so it explicitly asks for publish
        // (OCEAN-220). The wire authz gate lives on `room_livekit_token`.
        let token =
            match ocean_call::mint_join_token(&token_config, room, &token_req, ocean_call::PublishGrant::Allow) {
                Ok(t) => t.token,
                Err(e) => {
                    tracing::warn!(room = %room, error = %e, "call-session token mint failed");
                    return;
                }
            };
        let url = token_config.url.clone();

        // Persistence-enabled sink: a live call's transcript both streams onto the
        // SSE rail AND lands in a durable `call:<room>` room (OCEAN-170), so it
        // survives a daemon restart and is queryable after the call ends — same as
        // the demo path. `persist` keys off the orchestrator's `CallStarted.room_id`
        // and creates the room on the fly (an already-existing room is fine), so
        // this is safe whether or not the room was minted elsewhere first.
        let sink = BusSink::with_persistence(state.events.clone(), state.rooms.clone());
        let runner = DaemonTurnRunner::new(state.clone(), room.to_string());
        // The active lane speaks via xAI TTS too (same key as STT). Keep a copy
        // before the transcriber takes ownership of the key.
        let tts_key = xai_key.clone();
        let transcriber = ocean_call::session_task::live::XaiTranscriber::new(xai_key);
        let session = CallSession::new(
            room.to_string(),
            Summarizer::new(SummaryPolicy::default()),
            // Wake active by default; an `OCEAN_CALL_MUTED=1` keeps a sensitive
            // call passive-only.
            WakeGate::new(call_voice_muted(), 2_000),
        );
        let room_owned = room.to_string();

        tokio::spawn(async move {
            let (source, lk_room) = match LiveKitFrameSource::connect(&url, &token).await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(room = %room_owned, error = %e, "call tap connect failed");
                    // Close the lifecycle so no phantom in-progress call lingers.
                    state_emit_call_ended(&sink, &room_owned);
                    return;
                }
            };
            // Active-lane voice: real xAI TTS when built with `xai-tts`, else a
            // silence fallback (the publish path still runs and CallAgentSpoke
            // still fires). See ocean_call::tts_xai.
            let voice = LiveKitVoice::new(lk_room, default_tts_synth(&tts_key));
            run_call_session(
                session,
                source,
                transcriber,
                runner,
                voice,
                sink,
                room_owned,
                participants,
                UtterancePolicy::default(),
                || ocean_protocol::now_ms() as u64,
            )
            .await;
        });
    }
}

/// Emit a `CallEnded` for `room` through a [`BusSink`], so a failed live connect
/// still closes the call lifecycle (no phantom "in progress" call).
#[cfg(feature = "livekit-tap")]
fn state_emit_call_ended(sink: &BusSink, room: &str) {
    use ocean_call::EventSink;
    let mut sink = sink.clone();
    sink.emit(ocean_core::OceanEvent::CallEnded {
        call_id: room.to_string(),
        duration_ms: 0,
    });
}

/// Whether the call active lane is muted (passive-only). `OCEAN_CALL_MUTED=1`
/// (or `true`) keeps a sensitive call transcript-only — Ocean never speaks.
#[cfg(feature = "livekit-tap")]
fn call_voice_muted() -> bool {
    matches!(
        std::env::var("OCEAN_CALL_MUTED").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Demo: run the ocean-call orchestrator over a scripted transcript and emit
/// the real call events (CallStarted/Transcript/Task/Summary/Ended) onto the
/// SSE rail. Proves the daemon→orchestrator→EventBus path end to end WITHOUT
/// any Twilio/LiveKit account — the live `place_call` path is gated on those.
async fn call_demo(State(state): State<AppState>) -> Json<serde_json::Value> {
    use ocean_call::{
        CallSession, EventSink, Summarizer, SummaryPolicy, TranscriptSegment, WakeGate,
    };

    // Mint a unique `call:<uuid>` room so each demo run produces a fresh,
    // independently-queryable transcript (a fixed key would collide on the second
    // run). The same key is announced via CallStarted and returned below so the
    // caller can read it back at GET /v1/rooms/persistent/{room}/transcript.
    let room = format!("call:{}", Uuid::new_v4());
    // Persistence-enabled sink: events both stream onto the SSE rail AND land in
    // the durable room store (OCEAN-170), so the demo transcript survives a
    // restart with no LiveKit/Twilio account in the loop.
    let mut sink = BusSink::with_persistence(state.events.clone(), state.rooms.clone());
    let mut session = CallSession::new(
        format!("demo-{}", Uuid::new_v4()),
        Summarizer::new(SummaryPolicy {
            every_n_segments: 3,
            silence_ms: 15_000,
        }),
        WakeGate::new(false, 2_000),
    );

    session.start(&mut sink, &room, vec!["sip:+17035081859".into()]);
    let script = [
        ("caller", "hey thanks for jumping on", 0u64),
        ("caller", "so for the Warner Q3 push", 2_000),
        ("caller", "I'll send the master to Atlantic tonight", 4_000),
        ("caller", "and we need to verify the toll-free number by Friday", 7_000),
        ("caller", "hey Ocean what did we just agree to", 10_000),
    ];
    for (speaker, text, ms) in script {
        let outcome = session.on_segment(TranscriptSegment::final_(speaker, text, ms), ms, &mut sink);
        // Offline demo path: there's no agent runtime here to run the real summary
        // turn (that lives in the live `run_call_session` loop), so the debounced
        // raw transcript is emitted directly as the summary to exercise the
        // persistence path. The live call path produces an LLM summary instead.
        if let Some(transcript) = outcome.summary_due {
            sink.emit(OceanEvent::CallSummaryUpdated {
                summary: transcript,
                as_of_ms: ms,
            });
        }
    }
    session.end(&mut sink, 12_000);

    Json(json!({
        "ok": true,
        "room": room,
        "streaming_on": "/v1/events",
        "transcript_at": format!("/v1/rooms/persistent/{room}/transcript"),
    }))
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
        Ok(call) => {
            // Dial accepted — spawn the running call-session task so audio →
            // STT → orchestrator → TTS starts the moment the room has media.
            // (Inert without the `livekit-tap` feature / live creds; logs why.)
            spawn_call_session(&state, &call.room, vec![format!("sip:{dialed}")]);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "dialed": call.dialed,
                    "room": call.room,
                    "participant_id": call.participant_id,
                    "streaming_on": "/v1/events"
                })),
            )
        }
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

/// Map a decided [`ocean_call::WebhookAction`] to the lifecycle event (if any)
/// the daemon must emit. Split out from the handler so the mapping is unit
/// testable without LiveKit signatures or `AppState`:
///   - `JoinCall`  → `CallStarted` (room appeared; pipeline should attach)
///   - `EndCall`   → `CallEnded`   (room finished — clean hangup, crash, or
///                                   partition — so the call lifecycle MUST
///                                   close or the TUI/surface shows a phantom
///                                   "in progress" call forever; OCEAN-207)
///   - `Ignore`    → `None`        (non-call room / non-lifecycle event)
///
/// The room name is threaded through as the `call_id` so subscribers correlate
/// the end with the start. Emitting `CallEnded` for a `call_*`/`call:` room that
/// ended is always correct and idempotent; non-call rooms never reach here
/// because `decide` already returns `Ignore` for them.
fn webhook_action_to_event(action: ocean_call::WebhookAction) -> Option<ocean_core::OceanEvent> {
    match action {
        ocean_call::WebhookAction::JoinCall { room } => Some(ocean_core::OceanEvent::CallStarted {
            call_id: room.clone(),
            room_id: room,
            participants: vec![],
        }),
        ocean_call::WebhookAction::EndCall { room } => Some(ocean_core::OceanEvent::CallEnded {
            call_id: room,
            duration_ms: 0,
        }),
        ocean_call::WebhookAction::Ignore => None,
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
        Ok(action @ ocean_call::WebhookAction::JoinCall { .. }) => {
            let room = match &action {
                ocean_call::WebhookAction::JoinCall { room } => room.clone(),
                _ => unreachable!(),
            };
            if let Some(event) = webhook_action_to_event(action) {
                state.events.emit(ocean_core::EventEnvelope::new(event));
            }
            tracing::info!(%room, "inbound call room started");
            // Inbound call: a real caller SIP-routed into this room. Spawn the
            // running session task so Ocean joins, transcribes, and (on wake)
            // answers. Inert without the `livekit-tap` feature / live creds.
            spawn_call_session(&state, &room, vec![]);
            (StatusCode::OK, Json(json!({ "ok": true, "action": "join", "room": room })))
        }
        Ok(action @ ocean_call::WebhookAction::EndCall { .. }) => {
            let room = match &action {
                ocean_call::WebhookAction::EndCall { room } => room.clone(),
                _ => unreachable!(),
            };
            if let Some(event) = webhook_action_to_event(action) {
                state.events.emit(ocean_core::EventEnvelope::new(event));
            }
            tracing::info!(%room, "call room finished — emitting CallEnded");
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

/// Env var holding the operator's LiveKit **publish** capability secret
/// (OCEAN-220, P0). A request to `POST /v1/rooms/{room_id}/livekit-token` only
/// receives a publish-capable token if it presents this exact value (bearer
/// `Authorization` or `x-ocean-publish-token`); otherwise it gets a
/// subscribe/listen-only token. Unset ⇒ NO HTTP caller can publish (fail-closed)
/// — the in-process call lane is unaffected because it never hits this route.
const PUBLISH_TOKEN_ENV: &str = "OCEAN_LIVEKIT_PUBLISH_TOKEN";

/// The `call:` key prefix marks a server-authored call/meeting room: created by
/// the call lifecycle (`CallStarted`) or the inbound webhook (`JoinCall`), never
/// by a wire client. These are the sensitive rooms a live call runs in, so the
/// token route refuses to mint for a `call:` room the server didn't author
/// (OCEAN-220).
const CALL_ROOM_PREFIX: &str = "call:";

/// Decide whether this request is entitled to a PUBLISH grant (OCEAN-220, P0).
///
/// Publish = the right to inject audio/video into the room, so it is gated on
/// proof the caller is the operator: a server-side secret (`OCEAN_LIVEKIT_PUBLISH_TOKEN`)
/// presented as `Authorization: Bearer <token>` or `x-ocean-publish-token: <token>`,
/// compared in constant time (the same primitive OCEAN-185 uses for permission
/// decisions). Default-deny: no env configured, or a missing/wrong header, and
/// the caller gets [`ocean_call::PublishGrant::Deny`] (a listen-only token).
///
/// This is the OCEAN-160 move applied to publish: the wire `can_publish` flag is
/// inert; the capability is resolved purely from operator policy here.
fn resolve_publish_grant(headers: &HeaderMap) -> ocean_call::PublishGrant {
    // No operator secret configured → no HTTP caller may publish. Fail-closed.
    let Some(expected) = std::env::var(PUBLISH_TOKEN_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return ocean_call::PublishGrant::Deny;
    };

    let presented = headers
        .get("x-ocean-publish-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // `Authorization: Bearer <token>` form.
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });

    if ocean_core::decision_token_matches(Some(expected.as_str()), presented) {
        ocean_call::PublishGrant::Allow
    } else {
        // Secret is set but the caller didn't present a matching one: deny
        // publish (still allowed to subscribe). Never log the token value.
        if presented.is_some() {
            tracing::warn!(
                "livekit-token: publish denied — invalid publish capability token (OCEAN-220)"
            );
        }
        ocean_call::PublishGrant::Deny
    }
}

/// Whether a token may be minted for `room_id` given the room store (OCEAN-220,
/// P0 — gate 1, the existence check).
///
/// - A `call:` room is server-authored (created only by the call lifecycle /
///   inbound webhook). We mint ONLY if such a room currently EXISTS and is OPEN
///   in the store — a closed or never-created call room is refused, so a caller
///   cannot fabricate a token for an arbitrary in-progress call id.
/// - Any other room id (the operator's own `project:`/surface spaces, opened
///   ad-hoc by the local surface and created lazily by LiveKit on first join) is
///   allowed through this gate — existence-gating them would break the legitimate
///   "open a fresh surface room" flow, and they are not call-eavesdrop targets.
///   Publish into them is still independently gated by [`resolve_publish_grant`].
fn call_room_token_allowed(store: &ocean_store::SqliteRoomStore, room_id: &str) -> bool {
    if !room_id.starts_with(CALL_ROOM_PREFIX) {
        return true;
    }
    let key = RoomKey::new(room_id);
    matches!(store.get(&key), Ok(Some(_)))
}

/// `POST /v1/rooms/{room_id}/livekit-token` — mint a LiveKit join token for a
/// room (OCEAN-137), AUTHORIZED (OCEAN-220, P0).
///
/// This is the path the ocean-surface proxy and web surface call to get a JWT
/// for the `livekit-client` SDK. Reuses the LiveKit credentials + token signing
/// in `ocean_call::token` (just the three LiveKit auth vars; no Twilio SIP trunk
/// needed). If those aren't configured, returns a clean 503 the surface renders
/// as a degraded error — never a 404. The response is `{ ok, url, token, room }`,
/// the shape the web bridge decodes.
///
/// ## Authorization (OCEAN-220, P0)
///
/// The original route signed a 6-hour `room_join` JWT for ANY caller-supplied
/// `room_id`, with client-controlled `can_publish`/identity, with ZERO check
/// that the requester was entitled to that room. Any local process (CORS does
/// not gate non-browser clients; `OCEAN_BIND` can expose off-loopback) could
/// mint publish credentials into an in-progress call. Two server-side gates
/// close that, matching how the rest of the daemon does authz:
///
/// 1. **Existence-gate for `call:` rooms.** A `call:` room is server-authored
///    (the call lifecycle / inbound webhook create it). We refuse to mint for a
///    `call:` room that does not EXIST and is not OPEN in the room store (404,
///    the same `UnknownRoom` shape the other room routes use). So a caller can
///    no longer fabricate a token for an arbitrary live-call room id. Non-`call:`
///    rooms (the operator's own `project:`/surface spaces, opened ad-hoc by the
///    local surface) are not existence-gated — that would break the legitimate
///    "open a fresh surface room" flow, and they are not call-eavesdrop targets.
///
/// 2. **Publish is server-derived, never wire-trusted.** `req.can_publish` is
///    ignored (OCEAN-160 pattern). A token is publish-capable ONLY if the caller
///    proves operator entitlement via `OCEAN_LIVEKIT_PUBLISH_TOKEN` (see
///    [`resolve_publish_grant`]); otherwise it is subscribe/listen-only. So an
///    unauthorized caller, even for a room it is allowed to observe, can never
///    inject media.
///
/// This establishes: "no HTTP caller mints publish creds for any room without
/// the operator secret, and none mints any token for an unknown call room." It
/// is intentionally NOT full per-identity room membership (the route carries no
/// authenticated session to bind to, unlike `agent_turn`); that is the documented
/// next step. The in-process call lane is unaffected (it never hits this route).
async fn room_livekit_token(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ocean_call::LiveKitTokenRequest>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let room_id_trimmed = room_id.trim();
    if room_id_trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "room_id is empty" })),
        );
    }

    // OCEAN-220 gate 1: a `call:` room must be one the SERVER authored and that
    // is still open. Minting for an unknown/closed call room is refused (404),
    // so a caller cannot get any token for an arbitrary in-progress call id.
    let call_room_known =
        with_rooms(&state, |store| call_room_token_allowed(store, room_id_trimmed));
    if !call_room_known {
        tracing::warn!(
            room = %room_id_trimmed,
            "rejected livekit-token: unknown/closed call room (OCEAN-220)"
        );
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": format!("no open room '{room_id_trimmed}'"),
            })),
        );
    }

    // A missing/empty body is fine — identity falls back, subscribe defaults on.
    let req = body.map(|Json(r)| r).unwrap_or_default();

    // OCEAN-220 gate 2: publish is decided HERE from operator policy, never from
    // the wire `req.can_publish`.
    let publish = resolve_publish_grant(&headers);

    let config = match ocean_call::LiveKitTokenConfig::from_env() {
        Ok(c) => c,
        Err(missing) => {
            // Creds aren't provisioned — not a code failure. Clean 503 with a
            // typed error the surface already handles (OCEAN-123), not a 404.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": "LiveKit not configured",
                    "blocked_on": "livekit not configured",
                    "missing": missing,
                    "needed_env": ["LIVEKIT_URL", "LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"],
                })),
            );
        }
    };

    match ocean_call::mint_join_token(&config, room_id_trimmed, &req, publish) {
        Ok(resp) => (
            StatusCode::OK,
            Json(serde_json::to_value(resp).unwrap_or_else(|_| {
                json!({ "ok": false, "error": "failed to encode token response" })
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e })),
        ),
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

/// Where the persistent-rooms SQLite DB lives. `OCEAN_DB_PATH` overrides the
/// whole path; otherwise it is `rooms.db` under the agent's config dir
/// (`ocean_agent::config_dir_from_env`), so the DB sits next to sessions and
/// projects under one config directory.
fn room_db_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("OCEAN_DB_PATH") {
        return std::path::PathBuf::from(p);
    }
    ocean_agent::config_dir_from_env().join("rooms.db")
}

/// Run a closure with the locked room store, recovering a poisoned lock the same
/// way the longhouse handlers do (`into_inner`). Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock.
fn with_rooms<T>(state: &AppState, f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T) -> T {
    let mut guard = match state.rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Map a store error onto an HTTP status + typed JSON body.
fn room_store_error_response(
    err: ocean_store::RoomStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_store::RoomStoreError::*;
    let status = match &err {
        BadKey(_) => StatusCode::BAD_REQUEST,
        UnknownRoom(_) | UnknownParticipant { .. } => StatusCode::NOT_FOUND,
        AlreadyExists(_) => StatusCode::CONFLICT,
        // A durable backend can fail on I/O or (de)serialization, which the
        // in-memory registry never could. Surface those as 500s, not as a
        // misleading 4xx.
        Db(_) | Encode(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
        reg.create(key, &req.name, req.trigger_policy, Utc::now())
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
async fn rooms_list_persistent(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match with_rooms(&state, |reg| reg.list()) {
        Ok(rooms) => (StatusCode::OK, Json(json!({ "ok": true, "rooms": rooms }))),
        Err(e) => room_store_error_response(e),
    }
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
        Ok(Some(rec)) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "room": rec.room, "transcript": rec.transcript })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no room with key '{key}'") })),
        ),
        Err(e) => room_store_error_response(e),
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
/// the body. On a positive decision that resolves to an agent participant, emit a
/// `room_trigger` notice onto the agent event bus AND queue a real agent turn for
/// that agent (it reads the room context and posts its reply back into the
/// transcript). See `spawn_room_agent_turn` for the turn path (OCEAN-111/225).
async fn room_post_message(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<RoomMessageRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    // Append the message, then read back the policy AND the participant roster
    // in the same lock acquisition — we need the roster to resolve a mentioned
    // id to a runnable agent participant. The std mutex guard is dropped when
    // `with_rooms` returns; it is never held across an `.await`.
    let append = with_rooms(&state, |reg| {
        let msg = reg.append_message(
            &key,
            &req.author_id,
            req.author_kind,
            RoomMessageKind::Message,
            &req.body,
            Utc::now(),
        )?;
        let policy = reg.trigger_policy(&key)?;
        let roster = reg
            .get(&key)?
            .map(|rec| rec.room.participants)
            .unwrap_or_default();
        Ok::<_, ocean_store::RoomStoreError>((msg, policy, roster))
    });

    let (msg, policy, roster) = match append {
        Ok((msg, policy, roster)) => (msg, policy, roster),
        Err(e) => return room_store_error_response(e),
    };

    // ---- Auto-convene wiring point (OCEAN-65 / OCEAN-111) -------------------
    //
    // Parse @-mentions from the message body, evaluate each against the room's
    // trigger policy, and for every positive decision that resolves to an AGENT
    // participant in the roster: (a) emit the `room_trigger` notice + an audit
    // line (the observable contract, unchanged), and (b) ACTUALLY queue an
    // agent turn that wakes the agent, gives it the room context, and posts its
    // reply back into the transcript.
    //
    // Anti-loop guardrail #1 (the cheap, total one): an agent's OWN posted
    // reply is authored as `RoomParticipantKind::Agent`, and we never evaluate
    // triggers on agent-authored messages. So an agent that @-mentions another
    // agent (or itself) in its reply can never ping-pong the room. Only
    // human/bot/system-authored lines can convene an agent.
    let mut fired = Vec::new();
    if !matches!(req.author_kind, RoomParticipantKind::Agent) {
        for participant_id in parse_mentions(&req.body) {
            let decision = evaluate_trigger_policy(
                policy.as_ref(),
                &RoomTriggerEvent::Mention {
                    participant_id: participant_id.clone(),
                },
            );
            if !decision.should_convene {
                continue;
            }

            // Resolve the target participant id → an AGENT participant in the
            // roster BEFORE writing any convene footprint. Only genuine `Agent`
            // participants are runnable; a mention of a human/bot/tool id (or an
            // unknown id) resolves to `None`. The policy may say "convene", but
            // if there's no agent to wake then no convene actually happens — so
            // neither the `room_trigger` event nor the `auto-convene:` transcript
            // line may fire (OCEAN-128: writing the audit line for a non-agent
            // mention claimed a convene that never occurred).
            let resolved_agent = decision
                .target_participant
                .as_deref()
                .and_then(|id| resolve_agent_participant(&roster, id));

            // `triggers_fired` reflects raw policy evaluation; record it even
            // when the mention is a non-agent so the response is honest about
            // what the policy matched. The convene FOOTPRINT (event + audit line
            // + queued turn) below is gated on an actually-resolved agent.
            fired.push(decision.clone());

            let Some(agent) = resolved_agent else {
                continue;
            };

            // Emit a notice onto the agent event bus so any subscriber sees the
            // convene. Uses the generic Extension event so it never collides
            // with the Track-0/longhouse event scoping rules.
            state.agent_events.emit(AgentTurnEvent::Extension {
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

            // Audit line inside the room — only written now that an Agent has
            // actually been resolved and is about to be convened.
            let _ = with_rooms(&state, |reg| {
                reg.append_message(
                    &key,
                    "system",
                    RoomParticipantKind::System,
                    RoomMessageKind::System,
                    &format!(
                        "auto-convene: {} ({})",
                        decision.target_participant.clone().unwrap_or_default(),
                        decision.reason
                    ),
                    Utc::now(),
                )
            });

            spawn_room_agent_turn(state.clone(), key.clone(), agent, msg.seq);
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

// ---- Auto-convene: participant→session resolution + turn queueing (OCEAN-111)

/// Fixed namespace for deriving a stable per-(room, agent) session id with UUID
/// v5. Same room + same agent participant ⇒ same session every time, so the
/// agent RESUMES its room transcript across mentions instead of forking a fresh
/// session on every wake. The constant itself is arbitrary but must never
/// change, or existing room-agent sessions would orphan.
const ROOM_AGENT_SESSION_NS: Uuid = Uuid::from_u128(0x0ce1_a111_0000_4780_8000_526f_6f6d_4147);

/// How many recent transcript lines to feed the woken agent as context. Enough
/// to ground the reply in the conversation without bloating the prompt.
const ROOM_CONTEXT_TAIL: usize = 20;

/// Resolve a mentioned participant id to a runnable AGENT participant. Returns
/// the participant only when it exists in the roster AND is of kind `Agent` —
/// a mention of a human/bot/tool/system id (or an unknown id) resolves to
/// `None`, so the notice still fires but no turn is queued.
fn resolve_agent_participant(
    roster: &[RoomParticipant],
    participant_id: &str,
) -> Option<RoomParticipant> {
    roster
        .iter()
        .find(|p| p.id == participant_id && matches!(p.kind, RoomParticipantKind::Agent))
        .cloned()
}

/// Deterministic session id for a (room, agent-participant) pair. Stable across
/// daemon restarts and repeated mentions so the agent keeps one durable
/// transcript per room.
fn room_agent_session_id(room: &RoomKey, participant_id: &str) -> AgentSessionId {
    let seed = format!("{}:{}", room.as_str(), participant_id);
    sdk_sid(Uuid::new_v5(&ROOM_AGENT_SESSION_NS, seed.as_bytes()))
}

/// Build the prompt handed to a woken agent: a framing header that tells it it's
/// answering a mention in a room, the recent transcript as context, and a
/// pointer at the triggering line. `tail` is oldest→newest.
fn build_room_prompt(
    room: &RoomKey,
    agent: &RoomParticipant,
    tail: &[ocean_core::RoomMessage],
    triggered_by_seq: u64,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "You are \"{}\" (participant id `{}`), an agent in the Ocean room \"{}\". \
You were just @-mentioned. Read the recent transcript below and reply directly \
to the mention. Your reply will be posted back into the room for everyone to \
see, so address the room — do not narrate that you are an agent or that you \
were mentioned.\n\n",
        agent.display_name,
        agent.id,
        room.as_str(),
    ));
    out.push_str("--- recent room transcript ---\n");
    for m in tail {
        let marker = if m.seq == triggered_by_seq { "  «— mention" } else { "" };
        out.push_str(&format!(
            "[#{seq}] {author}: {body}{marker}\n",
            seq = m.seq,
            author = m.author_id,
            body = m.body,
            marker = marker,
        ));
    }
    out.push_str("--- end transcript ---\n\nYour reply:");
    out
}

/// Queue an agent turn in response to a room mention, run it asynchronously, and
/// post the reply back into the room. The room store mutex is NEVER held across
/// the await: every store touch goes through `with_rooms`, whose std guard is
/// dropped synchronously before `runtime.prompt(...).await`.
///
/// Anti-loop guardrail #2: the reply is posted with `author_kind = Agent`, and
/// `room_post_message` refuses to evaluate triggers on agent-authored messages,
/// so a reply can never re-convene anyone.
fn spawn_room_agent_turn(
    state: AppState,
    room: RoomKey,
    agent: RoomParticipant,
    triggered_by_seq: u64,
) {
    tokio::spawn(async move {
        // Resolve a working directory for the turn. A `Room` carries no
        // `workspace_root` of its own (see `ocean_core::Room`), so a room-bound
        // agent has no project to bind to from the room side — we fall back to
        // the daemon's launch dir, a sensible default that always exists, and
        // key the session by room+agent. (Sessions that DO land in a project's
        // workspace are still associated back to that project on read, via
        // `find_by_workspace` in `enrich_session_detail` — OCEAN-228. Giving
        // rooms their own workspace binding so room turns inherit a project is
        // the remaining follow-up.)
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());

        let session_id = room_agent_session_id(&room, &agent.id);

        // Read the recent transcript tail (read-before-answer context). Lock is
        // dropped when `with_rooms` returns, before any await below.
        let tail = with_rooms(&state, |reg| reg.transcript(&room, None))
            .unwrap_or_default();
        let tail: Vec<_> = tail
            .into_iter()
            .rev()
            .take(ROOM_CONTEXT_TAIL)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let prompt = build_room_prompt(&room, &agent, &tail, triggered_by_seq);

        // Does this session already exist on disk? If so we RESUME it (strict);
        // otherwise we create it under the deterministic id. This mirrors the
        // create-if-missing logic in `agent_turn`. `session_detail` errors on a
        // missing/corrupt session, so `Ok` ⇒ exists ⇒ resume.
        let is_new = state
            .runtime
            .session_detail(core_sid(session_id))
            .is_err();

        let request_id = Uuid::new_v4();
        // Auto-convene has no per-request flag, so the effective posture is the
        // operator's resolved default: env → persisted setting → off.
        let yolo = effective_yolo();
        let mut prompt_req = PromptRequest {
            prompt,
            images: None,
            request_id: Some(request_id),
            session_id: Some(core_sid(session_id)),
            create_if_missing: is_new,
            max_turns: None,
            yolo,
            cwd,
            project_id: None,
            client_type: Some("room".to_string()),
            // Daemon-internal auto-convene: no external submitter, so no
            // decision_token. Permission gating here defers to OCEAN_YOLO.
            decision_token: None,
        };

        let (_request_id, cancel) = register_running_request(
            &state,
            &mut prompt_req,
            format!("auto-convene: {} in room {}", agent.id, room.as_str()),
            RequestState::Running,
        )
        .await;

        let control = build_prompt_control(
            &state,
            request_id,
            Some(core_sid(session_id)),
            yolo,
            cancel,
            None,
        );

        let res = state.runtime.prompt(prompt_req, control).await;
        record_prompt_result(&state, request_id, &res).await;

        // Post the agent's reply back into the room as the agent participant.
        // The lock is taken synchronously here, after the await completed.
        if res.ok {
            let body = res.stdout.trim();
            if !body.is_empty() {
                let _ = with_rooms(&state, |reg| {
                    reg.append_message(
                        &room,
                        &agent.id,
                        RoomParticipantKind::Agent,
                        RoomMessageKind::Message,
                        body,
                        Utc::now(),
                    )
                });
            }
        } else {
            // Surface a failed convene as a system audit line so the room shows
            // the agent was woken but could not answer (e.g. no provider key).
            let _ = with_rooms(&state, |reg| {
                reg.append_message(
                    &room,
                    "system",
                    RoomParticipantKind::System,
                    RoomMessageKind::System,
                    &format!(
                        "auto-convene failed for {}: {}",
                        agent.id,
                        res.stderr.lines().next().unwrap_or("turn failed")
                    ),
                    Utc::now(),
                )
            });
        }
    });
}

#[derive(serde::Deserialize)]
struct TranscriptQuery {
    /// If set, return only entries with `seq > after_seq` (live-tail).
    #[serde(default)]
    after_seq: Option<u64>,
}

/// `GET /v1/rooms/persistent/{key}/transcript` — read a room's transcript,
/// optionally only entries after a given seq.
///
/// Falls back to the audit (soft-closed) view when the room is closed: a finished
/// call closes its room on `CallEnded` (OCEAN-170), but its transcript must stay
/// queryable afterwards — that frozen record is the whole reason it was persisted.
/// The `after_seq` tail filter is applied in-handler for that fallback path since
/// the audit getter returns the full transcript.
async fn room_transcript(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |reg| match reg.transcript(&key, q.after_seq) {
        // Open room (the live case): serve it directly.
        Ok(transcript) => Ok(transcript),
        // Closed room: a finished call's frozen transcript. Read the audit view
        // and apply the same `after_seq` tail filter the open path would.
        Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            match reg.get_including_closed(&key) {
                Ok(Some(rec)) => Ok(rec
                    .transcript
                    .into_iter()
                    .filter(|m| q.after_seq.map_or(true, |after| m.seq > after))
                    .collect()),
                // Genuinely no such room (never created): preserve the 404.
                Ok(None) => Err(ocean_store::RoomStoreError::UnknownRoom(key.clone())),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    });
    match result {
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

    // Session→project binding (OCEAN-228): map the session's bound
    // `workspace_root` back to the project that claims that directory, so a
    // client viewing this session sees the project it belongs to. This is the
    // reverse of the project→sessions map `GET /v1/projects/{id}` already uses;
    // resolving it on read (rather than storing a project id on the session)
    // means a renamed/rebound project is always reflected without rewriting
    // session files. A project-less workspace simply leaves this `None`; a
    // lookup error is logged and treated as "no project" so it can never fail a
    // session read.
    session.owning_project = session.workspace_root.as_deref().and_then(|root| {
        match state.runtime.project_for_workspace(root) {
            Ok(found) => found.map(ProjectRef::from),
            Err(error) => {
                tracing::warn!(%error, workspace_root = root, "owning-project lookup failed");
                None
            }
        }
    });
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

/// The active (in-flight) turn for a session, if any.
///
/// Single source of truth shared by the session LIST and DETAIL endpoints so
/// the two can't drift (OCEAN-205). A session's "active turn" is the request
/// the runtime is currently driving for it: the first non-terminal request
/// keyed to that session in the live request registry. Its `request_id` is a
/// stable id (unlike the ephemeral ids `turns_from_detail` mints per response),
/// so it's a meaningful handle a client can correlate against `/v1/requests`.
///
/// Returns `None` when the session has no live request — i.e. all its turns are
/// finished (or it has never run). Driven off the in-memory registry, this is a
/// cheap status peek: it never loads a transcript, so the LIST endpoint can call
/// it per session without an N-session full-history read.
fn active_turn_for_session(
    requests: &[RequestStatus],
    session_id: SessionId,
) -> Option<AgentTurnId> {
    requests
        .iter()
        .filter(|status| status.session_id == Some(session_id))
        .find(|status| !status.state.is_terminal())
        .map(|status| AgentTurnId(status.request_id))
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
            // OCEAN-185: bind the turn's permission gate to the submitter. The
            // token rides the request body (authenticated submit path) and is
            // copied into every PermissionWaiter; it is NEVER emitted on the
            // public /v1/events SSE.
            decision_token: req.decision_token.clone(),
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
    /// Per-turn permission secret (OCEAN-224, extending OCEAN-185). A voice
    /// caller that can answer a permission prompt (the surface proxy relaying
    /// `/v1/permissions/{id}/decision`, a future wake-word client) mints a token
    /// with [`ocean_core::mint_decision_token`], sends it here, and replays the
    /// SAME value on the decision POST. The daemon binds the turn's permission
    /// gate to it exactly like `POST /v1/agent/turns`, so a gated mutating tool in
    /// a voice turn becomes approvable instead of stalling on an un-answerable
    /// prompt. `None` is only valid when yolo is effective (every tool
    /// auto-approves); otherwise the handler rejects the turn fast so it can
    /// never silently hang on a prompt no voice caller can answer — see
    /// [`agent_voice`].
    #[serde(default)]
    decision_token: Option<String>,
}

/// Whether a voice turn would dead-end on a permission prompt no voice caller can
/// answer (OCEAN-224). True ⇒ the turn must be rejected up front rather than run.
///
/// A voice turn is *un-answerable* only when BOTH hold: it carries no
/// `decision_token` (so it cannot bind/approve a gate via OCEAN-185) AND yolo is
/// not effective (so a mutating tool *will* raise a gate). In that one case the
/// gate would surface as a `PermissionRequest` onto the SSE that a spoken
/// interface has no card to click — the turn would silently hang. With a token,
/// the gate is approvable; with yolo, no gate is ever raised. Pure so the
/// fail-fast contract is unit-tested directly, not asserted via a copy.
fn voice_turn_is_unanswerable(decision_token: Option<&str>, yolo_effective: bool) -> bool {
    decision_token.is_none() && !yolo_effective
}

/// POST /v1/agent/voice — accept a transcribed utterance and run it as a normal
/// agent turn tagged `client_type = "leo-voice"`. Thin wrapper over `agent_turn`
/// so it inherits cwd resolution, per-session locking, cancellation, and SSE
/// streaming with zero duplication.
///
/// # Permission posture (OCEAN-224)
///
/// A voice turn that hits a mutating tool is gated exactly like a text turn when
/// yolo is off. The trap this endpoint used to have: it hardcoded
/// `decision_token: None`, so a gated voice turn raised a `PermissionRequest`
/// onto the SSE that **no voice caller could answer** — the turn then hung until
/// it was cancelled or timed out. A spoken interface has no permission card to
/// click, so an un-answerable prompt is a silent dead-end, not a UX papercut.
///
/// The fix makes the posture explicit, with no silent stall possible:
///
/// 1. **Caller supplies a `decision_token`** → it is threaded onto the inner
///    turn (same OCEAN-185 binding as `agent_turn`). The gate is now answerable:
///    whoever can relay `/v1/permissions/{id}/decision` for this caller (the
///    surface proxy, a wake-word client) replays the same token and approves.
/// 2. **No token, but yolo is effective** → every tool auto-approves, so no gate
///    is ever raised. Unbound is fine; the turn runs.
/// 3. **No token and yolo is off** → a mutating tool *would* raise a gate nobody
///    can answer. Rather than accept the turn and let it hang, we **reject it up
///    front** with `400` and a short, speakable reason. The caller fails fast and
///    can tell the operator to enable yolo or send a token, instead of the agent
///    going silent mid-utterance.
async fn agent_voice(
    State(state): State<AppState>,
    Json(req): Json<AgentVoiceRequest>,
) -> (StatusCode, Json<AgentTurnResponse>) {
    // OCEAN-224: refuse the un-answerable case before any work. A voice turn with
    // no `decision_token` can only be safely run when yolo is effective (no gate
    // will ever be raised). Without a token AND without yolo, a mutating tool
    // would stall on a permission prompt no spoken interface can answer — so fail
    // fast with a clear, speakable message instead of hanging.
    if voice_turn_is_unanswerable(req.decision_token.as_deref(), effective_yolo()) {
        let session_id = req.session_id.unwrap_or_else(AgentSessionId::new_v4);
        return (
            StatusCode::BAD_REQUEST,
            Json(AgentTurnResponse {
                ok: false,
                turn_id: AgentTurnId::new_v4(),
                session_id,
                status: AgentTurnStatus::Failed,
                event_id_prefix: String::new(),
                error: Some(
                    "Voice turns can't approve permission prompts on their own. \
                     Turn on yolo mode, or send a decision_token with the voice \
                     turn and use it to approve."
                        .to_string(),
                ),
            }),
        );
    }

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
        // Voice turns carry no images.
        images: None,
        // OCEAN-224: thread the caller's per-turn secret through so a gated voice
        // turn is approvable (binds the gate to this submitter, OCEAN-185). `None`
        // here only ever reaches `agent_turn` when yolo is effective — the guard
        // above already rejected the un-answerable no-token, no-yolo case.
        decision_token: req.decision_token,
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

/// Workspace-scoping guard for the session-DETAIL read path (`GET
/// /v1/agent/sessions/{id}`), mirroring the turn path's session↔workspace
/// binding (OCEAN-52) so a caller cannot read another workspace's session by id
/// alone (OCEAN-74).
///
/// - `requested_workspace`: the workspace the caller declared via `?cwd=` /
///   `?workspace=`, already resolved to a workspace root. `None` = the caller
///   declared no scope.
/// - `session_workspace`: the session's bound workspace root. `None` = a legacy
///   session with no recorded workspace.
///
/// A cross-workspace read is rejected ONLY when BOTH are present and differ.
/// When either is absent the read is allowed (an unscoped caller, or a legacy
/// session with no boundary to enforce), preserving backward-compatible reads.
fn session_detail_scope_check(
    requested_workspace: Option<&str>,
    session_workspace: Option<&str>,
) -> Result<(), CwdBindingError> {
    match (requested_workspace, session_workspace) {
        (Some(requested), Some(bound)) if requested != bound => {
            Err(CwdBindingError::WorkspaceMismatch {
                requested_workspace: requested.to_string(),
                session_workspace: bound.to_string(),
            })
        }
        _ => Ok(()),
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
        // OCEAN-143: previously `guidance: _` — the documented `guidance`
        // turn-field was destructured and dropped on the floor, so it was
        // advertised as live but never reached the model. It is now folded into
        // the turn prompt below, layered with room guidance.
        guidance,
        room_id,
        project_id,
        client_type,
        thinking_level,
        model_id,
        images,
        decision_token,
    } = req;

    // OCEAN-115: map the wire-level `TurnImage`s onto `ocean-core`'s `PromptImage`
    // (kept separate so ocean-core stays free of an ocean-protocol dependency).
    // The agent layer turns each into a `Content::Image` block on the first user
    // message. `None`/empty leaves the turn text-only, unchanged.
    let images: Option<Vec<PromptImage>> = images.map(|imgs| {
        imgs.into_iter()
            .map(|img| PromptImage {
                mime_type: img.mime_type,
                data: img.data,
            })
            .collect()
    });

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

    // OCEAN-51: permission gating is ON by default. Previously this path
    // hardcoded `yolo: true`, auto-approving every tool call and making the
    // entire per-tool permission machinery dead code for the shipped product
    // surfaces. Now the mode is operator-controlled. `AgentTurnRequest` carries
    // no per-request yolo flag, so the effective posture is the operator's
    // resolved default: OCEAN_YOLO env → persisted setting (OCEAN-YOLO) → off.
    // The bypass is opt-in, never the silent default.
    let yolo = effective_yolo();

    let guided_prompt = apply_turn_guidance(room_id, guidance.as_deref(), &prompt);
    let mut prompt_req = PromptRequest {
        prompt: guided_prompt,
        images,
        request_id: Some(request_id),
        session_id: Some(core_sid(session_id)),
        // New session → allow creating under the freshly-minted id. Resume
        // (client supplied the id) → strict: error if that session is gone,
        // rather than silently forking a fresh transcript under the same id.
        create_if_missing: is_new_session,
        max_turns: None,
        yolo,
        cwd,
        project_id,
        client_type,
        // OCEAN-185: carry the submitter's per-turn secret onto the internal
        // PromptRequest so register_running_request binds the gate to it.
        decision_token: decision_token.clone(),
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
                    details,
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
                            // Forward the runtime's structured tool-result
                            // details (exit codes, counts, perf, …) to clients.
                            // The runtime uses `Value::Null` to mean "no
                            // metadata"; collapse that to `None` so the SDK
                            // field stays absent rather than carrying a null.
                            metadata_json: metadata_from_details(details),
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
                AgentEvent::SurfacePatch {
                    canvas_id, patches, ..
                } => {
                    // Slice 3: stamp each validated patch into a
                    // `SurfacePatchEnvelope` carrying the routing/persistence
                    // context (session/surface/canvas/actor/timestamp), then
                    // relay onto `/v1/agent/events`. The event carries this
                    // turn's `bridge_session_id`, so the SSE filter scopes it to
                    // the originating session — a second session never sees it.
                    use ocean_agent_sdk::surface::{
                        ActorRef, CanvasId, PatchId, SurfaceId, SurfacePatchEnvelope,
                    };
                    let canvas = CanvasId::new(canvas_id);
                    let created_at_ms = ocean_protocol::now_ms();
                    let envelopes: Vec<SurfacePatchEnvelope> = patches
                        .into_iter()
                        .map(|patch| SurfacePatchEnvelope {
                            patch_id: PatchId::new(Uuid::new_v4().to_string()),
                            session_id: bridge_session_id,
                            surface_id: SurfaceId::new("gpui:local"),
                            canvas_id: canvas.clone(),
                            actor: ActorRef::agent(None),
                            created_at_ms,
                            patch,
                        })
                        .collect();
                    bridge_bus.emit(AgentTurnEvent::SurfacePatch {
                        session_id: bridge_session_id,
                        turn_id: bridge_turn_id,
                        canvas_id: canvas,
                        patches: envelopes,
                    });
                }
                _ => {}
            }
        }
    });

    // Same `yolo` flag drives the permission policy: `false` (default) builds a
    // gating `DaemonPermissionPolicy`; `true` builds the auto-allow policy.
    let control = build_prompt_control(
        &state,
        request_id,
        Some(core_sid(session_id)),
        yolo,
        cancel,
        decision_token,
    )
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

/// Render the operator's per-turn `guidance` hints (OCEAN-143) into a steering
/// block, or `None` when there is nothing to inject.
///
/// `guidance` is the documented `guidance` turn-field on `POST /v1/agent/turns`
/// — short steering notes like `"focus on tests"` or `"be concise"`. Each
/// non-blank entry becomes one bullet under a labelled header so the model reads
/// it as explicit operator direction rather than part of the task text. Blank /
/// whitespace-only entries are dropped; an all-blank (or empty) list yields
/// `None`, leaving the prompt untouched.
fn render_turn_guidance(guidance: Option<&[String]>) -> Option<String> {
    let hints: Vec<&str> = guidance?
        .iter()
        .map(|hint| hint.trim())
        .filter(|hint| !hint.is_empty())
        .collect();
    if hints.is_empty() {
        return None;
    }
    let mut block = String::from("Operator guidance for this turn:");
    for hint in hints {
        block.push_str("\n- ");
        block.push_str(hint);
    }
    Some(block)
}

/// Compose the prompt the model actually sees for a turn, layering (in order)
/// any room guidance, then the operator's per-turn `guidance` hints, then the
/// operator's prompt.
///
/// Both layers are prepended — matching how room guidance was already injected
/// (`apply_room_guidance`) before OCEAN-143 — so steering text precedes the task
/// without mutating it. With neither present this is exactly the bare prompt,
/// preserving the legacy turn shape for clients that send no guidance.
fn apply_turn_guidance(
    room_id: Option<RoomId>,
    guidance: Option<&[String]>,
    prompt: &str,
) -> String {
    let with_room = apply_room_guidance(room_id, prompt);
    match render_turn_guidance(guidance) {
        Some(block) => format!("{block}\n\n{with_room}"),
        None => with_room,
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
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let want = q.session_id;
    let all = q
        .all
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes" | "on"));

    // OCEAN-129: honor `Last-Event-ID` on reconnect. Subscribe to the live
    // broadcast and snapshot the replay buffer under one lock so nothing falls
    // through the seam, then replay the buffered events newer than the client's
    // last-seen id BEFORE the live stream. The same `?session_id=`/`?all=`
    // scoping is applied to replayed events, so a reconnecting client never
    // sees another session's events on replay.
    let last_event_id = parse_last_event_id(&headers);
    let (replay, live_rx) = state.agent_events.subscribe_with_replay(last_event_id);

    // Track replayed ids so any event that lands on the live receiver between
    // the snapshot and now (there should be none, given the shared lock, but be
    // defensive) is not delivered twice across the replay/live seam.
    let mut replayed_ids: std::collections::HashSet<Uuid> =
        std::collections::HashSet::with_capacity(replay.len());
    let replay_events: Vec<Result<Event, Infallible>> = replay
        .into_iter()
        .filter_map(|envelope| {
            if !should_emit_agent_event(want, all, &envelope.event) {
                return None;
            }
            replayed_ids.insert(envelope.id);
            let id = envelope.id.to_string();
            let event_type = agent_event_type_name(&envelope.event);
            let data = serde_json::to_string(&envelope.event).unwrap_or_else(|_| {
                r#"{"type":"error","message":"serialize failed"}"#.to_string()
            });
            Some(Ok(Event::default().id(id).event(event_type).data(data)))
        })
        .collect();

    let live = BroadcastStream::new(live_rx).filter_map(move |event| match event {
        Ok(envelope) => {
            // Skip anything already delivered during replay (seam dedupe).
            if replayed_ids.remove(&envelope.id) {
                return None;
            }
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
            // A slow `/v1/agent/events` consumer overflowed the ring and lost
            // `skipped` agent-turn events (thinking deltas, tool chunks).
            // Log at warn so the drop is visible in the daemon log, not just
            // pushed to the client (OCEAN-87).
            tracing::warn!(
                skipped,
                "agent_events SSE subscriber lagged; dropped events"
            );
            let data =
                json!({ "type": "error", "message": format!("stream lagged by {skipped}") })
                    .to_string();
            Some(Ok(Event::default().event("error").data(data)))
        }
    });

    // Replay first (in emission order), then the live broadcast.
    let stream = tokio_stream::iter(replay_events).chain(live);
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
    // Snapshot the live request registry once, then derive each summary's
    // active_turn from it via the same helper the detail endpoint uses
    // (OCEAN-205). This is a cheap status peek — no per-session transcript load.
    let requests: Vec<RequestStatus> = {
        let guard = state.requests.read().await;
        guard.values().map(|ctl| ctl.status.clone()).collect()
    };
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
            active_turn: active_turn_for_session(&requests, s.id),
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
    Query(q): Query<SessionDetailQuery>,
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

            // Workspace-scoping guard (OCEAN-74). The turn path binds a session
            // to its workspace (OCEAN-52); this read path must honour the same
            // boundary so a caller in workspace A cannot read workspace B's
            // session transcript by id alone. When the caller declares a scope
            // (`?cwd=` / `?workspace=`) and the session carries a bound
            // workspace, a mismatch is a cross-workspace read: reject with the
            // turn path's 400 shape. A scopeless read, or a legacy session with
            // no bound workspace, falls through unchanged (backward compatible).
            let requested_ws = q.requested_workspace(&state.runtime);
            let session_ws =
                session_workspace_binding(&state.runtime, session_id).map(|(_cwd, root)| root);
            if let Err(err) =
                session_detail_scope_check(requested_ws.as_deref(), session_ws.as_deref())
            {
                tracing::warn!(
                    %session_id,
                    error = %err.message(),
                    "agent_session: rejected cross-workspace session-detail read"
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AgentSessionResponse {
                        ok: false,
                        session: None,
                        turns: vec![],
                        error: Some(err.message()),
                    }),
                );
            }

            let turns = turns_from_detail(&session);
            // A still-running session surfaces its in-flight turn as active.
            // Derive it from the live request registry via the shared helper so
            // the LIST and DETAIL endpoints can't drift (OCEAN-205).
            let requests: Vec<RequestStatus> = {
                let guard = state.requests.read().await;
                guard.values().map(|ctl| ctl.status.clone()).collect()
            };
            let active_turn = active_turn_for_session(&requests, core_id);
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
        AgentTurnEvent::SurfacePatch { .. } => None,
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
        AgentTurnEvent::SurfacePatch { .. } => "surface_patch",
    }
}

/// Map a runtime tool result's `details` (`serde_json::Value`) onto the SDK
/// `ToolResult.metadata_json` (`Option<Value>`). The runtime represents "no
/// structured metadata" as `Value::Null`; the SDK represents it as `None`, so
/// collapse `Null` to `None` and forward any real value as `Some(..)`.
fn metadata_from_details(details: Value) -> Option<Value> {
    if details.is_null() {
        None
    } else {
        Some(details)
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

    /// Serializes every test that mutates the process-global env this module
    /// reads for the YOLO resolution (`OCEAN_YOLO`, `OCEAN_CONFIG_DIR`). Rust
    /// runs unit tests on parallel threads sharing one process env, so without
    /// this lock two env-touching yolo tests can interleave and read each
    /// other's writes. Poison is swallowed — a panicking test should not cascade
    /// into spurious failures here.
    static YOLO_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn yolo_env_guard() -> std::sync::MutexGuard<'static, ()> {
        YOLO_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

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
            decision_token: None,
        }
    }

    /// Build a `RequestControl` with an explicit `finished_at` so the GC sweep's
    /// age comparison (`terminal_at()` reads `finished_at` first) is deterministic.
    fn request_control_at(state: RequestState, finished_at: DateTime<Utc>) -> RequestControl {
        RequestControl {
            status: RequestStatus {
                request_id: RequestId::new_v4(),
                session_id: None,
                state,
                permission_id: None,
                message: None,
                started_at: Some(finished_at),
                updated_at: Some(finished_at),
                finished_at: Some(finished_at),
            },
            cancel: CancellationToken::new(),
            handle: None,
            decision_token: None,
        }
    }

    /// OCEAN-207: a `room_finished` webhook for a `call_*` room must decide
    /// `EndCall` and the daemon must map that to a `CallEnded` event (with the
    /// room threaded through as `call_id`). Without this the call lifecycle
    /// never closes and the TUI/surface shows a phantom "in progress" call
    /// forever. We assert the full decide → emit mapping the handler relies on.
    #[test]
    fn room_finished_on_call_room_maps_to_call_ended() {
        // Inbound: SIP-routed `call_<caller>_<random>` hangup.
        let inbound = "call_+17035551234_aB3x";
        let action = ocean_call::decide_webhook("room_finished", inbound);
        assert_eq!(
            action,
            ocean_call::WebhookAction::EndCall {
                room: inbound.to_string()
            }
        );
        match webhook_action_to_event(action) {
            Some(OceanEvent::CallEnded { call_id, .. }) => assert_eq!(call_id, inbound),
            other => panic!("expected CallEnded for inbound room_finished, got {other:?}"),
        }

        // Outbound: `/v1/calls/place` rooms are `call:<uuid>`; a normal hangup
        // fires room_finished on them too and must also close the lifecycle.
        let outbound = "call:3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        let action = ocean_call::decide_webhook("room_finished", outbound);
        assert_eq!(
            action,
            ocean_call::WebhookAction::EndCall {
                room: outbound.to_string()
            }
        );
        match webhook_action_to_event(action) {
            Some(OceanEvent::CallEnded { call_id, .. }) => assert_eq!(call_id, outbound),
            other => panic!("expected CallEnded for outbound room_finished, got {other:?}"),
        }
    }

    /// A `room_finished` for a NON-call room must NOT emit CallEnded: `decide`
    /// returns `Ignore` and the daemon maps that to no event at all, so regular
    /// app rooms (e.g. `pm`, `writers`) never produce phantom call lifecycle.
    #[test]
    fn room_finished_on_non_call_room_emits_no_event() {
        let action = ocean_call::decide_webhook("room_finished", "pm");
        assert_eq!(action, ocean_call::WebhookAction::Ignore);
        assert!(
            webhook_action_to_event(action).is_none(),
            "non-call room_finished must not emit a lifecycle event"
        );
    }

    /// Symmetry guard: a `room_started` on a call room maps to CallStarted, so
    /// the start/end pair stays balanced and correlated by the same `call_id`.
    #[test]
    fn room_started_on_call_room_maps_to_call_started() {
        let room = "call_anon_xyz";
        let action = ocean_call::decide_webhook("room_started", room);
        match webhook_action_to_event(action) {
            Some(OceanEvent::CallStarted { call_id, room_id, .. }) => {
                assert_eq!(call_id, room);
                assert_eq!(room_id, room);
            }
            other => panic!("expected CallStarted for room_started, got {other:?}"),
        }
    }

    /// Build a terminal `PermissionWaiter` (sender consumed => `is_terminal`) with
    /// an explicit `created_at`, which is what its `terminal_at()` reads.
    fn terminal_waiter_at(created_at: DateTime<Utc>) -> PermissionWaiter {
        PermissionWaiter {
            status: PermissionStatus {
                permission_id: PermissionId::new_v4(),
                request_id: RequestId::new_v4(),
                session_id: None,
                tool: "write".into(),
                reason: "permission required for write".into(),
                args: json!({"path": "src/lib.rs"}),
                created_at,
            },
            sender: None,
            decision_token: None,
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

    // ---- OCEAN-129: Last-Event-ID replay ----

    fn delta_event(session_id: AgentSessionId, text: &str) -> AgentTurnEvent {
        AgentTurnEvent::AssistantTextDelta {
            session_id,
            turn_id: AgentTurnId::new_v4(),
            delta: text.into(),
        }
    }

    #[tokio::test]
    async fn agent_bus_replays_events_after_last_event_id() {
        let bus = AgentEventBus::new(64);
        let sid = AgentSessionId::new_v4();

        // No last-event-id => no replay (matches pre-OCEAN-129 behavior).
        let (none_replay, _rx) = bus.subscribe_with_replay(None);
        assert!(none_replay.is_empty(), "no last-event-id => no replay");

        bus.emit(delta_event(sid, "first"));
        // capture id of "first" via the internal buffer
        let first_id = {
            let h = bus.history.lock().unwrap();
            h.back().unwrap().id
        };
        bus.emit(delta_event(sid, "second"));
        bus.emit(delta_event(sid, "third"));

        let (replay, _rx) = bus.subscribe_with_replay(Some(first_id));
        let texts: Vec<String> = replay
            .iter()
            .filter_map(|env| match &env.event {
                AgentTurnEvent::AssistantTextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["second", "third"], "replay events after last id");
    }

    #[tokio::test]
    async fn agent_bus_unknown_last_event_id_replays_nothing() {
        let bus = AgentEventBus::new(64);
        let sid = AgentSessionId::new_v4();
        bus.emit(delta_event(sid, "a"));
        bus.emit(delta_event(sid, "b"));

        // An id never seen (or aged out) => empty replay, fall back to live only.
        let (replay, _rx) = bus.subscribe_with_replay(Some(Uuid::new_v4()));
        assert!(replay.is_empty());
    }

    #[tokio::test]
    async fn agent_bus_replay_respects_session_scope() {
        // Replayed events must still be filtered by the requested session, so a
        // reconnecting client cannot be leaked another session's events.
        let bus = AgentEventBus::new(64);
        let mine = AgentSessionId::new_v4();
        let other = AgentSessionId::new_v4();

        bus.emit(delta_event(mine, "mine-1"));
        let anchor = {
            let h = bus.history.lock().unwrap();
            h.back().unwrap().id
        };
        bus.emit(delta_event(other, "other-secret"));
        bus.emit(delta_event(mine, "mine-2"));

        let (replay, _rx) = bus.subscribe_with_replay(Some(anchor));
        // Apply the SAME scoping the handler applies on replay.
        let visible: Vec<String> = replay
            .iter()
            .filter(|env| should_emit_agent_event(Some(mine), false, &env.event))
            .filter_map(|env| match &env.event {
                AgentTurnEvent::AssistantTextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            visible,
            vec!["mine-2"],
            "only my session's later events replay; other session's are filtered out"
        );
        assert!(
            !replay
                .iter()
                .any(|env| should_emit_agent_event(Some(mine), false, &env.event)
                    && matches!(&env.event, AgentTurnEvent::AssistantTextDelta { delta, .. } if delta == "other-secret")),
            "other session's event must never pass the scope filter on replay"
        );
    }

    // ---- OCEAN-150 (Gate B): surface_patch is session-scoped ----

    fn surface_patch_event(session_id: AgentSessionId, canvas: &str) -> AgentTurnEvent {
        use ocean_agent_sdk::surface::{
            ActorRef, CanvasComponentPatch, CanvasId, ComponentId, PatchId, SurfaceId, SurfacePatch,
            SurfacePatchEnvelope,
        };
        let canvas_id = CanvasId::new(canvas);
        let patch = SurfacePatch::UpsertComponent {
            component: CanvasComponentPatch {
                id: ComponentId::new("card-1"),
                kind: "card".into(),
                rect: None,
                z_index: None,
                content: json!({"title": "secret"}),
                metadata: Value::Null,
            },
        };
        AgentTurnEvent::SurfacePatch {
            session_id,
            turn_id: AgentTurnId::new_v4(),
            canvas_id: canvas_id.clone(),
            patches: vec![SurfacePatchEnvelope {
                patch_id: PatchId::new(Uuid::new_v4().to_string()),
                session_id,
                surface_id: SurfaceId::new("gpui:local"),
                canvas_id,
                actor: ActorRef::agent(None),
                created_at_ms: 0,
                patch,
            }],
        }
    }

    #[test]
    fn surface_patch_event_reports_its_session() {
        // The new variant must carry its session id so the SSE filter can scope
        // it — a None here would make it leak to `?all=1` only (or nowhere).
        let sid = AgentSessionId::new_v4();
        let ev = surface_patch_event(sid, "canvas:main");
        assert_eq!(
            ev.session_id(),
            Some(sid),
            "SurfacePatch must be session-scoped, not global"
        );
    }

    #[tokio::test]
    async fn surface_patch_is_scoped_to_its_session() {
        // Gate B: a surface_patch emitted for session A must reach A's scoped
        // stream and MUST NOT reach an unrelated session B's scoped stream
        // (cross-session isolation hardened by OCEAN-129 must not regress).
        let a = AgentSessionId::new_v4();
        let b = AgentSessionId::new_v4();
        let ev = surface_patch_event(a, "canvas:main");

        // A's scoped subscriber receives it.
        assert!(
            should_emit_agent_event(Some(a), false, &ev),
            "the originating session must receive its own surface_patch"
        );
        // B's scoped subscriber does NOT.
        assert!(
            !should_emit_agent_event(Some(b), false, &ev),
            "an unrelated session must NOT receive another session's surface_patch"
        );
        // A session-less (non-`?all=1`) subscriber does NOT (session-bearing
        // event requires the firehose opt-in).
        assert!(
            !should_emit_agent_event(None, false, &ev),
            "session-bearing surface_patch needs ?all=1 to reach the firehose"
        );
        // The `?all=1` firehose does receive it.
        assert!(
            should_emit_agent_event(None, true, &ev),
            "the ?all=1 firehose receives session-bearing events"
        );
    }

    #[tokio::test]
    async fn surface_patch_replay_respects_session_scope() {
        // The replay path must apply the same scope filter so a reconnecting
        // session B can never be handed A's buffered surface_patch.
        let bus = AgentEventBus::new(64);
        let a = AgentSessionId::new_v4();
        let b = AgentSessionId::new_v4();

        bus.emit(delta_event(b, "b-anchor"));
        let anchor = {
            let h = bus.history.lock().unwrap();
            h.back().unwrap().id
        };
        bus.emit(surface_patch_event(a, "canvas:main"));
        bus.emit(delta_event(b, "b-after"));

        let (replay, _rx) = bus.subscribe_with_replay(Some(anchor));
        // B's scoped replay sees only B's text, never A's surface_patch.
        let leaked_patch = replay
            .iter()
            .any(|env| should_emit_agent_event(Some(b), false, &env.event)
                && matches!(&env.event, AgentTurnEvent::SurfacePatch { .. }));
        assert!(
            !leaked_patch,
            "session B's replay must never include session A's surface_patch"
        );
    }

    #[tokio::test]
    async fn agent_bus_replay_buffer_is_bounded() {
        // Buffer caps at AGENT_EVENT_REPLAY_BUFFER; oldest entries evict so
        // memory stays bounded and aged-out ids replay nothing.
        let bus = AgentEventBus::new(8);
        let sid = AgentSessionId::new_v4();
        let mut first_id = None;
        for i in 0..(AGENT_EVENT_REPLAY_BUFFER + 50) {
            bus.emit(delta_event(sid, &format!("e{i}")));
            if i == 0 {
                first_id = Some(bus.history.lock().unwrap().back().unwrap().id);
            }
        }
        let len = bus.history.lock().unwrap().len();
        assert!(len <= AGENT_EVENT_REPLAY_BUFFER, "buffer must stay bounded");
        // The very first id has aged out => no replay (graceful fallback).
        let (replay, _rx) = bus.subscribe_with_replay(first_id);
        assert!(replay.is_empty(), "aged-out id replays nothing");
    }

    #[tokio::test]
    async fn parse_last_event_id_reads_header() {
        let id = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert(
            "last-event-id",
            HeaderValue::from_str(&id.to_string()).unwrap(),
        );
        assert_eq!(parse_last_event_id(&headers), Some(id));

        let empty = HeaderMap::new();
        assert_eq!(parse_last_event_id(&empty), None);

        let mut bad = HeaderMap::new();
        bad.insert("last-event-id", HeaderValue::from_static("not-a-uuid"));
        assert_eq!(parse_last_event_id(&bad), None);
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
                decision_token: None,
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
                decision_token: None,
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

    // OCEAN-143: the documented `guidance` turn-field used to be destructured
    // and discarded (`guidance: _`), so it never reached the model. These tests
    // pin the fix: guidance is folded into the turn prompt the model sees.

    #[test]
    fn turn_guidance_is_injected_into_the_prompt() {
        let guidance = vec!["focus on tests".to_string(), "be concise".to_string()];
        let guided = apply_turn_guidance(None, Some(&guidance), "ship the feature");

        // Both hints reach the prompt, as bullets under the operator header,
        // and the operator's prompt is preserved at the end.
        assert!(guided.contains("Operator guidance for this turn:"));
        assert!(guided.contains("- focus on tests"));
        assert!(guided.contains("- be concise"));
        assert!(guided.ends_with("ship the feature"));
    }

    #[test]
    fn turn_guidance_absent_or_blank_leaves_the_prompt_untouched() {
        // No guidance field at all → bare prompt (legacy turn shape).
        assert_eq!(apply_turn_guidance(None, None, "do the thing"), "do the thing");
        // Empty list → nothing to inject.
        assert_eq!(
            apply_turn_guidance(None, Some(&[]), "do the thing"),
            "do the thing"
        );
        // All-whitespace entries are dropped, yielding the bare prompt.
        let blank = vec!["   ".to_string(), "\t".to_string()];
        assert_eq!(
            apply_turn_guidance(None, Some(&blank), "do the thing"),
            "do the thing"
        );
        // render_turn_guidance reports "nothing to inject" directly.
        assert!(render_turn_guidance(None).is_none());
        assert!(render_turn_guidance(Some(&blank)).is_none());
    }

    #[test]
    fn turn_guidance_layers_on_top_of_room_guidance() {
        let room = parse_agent_turn_room(Some("review")).unwrap().unwrap();
        let guidance = vec!["check the migration".to_string()];
        let guided = apply_turn_guidance(Some(room), Some(&guidance), "verify the diff");

        // All three layers are present...
        assert!(guided.contains("Operator guidance for this turn:"));
        assert!(guided.contains("- check the migration"));
        assert!(guided.contains("Review Room"));
        assert!(guided.ends_with("verify the diff"));

        // ...in order: operator guidance precedes room guidance precedes prompt.
        let op = guided.find("Operator guidance for this turn:").unwrap();
        let room_at = guided.find("Review Room").unwrap();
        let prompt_at = guided.find("verify the diff").unwrap();
        assert!(op < room_at, "operator guidance should precede room guidance");
        assert!(room_at < prompt_at, "room guidance should precede the prompt");
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
            images: vec![],
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
            owning_project: None,
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

    // --- OCEAN-205: active_turn shared between LIST and DETAIL ----------------

    fn request_status_for(
        session_id: Option<SessionId>,
        state: RequestState,
    ) -> RequestStatus {
        RequestStatus {
            request_id: RequestId::new_v4(),
            session_id,
            state,
            permission_id: None,
            message: None,
            started_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            finished_at: None,
        }
    }

    #[test]
    fn active_turn_for_session_returns_running_request_id() {
        let session = SessionId::new_v4();
        let running = request_status_for(Some(session), RequestState::Running);
        let want = AgentTurnId(running.request_id);
        // Mix in noise: another session's running request + a finished one for
        // this session. Only this session's live request should be reported.
        let other = request_status_for(Some(SessionId::new_v4()), RequestState::Running);
        let done = request_status_for(Some(session), RequestState::Completed);
        let registry = vec![done, other, running];

        assert_eq!(active_turn_for_session(&registry, session), Some(want));
    }

    #[test]
    fn active_turn_for_session_is_none_when_all_finished() {
        let session = SessionId::new_v4();
        // Only terminal requests for this session => no active turn. The LIST
        // endpoint must report None here, matching the DETAIL endpoint.
        let registry = vec![
            request_status_for(Some(session), RequestState::Completed),
            request_status_for(Some(session), RequestState::Cancelled),
            request_status_for(Some(session), RequestState::Errored),
        ];

        assert_eq!(active_turn_for_session(&registry, session), None);
    }

    #[test]
    fn active_turn_for_session_is_none_for_unknown_session() {
        // A session with no requests in the registry at all (e.g. a stored
        // session that has never run this process) reports no active turn.
        let registry = vec![request_status_for(
            Some(SessionId::new_v4()),
            RequestState::Running,
        )];
        assert_eq!(active_turn_for_session(&registry, SessionId::new_v4()), None);
    }

    #[test]
    fn active_turn_for_session_treats_waiting_permission_as_active() {
        // A turn paused on a permission gate is still in-flight, so it must
        // surface as the active turn (parity with enrich_session_detail).
        let session = SessionId::new_v4();
        let waiting =
            request_status_for(Some(session), RequestState::WaitingForPermission);
        let want = AgentTurnId(waiting.request_id);
        assert_eq!(
            active_turn_for_session(&[waiting], session),
            Some(want)
        );
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
                    decision_token: None,
                },
            ),
            // still pending (Some) => never reaped by age
            (
                pending,
                PermissionWaiter {
                    status: pending_status,
                    sender: Some(tx),
                    decision_token: None,
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

    // ---- OCEAN-184: graceful shutdown drains in-flight turn tasks ----

    /// Build a Running `RequestControl` carrying a real spawned task handle, so
    /// the drain path has something to await.
    fn running_with_handle(handle: JoinHandle<()>) -> RequestControl {
        let id = RequestId::new_v4();
        let mut ctl = status(id, RequestState::Running);
        ctl.handle = Some(handle);
        ctl
    }

    #[tokio::test]
    async fn drain_waits_for_in_flight_task_to_finish() {
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_w = done.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            done_w.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let id = RequestId::new_v4();
        requests
            .write()
            .await
            .insert(id, running_with_handle(handle));

        // Generous grace: the drain must wait for the 50ms task to complete.
        drain_request_tasks(&requests, std::time::Duration::from_secs(5)).await;

        assert!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            "drain awaited the in-flight task to completion"
        );
        // Handle was taken out of the registry during drain.
        assert!(requests.read().await.get(&id).unwrap().handle.is_none());
    }

    #[tokio::test]
    async fn drain_returns_bounded_when_grace_elapses() {
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        // A task that runs far longer than the grace window. The drain must NOT
        // hang on it — it returns once the bounded timeout fires.
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let id = RequestId::new_v4();
        requests
            .write()
            .await
            .insert(id, running_with_handle(handle));

        let start = std::time::Instant::now();
        drain_request_tasks(&requests, std::time::Duration::from_millis(100)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "drain returned promptly on timeout instead of hanging (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn drain_is_a_noop_with_no_registered_handles() {
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let id = RequestId::new_v4();
        // Entry exists but its handle is None (e.g. already drained / never
        // attached) — drain should skip it and return immediately.
        requests
            .write()
            .await
            .insert(id, status(id, RequestState::Running));
        drain_request_tasks(&requests, std::time::Duration::from_secs(5)).await;
        // Nothing to assert beyond "it returned"; the test passing IS the assert.
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
        use ocean_store::RoomStoreError;
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
        // Durable-backend failures are 500s, not misleading 4xx.
        let (s, _) = room_store_error_response(RoomStoreError::Encode("boom".into()));
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// OCEAN-107: rooms + transcripts must survive a daemon restart. Open the
    /// SQLite store at a path, create a room and post a message, drop the store
    /// (simulating daemon shutdown), reopen at the same path, and assert the room
    /// and its transcript are still there. This is the regression that the
    /// in-memory `RoomRegistry` could never pass.
    #[test]
    fn persistent_rooms_survive_store_reopen() {
        use ocean_store::{RoomStore, SqliteRoomStore};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rooms.db");
        let key = RoomKey::new("survives-restart");

        {
            let mut store = SqliteRoomStore::open(&db_path).unwrap();
            store
                .create(key.clone(), "Survives Restart", None, Utc::now())
                .unwrap();
            store
                .append_message(
                    &key,
                    "john",
                    RoomParticipantKind::Human,
                    RoomMessageKind::Message,
                    "still here after restart?",
                    Utc::now(),
                )
                .unwrap();
            // store dropped here — the daemon process "restarts".
        }

        let store = SqliteRoomStore::open(&db_path).unwrap();
        let rec = store
            .get(&key)
            .unwrap()
            .expect("room must survive the reopen");
        assert_eq!(rec.room.name, "Survives Restart");
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].body, "still here after restart?");
        // And it shows up in the list view after the reopen.
        let listed = store.list().unwrap();
        assert!(listed.iter().any(|r| r.id == key));
    }

    // ---- OCEAN-170: call transcripts persisted to a room --------------------

    /// Build a `RoomStoreHandle` over a temp-file SQLite store, matching the
    /// daemon's `Arc<Mutex<SqliteRoomStore>>` so the call sink can write through.
    fn room_handle(path: &std::path::Path) -> RoomStoreHandle {
        Arc::new(Mutex::new(
            ocean_store::SqliteRoomStore::open(path).unwrap(),
        ))
    }

    /// Drives the SAME orchestrator script `call_demo` runs (CallStarted →
    /// final transcript segments → CallSummaryUpdated → CallEnded) through a
    /// persistence-enabled `BusSink`, then asserts the transcript rows landed in
    /// the store, survive a reopen (daemon restart), and read back via the audit
    /// view the `/transcript` endpoint falls back to once the call closes the
    /// room. This is the end-to-end OCEAN-170 path with NO LiveKit/Twilio in the
    /// loop — exactly the demo's guarantee.
    #[test]
    fn call_demo_script_persists_transcript_to_a_room() {
        use ocean_call::{
            CallSession, EventSink, Summarizer, SummaryPolicy, TranscriptSegment, WakeGate,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rooms.db");
        let rooms = room_handle(&db_path);
        let room = format!("call:{}", Uuid::new_v4());

        // A persisting sink over a real EventBus (events also fan out live; we
        // only assert on the durable side here).
        let mut sink = BusSink::with_persistence(EventBus::new(8), rooms.clone());
        let mut session = CallSession::new(
            "demo-test",
            Summarizer::new(SummaryPolicy {
                every_n_segments: 3,
                silence_ms: 15_000,
            }),
            WakeGate::new(false, 2_000),
        );

        // Same shape as call_demo: 5 final segments; the summarizer fires once at
        // the 3rd final segment (every_n_segments = 3). The orchestrator now hands
        // the debounced transcript back instead of emitting it; this offline demo
        // (no agent runtime) emits it directly to exercise summary persistence.
        session.start(&mut sink, &room, vec!["sip:+17035081859".into()]);
        let script = [
            ("caller", "hey thanks for jumping on", 0u64),
            ("caller", "so for the Warner Q3 push", 2_000),
            ("caller", "I'll send the master to Atlantic tonight", 4_000),
            ("caller", "and we need to verify the toll-free number by Friday", 7_000),
            ("caller", "hey Ocean what did we just agree to", 10_000),
        ];
        for (speaker, text, ms) in script {
            let outcome =
                session.on_segment(TranscriptSegment::final_(speaker, text, ms), ms, &mut sink);
            if let Some(transcript) = outcome.summary_due {
                sink.emit(OceanEvent::CallSummaryUpdated {
                    summary: transcript,
                    as_of_ms: ms,
                });
            }
        }
        session.end(&mut sink, 12_000);

        // Drop the live store handle to simulate a daemon restart, then reopen the
        // same DB file: the transcript must survive (the whole point of OCEAN-170).
        drop(sink);
        drop(rooms);
        let key = RoomKey::new(room.as_str());
        let store = ocean_store::SqliteRoomStore::open(&db_path).unwrap();

        // The room closed on CallEnded, so it's a soft-closed audit record now —
        // hidden from the open view, recoverable for the transcript.
        assert!(
            store.get(&key).unwrap().is_none(),
            "room must be soft-closed after CallEnded"
        );
        let rec = store
            .get_including_closed(&key)
            .unwrap()
            .expect("closed call room must survive the restart");

        // Every FINAL caller segment is a Human Message authored by the speaker.
        let caller_msgs: Vec<_> = rec
            .transcript
            .iter()
            .filter(|m| {
                m.kind == RoomMessageKind::Message
                    && m.author_kind == RoomParticipantKind::Human
            })
            .collect();
        assert_eq!(
            caller_msgs.len(),
            script.len(),
            "every final transcript segment must be persisted as a message"
        );
        assert!(caller_msgs.iter().all(|m| m.author_id == "caller"));
        assert_eq!(caller_msgs[0].body, "hey thanks for jumping on");
        assert_eq!(
            caller_msgs.last().unwrap().body,
            "hey Ocean what did we just agree to"
        );

        // The rolling summary landed as a System-kind message (author "ocean").
        let summaries: Vec<_> = rec
            .transcript
            .iter()
            .filter(|m| m.kind == RoomMessageKind::System)
            .collect();
        assert!(
            !summaries.is_empty(),
            "CallSummaryUpdated must be persisted as a System message"
        );
        assert!(summaries.iter().all(|m| m.author_kind == RoomParticipantKind::System));

        // Interim segments are never persisted — assert no row duplicates a body
        // (the assembler emits an interim+final pair per utterance; only finals
        // are written, so each body appears exactly once).
        for (_, text, _) in script {
            let hits = rec.transcript.iter().filter(|m| m.body == text).count();
            assert_eq!(hits, 1, "interim segments must not double-write '{text}'");
        }
    }

    /// The transcript HTTP handler's fallback contract in isolation: once a call
    /// closes its room, `transcript()` returns `UnknownRoom`, but the audit
    /// (`get_including_closed`) view still yields the frozen rows. This pins the
    /// behaviour `room_transcript` relies on so a closed call transcript stays
    /// queryable instead of 404ing.
    #[test]
    fn closed_call_room_transcript_is_still_readable_via_audit_view() {
        use ocean_store::{RoomStore, RoomStoreError, SqliteRoomStore};
        let mut store = SqliteRoomStore::open_in_memory().unwrap();
        let key = RoomKey::new("call:abc");
        store
            .create(key.clone(), "Call transcript", None, Utc::now())
            .unwrap();
        store
            .append_message(
                &key,
                "caller",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "we agreed to ship Friday",
                Utc::now(),
            )
            .unwrap();
        store.close(&key).unwrap();

        // Open-view read now fails (room is closed)...
        assert!(matches!(
            store.transcript(&key, None),
            Err(RoomStoreError::UnknownRoom(_))
        ));
        // ...but the audit view the handler falls back to still has the row.
        let rec = store.get_including_closed(&key).unwrap().unwrap();
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].body, "we agreed to ship Friday");
    }

    /// A bus-only sink (no room store) must never touch persistence: it forwards
    /// events to the SSE rail and nothing else. Guards against a regression where
    /// the place_call lifecycle's sink accidentally starts writing rooms.
    #[test]
    fn bus_only_sink_does_not_persist() {
        use ocean_call::EventSink;
        let mut sink = BusSink::bus_only(EventBus::new(8));
        // Emitting a full call lifecycle must be a no-op on the (absent) store and
        // must not panic — room_key stays empty, rooms is None.
        sink.emit(ocean_core::OceanEvent::CallStarted {
            call_id: "c".into(),
            room_id: "call:x".into(),
            participants: vec![],
        });
        sink.emit(ocean_core::OceanEvent::CallTranscriptSegment {
            speaker: "caller".into(),
            text: "hi".into(),
            start_ms: 0,
            is_final: true,
        });
        sink.emit(ocean_core::OceanEvent::CallEnded {
            call_id: "c".into(),
            duration_ms: 1,
        });
        assert!(sink.rooms.is_none());
    }

    #[test]
    fn mention_with_policy_produces_convene_decision() {
        // The exact inputs the message handler feeds the evaluator: a stored
        // room's policy + a Mention event parsed from the body. Proves the
        // trigger wiring point fires on a matching event.
        use ocean_store::{RoomStore, SqliteRoomStore};
        let mut reg = SqliteRoomStore::open_in_memory().unwrap();
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
        let policy = reg.trigger_policy(&key).unwrap();
        let decision = evaluate_trigger_policy(
            policy.as_ref(),
            &RoomTriggerEvent::Mention {
                participant_id: mentions[0].clone(),
            },
        );
        assert!(decision.should_convene);
        assert_eq!(decision.target_participant.as_deref(), Some("ocean"));
    }

    #[test]
    fn convene_audit_line_gated_on_agent_resolution() {
        // OCEAN-128: `evaluate_trigger_policy` returns should_convene=true for
        // ANY on_mention match, regardless of participant kind. The convene
        // FOOTPRINT (the `auto-convene:` transcript line, the room_trigger
        // event, and the queued turn) must only be written when the mention
        // resolves to a runnable Agent — never for a human/bot/tool mention.
        //
        // This pins the split the handler now relies on: a convene decision for
        // a human resolves to None (no footprint), and the same decision for an
        // agent resolves to Some (footprint written + turn spawned).
        let roster = vec![
            RoomParticipant {
                id: "john".into(),
                kind: RoomParticipantKind::Human,
                display_name: "John".into(),
            },
            RoomParticipant {
                id: "ocean".into(),
                kind: RoomParticipantKind::Agent,
                display_name: "Ocean".into(),
            },
        ];

        // A human mention: policy says convene, but there's no agent to wake.
        // The handler's `let Some(agent) = ... else { continue }` short-circuits
        // BEFORE the audit-line / event writes — so no convene footprint.
        let human_decision = evaluate_trigger_policy(
            Some(&RoomTriggerPolicy { on_mention: true, ..Default::default() }),
            &RoomTriggerEvent::Mention { participant_id: "john".into() },
        );
        assert!(human_decision.should_convene, "policy still matches on @john");
        assert!(
            human_decision
                .target_participant
                .as_deref()
                .and_then(|id| resolve_agent_participant(&roster, id))
                .is_none(),
            "a human mention must not resolve to an agent (no convene footprint)"
        );

        // An agent mention: resolves to the Agent participant, so the footprint
        // (audit line + event + turn) is written.
        let agent_decision = evaluate_trigger_policy(
            Some(&RoomTriggerPolicy { on_mention: true, ..Default::default() }),
            &RoomTriggerEvent::Mention { participant_id: "ocean".into() },
        );
        assert!(agent_decision.should_convene);
        let resolved = agent_decision
            .target_participant
            .as_deref()
            .and_then(|id| resolve_agent_participant(&roster, id));
        assert!(
            resolved.is_some(),
            "an agent mention must resolve to the agent (convene footprint written)"
        );
        assert_eq!(resolved.unwrap().id, "ocean");
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

    // --- OCEAN-51: permission gating on by default, opt-in yolo --------------

    fn gating_policy(allow_mutating: bool) -> DaemonPermissionPolicy {
        gating_policy_with_token(allow_mutating, None)
    }

    /// Like [`gating_policy`] but binds the policy to a per-turn `decision_token`
    /// (OCEAN-185), so the waiter it mints carries the secret a decision POST
    /// must replay.
    fn gating_policy_with_token(
        allow_mutating: bool,
        decision_token: Option<String>,
    ) -> DaemonPermissionPolicy {
        DaemonPermissionPolicy {
            allow_mutating,
            request_id: RequestId::new_v4(),
            session_id: None,
            events: EventBus::new(16),
            permissions: Arc::new(RwLock::new(HashMap::new())),
            requests: Arc::new(RwLock::new(HashMap::new())),
            cancel: CancellationToken::new(),
            seen_permissions: Arc::new(Mutex::new(HashMap::new())),
            decision_token,
        }
    }

    /// Default mode (allow_mutating = false): a tool call must NOT auto-allow.
    /// `check` blocks waiting on an operator decision, so a bounded wait must
    /// time out rather than returning a decision. This proves the per-tool
    /// gating machinery is live (the bug was that it was dead — auto-allowed).
    #[tokio::test]
    async fn permission_gating_on_by_default_blocks_until_decision() {
        let policy = gating_policy(false);
        let args = json!({"path": "src/lib.rs"});
        let check = policy.check("write", &args);
        let timed =
            tokio::time::timeout(std::time::Duration::from_millis(150), check).await;
        assert!(
            timed.is_err(),
            "default gating must suspend on a permission decision, not auto-allow"
        );

        // And it must have registered a pending permission + emitted a request,
        // i.e. the gating path actually ran (not a silent allow).
        assert_eq!(
            policy.permissions.read().await.len(),
            1,
            "a pending permission waiter must be registered while gated"
        );
    }

    /// Opt-in yolo (allow_mutating = true) restores fire-and-forget: every tool
    /// call resolves to Allow immediately, no waiter, no blocking.
    #[tokio::test]
    async fn permission_yolo_opt_in_auto_allows() {
        let policy = gating_policy(true);
        let decision = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            policy.check("write", &json!({"path": "src/lib.rs"})),
        )
        .await
        .expect("yolo mode must resolve immediately");
        assert!(
            matches!(decision, AgentPermissionDecision::Allow),
            "yolo mode auto-allows mutating tools"
        );
        assert_eq!(
            policy.permissions.read().await.len(),
            0,
            "yolo mode registers no permission waiter"
        );
    }

    // --- OCEAN-185 (P0): permission decisions bound to the turn submitter -----
    //
    // The decision endpoint validated only the `permission_id`, which is
    // broadcast on the unauthenticated /v1/events SSE — so any localhost page
    // could sniff it and approve a gated tool. These tests prove the per-turn
    // `decision_token` binding closes that hole: the token is required on the
    // decision POST, verified constant-time, and is ABSENT from the SSE payload.

    /// Build a minimal AppState carrying real `requests`/`permissions`/`events`
    /// registries for direct handler-level tests. No runtime turn is run.
    fn permission_test_state() -> AppState {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("OCEAN_CONFIG_DIR", tmp.path());
        std::env::set_var("OCEAN_MODEL", "fake-ok");
        let runtime = Arc::new(AgentRuntime::from_env().expect("fake runtime"));
        let store = ocean_store::SqliteRoomStore::open_in_memory().expect("in-mem store");
        AppState {
            runtime,
            events: EventBus::new(64),
            agent_events: AgentEventBus::new(64),
            requests: Arc::new(RwLock::new(HashMap::new())),
            permissions: Arc::new(RwLock::new(HashMap::new())),
            longhouse: Arc::new(Mutex::new(ocean_longhouse::LonghouseRegistry::new())),
            rooms: Arc::new(Mutex::new(store)),
        }
    }

    /// Register a pending permission waiter bound to `token`, returning the
    /// permission id plus the decision receiver the "agent loop" would await.
    async fn register_bound_waiter(
        state: &AppState,
        token: Option<String>,
    ) -> (PermissionId, oneshot::Receiver<AgentPermissionDecision>) {
        let permission_id = PermissionId::new_v4();
        let request_id = RequestId::new_v4();
        let (tx, rx) = oneshot::channel();
        state.permissions.write().await.insert(
            permission_id,
            PermissionWaiter {
                status: PermissionStatus {
                    permission_id,
                    request_id,
                    session_id: None,
                    tool: "write".into(),
                    reason: "permission required for write".into(),
                    args: json!({"path": "src/lib.rs"}),
                    created_at: Utc::now(),
                },
                sender: Some(tx),
                decision_token: token,
            },
        );
        (permission_id, rx)
    }

    /// THE ATTACKER CASE: a decision POST that knows only the broadcast
    /// `permission_id` but presents NO token must be rejected 403, the waiter
    /// must survive (not be burned), and no decision must reach the agent loop.
    #[tokio::test]
    async fn decision_without_token_is_rejected_403_and_tool_not_run() {
        let state = permission_test_state();
        let token = ocean_core::mint_decision_token();
        let (permission_id, mut rx) =
            register_bound_waiter(&state, Some(token.clone())).await;

        // Attacker forges an Allow with only the sniffed permission_id.
        let body = PermissionDecisionRequest {
            permission_id,
            decision: PermissionDecisionBody::Allow,
            decision_token: None,
        };
        let (status, resp) = permission_decision(
            State(state.clone()),
            Path(permission_id),
            Json(body),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a token-less decision must be forbidden"
        );
        assert!(!resp.0.ok, "forbidden decision must report ok=false");

        // The waiter must still be pending — the attacker can't even burn it.
        assert_eq!(
            state.permissions.read().await.len(),
            1,
            "rejected decision must leave the pending waiter intact"
        );
        // And no decision reached the agent loop (the gated tool never runs).
        assert!(
            rx.try_recv().is_err(),
            "no decision must be delivered to the runtime waiter"
        );
    }

    /// A wrong token (attacker guesses) is likewise rejected 403.
    #[tokio::test]
    async fn decision_with_wrong_token_is_rejected_403() {
        let state = permission_test_state();
        let (permission_id, mut rx) =
            register_bound_waiter(&state, Some(ocean_core::mint_decision_token())).await;

        let body = PermissionDecisionRequest {
            permission_id,
            decision: PermissionDecisionBody::Allow,
            decision_token: Some(ocean_core::mint_decision_token()), // different secret
        };
        let (status, _resp) =
            permission_decision(State(state.clone()), Path(permission_id), Json(body)).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "a wrong token must be forbidden");
        assert_eq!(state.permissions.read().await.len(), 1);
        assert!(rx.try_recv().is_err());
    }

    /// THE LEGIT CASE: the submitter replays the exact token it sent on the turn;
    /// the decision is accepted (200), the waiter resolves, and Allow reaches the
    /// agent loop so the tool runs.
    #[tokio::test]
    async fn decision_with_correct_token_is_accepted_and_tool_runs() {
        let state = permission_test_state();
        let token = ocean_core::mint_decision_token();
        let (permission_id, rx) = register_bound_waiter(&state, Some(token.clone())).await;

        let body = PermissionDecisionRequest {
            permission_id,
            decision: PermissionDecisionBody::Allow,
            decision_token: Some(token),
        };
        let (status, resp) =
            permission_decision(State(state.clone()), Path(permission_id), Json(body)).await;

        assert_eq!(status, StatusCode::OK, "the correct token must be accepted");
        assert!(resp.0.ok);
        // Waiter consumed, decision delivered to the runtime as Allow.
        assert_eq!(state.permissions.read().await.len(), 0);
        let delivered = rx.await.expect("a decision must reach the runtime waiter");
        assert!(
            matches!(delivered, AgentPermissionDecision::Allow),
            "the submitter's Allow must reach the agent loop"
        );
    }

    /// The per-turn token must NEVER appear on the public /v1/events SSE payload
    /// — that is the whole point (the broadcast carries permission_id, not the
    /// secret). Drive the real gating policy `check`, snapshot the emitted
    /// `PermissionRequest` envelope, and assert the token string is absent from
    /// its serialized JSON while the waiter privately holds it.
    #[tokio::test]
    async fn decision_token_is_absent_from_the_sse_permission_request_payload() {
        let token = ocean_core::mint_decision_token();
        let policy = gating_policy_with_token(false, Some(token.clone()));
        let mut rx = policy.events.subscribe();

        // `check` blocks awaiting a decision; run it bounded so we only need the
        // emitted PermissionRequest envelope, then drop the future.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            policy.check("write", &json!({"path": "src/lib.rs"})),
        )
        .await;

        // The waiter must privately carry the token (binding is live)...
        let waiter_token = {
            let perms = policy.permissions.read().await;
            perms
                .values()
                .next()
                .expect("a waiter must be registered")
                .decision_token
                .clone()
        };
        assert_eq!(
            waiter_token.as_deref(),
            Some(token.as_str()),
            "the waiter must hold the per-turn token for verification"
        );

        // ...but the broadcast envelope must NOT leak it.
        let envelope = rx.try_recv().expect("a PermissionRequest must be broadcast");
        assert!(
            matches!(envelope.event, OceanEvent::PermissionRequest { .. }),
            "first broadcast event is the PermissionRequest"
        );
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        assert!(
            !serialized.contains(&token),
            "the decision_token must NEVER appear on the public /v1/events SSE payload; got: {serialized}"
        );
    }

    /// Backward-compatibility / phased rollout: a waiter bound to NO token (a
    /// legacy/daemon-internal turn) accepts a token-less decision, so existing
    /// internal flows (auto-convene, voice) keep working. The hole is closed for
    /// the clients that DO bind (cli/acp/tui) without breaking unbound callers.
    #[tokio::test]
    async fn unbound_waiter_accepts_token_less_decision() {
        let state = permission_test_state();
        let (permission_id, rx) = register_bound_waiter(&state, None).await;

        let body = PermissionDecisionRequest {
            permission_id,
            decision: PermissionDecisionBody::Allow,
            decision_token: None,
        };
        let (status, resp) =
            permission_decision(State(state.clone()), Path(permission_id), Json(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(resp.0.ok);
        assert!(matches!(
            rx.await.expect("decision delivered"),
            AgentPermissionDecision::Allow
        ));
    }

    /// Constant-time token compare semantics (the verify primitive).
    #[test]
    fn decision_token_matches_semantics() {
        let t = ocean_core::mint_decision_token();
        assert!(ocean_core::decision_token_matches(Some(&t), Some(&t)));
        assert!(!ocean_core::decision_token_matches(Some(&t), Some("nope")));
        assert!(!ocean_core::decision_token_matches(Some(&t), None));
        assert!(!ocean_core::decision_token_matches(None, Some(&t)));
        assert!(!ocean_core::decision_token_matches(None, None));
        // Differing lengths must not match and must not panic.
        assert!(!ocean_core::decision_token_matches(Some("abc"), Some("abcd")));
    }

    /// `OCEAN_YOLO` parsing: default/empty/garbage = gated (false); the
    /// documented truthy spellings = bypass (true). This is the operator opt-in
    /// switch; it must default safe.
    #[test]
    fn ocean_yolo_env_defaults_off_and_opts_in_explicitly() {
        let _guard = yolo_env_guard();
        // Serialize env mutation within this test; restore the prior value.
        let prior = env::var("OCEAN_YOLO").ok();

        env::remove_var("OCEAN_YOLO");
        assert!(!yolo_enabled(), "unset OCEAN_YOLO must gate (default off)");

        for off in ["", "0", "false", "no", "off", "nonsense"] {
            env::set_var("OCEAN_YOLO", off);
            assert!(!yolo_enabled(), "OCEAN_YOLO={off:?} must stay gated");
        }
        for on in ["1", "true", "TRUE", "Yes", "on"] {
            env::set_var("OCEAN_YOLO", on);
            assert!(yolo_enabled(), "OCEAN_YOLO={on:?} must opt into bypass");
        }

        match prior {
            Some(v) => env::set_var("OCEAN_YOLO", v),
            None => env::remove_var("OCEAN_YOLO"),
        }
    }

    /// OCEAN-YOLO persistence round-trip: writing the preference under a config
    /// dir and reading it back (simulating a daemon restart, which re-reads the
    /// file from scratch) returns the saved value. Default-on-first-run is
    /// `None` ⇒ the caller treats it as off.
    #[test]
    fn yolo_pref_persists_and_roundtrips() {
        let tmp = std::env::temp_dir().join(format!("ocean-yolo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        // First run: nothing persisted.
        assert_eq!(
            ocean_agent::load_yolo_pref(&tmp),
            None,
            "no file ⇒ no persisted default"
        );

        // Persist true, then a fresh read (no in-memory cache) returns true.
        ocean_agent::persist_yolo_pref(&tmp, true);
        assert_eq!(
            ocean_agent::load_yolo_pref(&tmp),
            Some(true),
            "persisted true must survive a fresh read (restart)"
        );

        // Overwrite with false; the new value wins.
        ocean_agent::persist_yolo_pref(&tmp, false);
        assert_eq!(
            ocean_agent::load_yolo_pref(&tmp),
            Some(false),
            "persisted false must overwrite the prior true"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// OCEAN-YOLO precedence: `effective_yolo()` resolves env → persisted → off.
    /// - persisted=true + no env ⇒ effective true (the personal default sticks);
    /// - `OCEAN_YOLO=0` overrides persisted=true ⇒ effective off (env wins);
    /// - `OCEAN_YOLO=1` ⇒ effective true even with persisted=false;
    /// - nothing set (no env, no file) ⇒ effective off (the safety default).
    #[test]
    fn effective_yolo_precedence_env_over_persisted_over_off() {
        let _guard = yolo_env_guard();
        // Also serialize against the auto-convene tests, the other suite that
        // mutates `OCEAN_CONFIG_DIR`/`OCEAN_YOLO` process-globally. Acquire it
        // AFTER the yolo lock; no path takes these in the reverse order, so this
        // can't deadlock.
        let _convene_guard = AUTO_CONVENE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior_yolo = env::var("OCEAN_YOLO").ok();
        let prior_cfg = env::var("OCEAN_CONFIG_DIR").ok();

        let tmp = std::env::temp_dir().join(format!("ocean-yolo-prec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        env::set_var("OCEAN_CONFIG_DIR", &tmp);

        // Nothing set anywhere ⇒ off (safety default).
        env::remove_var("OCEAN_YOLO");
        assert!(!effective_yolo(), "no env + no persisted ⇒ off");

        // Persisted true, no env ⇒ the personal default takes effect.
        ocean_agent::persist_yolo_pref(&tmp, true);
        env::remove_var("OCEAN_YOLO");
        assert!(effective_yolo(), "persisted true + no env ⇒ on");

        // OCEAN_YOLO=0 overrides persisted true ⇒ env wins, gated.
        env::set_var("OCEAN_YOLO", "0");
        assert!(!effective_yolo(), "OCEAN_YOLO=0 must override persisted true");

        // OCEAN_YOLO=1 overrides persisted false ⇒ env wins, bypass.
        ocean_agent::persist_yolo_pref(&tmp, false);
        env::set_var("OCEAN_YOLO", "1");
        assert!(effective_yolo(), "OCEAN_YOLO=1 must override persisted false");

        // Unrecognized env ⇒ falls through to persisted (false here).
        env::set_var("OCEAN_YOLO", "maybe");
        assert!(
            !effective_yolo(),
            "garbage env falls through to persisted false"
        );

        // Restore env.
        match prior_yolo {
            Some(v) => env::set_var("OCEAN_YOLO", v),
            None => env::remove_var("OCEAN_YOLO"),
        }
        match prior_cfg {
            Some(v) => env::set_var("OCEAN_CONFIG_DIR", v),
            None => env::remove_var("OCEAN_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// OCEAN-224: the fail-fast predicate truth table. A voice turn is
    /// "un-answerable" — and so must be rejected up front rather than allowed to
    /// hang on a permission prompt — ONLY when it carries no `decision_token` AND
    /// yolo is off. A token (it can approve the gate) OR yolo (no gate is raised)
    /// makes it answerable. This is the heart of the no-silent-stall guarantee.
    #[test]
    fn voice_turn_unanswerable_predicate_truth_table() {
        // No token + no yolo ⇒ the one dead-end case ⇒ reject up front.
        assert!(
            voice_turn_is_unanswerable(None, false),
            "no decision_token and no yolo ⇒ gate would hang ⇒ must fail fast"
        );
        // A token makes the gate approvable, regardless of yolo.
        assert!(
            !voice_turn_is_unanswerable(Some("tok"), false),
            "a decision_token can approve the gate ⇒ answerable, run it"
        );
        assert!(
            !voice_turn_is_unanswerable(Some("tok"), true),
            "token + yolo ⇒ answerable"
        );
        // Yolo means no gate is ever raised, so a missing token is harmless.
        assert!(
            !voice_turn_is_unanswerable(None, true),
            "yolo auto-approves every tool ⇒ no gate ⇒ unbound voice turn is fine"
        );
    }

    /// OCEAN-224 — THE BUG, CLOSED: a non-yolo voice turn with no `decision_token`
    /// must fail fast (HTTP 400 + a clear, speakable error) instead of being
    /// accepted and silently stalling on a permission prompt no spoken interface
    /// can answer. We drive the real `agent_voice` handler with yolo forced off;
    /// the rejection path returns BEFORE the runtime is touched, so this is a
    /// fast, deterministic assertion of the no-silent-stall contract.
    #[tokio::test]
    async fn voice_turn_without_token_and_no_yolo_fails_fast_not_hang() {
        let _guard = yolo_env_guard();
        let _convene_guard = AUTO_CONVENE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior_yolo = env::var("OCEAN_YOLO").ok();
        let prior_cfg = env::var("OCEAN_CONFIG_DIR").ok();

        // Force operator policy OFF: no persisted file (fresh config dir) and
        // OCEAN_YOLO explicitly off, so `effective_yolo()` is false.
        let tmp = std::env::temp_dir().join(format!("ocean-voice-224-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        env::set_var("OCEAN_CONFIG_DIR", &tmp);
        env::set_var("OCEAN_YOLO", "0");
        assert!(!effective_yolo(), "test precondition: yolo must be off");

        let state = permission_test_state();
        let req = AgentVoiceRequest {
            session_id: None,
            transcript: "delete the old branches".to_string(),
            cwd: String::new(),
            room_id: None,
            project_id: None,
            // The dead-end input: no token to approve a gate with.
            decision_token: None,
        };

        let (status, resp) = agent_voice(State(state), Json(req)).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a non-yolo, token-less voice turn must be rejected up front, not run"
        );
        assert!(!resp.ok, "rejection response must report ok=false");
        assert_eq!(resp.status, AgentTurnStatus::Failed);
        let err = resp.error.clone().unwrap_or_default();
        assert!(
            err.to_lowercase().contains("yolo")
                && err.to_lowercase().contains("decision_token"),
            "the error must name BOTH escape hatches (enable yolo / send a \
             decision_token) so the caller can act; got: {err:?}"
        );

        // Restore env.
        match prior_yolo {
            Some(v) => env::set_var("OCEAN_YOLO", v),
            None => env::remove_var("OCEAN_YOLO"),
        }
        match prior_cfg {
            Some(v) => env::set_var("OCEAN_CONFIG_DIR", v),
            None => env::remove_var("OCEAN_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// OCEAN-224: a voice turn that DOES carry a `decision_token` is answerable
    /// even with yolo off (the token binds + approves the gate exactly like a text
    /// turn, OCEAN-185), so the fail-fast guard must NOT trip on it. Asserted via
    /// the predicate to avoid spinning up the full agent loop for the happy path.
    #[test]
    fn voice_turn_with_token_is_answerable_even_without_yolo() {
        let token = ocean_core::mint_decision_token();
        assert!(
            !voice_turn_is_unanswerable(Some(&token), false),
            "a real minted decision_token makes a non-yolo voice turn approvable, \
             not a dead-end — the guard must let it through to agent_turn"
        );
    }

    /// OCEAN-160 (P0): the legacy `POST /v1/prompt` (and its async sibling
    /// `POST /v1/requests`) must NOT trust the wire `yolo` flag to escalate.
    /// `resolve_request_yolo` is the handler's yolo resolver; it must ignore the
    /// client-supplied flag and return exactly `effective_yolo()` (operator
    /// policy: env → persisted → off) in every state.
    ///
    /// The load-bearing assertion is the first block: operator policy OFF +
    /// wire `yolo: true` ⇒ STILL off. That is the auth-bypass the ticket closes.
    #[test]
    fn resolve_request_yolo_ignores_wire_flag() {
        let _guard = yolo_env_guard();
        let _convene_guard = AUTO_CONVENE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior_yolo = env::var("OCEAN_YOLO").ok();
        let prior_cfg = env::var("OCEAN_CONFIG_DIR").ok();

        let tmp = std::env::temp_dir().join(format!("ocean-yolo-wire-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        env::set_var("OCEAN_CONFIG_DIR", &tmp);

        // Operator policy OFF (no env, no persisted). A wire `yolo: true` must
        // NOT flip the bypass on — this is the OCEAN-160 vuln, closed.
        env::remove_var("OCEAN_YOLO");
        assert!(
            !resolve_request_yolo(true),
            "wire yolo=true must NOT bypass the gate when operator policy is off (OCEAN-160)"
        );
        assert!(
            !resolve_request_yolo(false),
            "wire yolo=false with policy off stays off"
        );

        // Operator opted in via persisted default ⇒ on, independent of the wire
        // flag (a legitimate operator default still works, both inputs).
        ocean_agent::persist_yolo_pref(&tmp, true);
        env::remove_var("OCEAN_YOLO");
        assert!(
            resolve_request_yolo(false),
            "persisted operator default true is honored even with wire yolo=false"
        );
        assert!(
            resolve_request_yolo(true),
            "persisted operator default true stays on with wire yolo=true"
        );

        // Operator opted in via env ⇒ on regardless of wire flag.
        ocean_agent::persist_yolo_pref(&tmp, false);
        env::set_var("OCEAN_YOLO", "1");
        assert!(resolve_request_yolo(false), "OCEAN_YOLO=1 ⇒ on (wire false)");
        assert!(resolve_request_yolo(true), "OCEAN_YOLO=1 ⇒ on (wire true)");

        // env explicitly OFF must override even a wire true.
        env::set_var("OCEAN_YOLO", "0");
        assert!(
            !resolve_request_yolo(true),
            "OCEAN_YOLO=0 must keep the gate on even with wire yolo=true"
        );

        // resolve_request_yolo is exactly effective_yolo, flag aside.
        env::remove_var("OCEAN_YOLO");
        ocean_agent::persist_yolo_pref(&tmp, true);
        assert_eq!(resolve_request_yolo(true), effective_yolo());
        assert_eq!(resolve_request_yolo(false), effective_yolo());

        match prior_yolo {
            Some(v) => env::set_var("OCEAN_YOLO", v),
            None => env::remove_var("OCEAN_YOLO"),
        }
        match prior_cfg {
            Some(v) => env::set_var("OCEAN_CONFIG_DIR", v),
            None => env::remove_var("OCEAN_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- OCEAN-130: fake-tool provider drives the full block→decide→proceed
    //     cycle through the real agent loop + the gating DaemonPermissionPolicy.
    //
    // This is the live-cycle coverage the ticket asked for, in-process: the
    // FakeToolProvider emits one `write` tool call, the gating policy suspends
    // the loop (blocking half), a separate task releases the waiter with Allow
    // (release half), and the loop then runs the real `write` tool and finishes.
    // No network, no key — the whole point of the fake-tool mode.
    #[tokio::test]
    async fn fake_tool_provider_blocks_on_gate_then_runs_tool_after_allow() {
        use ocean_runtime::types::AgentConfig;
        use ocean_runtime::{run_agent_with_history, tools::write::WriteTool, FakeToolProvider};
        use ocean_protocol::{Message, Model};

        // A unique temp target so this test never collides with the live-test
        // file or a parallel run.
        let dir = std::env::temp_dir().join(format!("ocean-130-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("written.txt");

        // Gating ON (allow_mutating = false) — the default-safe daemon policy.
        let policy = Arc::new(gating_policy(false));
        let permissions = policy.permissions.clone();

        // A FakeToolProvider that scripts a `write` to OUR target path, so the
        // assertion is self-contained (it doesn't depend on the crate-default
        // constant path that the live HTTP test uses).
        struct TargetedFakeTool {
            path: String,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ocean_protocol::Provider for TargetedFakeTool {
            async fn stream(
                &self,
                _m: &Model,
                _c: &ocean_protocol::Context,
                _o: &ocean_protocol::StreamOptions,
            ) -> ocean_protocol::Result<ocean_protocol::AssistantMessageEventStream> {
                use ocean_protocol::{
                    AssistantMessage, AssistantMessageEvent, Content, StopReason, Usage,
                };
                let round = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let msg = |content: Vec<Content>, stop: StopReason| AssistantMessage {
                    content,
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-tool".into(),
                    usage: Usage::default(),
                    stop_reason: stop,
                    error_message: None,
                    timestamp: 0,
                };
                let evs: Vec<AssistantMessageEvent> = if round == 0 {
                    vec![AssistantMessageEvent::Done {
                        reason: StopReason::ToolUse,
                        message: msg(
                            vec![Content::ToolCall {
                                id: "fake-tool-call-1".into(),
                                name: "write".into(),
                                arguments: json!({"path": self.path, "content": "ok"}),
                            }],
                            StopReason::ToolUse,
                        ),
                    }]
                } else {
                    vec![AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: msg(vec![Content::text("done")], StopReason::Stop),
                    }]
                };
                Ok(Box::pin(futures::stream::iter(
                    evs.into_iter().map(Ok).collect::<Vec<_>>(),
                )))
            }
        }
        let _ = FakeToolProvider::new(); // touch the public type the daemon ships
        let provider = Arc::new(TargetedFakeTool {
            path: target.to_string_lossy().into_owned(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let cfg = AgentConfig::new(Model::openai_compat("fake", "fake-tool", "fake://local", 1000, 1000), "sys")
            .with_tools(vec![Arc::new(WriteTool)])
            .with_permission(policy.clone())
            .with_provider(provider)
            .with_max_turns(4);

        // Drive the real loop.
        let run = tokio::spawn(async move {
            run_agent_with_history(&cfg, vec![Message::user_text("write it")], None).await
        });

        // BLOCKING half: a pending permission waiter must appear (the gate
        // tripped on the `write` tool call), and it must NOT auto-resolve.
        let mut waited = false;
        for _ in 0..100 {
            if permissions.read().await.len() == 1 {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            waited,
            "fake-tool must trip the gate: a pending `write` permission waiter must register"
        );
        // The run is still in flight (suspended), not finished.
        assert!(!run.is_finished(), "the turn must be suspended on the gate");

        // RELEASE half: allow the pending permission, exactly like
        // POST /v1/permissions/{id}/decision {allow} does.
        let (pid, sender) = {
            let mut perms = permissions.write().await;
            let pid = *perms.keys().next().unwrap();
            let mut waiter = perms.remove(&pid).unwrap();
            (pid, waiter.sender.take().unwrap())
        };
        assert!(
            permissions.read().await.get(&pid).is_none(),
            "the waiter must be consumed on decision"
        );
        sender.send(AgentPermissionDecision::Allow).unwrap();

        // PROCEED: the loop resumes, runs the real `write` tool, and completes.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("run must finish after the gate is released")
            .expect("join")
            .expect("agent run ok");
        assert!(
            !result.stopped_at_turn_limit,
            "the run must complete cleanly, not stall at the turn limit"
        );

        // The gated tool ACTUALLY RAN — the file exists with the scripted content.
        let written = std::fs::read_to_string(&target).expect("write tool must have run");
        assert_eq!(written, "ok", "the released `write` tool wrote the file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- OCEAN-53: CORS origin whitelist ------------------------------------

    fn origin(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn cors_allows_localhost_on_any_port() {
        let extra: Vec<String> = vec![];
        for o in [
            "http://localhost:8080",  // trunk serve (PWA)
            "http://localhost:5173",  // vite (canvas-web)
            "http://127.0.0.1:8790",  // surface proxy
            "http://127.0.0.1:4780",  // daemon itself
            "https://localhost:3000", // https loopback
            "http://[::1]:8080",      // ipv6 loopback
            "http://localhost",       // no explicit port
        ] {
            assert!(
                is_trusted_origin(&origin(o), &extra),
                "loopback origin {o} must be allowed"
            );
        }
    }

    #[test]
    fn cors_allows_chrome_extension_origin() {
        let extra: Vec<String> = vec![];
        assert!(
            is_trusted_origin(&origin("chrome-extension://abcdefghijklmnop"), &extra),
            "the Ocean side-panel extension origin must be allowed"
        );
    }

    #[test]
    fn cors_allows_configured_extra_origins() {
        let extra = parse_allowed_origins("https://ocean.example.com, https://tunnel.test/");
        assert!(is_trusted_origin(
            &origin("https://ocean.example.com"),
            &extra
        ));
        // trailing slash in config is normalized away to match the Origin header
        assert!(is_trusted_origin(&origin("https://tunnel.test"), &extra));
    }

    #[test]
    fn cors_rejects_untrusted_public_origins() {
        let extra = parse_allowed_origins("https://ocean.example.com");
        for o in [
            "https://evil.com",
            "http://localhost.evil.com",   // not a real loopback host
            "http://127.0.0.1.evil.com",   // not a real loopback host
            "https://notlocalhost",        // unrelated
            "http://ocean.example.com",    // http when only https was allowed
        ] {
            assert!(
                !is_trusted_origin(&origin(o), &extra),
                "untrusted origin {o} must be rejected"
            );
        }
    }

    #[test]
    fn parse_allowed_origins_trims_and_drops_empties() {
        let parsed = parse_allowed_origins("  https://a.com ,, https://b.com/ ,  ");
        assert_eq!(parsed, vec!["https://a.com", "https://b.com"]);
        assert!(parse_allowed_origins("").is_empty());
        assert!(parse_allowed_origins("   ,  , ").is_empty());
    }

    // --- OCEAN-87: CORS preflight covers every served method ----------------

    /// The router serves PATCH (`/v1/projects/{id}`) and DELETE
    /// (`/v1/projects/{id}`, room participants). Both MUST be in the preflight
    /// allow-list or the browser's OPTIONS check fails and the call never fires.
    #[test]
    fn cors_allow_methods_include_patch_and_delete() {
        let methods = cors_allowed_methods();
        for required in [
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert!(
                methods.contains(&required),
                "CORS allow_methods must advertise {required} (a route serves it)"
            );
        }
    }

    // --- OCEAN-74: session-detail workspace scoping -------------------------

    /// A caller declaring workspace A must NOT read a session bound to workspace
    /// B: the detail read path enforces the same boundary as the turn path.
    #[test]
    fn session_detail_rejects_cross_workspace_read() {
        let err = session_detail_scope_check(Some("/work/repo-a"), Some("/work/repo-b"))
            .expect_err("a cross-workspace detail read must be rejected");
        match err {
            CwdBindingError::WorkspaceMismatch {
                requested_workspace,
                session_workspace,
            } => {
                assert_eq!(requested_workspace, "/work/repo-a");
                assert_eq!(session_workspace, "/work/repo-b");
            }
            other => panic!("expected WorkspaceMismatch, got {other:?}"),
        }
    }

    /// A caller in the same workspace, an unscoped caller, and a legacy session
    /// with no bound workspace all read successfully (backward compatible).
    #[test]
    fn session_detail_allows_matching_or_unscoped_read() {
        // Same workspace → allowed.
        assert!(
            session_detail_scope_check(Some("/work/repo"), Some("/work/repo")).is_ok(),
            "a same-workspace read must be allowed"
        );
        // No declared scope → allowed (legacy first-party caller).
        assert!(
            session_detail_scope_check(None, Some("/work/repo")).is_ok(),
            "an unscoped read must remain allowed"
        );
        // Legacy session with no bound workspace → allowed (no boundary to enforce).
        assert!(
            session_detail_scope_check(Some("/work/repo"), None).is_ok(),
            "a session with no bound workspace has no boundary to enforce"
        );
    }

    // --- OCEAN-74: AllowSession wire semantics ------------------------------

    /// `{"decision":"allow_session"}` must decode to the wire `AllowSession`
    /// variant and map to the runtime's `AllowSession` (allow + remember for the
    /// run), not collapse into a plain `Allow` or fail to decode at all.
    #[test]
    fn allow_session_wire_decodes_and_maps_to_runtime() {
        let permission_id = PermissionId::new_v4();
        let body = json!({
            "permission_id": permission_id,
            "decision": "allow_session",
        });
        let req: PermissionDecisionRequest =
            serde_json::from_value(body).expect("allow_session must be a valid wire decision");
        assert_eq!(req.permission_id, permission_id);
        assert!(
            matches!(req.decision, PermissionDecisionBody::AllowSession),
            "wire decision must decode to AllowSession, not Allow/Deny"
        );

        // The handler's mapping: AllowSession (wire) → AllowSession (runtime).
        let agent_decision = match req.decision {
            PermissionDecisionBody::Allow => AgentPermissionDecision::Allow,
            PermissionDecisionBody::AllowSession => AgentPermissionDecision::AllowSession,
            PermissionDecisionBody::Deny { reason } => AgentPermissionDecision::Deny {
                reason: reason.unwrap_or_else(|| "denied".into()),
            },
        };
        assert!(
            matches!(agent_decision, AgentPermissionDecision::AllowSession),
            "AllowSession must reach the runtime as AllowSession, granting for the run"
        );
        // And it counts as an allow for client-facing reporting (not a block).
        assert!(matches!(
            agent_decision,
            AgentPermissionDecision::Allow | AgentPermissionDecision::AllowSession
        ));
    }

    // ---- Auto-convene end-to-end (OCEAN-111) -------------------------------

    /// Serialize the runtime-building auto-convene tests: they mutate process
    /// env (`OCEAN_MODEL`/`OCEAN_CONFIG_DIR`) to select the Fake provider, and
    /// env is process-global, so two of them racing would clobber each other.
    static AUTO_CONVENE_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build an `AppState` whose runtime is pinned to the Fake provider (so a
    /// turn runs synchronously and deterministically with no live LLM) and whose
    /// room store is a fresh in-memory SQLite DB. Returns the state plus the
    /// tempdir guard (kept alive for the session config dir). Caller must hold
    /// `AUTO_CONVENE_ENV_LOCK` for the duration.
    fn fake_convene_state(tmp: &tempfile::TempDir) -> AppState {
        std::env::set_var("OCEAN_CONFIG_DIR", tmp.path());
        std::env::set_var("OCEAN_MODEL", "fake-ok");
        // YOLO so the fake turn never blocks on a permission prompt (the fake
        // provider does no tool calls, but keep the gate out of the path).
        std::env::set_var("OCEAN_YOLO", "1");
        let runtime = Arc::new(AgentRuntime::from_env().expect("fake runtime"));
        let store = ocean_store::SqliteRoomStore::open_in_memory().expect("in-mem store");
        AppState {
            runtime,
            events: EventBus::new(1024),
            agent_events: AgentEventBus::new(1024),
            requests: Arc::new(RwLock::new(HashMap::new())),
            permissions: Arc::new(RwLock::new(HashMap::new())),
            longhouse: Arc::new(Mutex::new(ocean_longhouse::LonghouseRegistry::new())),
            rooms: Arc::new(Mutex::new(store)),
        }
    }

    /// Poll the room transcript until `pred` matches a message or the deadline
    /// passes. The convened turn runs on a spawned task, so the reply lands
    /// asynchronously after `room_post_message` returns.
    async fn wait_for_message(
        state: &AppState,
        key: &RoomKey,
        pred: impl Fn(&ocean_core::RoomMessage) -> bool,
    ) -> Option<ocean_core::RoomMessage> {
        for _ in 0..200 {
            let found = with_rooms(state, |reg| reg.transcript(key, None))
                .unwrap_or_default()
                .into_iter()
                .find(&pred);
            if found.is_some() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        None
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_mention_queues_turn_and_posts_reply_back() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);

        // Room with an agent participant `helper` and an on_mention policy.
        let key = RoomKey::new("convene-room");
        with_rooms(&state, |reg| {
            reg.create(
                key.clone(),
                "Convene Room",
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                Utc::now(),
            )
            .unwrap();
            reg.add_participant(
                &key,
                RoomParticipant {
                    id: "helper".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Helper".into(),
                },
                Utc::now(),
            )
            .unwrap();
        });

        // A human @-mentions the agent → should convene + queue a turn.
        let (status, body) = room_post_message(
            State(state.clone()),
            Path("convene-room".to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@helper can you summarize the plan?".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let fired = body.0.get("triggers_fired").and_then(|v| v.as_array()).unwrap();
        assert_eq!(fired.len(), 1, "mention of an agent must fire exactly one trigger");

        // The convened turn runs async; its reply lands as an Agent-authored
        // message authored by `helper` carrying the fake provider's output.
        let reply = wait_for_message(&state, &key, |m| {
            m.author_id == "helper"
                && matches!(m.author_kind, RoomParticipantKind::Agent)
                && matches!(m.kind, RoomMessageKind::Message)
        })
        .await
        .expect("the woken agent must post a reply back into the room");
        assert!(
            reply.body.contains("OCEAN_FAKE_OK"),
            "reply should carry the (fake) provider output, got: {:?}",
            reply.body
        );

        // A session was queued/registered for the deterministic room+agent id.
        let expected_sid = core_sid(room_agent_session_id(&key, "helper"));
        let registered = state
            .requests
            .read()
            .await
            .values()
            .any(|c| c.status.session_id == Some(expected_sid));
        assert!(registered, "a turn must be registered for the room+agent session");

        std::env::remove_var("OCEAN_YOLO");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_authored_message_does_not_self_trigger() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);

        let key = RoomKey::new("no-loop-room");
        with_rooms(&state, |reg| {
            reg.create(
                key.clone(),
                "No Loop Room",
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                Utc::now(),
            )
            .unwrap();
            reg.add_participant(
                &key,
                RoomParticipant {
                    id: "helper".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Helper".into(),
                },
                Utc::now(),
            )
            .unwrap();
        });

        // An AGENT posts a message that @-mentions another agent (and itself).
        // This is exactly the ping-pong shape we must NOT amplify: agent-authored
        // messages are never evaluated for triggers.
        let (status, body) = room_post_message(
            State(state.clone()),
            Path("no-loop-room".to_string()),
            Json(RoomMessageRequest {
                author_id: "helper".into(),
                author_kind: RoomParticipantKind::Agent,
                body: "done — cc @helper @other".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let fired = body.0.get("triggers_fired").and_then(|v| v.as_array()).unwrap();
        assert!(
            fired.is_empty(),
            "an agent-authored message must never fire a trigger (anti-loop guard)"
        );

        // Give any errant spawned turn a moment; assert no turn was registered.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            state.requests.read().await.is_empty(),
            "no turn may be queued from an agent-authored message"
        );

        std::env::remove_var("OCEAN_YOLO");
    }

    // ---- Council convene alias (OCEAN-227) ---------------------------------

    /// Drive a request through the **real** `longhouse_routes()` table — the same
    /// group `main()` mounts — to prove `POST /v1/council/convene` is wired as a
    /// live alias of `POST /v1/longhouse/convene`. The canonical `docs/LONGHOUSE.md`
    /// documents the council path; before this alias a doc-following client 404'd.
    ///
    /// We assert the alias returns the convene handler's synchronous ack
    /// (`200 { ok: true, .. }`), that the canonical path returns the identical
    /// shape, and that an unregistered sibling (`/v1/council/nope`) still 404s so
    /// the alias is a specific route — not a catch-all swallowing everything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn council_convene_is_a_live_alias_of_longhouse_convene() {
        use http_body_util::BodyExt;
        use tower::ServiceExt; // for `oneshot`

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // Fake provider so the background `convene` task never touches a live LLM.
        let state = fake_convene_state(&tmp);
        let app = longhouse_routes().with_state(state);

        // Helper: POST a convene body to `path`, returning (status, json).
        async fn post_convene(
            app: Router,
            path: &str,
        ) -> (StatusCode, serde_json::Value) {
            let req = axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri(path)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    json!({ "question": "ship it?" }).to_string(),
                ))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, body)
        }

        // The alias must route to the handler, not 404.
        let (alias_status, alias_body) =
            post_convene(app.clone(), "/v1/council/convene").await;
        assert_eq!(
            alias_status,
            StatusCode::OK,
            "POST /v1/council/convene must be a live route (the canonical doc lists it), got {alias_status}"
        );
        assert_eq!(alias_body["ok"], json!(true), "alias body: {alias_body}");
        assert_eq!(
            alias_body["question"],
            json!("ship it?"),
            "alias must reach the real convene handler and echo the question"
        );

        // The canonical longhouse path returns the identical shape — same handler.
        let (canon_status, canon_body) =
            post_convene(app.clone(), "/v1/longhouse/convene").await;
        assert_eq!(canon_status, StatusCode::OK);
        assert_eq!(canon_body["ok"], json!(true));
        assert_eq!(
            canon_body["question"], alias_body["question"],
            "alias and canonical path must be the same convene flow"
        );

        // A sibling that was never registered still 404s: the alias is a specific
        // route, not a wildcard swallowing the whole /v1/council namespace.
        let (missing_status, _) = post_convene(app, "/v1/council/nope").await;
        assert_eq!(
            missing_status,
            StatusCode::NOT_FOUND,
            "only the documented /v1/council/convene is aliased; other council paths 404"
        );

        std::env::remove_var("OCEAN_YOLO");
    }

    /// The convene FOOTPRINT (notice + audit line + turn) is gated on the mention
    /// resolving to a runnable AGENT. A human-authored message that @-mentions a
    /// *human* id matches the policy (`triggers_fired` is non-empty) but must
    /// queue NO turn — there is no agent to wake. This is the end-to-end negative
    /// of `at_mention_queues_turn_and_posts_reply_back`, asserted through the real
    /// handler at the turn-registration level (OCEAN-225).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mention_of_non_agent_queues_no_turn() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);

        // Room whose only non-author participant is a HUMAN, plus an on_mention
        // policy. Mentioning the human must convene nobody.
        let key = RoomKey::new("no-agent-room");
        with_rooms(&state, |reg| {
            reg.create(
                key.clone(),
                "No Agent Room",
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                Utc::now(),
            )
            .unwrap();
            reg.add_participant(
                &key,
                RoomParticipant {
                    id: "dana".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "Dana".into(),
                },
                Utc::now(),
            )
            .unwrap();
        });

        let (status, _body) = room_post_message(
            State(state.clone()),
            Path("no-agent-room".to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@dana what did you think?".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Give any errant spawned turn a moment, then assert nothing was queued.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            state.requests.read().await.is_empty(),
            "a mention that resolves to a non-agent must queue no turn"
        );

        std::env::remove_var("OCEAN_YOLO");
    }

    // ---- Tool-result metadata forwarding (OCEAN-203) -----------------------

    /// A runtime tool result carrying structured `details` must round-trip into
    /// the SDK `ToolResult.metadata_json` — not be dropped to `None`. This is
    /// the exact mapping the `ToolExecutionEnd` bridge applies.
    #[test]
    fn tool_details_forward_into_metadata_json() {
        // Stand in for `AgentEvent::ToolExecutionEnd.details` (serde_json Value).
        let details = json!({ "exit_code": 0, "files": 3 });

        let result = ToolResult {
            ok: true,
            output: "done".into(),
            metadata_json: metadata_from_details(details.clone()),
        };

        assert_eq!(
            result.metadata_json,
            Some(details),
            "runtime tool-result details must reach the SDK ToolResult.metadata_json"
        );
    }

    /// A tool result with no structured detail (`Value::Null`) must yield
    /// `None` — no spurious/null metadata leaking to clients.
    #[test]
    fn tool_null_details_yield_none_metadata() {
        let result = ToolResult {
            ok: true,
            output: "done".into(),
            metadata_json: metadata_from_details(Value::Null),
        };

        assert_eq!(
            result.metadata_json, None,
            "absent runtime details (Value::Null) must stay None, not Some(null)"
        );
    }

    /// The `PermissionDenied` bridge site has no runtime details to forward, so
    /// it legitimately emits `None`. Lock that in so it isn't mistaken for a bug.
    #[test]
    fn permission_denied_result_has_no_metadata() {
        let result = ToolResult {
            ok: false,
            output: "permission denied for bash: not allowed".into(),
            metadata_json: None,
        };
        assert_eq!(result.metadata_json, None);
    }

    // ---- OCEAN-204: registry GC eviction (memory-leak guard) ----
    //
    // `gc_registries` is the only thing standing between a long-lived daemon and
    // unbounded request/permission registry growth. A regressed TTL comparison
    // (e.g. `<` instead of `>`, or dropping the `is_terminal` guard) would leak
    // silently until OOM. These tests pin the behavior: old terminal entries are
    // evicted, recent-terminal and still-in-flight entries are retained, and the
    // hard cap bounds growth by evicting oldest-terminal first.

    #[tokio::test]
    async fn gc_evicts_old_terminal_but_keeps_recent_and_inflight() {
        let now = Utc::now();
        let old = now - chrono::Duration::hours(2); // past the 1h TTL
        let recent = now - chrono::Duration::minutes(5); // inside the TTL

        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let permissions: PermissionRegistry = Arc::new(RwLock::new(HashMap::new()));

        // request entries
        let old_terminal = RequestId::new_v4();
        let recent_terminal = RequestId::new_v4();
        let old_running = RequestId::new_v4();
        {
            let mut reqs = requests.write().await;
            reqs.insert(
                old_terminal,
                request_control_at(RequestState::Completed, old),
            );
            reqs.insert(
                recent_terminal,
                request_control_at(RequestState::Completed, recent),
            );
            // Non-terminal (still running) but OLD — must NOT be reaped; evicting
            // an in-flight turn would drop a live cancel handle.
            reqs.insert(old_running, request_control_at(RequestState::Running, old));
        }

        // permission entries (terminal == sender consumed)
        let old_perm = PermissionId::new_v4();
        let recent_perm = PermissionId::new_v4();
        let pending_perm = PermissionId::new_v4();
        {
            let mut perms = permissions.write().await;
            perms.insert(old_perm, terminal_waiter_at(old));
            perms.insert(recent_perm, terminal_waiter_at(recent));
            // A pending waiter (Some sender) is never terminal => never reaped by
            // age, even when old.
            perms.insert(
                pending_perm,
                PermissionWaiter {
                    status: PermissionStatus {
                        permission_id: pending_perm,
                        request_id: RequestId::new_v4(),
                        session_id: None,
                        tool: "write".into(),
                        reason: "permission required for write".into(),
                        args: json!({"path": "src/lib.rs"}),
                        created_at: old,
                    },
                    sender: {
                        let (tx, _rx) = oneshot::channel();
                        Some(tx)
                    },
                    decision_token: None,
                },
            );
        }

        gc_registries(&requests, &permissions, now).await;

        let reqs = requests.read().await;
        assert!(
            !reqs.contains_key(&old_terminal),
            "old terminal request must be evicted"
        );
        assert!(
            reqs.contains_key(&recent_terminal),
            "recent terminal request must be retained (inside TTL)"
        );
        assert!(
            reqs.contains_key(&old_running),
            "old in-flight (non-terminal) request must NOT be evicted"
        );

        let perms = permissions.read().await;
        assert!(
            !perms.contains_key(&old_perm),
            "old terminal permission waiter must be evicted"
        );
        assert!(
            perms.contains_key(&recent_perm),
            "recent terminal permission waiter must be retained"
        );
        assert!(
            perms.contains_key(&pending_perm),
            "old PENDING permission waiter must NOT be evicted (never terminal)"
        );
    }

    #[tokio::test]
    async fn gc_max_entries_cap_bounds_growth_evicting_oldest_terminal_first() {
        // All entries are terminal but RECENT (inside TTL), so the TTL pass keeps
        // every one — only the hard cap can trim them. Insert > the cap and assert
        // the count is bounded to exactly REGISTRY_MAX_ENTRIES, and that the very
        // oldest terminal entries are the ones dropped.
        let now = Utc::now();
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let permissions: PermissionRegistry = Arc::new(RwLock::new(HashMap::new()));

        let overflow = 5usize;
        let total = REGISTRY_MAX_ENTRIES + overflow;

        // Track the `overflow` oldest ids — these should be the ones evicted.
        let mut oldest_ids = Vec::new();
        {
            let mut reqs = requests.write().await;
            for i in 0..total {
                // Larger i => more recent (closer to `now`). Use millisecond spacing
                // so even the oldest entry stays well inside the 1h TTL — the TTL
                // pass must keep them all, leaving only the hard cap to trim.
                let ts = now - chrono::Duration::milliseconds((total - i) as i64);
                let id = RequestId::new_v4();
                if i < overflow {
                    oldest_ids.push(id);
                }
                reqs.insert(id, request_control_at(RequestState::Completed, ts));
            }
        }

        gc_registries(&requests, &permissions, now).await;

        let reqs = requests.read().await;
        assert_eq!(
            reqs.len(),
            REGISTRY_MAX_ENTRIES,
            "max-entries cap must bound the registry to REGISTRY_MAX_ENTRIES"
        );
        for id in &oldest_ids {
            assert!(
                !reqs.contains_key(id),
                "the oldest terminal entries must be the ones evicted by the cap"
            );
        }
    }

    // ---- OCEAN-220 (P0): LiveKit token authorization -----------------------
    //
    // The token route used to mint a 6-hour publish-capable `room_join` JWT for
    // ANY caller-supplied room id, with client-controlled `can_publish`, with no
    // entitlement check. These tests pin the two server-side gates that close it:
    //   gate 1 — `call_room_token_allowed`: no token for an unknown/closed call room
    //   gate 2 — `resolve_publish_grant`:   no publish without the operator secret

    /// Serializes tests that mutate the publish-token env var, like the yolo
    /// tests do for their env (parallel unit tests share one process env).
    static PUBLISH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn publish_env_guard() -> std::sync::MutexGuard<'static, ()> {
        PUBLISH_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// GATE 1 — the load-bearing rejection: a token request for a `call:` room
    /// the server never authored is refused. This is the exact attack the ticket
    /// names — minting credentials into an arbitrary in-progress call room.
    #[test]
    fn unknown_call_room_is_rejected() {
        let store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        // Nothing created → an attacker-chosen call room id is unknown.
        assert!(
            !call_room_token_allowed(&store, "call:victims-meeting"),
            "must NOT mint a token for a call room the server didn't author"
        );
    }

    /// GATE 1 — the legit path still works: once the call lifecycle has authored
    /// the room (as `BusSink::persist` does on `CallStarted`), the same id is
    /// accepted, so a real in-call participant can still get a token.
    #[test]
    fn known_open_call_room_is_allowed() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let room = "call:real-call-abc";
        store
            .create(RoomKey::new(room), "Call transcript", None, Utc::now())
            .unwrap();
        assert!(
            call_room_token_allowed(&store, room),
            "a server-authored, open call room must still mint a token"
        );
    }

    /// GATE 1 — a closed call room is refused: once the call ends the room is
    /// soft-closed, and a stale token request for it must not succeed.
    #[test]
    fn closed_call_room_is_rejected() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let room = "call:ended-call";
        store
            .create(RoomKey::new(room), "Call transcript", None, Utc::now())
            .unwrap();
        store.close(&RoomKey::new(room)).unwrap();
        assert!(
            !call_room_token_allowed(&store, room),
            "a closed call room must not mint a token"
        );
    }

    /// GATE 1 — non-`call:` rooms are NOT existence-gated: the operator's own
    /// surface/`project:` spaces are opened ad-hoc and created lazily by LiveKit
    /// on first join, so requiring them to pre-exist would break the legitimate
    /// "open a fresh surface room" flow. (Publish into them is still gated.)
    #[test]
    fn non_call_rooms_pass_existence_gate() {
        let store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        // The surface's default room id — never pre-created in the store.
        assert!(call_room_token_allowed(&store, "project:surface-main"));
        assert!(call_room_token_allowed(&store, "anything-else"));
    }

    /// GATE 2 — fail-closed default: with NO operator secret configured, NO HTTP
    /// caller can publish, even one that sets `can_publish` on the wire and sends
    /// arbitrary auth headers. Listen-only is the most a wire caller ever gets by
    /// default.
    #[test]
    fn publish_denied_when_no_secret_configured() {
        let _g = publish_env_guard();
        std::env::remove_var(PUBLISH_TOKEN_ENV);

        let mut headers = HeaderMap::new();
        headers.insert("x-ocean-publish-token", HeaderValue::from_static("anything"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer anything"),
        );
        assert_eq!(
            resolve_publish_grant(&headers),
            ocean_call::PublishGrant::Deny,
            "no operator secret ⇒ no HTTP caller may publish"
        );
        // And an empty request (no headers) is likewise listen-only.
        assert_eq!(
            resolve_publish_grant(&HeaderMap::new()),
            ocean_call::PublishGrant::Deny
        );
    }

    /// GATE 2 — with the operator secret set, a caller that presents the matching
    /// value (via either header form) is granted publish; a wrong/absent value is
    /// denied. This is the entitled-operator path that keeps in-room voice working.
    #[test]
    fn publish_requires_matching_operator_secret() {
        let _g = publish_env_guard();
        std::env::set_var(PUBLISH_TOKEN_ENV, "s3cret-operator-token");

        // Correct secret via the dedicated header → publish allowed.
        let mut ok_hdr = HeaderMap::new();
        ok_hdr.insert(
            "x-ocean-publish-token",
            HeaderValue::from_static("s3cret-operator-token"),
        );
        assert_eq!(
            resolve_publish_grant(&ok_hdr),
            ocean_call::PublishGrant::Allow
        );

        // Correct secret via `Authorization: Bearer` → publish allowed.
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer s3cret-operator-token"),
        );
        assert_eq!(
            resolve_publish_grant(&bearer),
            ocean_call::PublishGrant::Allow
        );

        // Wrong secret → denied (constant-time compare under the hood).
        let mut wrong = HeaderMap::new();
        wrong.insert(
            "x-ocean-publish-token",
            HeaderValue::from_static("not-the-secret"),
        );
        assert_eq!(
            resolve_publish_grant(&wrong),
            ocean_call::PublishGrant::Deny
        );

        // No header at all → denied.
        assert_eq!(
            resolve_publish_grant(&HeaderMap::new()),
            ocean_call::PublishGrant::Deny
        );

        std::env::remove_var(PUBLISH_TOKEN_ENV);
    }

    // ---- OCEAN-226: POST /v1/longhouse/prepare wires SkillIndex::prepare() ----
    //
    // OCEAN-215 ph1 shipped `prepare()`/`prepare_top_n()` as a read-only library
    // capability with NO daemon consumer. These tests exercise the new endpoint
    // that finally calls it, asserting it (a) actually invokes `prepare()` and
    // returns the ranked brief, (b) stays advisory (no gate / no side effect),
    // and (c) is fail-open.
    //
    // Hermetic without touching the process-global `HOME`: each test plants a
    // uniquely-named repo-local skill under a temp `cwd` and queries it with a
    // token only that skill matches. Whatever real `~/.spawner` / `~/.codex`
    // libraries exist on the host can't match the nonce token, so the asserted
    // result is deterministic regardless of the machine.

    /// Plant a repo-local `./skills/<dir>/skill.yaml` under `cwd`.
    fn plant_repo_skill(cwd: &std::path::Path, dir: &str, name: &str, description: &str) {
        let skill_dir = cwd.join("skills").join(dir);
        std::fs::create_dir_all(&skill_dir).expect("mk skill dir");
        std::fs::write(
            skill_dir.join("skill.yaml"),
            format!("name: {name}\ndescription: {description}\n"),
        )
        .expect("write skill.yaml");
    }

    #[tokio::test]
    async fn longhouse_prepare_invokes_prepare_and_returns_the_brief() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        // A nonce token no real host skill will carry, so the match is ours.
        plant_repo_skill(
            cwd,
            "zorptastic",
            "Zorptastic Widget",
            "Use when building a zorptastic widget for the flux capacitor",
        );

        let req = LonghousePrepareRequest {
            prompt: "help me build a zorptastic widget".to_string(),
            session_id: Some("sess-1".to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            client_type: Some("tui".to_string()),
            top_n: None,
        };

        let Json(body) = longhouse_prepare(Json(req)).await;

        assert_eq!(body["ok"], json!(true));
        // The advisory contract is asserted on the wire: Longhouse recommends,
        // it never acts. If this flips, the endpoint stopped being read-only.
        assert_eq!(
            body["advisory"],
            json!(true),
            "prepare endpoint must advertise itself advisory (no gate bypass)"
        );

        // Round-trip the `prep` back into a real TurnPrep — proves `prepare()`
        // ran and produced a well-formed brief, not just an arbitrary blob.
        let prep: ocean_longhouse::TurnPrep =
            serde_json::from_value(body["prep"].clone()).expect("prep is a valid TurnPrep");
        assert!(
            prep.skills.iter().any(|s| s.name == "Zorptastic Widget"
                && s.source == ocean_longhouse::SkillSource::Repo),
            "the planted repo skill must surface in the ranked brief, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        // SOPs / workflows are always empty in phase 1 — assert the contract holds
        // through the endpoint, not just at the library boundary.
        assert!(prep.sops.is_empty() && prep.workflows.is_empty());
    }

    #[tokio::test]
    async fn longhouse_prepare_honors_top_n_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        // Three skills that all carry the nonce term `zorpquok` — a token no real
        // host skill will match, so ONLY these three can score against the query.
        // That makes both the cap and the membership deterministic regardless of
        // whatever `~/.spawner` / `~/.codex` libraries exist on the machine.
        plant_repo_skill(cwd, "a", "Zorpquok Alpha", "a zorpquok skill alpha");
        plant_repo_skill(cwd, "b", "Zorpquok Bravo", "a zorpquok skill bravo");
        plant_repo_skill(cwd, "c", "Zorpquok Charlie", "a zorpquok skill charlie");

        let req = LonghousePrepareRequest {
            prompt: "zorpquok please".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            client_type: None,
            // Cap to 2 → proves the request's top_n routes to prepare_top_n():
            // three skills match the nonce, but only two come back.
            top_n: Some(2),
        };

        let Json(body) = longhouse_prepare(Json(req)).await;
        let prep: ocean_longhouse::TurnPrep =
            serde_json::from_value(body["prep"].clone()).expect("valid TurnPrep");
        assert_eq!(prep.skills.len(), 2, "top_n=2 must cap the returned briefs");
        assert!(
            prep.skills.iter().all(|s| s.name.starts_with("Zorpquok")),
            "only the nonce-matching planted skills can rank, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn longhouse_prepare_is_fail_open_on_empty_and_irrelevant() {
        // An empty cwd `./skills` plus a prompt nothing matches → empty prep, but
        // still `ok: true` (consulting Longhouse can never block a turn). This
        // does NOT assert the home libraries are empty; it asserts that even with
        // a uniquely-irrelevant prompt the call succeeds and stays advisory.
        let tmp = tempfile::tempdir().expect("tempdir");
        let req = LonghousePrepareRequest {
            prompt: "qqzzxx-nonexistent-nonsense-token-7yt".to_string(),
            session_id: None,
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            client_type: None,
            top_n: None,
        };

        let Json(body) = longhouse_prepare(Json(req)).await;
        assert_eq!(body["ok"], json!(true), "must stay ok even with no matches");
        assert_eq!(body["advisory"], json!(true));
        let prep: ocean_longhouse::TurnPrep =
            serde_json::from_value(body["prep"].clone()).expect("valid TurnPrep");
        assert!(
            prep.skills.is_empty(),
            "a nonsense prompt must match nothing, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}
