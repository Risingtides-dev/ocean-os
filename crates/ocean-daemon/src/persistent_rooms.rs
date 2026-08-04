use std::{
    collections::HashSet,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use chrono::Utc;
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};
use ocean_core::{
    evaluate_trigger_policy, PermissionMode, PromptRequest, PublicAgentDescriptor, RequestState,
    RoomAccessProjection, RoomAccessState, RoomKey, RoomMessage, RoomMessageKind, RoomParticipant,
    RoomParticipantKind, RoomTriggerEvent, RoomTriggerPolicy,
};
#[cfg(test)]
use ocean_core::{OutboxItemState, RoomOutboxItem};
use ocean_store::{RoomStore, ThreadAppendError};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    build_prompt_control, core_sid, emit_session_changed, record_prompt_result, sdk_sid,
    sse_until_shutdown, AppState, SSE_KEEPALIVE_INTERVAL,
};
use crate::request_control::register_running_request;
use crate::room_federation::{AgentRegistrationInput, FederatedTriggerDispatch, IntentError};
use crate::yolo_settings::effective_permission_mode;

/// Shared handle to the daemon's single durable room store. Every closure is
/// synchronous, and both adapters recover a poisoned mutex without holding the
/// guard across an await.
pub(super) type RoomStoreHandle = Arc<Mutex<ocean_store::SqliteRoomStore>>;

/// A room-scoped wake hint. It deliberately carries no transcript payload:
/// SQLite remains the durable authority and every subscriber pages the store
/// after a hint, closing lag and replay/live seam gaps without trusting the
/// bounded channel for delivery.
#[derive(Debug, Clone)]
pub(super) struct RoomWakeHint {
    room: RoomKey,
    seq: u64,
}

/// Daemon-wide bounded wake channel for durable room transcript tails.
#[derive(Clone)]
pub(super) struct RoomWakeBus {
    tx: broadcast::Sender<RoomWakeHint>,
}

impl Default for RoomWakeBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl RoomWakeBus {
    pub(super) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    fn subscribe(&self) -> broadcast::Receiver<RoomWakeHint> {
        self.tx.subscribe()
    }

    #[cfg(test)]
    pub(super) fn test_subscribe(&self) -> broadcast::Receiver<RoomWakeHint> {
        self.subscribe()
    }

    fn publish(&self, room: &RoomKey, message: &RoomMessage) {
        let _ = self.tx.send(RoomWakeHint {
            room: room.clone(),
            seq: message.seq,
        });
    }

    #[cfg(test)]
    fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Publish only after the store adapter has returned, which means the allocating
/// SQLite transaction has committed. A missing subscriber is harmless: hints
/// are advisory and reconnect/recovery always pages the durable log.
pub(super) fn publish_room_wake(state: &AppState, room: &RoomKey, message: &RoomMessage) {
    publish_room_wake_on(&state.room_wakes, room, message);
}

/// Post-commit transcript wake seam for sibling background producers.
/// SQLite remains authoritative; callers invoke this only after the store
/// transaction has returned successfully.
pub(super) fn publish_room_wake_on(wakes: &RoomWakeBus, room: &RoomKey, message: &RoomMessage) {
    wakes.publish(room, message);
}

// ── RoomAccessWakeBus: separate bounded channel for access projection hints ──

/// A room access-projection wake hint. Carries no payload: SQLite remains the
/// durable authority and every subscriber re-reads the access projection after
/// a hint. Separate from transcript `RoomWakeBus` so a heavy transcript SSE tail
/// does not back-pressure access-projection subscribers.
#[derive(Debug, Clone)]
pub(super) struct RoomAccessWakeHint {
    room: RoomKey,
}

/// Daemon-wide bounded wake channel for room access projection changes.
#[derive(Clone)]
pub(super) struct RoomAccessWakeBus {
    tx: broadcast::Sender<RoomAccessWakeHint>,
}

impl Default for RoomAccessWakeBus {
    fn default() -> Self {
        Self::new(64)
    }
}

impl RoomAccessWakeBus {
    pub(super) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<RoomAccessWakeHint> {
        self.tx.subscribe()
    }

    #[cfg(test)]
    pub(super) fn test_subscribe(&self) -> broadcast::Receiver<RoomAccessWakeHint> {
        self.subscribe()
    }

    #[cfg(test)]
    fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    fn publish(&self, room: &RoomKey) {
        let _ = self.tx.send(RoomAccessWakeHint { room: room.clone() });
    }
}

/// Publish an access-projection wake hint only after the store adapter has
/// returned (the allocating SQLite transaction has committed).
pub(super) fn publish_room_access_wake(state: &AppState, room: &RoomKey) {
    publish_room_access_wake_on(&state.room_access_wakes, room);
}

/// Post-commit access-projection wake seam for sibling background producers.
/// The hint carries no payload; subscribers reread SQLite.
pub(super) fn publish_room_access_wake_on(wakes: &RoomAccessWakeBus, room: &RoomKey) {
    wakes.publish(room);
}

/// Append one durable transcript row and issue its post-commit wake hint.
fn append_room_message(
    state: &AppState,
    room: &RoomKey,
    author_id: &str,
    author_kind: RoomParticipantKind,
    kind: RoomMessageKind,
    body: &str,
) -> Result<RoomMessage, ocean_store::RoomStoreError> {
    let message = with_rooms(state, |store| {
        store.append_message(room, author_id, author_kind, kind, body, Utc::now())
    })?;
    publish_room_wake(state, room, &message);
    Ok(message)
}

/// Post a convened agent's answer back into a room (G3).
///
/// Two invariants live here, and neither is client- or caller-controllable:
///
/// 1. **Session attribution is daemon-derived, structurally.** The persisted
///    `session_id` is minted HERE via [`room_agent_session_id`] from the
///    (room, agent) pair — it is not a parameter, so no caller (and certainly
///    no request body) can attribute a row to a session it does not own.
/// 2. **Threading degrades, it never drops.** The agent answers *after* its
///    turn ran, so the parent row it should hang under may have been closed,
///    re-parented, or otherwise invalidated in the meantime. A typed
///    [`ThreadAppendError::InvalidThreadParent`] therefore does not fail the
///    reply: it is re-appended top-level (the pre-thread behaviour) and the
///    stale parent is logged. Only a real store error propagates.
pub(super) fn append_room_agent_reply(
    state: &AppState,
    room: &RoomKey,
    agent_id: &str,
    body: &str,
    thread_parent_seq: Option<u64>,
) -> Result<RoomMessage, ocean_store::RoomStoreError> {
    let session_id = room_agent_session_id(room, agent_id).to_string();
    let session_id = Some(session_id.as_str());
    let message = with_rooms(state, |store| {
        let first = store.append_message_threaded(
            room,
            agent_id,
            RoomParticipantKind::Agent,
            RoomMessageKind::Message,
            body,
            Utc::now(),
            thread_parent_seq,
            session_id,
        );
        match first {
            Ok(message) => Ok(message),
            Err(ThreadAppendError::Store(e)) => Err(e),
            Err(ThreadAppendError::InvalidThreadParent {
                parent_seq, reason, ..
            }) => {
                tracing::warn!(
                    room = %room,
                    agent = %agent_id,
                    parent_seq,
                    reason = %reason,
                    "stale thread parent for agent reply; posting top-level"
                );
                store
                    .append_message_threaded(
                        room,
                        agent_id,
                        RoomParticipantKind::Agent,
                        RoomMessageKind::Message,
                        body,
                        Utc::now(),
                        None,
                        session_id,
                    )
                    .map_err(ocean_store::RoomStoreError::from)
            }
        }
    })?;
    debug_assert_eq!(
        message.session_id.as_deref(),
        session_id,
        "agent reply must persist the daemon-derived session id"
    );
    publish_room_wake(state, room, &message);
    Ok(message)
}

// ---- Persistent Rooms (OCEAN-65) -------------------------------------------
//
// These routes serve the *persistent* `Room` lifecycle: create, fetch, roster
// join/leave, post message, read transcript. They are intentionally additive and
// fully separate from ephemeral agent sessions and the caller-submitted
// `agent_turn` handler. Auto-convene delegates a daemon-internal prompt through
// the existing runtime/session/permission owners; this module does not replace
// their authority.
//
// Error shape mirrors `GET /v1/longhouse/topics/{topic_id}`: a typed `{ ok,
// error }` body, 400 on a bad key, 404 on an unknown room. The store maps to
// status codes in `room_store_error_response`.

/// Where the persistent-rooms SQLite DB lives. `OCEAN_DB_PATH` overrides the
/// whole path; otherwise it is `rooms.db` under the agent's config dir
/// (`ocean_agent::config_dir_from_env`), so the DB sits next to sessions and
/// projects under one config directory.
pub(super) fn room_db_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("OCEAN_DB_PATH") {
        return std::path::PathBuf::from(p);
    }
    ocean_agent::config_dir_from_env().join("rooms.db")
}

/// Run a closure with a locked room store behind a [`RoomStoreHandle`], recovering
/// a poisoned lock the same way [`with_rooms`] does. Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock. Takes
/// the handle directly (rather than `&AppState`) so the call sink — which only
/// holds the `rooms` handle, not the whole state — can write through.
pub(super) fn with_rooms_handle<T>(
    rooms: &RoomStoreHandle,
    f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T,
) -> T {
    let mut guard = match rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Run a closure with the locked room store, recovering a poisoned lock the same
/// way the longhouse handlers do (`into_inner`). Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock.
pub(super) fn with_rooms<T>(
    state: &AppState,
    f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T,
) -> T {
    let mut guard = match state.rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Map a store error onto an HTTP status + typed JSON body.
pub(super) fn room_store_error_response(
    err: ocean_store::RoomStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_store::RoomStoreError::*;
    let status = match &err {
        BadKey(_) => StatusCode::BAD_REQUEST,
        UnknownRoom(_) | UnknownParticipant { .. } => StatusCode::NOT_FOUND,
        AlreadyExists(_) => StatusCode::CONFLICT,
        // The room exists but is not federated: a client-side misuse of a
        // federation-only operation, not a server fault.
        RoomNotFederated(_) => StatusCode::CONFLICT,
        // The caller named an owner that is not a Human in this room's roster
        // (or gave an owner to a non-Agent). That is a malformed request, and
        // the store refused it having written nothing.
        InvalidAgentOwner { .. } => StatusCode::BAD_REQUEST,
        // A join that would re-kind an existing participant is a takeover, not
        // a reconnect. 409: the id is taken by a different kind of actor.
        ParticipantKindConflict { .. } => StatusCode::CONFLICT,
        // A durable backend can fail on I/O or (de)serialization, which the
        // in-memory registry never could. Surface those as 500s, not as a
        // misleading 4xx. Federation corruption is a fail-closed integrity
        // stop and never carries secrets in its message.
        Db(_) | Encode(_) | FederationCorruption(_) | Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({ "ok": false, "error": err.to_string() })),
    )
}

// ---- Named-agent resolution seam (TASK-9 / OCEAN Rooms Gate-1) -------------
//
// A folder-as-agent definition resolved down to the four values a turn needs to
// actually DRIVE that agent: its instructions layer, tool allowlist, model, and
// tier-1 subprocess capabilities. This is the SINGLE named-agent resolution
// path shared by `agent_turn` (the folder-as-agent turn in `main.rs`) and the
// persistent-room convene path (`room_join` validation, the `room_post_message`
// footprint gate, and `spawn_room_agent_turn`). Binding truth flows one way:
// only an Agent participant that resolves to a real AgentDef may be convened;
// a default assistant is never silently substituted for an unresolved name.

/// A resolved folder-as-agent, reduced to the four turn-driving values. Each
/// field is independently `Option`, and a valid data-only agent (an
/// `agent.toml` that declares none of instructions/tools/model/caps)
/// legitimately resolves to all-four-`None` — that is NOT an error. Resolution
/// failure (empty name or an unresolvable folder) is signaled only by
/// [`resolve_named_agent`] returning `Err`, never by an all-`None` `Ok`.
#[derive(Debug, Clone)]
pub(super) struct ResolvedAgent {
    /// Trimmed `instructions.md` when the agent authored any. Prepended as a
    /// steering layer above the (guided) prompt, exactly as `agent_turn` does.
    pub(super) instructions_layer: Option<String>,
    /// `agent.toml` `tools` + `tools/` filename stems, non-empty only when the
    /// agent narrows its toolset. Applied via `PromptControl::with_tool_allowlist`.
    pub(super) tool_allowlist: Option<Vec<String>>,
    /// Declared per-agent model. Fail-soft to the global model when `None`/empty
    /// (the emptiness trim happens inside `PromptControl::with_agent_model`).
    pub(super) model: Option<String>,
    /// Declared tier-1 subprocess capabilities plus the agent root used to
    /// resolve relative commands. Non-empty only when the agent declares caps;
    /// applied via `PromptControl::with_agent_capabilities`.
    pub(super) subprocess_caps: Option<(
        std::path::PathBuf,
        Vec<ocean_agent::agentdir::SubprocessCapability>,
    )>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct RoomTurnCapture {
    agent_id: String,
    prompt: String,
    tool_allowlist: Option<Vec<String>>,
    model: Option<String>,
    subprocess_caps: Option<(
        std::path::PathBuf,
        Vec<ocean_agent::agentdir::SubprocessCapability>,
    )>,
}

#[cfg(test)]
static ROOM_TURN_CAPTURES: Mutex<Vec<RoomTurnCapture>> = Mutex::new(Vec::new());

#[cfg(test)]
fn capture_room_turn(agent_id: &str, prompt: &str, control: &ocean_agent::PromptControl) {
    let capture = RoomTurnCapture {
        agent_id: agent_id.to_string(),
        prompt: prompt.to_string(),
        tool_allowlist: control.tool_allowlist.clone(),
        model: control.agent_model.clone(),
        subprocess_caps: control.agent_capabilities.clone(),
    };
    match ROOM_TURN_CAPTURES.lock() {
        Ok(mut captures) => captures.push(capture),
        Err(poisoned) => poisoned.into_inner().push(capture),
    }
}

/// Resolve a named folder-as-agent to the four turn-driving values above.
///
/// Returns `Err` ONLY for an empty name or an `agentdir::resolve` failure
/// (missing folder, bad name, unparseable `agent.toml`). A resolved-but-
/// data-only agent returns `Ok` with all four fields `None` — that distinction
/// is load-bearing: all-`None` `Ok` is a real, bound agent that declares no
/// overrides, NOT a sentinel for "unresolved". Callers must branch on the
/// `Result`, never on the presence of any single field.
pub(super) fn resolve_named_agent(
    name: &str,
) -> Result<ResolvedAgent, ocean_agent::agentdir::ResolveError> {
    let def = ocean_agent::agentdir::resolve(&super::agents_root(), name)?;
    let instructions_layer = def.system_prompt().map(str::to_owned);
    let tool_allowlist = {
        let tools = def.effective_tools();
        (!tools.is_empty()).then_some(tools)
    };
    let model = def.config.model.clone();
    let subprocess_caps = {
        let caps = def.config.subprocess_capabilities.clone();
        (!caps.is_empty()).then(|| (def.root.clone(), caps))
    };
    Ok(ResolvedAgent {
        instructions_layer,
        tool_allowlist,
        model,
        subprocess_caps,
    })
}

fn resolve_agent_registration(name: &str) -> Option<(String, PublicAgentDescriptor)> {
    let def = ocean_agent::agentdir::resolve(&super::agents_root(), name).ok()?;
    let skills_count = u32::try_from(def.skills.len()).ok()?;
    let canonical = def.name.clone();
    Some((
        canonical.clone(),
        PublicAgentDescriptor {
            display_name: canonical,
            description: def.config.description.clone(),
            model_alias: def.config.model.clone(),
            skills_count,
            subagent_names: def.subagents.clone(),
        },
    ))
}

fn registration_key(instance_id: &str, room: &RoomKey, agent_name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ocean-regkey-v1");
    for value in [instance_id, room.as_str(), agent_name] {
        let byte_len = u64::try_from(value.len()).expect("UTF-8 length fits frozen u64 prefix");
        digest.update(byte_len.to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[derive(serde::Deserialize)]
pub(super) struct RoomCreateRequest {
    /// Persistent room key, e.g. `"ocean-surface-map-fix"`. Must be non-empty.
    pub(super) key: String,
    /// Human-readable room name.
    pub(super) name: String,
    /// Optional trigger policy controlling auto-convene/notify behaviour.
    #[serde(default)]
    pub(super) trigger_policy: Option<RoomTriggerPolicy>,
    /// Optional workspace directory the room belongs to (OCEAN-260). When set,
    /// the room is bound to this project/cwd, so a room-bound agent turn resolves
    /// its owning project and `cwd` from it. Absent/empty ⇒ no binding (room
    /// agents fall back to room+agent keying with the daemon's launch dir).
    #[serde(default)]
    pub(super) workspace_root: Option<String>,
}

/// `POST /v1/rooms/persistent` — create a persistent room.
pub(super) async fn room_create(
    State(state): State<AppState>,
    Json(req): Json<RoomCreateRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(req.key.trim());
    // Normalize an empty/whitespace workspace_root to None so a blank field is
    // treated as "no binding" rather than a bound-to-empty-string room.
    let workspace_root = req
        .workspace_root
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty());
    let result = with_rooms(&state, |reg| {
        reg.create_in_workspace(
            key,
            &req.name,
            workspace_root,
            req.trigger_policy,
            Utc::now(),
        )
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
/// Pagination query for `GET /v1/rooms/persistent` (OCEAN-250).
#[derive(Debug, serde::Deserialize, Default)]
pub(super) struct RoomsListQuery {
    /// Max rooms to return in this page. Omitted ⇒ the store's default cap
    /// (`DEFAULT_LIST_LIMIT`); any value is clamped to `MAX_LIST_LIMIT`.
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Cursor: the room key of the last room from the previous page. Omitted ⇒
    /// the first page. Replay `next_cursor` here for the following page.
    #[serde(default)]
    pub(super) cursor: Option<String>,
}

/// `GET /v1/rooms/persistent?limit=&cursor=` — list open persistent rooms, one
/// bounded page at a time (OCEAN-250). Rooms are ordered most-recently-updated
/// first; the `rooms` array shape is unchanged, with additive
/// `next_cursor`/`has_more` so a poller doesn't re-serialize every room each call.
pub(super) async fn rooms_list_persistent(
    State(state): State<AppState>,
    Query(q): Query<RoomsListQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match with_rooms(&state, |reg| reg.list_page(q.cursor.as_deref(), q.limit)) {
        Ok(page) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "rooms": page.rooms,
                "next_cursor": page.next_cursor,
                "has_more": page.has_more,
            })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}` — one persistent room (with its transcript
/// and access projection). Open rooms only; soft-closed rooms return 404.
pub(super) async fn room_get(
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
    match with_rooms(&state, |reg| {
        let Some(record) = reg.get(&key)? else {
            return Ok(None);
        };
        let access = reg.room_access(&key)?;
        Ok(Some((record, access)))
    }) {
        Ok(Some((rec, access))) => (
            StatusCode::OK,
            Json(
                json!({ "ok": true, "room": rec.room, "transcript": rec.transcript, "access": access }),
            ),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no room with key '{key}'") })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct RoomJoinRequest {
    /// Stable participant id, unique within the room.
    pub(super) id: String,
    /// Display name shown in the roster and transcript.
    pub(super) display_name: String,
    /// What kind of actor is joining. Defaults to `human`.
    #[serde(default = "default_participant_kind")]
    pub(super) kind: RoomParticipantKind,
}

fn default_participant_kind() -> RoomParticipantKind {
    RoomParticipantKind::Human
}

/// `POST /v1/rooms/persistent/{key}/participants` — add a participant.
pub(super) async fn room_join(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<RoomJoinRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    // `System` is the DAEMON'S OWN author identity. Every audit row it writes is
    // authored `("system", System)` — the auto-convene notice, the "not bound"
    // note, the turn-failure line. If a client may join as System, the
    // `ParticipantJoined` marker it produces is a System-authored transcript row
    // that a reader cannot tell apart from a genuine daemon audit line. That is
    // transcript forgery, so System is refused at JOIN for the same reason
    // `classify_local_author` refuses it at POST (:743-748) — the two gates now
    // agree instead of only the second one holding.
    if matches!(req.kind, RoomParticipantKind::System) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "forged_participant_kind",
                "error": "'system' is the daemon's own author identity and cannot be joined",
            })),
        );
    }
    // Join and post must agree on what an id IS. `classify_local_author` treats
    // roster ids as canonical and refuses anything that is empty or not equal to
    // its own trim (:751-753). Without the same rule here, joining as `" john "`
    // succeeds and then that participant can NEVER post — a permanent, silent,
    // self-inflicted denial with no way to discover the cause. Refuse it at the
    // door instead.
    let id = req.id.trim();
    if id.is_empty() || id != req.id {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "invalid_participant_id",
                "error": "participant id must be non-empty and carry no leading or trailing whitespace",
            })),
        );
    }
    if req.display_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "invalid_display_name",
                "error": "display_name must not be empty",
            })),
        );
    }
    // Named-agent binding (TASK-9): an Agent participant MUST name a resolvable
    // folder-as-agent. Reject an unresolved name with a typed 4xx so a phantom
    // agent can never enter the roster as an Agent (it would later convene a
    // default-assistant turn it never authorized). Human/Bot/System participants
    // are unaffected — only `kind == Agent` is bound to a real AgentDef.
    if matches!(req.kind, RoomParticipantKind::Agent) {
        if let Err(_e) = resolve_named_agent(&req.id) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "code": "agent_unresolved",
                    "error": format!("agent '{}' does not resolve", req.id),
                })),
            );
        }
    }
    let participant = RoomParticipant {
        id: req.id,
        kind: req.kind,
        display_name: req.display_name,
    };
    let result = with_rooms(&state, |reg| {
        reg.add_participant_with_message(&key, participant, Utc::now())
    });
    match result {
        Ok((rec, message)) => {
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "room": rec.room })),
            )
        }
        Err(e) => room_store_error_response(e),
    }
}

/// `DELETE /v1/rooms/persistent/{key}/participants/{participant_id}` — remove a
/// participant from the roster.
pub(super) async fn room_leave(
    State(state): State<AppState>,
    Path((key, participant_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |reg| {
        reg.remove_participant_with_message(&key, participant_id.trim(), Utc::now())
    });
    match result {
        Ok((rec, message)) => {
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "room": rec.room })),
            )
        }
        Err(e) => room_store_error_response(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RoomMessageRequest {
    /// Author participant id (or a synthetic id like `"system"`).
    pub(super) author_id: String,
    /// Author kind for attribution. Defaults to `human`.
    #[serde(default = "default_participant_kind")]
    pub(super) author_kind: RoomParticipantKind,
    /// Message body. `@id` mentions in the body drive trigger evaluation.
    pub(super) body: String,
    /// When this is a reply, the `seq` of the parent message (G1-B real
    /// threads). `None` for top-level messages.
    #[serde(default)]
    pub(super) thread_parent_seq: Option<u64>,
    // NOTE (G3): there is deliberately NO `session_id` field. Session
    // attribution is derived by the daemon from the path that produced the row
    // — `room_agent_session_id` for a convened agent reply (see
    // [`append_room_agent_reply`]) — so a client can never attribute its post
    // to a session it does not own. A locally posted HTTP message has no
    // owning daemon session and is stored with `session_id = NULL`.
}

/// Why the daemon refused a locally authored post *before* it could reach the
/// transcript (G3 author authority + thread integrity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PostRejection {
    /// The caller claimed an `agent`/`system` author kind. Those rows are
    /// daemon-authored only (convened replies and audit lines); accepting a
    /// client-supplied one would let a browser forge an agent utterance *and*
    /// bypass the anti-loop guard, which skips trigger evaluation for
    /// agent-authored messages.
    ForgedAuthorKind,
    /// The `(author_id, author_kind)` pair is not on the room's roster, so the
    /// caller is claiming an identity this room never admitted.
    AuthorNotInRoster,
    /// `thread_parent_seq` violated the store's one-level thread policy. The
    /// store rejected it inside the append transaction; nothing was written.
    InvalidThreadParent,
}

/// The local post path's error: either the durable store failed, or the daemon
/// itself refused the post. Keeping them apart is what lets a refusal answer
/// with a fixed 4xx while a store fault keeps its existing mapping.
#[derive(Debug)]
pub(super) enum LocalPostError {
    /// An underlying store error (unknown room, federation corruption, SQLite).
    Store(ocean_store::RoomStoreError),
    /// A daemon-side refusal with a fixed, body-free reason.
    Rejected(PostRejection),
}

impl From<ocean_store::RoomStoreError> for LocalPostError {
    fn from(e: ocean_store::RoomStoreError) -> Self {
        Self::Store(e)
    }
}

/// Map a [`PostRejection`] onto its frozen `(status, body)` pair. The body
/// carries a stable machine code and never echoes the rejected author id,
/// claimed kind, or message body.
pub(super) fn post_rejection_response(
    rejection: PostRejection,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, code) = match rejection {
        PostRejection::ForgedAuthorKind => (StatusCode::FORBIDDEN, "forged_author_kind"),
        PostRejection::AuthorNotInRoster => (StatusCode::FORBIDDEN, "author_not_in_roster"),
        PostRejection::InvalidThreadParent => (StatusCode::BAD_REQUEST, "invalid_thread_parent"),
    };
    (status, Json(json!({ "ok": false, "error": code })))
}

/// Decide whether a client may author this local post (G3).
///
/// Two rules, both fail-closed:
///
/// 1. `agent` and `system` are daemon-only author kinds. The daemon writes
///    those rows itself ([`append_room_agent_reply`] and the audit appends);
///    a request that claims one is a forgery regardless of its id.
/// 2. The `(id, kind)` pair must already be on the roster. Membership — not
///    the request body — is the authority on who may speak in a room, so an
///    unknown id, or a known id claiming the wrong kind, is refused.
pub(super) fn classify_local_author<'a>(
    roster: &'a [RoomParticipant],
    author_id: &str,
    author_kind: RoomParticipantKind,
) -> Result<&'a str, PostRejection> {
    if matches!(
        author_kind,
        RoomParticipantKind::Agent | RoomParticipantKind::System
    ) {
        return Err(PostRejection::ForgedAuthorKind);
    }
    // Roster ids are canonical authority. Do not admit a trimmed spelling and
    // then persist the caller's non-canonical bytes (for example `" john "`).
    if author_id.is_empty() || author_id != author_id.trim() {
        return Err(PostRejection::AuthorNotInRoster);
    }
    roster
        .iter()
        .find(|participant| participant.id == author_id && participant.kind == author_kind)
        .map(|participant| participant.id.as_str())
        .ok_or(PostRejection::AuthorNotInRoster)
}

/// Read just the author of one thread root, as a bounded single-row query.
///
/// Uses the `LIMIT`ed [`RoomStore::transcript_page`] with `after_seq =
/// root_seq - 1` and `limit = 1`, so this never loads a transcript to answer a
/// one-row question. Returns `None` when no row with exactly `root_seq` exists
/// in this room; a caller treats that as "no thread-reply trigger", never as an
/// error.
fn thread_root_author(
    reg: &ocean_store::SqliteRoomStore,
    key: &RoomKey,
    root_seq: u64,
) -> Result<Option<String>, ocean_store::RoomStoreError> {
    // `seq` is 0-based, and `transcript_page` is exclusive on `after_seq`, so
    // seq 0 must page from the start rather than from `-1`.
    let after_seq = root_seq.checked_sub(1);
    let page = reg.transcript_page(key, after_seq, Some(1))?;
    Ok(page
        .messages
        .into_iter()
        .find(|m| m.seq == root_seq)
        .map(|m| m.author_id))
}

/// `POST /v1/rooms/persistent/{key}/messages` — append a chat message to the
/// transcript, then evaluate the room's trigger policy against any @-mentions in
/// the body. On a positive decision that resolves to an agent participant, emit a
/// `room_trigger` notice onto the agent event bus AND queue a real agent turn for
/// that agent (it reads the room context and posts its reply back into the
/// transcript). See `spawn_room_agent_turn` for the turn path (OCEAN-111/225).
pub(super) async fn room_post_message(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<RoomMessageRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    // Classification and the Local append share one store guard. Credential
    // installation can therefore linearize only before or after this commit,
    // never between a Local check and a later append.
    let append = with_rooms(&state, |reg| {
        if reg.get(&key)?.is_none() {
            return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()).into());
        }
        if reg.room_credential(&key)?.is_some() {
            return Ok(None);
        }
        if reg.room_access(&key)?.state != RoomAccessState::Local {
            return Err(ocean_store::RoomStoreError::FederationCorruption(
                "missing credential for non-local room".into(),
            )
            .into());
        }
        let roster = reg
            .get(&key)?
            .map(|rec| rec.room.participants)
            .unwrap_or_default();
        // G3 author authority: decide who may speak BEFORE anything is
        // written, under the same guard as the append, so a roster change
        // cannot land between the decision and the row. The returned id is the
        // exact roster-owned canonical spelling used for persistence.
        let canonical_author_id = classify_local_author(&roster, &req.author_id, req.author_kind)
            .map_err(LocalPostError::Rejected)?;
        // Read the thread root's author before appending: the reply itself is
        // not a valid trigger source, and after the append the root is one row
        // further back. `None` for a top-level post or a vanished root.
        let root_author = match req.thread_parent_seq {
            Some(parent_seq) => thread_root_author(reg, &key, parent_seq)?,
            None => None,
        };
        let msg = match reg.append_message_threaded(
            &key,
            canonical_author_id,
            req.author_kind,
            RoomMessageKind::Message,
            &req.body,
            Utc::now(),
            req.thread_parent_seq,
            // G3: session attribution is daemon-derived, never client-supplied.
            // An HTTP post has no owning daemon session.
            None,
        ) {
            Ok(msg) => msg,
            // A bad parent is a client mistake, not a server fault: keep it a
            // typed 400 instead of collapsing onto `RoomStoreError::Encode`.
            Err(ThreadAppendError::InvalidThreadParent { .. }) => {
                return Err(LocalPostError::Rejected(PostRejection::InvalidThreadParent))
            }
            Err(ThreadAppendError::Store(e)) => return Err(LocalPostError::Store(e)),
        };
        let policy = reg.trigger_policy(&key)?;
        Ok::<_, LocalPostError>(Some((msg, policy, roster, root_author)))
    });

    let (msg, policy, roster, root_author) = match append {
        Ok(Some(local)) => local,
        Ok(None) => {
            return match state
                .room_federation
                .enqueue_federated_message(&key, None, &req.body)
                .await
            {
                Ok(access) => (
                    StatusCode::ACCEPTED,
                    Json(json!({ "ok": true, "access": access })),
                ),
                Err(error) => intent_error_response(error),
            };
        }
        Err(LocalPostError::Rejected(rejection)) => return post_rejection_response(rejection),
        Err(LocalPostError::Store(ocean_store::RoomStoreError::UnknownRoom(_))) => {
            return intent_error_response(IntentError::NotFound)
        }
        Err(LocalPostError::Store(ocean_store::RoomStoreError::FederationCorruption(_))) => {
            return intent_error_response(IntentError::Store)
        }
        Err(LocalPostError::Store(e)) => return room_store_error_response(e),
    };
    publish_room_wake(&state, &key, &msg);

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
    let mut convened = std::collections::HashSet::new();
    if !matches!(req.author_kind, RoomParticipantKind::Agent) {
        // Every trigger source for THIS row, in a fixed order: each @-mention in
        // body order, then (G3) the thread-root author when this post is a reply.
        // A single evaluation loop keeps one convene footprint per agent.
        let events = parse_mentions(&req.body)
            .into_iter()
            .map(|participant_id| RoomTriggerEvent::Mention { participant_id })
            .chain(
                root_author
                    .into_iter()
                    .map(|participant_id| RoomTriggerEvent::ThreadReply { participant_id }),
            );
        for event in events {
            let decision = evaluate_trigger_policy(policy.as_ref(), &event);
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

            // One convene footprint per agent per posted row. Mentioning an
            // agent twice — or mentioning the same agent that owns the thread
            // root — must not queue two turns for one message.
            if !convened.insert(agent.id.clone()) {
                continue;
            }

            // Named-agent binding gate (TASK-9): a roster Agent participant must
            // ALSO resolve to a real folder-as-agent definition before any
            // convene footprint is written. A legacy phantom Agent (already in
            // the roster from before this gate, or whose folder was since
            // removed) must NOT claim a convene — emit NO `room_trigger` event
            // and NO `auto-convene:` audit line, and queue NO turn. Instead post
            // one honest System note via a direct `append_message`. System rows
            // skip trigger evaluation, and this is a direct store write (not a
            // recursive `room_post_message`), so it convenes nobody. `room_join`
            // blocks NEW unresolved agents; this gate catches the legacy ones.
            if resolve_named_agent(&agent.id).is_err() {
                let _ = append_room_message(
                    &state,
                    &key,
                    "system",
                    RoomParticipantKind::System,
                    RoomMessageKind::System,
                    &format!("agent '{}' is not bound; no turn queued", agent.id),
                );
                continue;
            }

            // Emit a notice onto the agent event bus so any subscriber sees the
            // convene. Uses the generic Extension event so it respects the
            // existing agent-event scoping rules.
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
            let _ = append_room_message(
                &state,
                &key,
                "system",
                RoomParticipantKind::System,
                RoomMessageKind::System,
                &format!(
                    "auto-convene: {} ({})",
                    decision.target_participant.clone().unwrap_or_default(),
                    decision.reason
                ),
            );

            spawn_room_agent_turn(state.clone(), key.clone(), agent, msg.seq, None);
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({ "ok": true, "message": msg, "triggers_fired": fired })),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateInviteBody {
    #[serde(default)]
    recipient_name: Option<String>,
    #[serde(default)]
    ttl_minutes: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RedeemInviteBody {
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegisterAgentsBody {
    agent_names: Vec<String>,
}

fn invalid_request_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok": false, "error": "invalid_request"})),
    )
}

pub(super) fn intent_error_response(error: IntentError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, code) = match error {
        IntentError::Invalid => (StatusCode::BAD_REQUEST, "invalid_request"),
        IntentError::NotFound => (StatusCode::NOT_FOUND, "room_not_found"),
        IntentError::Conflict => (StatusCode::CONFLICT, "federation_conflict"),
        IntentError::Forbidden => (StatusCode::FORBIDDEN, "federation_forbidden"),
        IntentError::InviteForbidden => (StatusCode::FORBIDDEN, "invite_forbidden"),
        IntentError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "federation_unavailable"),
        IntentError::Protocol => (StatusCode::BAD_GATEWAY, "federation_protocol"),
        IntentError::Store => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    (status, Json(json!({"ok": false, "error": code})))
}

pub(super) async fn room_create_invite(
    State(state): State<AppState>,
    Path(key): Path<String>,
    body: Result<Json<CreateInviteBody>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Ok(Json(body)) = body else {
        return invalid_request_response();
    };
    let ttl = body.ttl_minutes.unwrap_or(1440);
    if !(1..=10080).contains(&ttl) {
        return invalid_request_response();
    }
    let key = RoomKey::new(key.trim());
    match state
        .room_federation
        .create_invite(&key, body.recipient_name, ttl)
        .await
    {
        Ok(invite) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(invite).expect("InviteResponse serializes")),
        ),
        Err(error) => intent_error_response(error),
    }
}

pub(super) async fn room_redeem_invite(
    State(state): State<AppState>,
    body: Result<Json<RedeemInviteBody>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Ok(Json(body)) = body else {
        return invalid_request_response();
    };
    if body.code.trim().is_empty() {
        return invalid_request_response();
    }
    match state.room_federation.redeem_invite(&body.code).await {
        Ok(access) => (
            StatusCode::OK,
            Json(serde_json::to_value(access).expect("RoomAccessProjection serializes")),
        ),
        Err(error) => intent_error_response(error),
    }
}

pub(super) async fn room_register_agents(
    State(state): State<AppState>,
    Path(key): Path<String>,
    body: Result<Json<RegisterAgentsBody>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Ok(Json(body)) = body else {
        return invalid_request_response();
    };
    if body.agent_names.is_empty() || body.agent_names.len() > 32 {
        return invalid_request_response();
    }
    let key = RoomKey::new(key.trim());
    let preflight = with_rooms(&state, |store| {
        if store.get(&key)?.is_none() {
            return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
        }
        let credential = store.room_credential(&key)?;
        let access = store.room_access(&key)?;
        Ok::<_, ocean_store::RoomStoreError>((credential.is_some(), access.state))
    });
    match preflight {
        Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            return intent_error_response(IntentError::NotFound)
        }
        Err(_) => return intent_error_response(IntentError::Store),
        Ok((false, _)) => return intent_error_response(IntentError::Conflict),
        Ok((true, RoomAccessState::Revoked)) => {
            return intent_error_response(IntentError::Forbidden)
        }
        Ok((true, _)) => {}
    }
    let mut seen = HashSet::new();
    let mut resolved = Vec::with_capacity(body.agent_names.len());
    for requested in body.agent_names {
        if requested.trim().is_empty() {
            return invalid_request_response();
        }
        let Some((agent_name, descriptor)) = resolve_agent_registration(&requested) else {
            return invalid_request_response();
        };
        if !seen.insert(agent_name.clone()) {
            return invalid_request_response();
        }
        resolved.push((agent_name, descriptor));
    }
    let instance_id = match with_rooms(&state, |store| store.federation_instance_id()) {
        Ok(id) => id,
        Err(_) => return intent_error_response(IntentError::Store),
    };
    let inputs = resolved
        .into_iter()
        .map(|(agent_name, descriptor)| AgentRegistrationInput {
            registration_key: registration_key(&instance_id, &key, &agent_name),
            agent_name,
            descriptor,
        })
        .collect();
    match state.room_federation.register_agents(&key, inputs).await {
        Ok(access) => (
            StatusCode::OK,
            Json(serde_json::to_value(access).expect("RoomAccessProjection serializes")),
        ),
        Err(error) => intent_error_response(error),
    }
}

pub(super) async fn run_federated_trigger_dispatcher(
    state: AppState,
    mut receiver: mpsc::UnboundedReceiver<FederatedTriggerDispatch>,
    cancel: CancellationToken,
) {
    loop {
        let dispatch = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            dispatch = receiver.recv() => match dispatch {
                Some(dispatch) => dispatch,
                None => break,
            },
        };
        let agent_name = match with_rooms(&state, |store| {
            if store.get(&dispatch.room)?.is_none() {
                return Ok(None);
            }
            let Some(credential) = store.room_credential(&dispatch.room)? else {
                return Ok(None);
            };
            let access = store.room_access(&dispatch.room)?;
            if access.state == RoomAccessState::Revoked
                || !access.members.iter().any(|member| {
                    member.member_id == dispatch.target_member_id
                        && member.actor_type == ocean_core::FederatedActorType::Agent
                        && member.owner_member_id.as_deref()
                            == Some(credential.local_human_member_id.as_str())
                        && member.local_binding_available == Some(true)
                })
            {
                return Ok(None);
            }
            store.resolve_room_agent(&dispatch.room, &dispatch.target_member_id)
        }) {
            Ok(Some(name)) => name,
            _ => continue,
        };
        if resolve_named_agent(&agent_name).is_err() {
            continue;
        }
        let agent = RoomParticipant {
            id: agent_name.clone(),
            kind: RoomParticipantKind::Agent,
            display_name: agent_name.clone(),
        };
        state.agent_events.emit(AgentTurnEvent::Extension {
            extension: "room_trigger".into(),
            payload: json!({
                "room": dispatch.room.as_str(),
                "target": dispatch.target_member_id.clone(),
                "agent_name": agent_name,
                "reason": format!("on_mention: @{} mentioned", dispatch.target_member_id),
                "triggered_by_seq": dispatch.local_seq,
                "ledger_event_id": dispatch.ledger_event_id,
            }),
            scope: None,
        });
        spawn_room_agent_turn(
            state.clone(),
            dispatch.room,
            agent,
            dispatch.local_seq,
            Some(dispatch.target_member_id),
        );
    }
}

/// Extract `@id` mentions from a message body. A mention is `@` followed by a
/// run of id-safe characters (alphanumerics, `-`, `_`). Returns ids without the
/// leading `@`, de-duplicated in first-seen order.
pub(super) fn parse_mentions(body: &str) -> Vec<String> {
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
pub(super) fn resolve_agent_participant(
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
pub(super) fn room_agent_session_id(room: &RoomKey, participant_id: &str) -> AgentSessionId {
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
        let marker = if m.seq == triggered_by_seq {
            "  «— mention"
        } else {
            ""
        };
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
    federated_member_id: Option<String>,
) {
    tokio::spawn(async move {
        // Resolve a working directory for the turn. A `Room` may now carry its own
        // `workspace_root` (OCEAN-260): if it does, that binding is the project the
        // room belongs to, so the turn runs in that dir and resolves its owning
        // project from it via the reverse map (`project_for_workspace`, OCEAN-228).
        // If the room has no binding (None — the legacy default, and every room
        // created before OCEAN-260), we fall back to the daemon's launch dir and
        // key the session by room+agent, exactly as before. (Sessions that land in
        // a project's workspace are still associated back to that project on read,
        // via `find_by_workspace` in `enrich_session_detail`.)
        let room_workspace = with_rooms(&state, |reg| {
            reg.get(&room)
                .ok()
                .flatten()
                .and_then(|rec| rec.room.workspace_root)
        });

        let (cwd, project_id) = match room_workspace {
            Some(ws) => {
                // Bound room: cwd is the room's workspace. Resolve the owning
                // project (best-effort) so the turn is project-scoped; a lookup
                // error or "no project at this root" degrades to no project_id
                // rather than failing the convene.
                let project_id = state
                    .runtime
                    .project_for_workspace(&ws)
                    .ok()
                    .flatten()
                    .map(|p| p.id);
                (ws, project_id)
            }
            None => {
                // Unbound room (legacy): the daemon's launch dir, no project.
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".to_string());
                (cwd, None)
            }
        };

        let session_id = room_agent_session_id(&room, &agent.id);

        // Read the recent transcript tail (read-before-answer context). Lock is
        // dropped when `with_rooms` returns, before any await below.
        let tail = with_rooms(&state, |reg| reg.transcript(&room, None)).unwrap_or_default();
        let tail: Vec<_> = tail
            .into_iter()
            .rev()
            .take(ROOM_CONTEXT_TAIL)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let prompt = build_room_prompt(&room, &agent, &tail, triggered_by_seq);

        // Named-agent binding (TASK-9): a room agent turn must DRIVE a resolved
        // folder-as-agent, never a default assistant. Re-resolve the participant
        // id to a real AgentDef before building the turn; on failure fail-closed
        // (post an honest System note, queue NO turn) rather than silently
        // running the default model. `room_join` blocks NEW unresolved agents;
        // this re-resolve catches LEGACY phantoms already in stored rosters.
        // (room_post_message's gate already short-circuited a phantom before the
        // footprint, but a re-resolve here is defense-in-depth: the roster may
        // have changed between the mention and the spawn, or the folder removed.)
        let resolved = match resolve_named_agent(&agent.id) {
            Ok(r) => r,
            Err(_) => {
                if federated_member_id.is_none() {
                    let _ = append_room_message(
                        &state,
                        &room,
                        "system",
                        RoomParticipantKind::System,
                        RoomMessageKind::System,
                        &format!("agent '{}' is not bound; no turn queued", agent.id),
                    );
                }
                return;
            }
        };
        // Prepend the resolved agent's instructions as a steering layer, exactly
        // as `agent_turn` does for a named folder-as-agent.
        let prompt = match resolved.instructions_layer {
            Some(instr) => super::compose_folder_agent_prompt(&instr, &prompt),
            None => prompt,
        };

        // Does this session already exist on disk? If so we RESUME it (strict);
        // otherwise we create it under the deterministic id. This mirrors the
        // create-if-missing logic in `agent_turn`. `session_detail` errors on a
        // missing/corrupt session, so `Ok` ⇒ exists ⇒ resume.
        let is_new = state.runtime.session_detail(core_sid(session_id)).is_err();

        let request_id = Uuid::new_v4();
        // Auto-convene has no per-request flag, so the effective posture is the
        // operator's resolved global permission mode.
        let permission_mode = effective_permission_mode();
        let yolo = permission_mode == PermissionMode::SkipAll;
        let mut prompt_req = PromptRequest {
            prompt,
            images: None,
            request_id: Some(request_id),
            session_id: Some(core_sid(session_id)),
            create_if_missing: is_new,
            max_turns: None,
            yolo,
            cwd,
            // The room's workspace binding resolves to its owning project
            // (OCEAN-260); `None` for unbound rooms preserves the legacy posture.
            project_id,
            client_type: Some("room".to_string()),
            // Daemon-internal auto-convene: no external submitter, so no
            // decision_token. Permission gating here defers to OCEAN_YOLO.
            decision_token: None,
        };

        // The durable room trigger/audit footprint already committed before
        // this spawned turn. Wait for the shared lane rather than dropping the
        // acknowledged trigger; registration still happens only after admission.
        let session_lease = state.runtime.session_operation(core_sid(session_id)).await;
        emit_session_changed(&state.agent_events, session_id);

        let (_request_id, cancel) = register_running_request(
            &state.requests,
            &mut prompt_req,
            format!("auto-convene: {} in room {}", agent.id, room.as_str()),
            RequestState::Running,
        )
        .await;

        let control = build_prompt_control(
            &state,
            request_id,
            Some(core_sid(session_id)),
            permission_mode,
            cancel,
            None,
        );
        // Apply the resolved agent's declared tool allowlist, model, and tier-1
        // subprocess capabilities to this turn, exactly as `agent_turn` does:
        // allowlist narrows the toolset, model drives the turn (fail-soft to the
        // global model via `with_agent_model`'s empty-trim), and caps launch
        // per-turn and merge their tools (fail-soft). PRESERVED vs the prior
        // room path: no `without_tools()`, `yolo` from `effective_permission_mode()`,
        // and `decision_token: None` above — only the four resolved fields are
        // newly applied.
        let control = match resolved.tool_allowlist {
            Some(tools) => control.with_tool_allowlist(tools),
            None => control,
        };
        let control = control.with_agent_model(resolved.model);
        let control = match resolved.subprocess_caps {
            Some((root, caps)) => control.with_agent_capabilities(root, caps),
            None => control,
        };
        #[cfg(test)]
        capture_room_turn(&agent.id, &prompt_req.prompt, &control);

        let res = state
            .runtime
            .prompt_with_lease(prompt_req, control, &session_lease)
            .await;
        // Federated room turn: delivery is the legacy-bus record itself (origin
        // None), with no separate agent-bus terminal frame, so pass `None`.
        record_prompt_result(&state, request_id, &res, None, None).await;
        emit_session_changed(&state.agent_events, session_id);

        // Post the agent's reply back into the room as the agent participant.
        // The lock is taken synchronously here, after the await completed.
        if res.ok {
            let body = res.stdout.trim();
            if !body.is_empty() {
                if let Some(member_id) = federated_member_id.as_deref() {
                    if state
                        .room_federation
                        .enqueue_federated_message(&room, Some(member_id), body)
                        .await
                        .is_err()
                    {
                        tracing::warn!(room = %room, outcome = "agent_reply_enqueue_failed", "federated agent reply suppressed");
                    }
                } else {
                    // G3: thread the answer under the ROOT of the line that
                    // convened it. When the trigger row is itself a thread
                    // reply, its own `thread_parent_seq` is the root — parenting
                    // under the reply row would violate one-level threading and
                    // demote the answer to top-level. A top-level trigger is its
                    // own root. Stamp the daemon-derived session either way; a
                    // stale/invalid parent still degrades inside the helper.
                    let thread_root = with_rooms(&state, |store| {
                        store.transcript(&room, Some(triggered_by_seq.saturating_sub(1)))
                    })
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|m| m.seq == triggered_by_seq))
                    .and_then(|m| m.thread_parent_seq)
                    .unwrap_or(triggered_by_seq);
                    let _ =
                        append_room_agent_reply(&state, &room, &agent.id, body, Some(thread_root));
                }
            }
        } else if federated_member_id.is_none() {
            // Local rooms retain the historical failure audit row. Federated
            // failures stay operational-only to avoid divergent transcripts.
            let _ = append_room_message(
                &state,
                &room,
                "system",
                RoomParticipantKind::System,
                RoomMessageKind::System,
                &format!(
                    "auto-convene failed for {}: {}",
                    agent.id,
                    res.stderr.lines().next().unwrap_or("turn failed")
                ),
            );
        }
    });
}

#[derive(serde::Deserialize)]
pub(super) struct TranscriptQuery {
    /// If set, return only entries with `seq > after_seq` (live-tail).
    #[serde(default)]
    pub(super) after_seq: Option<u64>,
    /// Max rows to return in this page (OCEAN-249). Omitted ⇒ the store's default
    /// cap; any value is clamped to `MAX_TRANSCRIPT_LIMIT`. Transcript reads are
    /// never unbounded — page with the returned `next_seq` cursor.
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

/// Read one bounded transcript page for a room, transparently falling back to the
/// soft-closed audit view (OCEAN-249 + OCEAN-170).
///
/// The open path defers to `transcript_page` (the `LIMIT`ed query). For a closed
/// room — a finished call's frozen transcript that must stay queryable — the audit
/// getter still returns a (now `MAX_TRANSCRIPT_LIMIT`-bounded) record, so we apply
/// the same `after_seq` filter and `limit + 1` sentinel paging in memory to hand
/// back an identical `TranscriptPage` shape regardless of room state. `Ok(None)`
/// from the audit view (room never existed) is mapped back to `UnknownRoom` so the
/// handlers preserve their 404.
fn read_transcript_page(
    reg: &ocean_store::SqliteRoomStore,
    key: &RoomKey,
    after_seq: Option<u64>,
    limit: Option<usize>,
) -> Result<ocean_store::TranscriptPage, ocean_store::RoomStoreError> {
    use ocean_store::RoomStore as _;
    match reg.transcript_page(key, after_seq, limit) {
        // Open room (the live case): the store already paged it.
        Ok(page) => Ok(page),
        // Closed room: page the frozen audit transcript in-handler with the same
        // contract the store would apply.
        Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            match reg.get_including_closed(key) {
                Ok(Some(rec)) => {
                    let effective_limit = ocean_store::clamp_transcript_limit(limit);
                    let mut msgs: Vec<_> = rec
                        .transcript
                        .into_iter()
                        .filter(|m| after_seq.is_none_or(|after| m.seq > after))
                        .collect();
                    let has_more = msgs.len() > effective_limit;
                    if has_more {
                        msgs.truncate(effective_limit);
                    }
                    let next_seq = if has_more {
                        msgs.last().map(|m| m.seq)
                    } else {
                        None
                    };
                    Ok(ocean_store::TranscriptPage {
                        messages: msgs,
                        next_seq,
                        has_more,
                    })
                }
                // Genuinely no such room (never created): preserve the 404.
                Ok(None) => Err(ocean_store::RoomStoreError::UnknownRoom(key.clone())),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// `GET /v1/rooms/persistent/{key}/transcript?after_seq=N&limit=M` — read one
/// bounded page of a room's transcript, optionally only entries after a given seq.
///
/// Bounded + paginated (OCEAN-249): the read is capped (default cap when `limit`
/// is omitted, clamped to `MAX_TRANSCRIPT_LIMIT`), and the response carries
/// additive `next_seq` (cursor to replay as `after_seq`) and `has_more` fields so
/// a client can page through a long transcript instead of forcing a full-table
/// read on every call. The `transcript` array shape is unchanged.
///
/// Falls back to the audit (soft-closed) view when the room is closed: a finished
/// call closes its room on `CallEnded` (OCEAN-170), but its transcript must stay
/// queryable afterwards — that frozen record is the whole reason it was persisted.
pub(super) async fn room_transcript(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |reg| {
        read_transcript_page(reg, &key, q.after_seq, q.limit)
    });
    match result {
        Ok(page) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "transcript": page.messages,
                "next_seq": page.next_seq,
                "has_more": page.has_more,
            })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}/snapshot` — full room hydration in one read:
/// the room entity (id, name, roster, timestamps, trigger policy), its complete
/// transcript, and `last_seq` so the caller can immediately tail live updates via
/// `GET /v1/rooms/persistent/{key}/events?after_seq=last_seq`.
///
/// This is the store-backed realization of the collaboration model's "Room
/// hydration / snapshot" step (OCEAN-232): switching into a room must load full
/// state, not just subscribe to future events. Persistent rooms carry everything
/// hydration needs, so this endpoint serves the durable snapshot directly.
///
/// Like `room_get`/`room_transcript`, falls back to the soft-closed audit view so
/// a finished call's frozen room (closed on `CallEnded`, OCEAN-170) stays
/// hydratable for replay.
pub(super) async fn room_snapshot(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    // Hydrate room metadata (entity + roster) and the FIRST bounded transcript page
    // under one lock. The transcript is no longer the room's entire log poured into
    // one response (OCEAN-249): a long-lived call room would make every hydration a
    // full-table read. We serve `limit` rows + a `next_seq` cursor so the client
    // immediately knows whether to page (`/transcript?after_seq=next_seq`) or tail
    // (`/events?after_seq=last_seq`). Both reads prefer the live room and fall back
    // to the soft-closed audit view (OCEAN-170). The std mutex guard is dropped
    // inside `with_rooms`; it is never held across an `.await`.
    let result = with_rooms(&state, |reg| {
        // Room metadata: live first, then audit for a soft-closed room.
        let record = match reg.get(&key) {
            Ok(Some(rec)) => Ok(Some(rec)),
            Ok(None) => reg.get_including_closed(&key),
            Err(e) => Err(e),
        }?;
        let Some(record) = record else {
            return Ok(None);
        };
        // First bounded page of the transcript (from the start of the log).
        let page = read_transcript_page(reg, &key, q.after_seq, q.limit)?;
        // Access projection (S2-P1): the room's federated state, outbox, and
        // member roster (Local if no access row exists).
        let access = reg.room_access(&key)?;
        Ok(Some((record, page, access)))
    });
    match result {
        Ok(Some((rec, page, access))) => {
            let last_seq = page.messages.last().map(|m| m.seq);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "room": rec.room.clone(),
                    "participants": rec.room.participants,
                    "transcript": page.messages,
                    "last_seq": last_seq,
                    "next_seq": page.next_seq,
                    "has_more": page.has_more,
                    "access": access,
                })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no room with key '{key}'") })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub(super) struct RoomEventsQuery {
    /// Replay starts strictly after this room-scoped sequence number.
    #[serde(default)]
    after_seq: Option<u64>,
}

type RoomEventsError = (StatusCode, Json<serde_json::Value>);
type RoomTailSeam = (oneshot::Sender<()>, oneshot::Receiver<()>);

fn room_events_error(status: StatusCode, code: &str, error: impl Into<String>) -> RoomEventsError {
    (
        status,
        Json(json!({ "ok": false, "code": code, "error": error.into() })),
    )
}

fn room_resume_seq(
    headers: &HeaderMap,
    query: &RoomEventsQuery,
) -> Result<Option<u64>, RoomEventsError> {
    let Some(raw) = headers.get("last-event-id") else {
        return Ok(query.after_seq);
    };
    let value = raw.to_str().map_err(|_| {
        room_events_error(
            StatusCode::BAD_REQUEST,
            "invalid_last_event_id",
            "Last-Event-ID must be an unsigned integer",
        )
    })?;
    value.parse::<u64>().map(Some).map_err(|_| {
        room_events_error(
            StatusCode::BAD_REQUEST,
            "invalid_last_event_id",
            "Last-Event-ID must be an unsigned integer",
        )
    })
}

/// Page every durable row after `last_sent_seq`, sending in ascending seq order.
/// Each query is bounded; newly committed rows may extend the loop, while a
/// caught-up empty/final page returns control to the wake receiver.
async fn send_room_catch_up(
    state: &AppState,
    room: &RoomKey,
    last_sent_seq: &mut Option<u64>,
    tx: &mpsc::Sender<RoomMessage>,
) -> Result<bool, ocean_store::RoomStoreError> {
    loop {
        let page = with_rooms(state, |store| {
            store.transcript_page(room, *last_sent_seq, Some(128))
        })?;
        if page.messages.is_empty() {
            return Ok(true);
        }
        for message in page.messages {
            if last_sent_seq.is_some_and(|last| message.seq <= last) {
                continue;
            }
            let seq = message.seq;
            if tx.send(message).await.is_err() {
                return Ok(false);
            }
            *last_sent_seq = Some(seq);
        }
        if !page.has_more {
            return Ok(true);
        }
    }
}

async fn run_room_tail(
    state: AppState,
    room: RoomKey,
    resume: Option<u64>,
    mut hints: broadcast::Receiver<RoomWakeHint>,
    tx: mpsc::Sender<RoomMessage>,
    seam: Option<RoomTailSeam>,
) {
    let mut last_sent_seq = resume;
    match send_room_catch_up(&state, &room, &mut last_sent_seq, &tx).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(room = %room, %error, "room SSE replay failed");
            return;
        }
    }

    // Tests can hold this exact replay/live seam open. The broadcast receiver was
    // already subscribed, so hints accumulate while replay is paused; production
    // passes `None` and continues immediately.
    if let Some((ready, release)) = seam {
        let _ = ready.send(());
        tokio::select! {
            _ = tx.closed() => return,
            _ = release => {}
        }
    }

    loop {
        let hint = tokio::select! {
            _ = tx.closed() => return,
            hint = hints.recv() => hint,
        };
        match hint {
            Ok(hint) if hint.room != room || Some(hint.seq) <= last_sent_seq => continue,
            Ok(hint) => {
                let expected = last_sent_seq.map_or(0, |last| last.saturating_add(1));
                if hint.seq > expected {
                    tracing::debug!(
                        room = %room,
                        observed_seq = hint.seq,
                        ?last_sent_seq,
                        "room SSE observed seq gap; paging durable log"
                    );
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(room = %room, skipped, "room SSE wake receiver lagged; paging durable log");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }

        match send_room_catch_up(&state, &room, &mut last_sent_seq, &tx).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(room = %room, %error, "room SSE durable catch-up failed");
                return;
            }
        }
    }
}

#[allow(dead_code)]
fn room_message_tail(
    state: AppState,
    room: RoomKey,
    resume: Option<u64>,
    hints: broadcast::Receiver<RoomWakeHint>,
    seam: Option<RoomTailSeam>,
) -> ReceiverStream<RoomMessage> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(run_room_tail(state, room, resume, hints, tx, seam));
    ReceiverStream::new(rx)
}

/// `GET /v1/rooms/persistent/{key}/events?after_seq=N` — durable replay plus a
/// room-scoped live SSE tail. Every frame is `event: room_message`, `id: <seq>`
/// with the exact existing `RoomMessage` JSON. SQLite is authoritative; the
/// bounded broadcast carries wake hints only.
///
/// S2-P1 merged SSE: also carries `event: room_access` frames (no `id`) with
/// RoomAccessProjection JSON. An initial access frame ships before any messages;
/// a separate access-tail task re-reads on access wake hints.
pub(super) async fn room_events(
    State(state): State<AppState>,
    Path(raw_key): Path<String>,
    Query(query): Query<RoomEventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, RoomEventsError> {
    let resume = room_resume_seq(&headers, &query)?;
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return Err(room_events_error(
            StatusCode::BAD_REQUEST,
            "invalid_room_key",
            "invalid room key; must be non-empty",
        ));
    }
    let room = RoomKey::new(trimmed);
    if room.as_str().starts_with("call:") {
        return Err(room_events_error(
            StatusCode::BAD_REQUEST,
            "call_room_events_unsupported",
            "call-prefixed rooms do not support room event streams",
        ));
    }

    // Subscribe to BOTH wake buses BEFORE the first replay query.
    let message_hints = state.room_wakes.subscribe();
    let access_hints = state.room_access_wakes.subscribe();

    // Verify room exists (open rooms only) and read initial access snapshot.
    let initial_access = match with_rooms(&state, |store| {
        if store.get(&room)?.is_none() {
            return Err(ocean_store::RoomStoreError::UnknownRoom(room.clone()));
        }
        store.room_access(&room)
    }) {
        Ok(proj) => proj,
        Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            return Err(room_events_error(
                StatusCode::NOT_FOUND,
                "room_not_found",
                format!("no open room with key '{room}'"),
            ));
        }
        Err(error) => return Err(room_store_error_response(error)),
    };

    // Existing message tail as a stream of SSE events.
    let msg_stream = room_message_tail(state.clone(), room.clone(), resume, message_hints, None)
        .map(|message| -> Result<Event, Infallible> {
            let seq = message.seq.to_string();
            let data = serde_json::to_string(&message).expect("RoomMessage serializable");
            Ok(Event::default().id(seq).event("room_message").data(data))
        });

    // Access tail.
    let (access_tx, access_rx) = mpsc::channel::<RoomAccessProjection>(16);
    tokio::spawn(run_room_access_tail(
        state.clone(),
        room.clone(),
        Some(initial_access.clone()),
        access_hints,
        access_tx,
    ));
    let acc_stream = ReceiverStream::new(access_rx).map(|proj| -> Result<Event, Infallible> {
        let data = serde_json::to_string(&proj).expect("RoomAccessProjection serializable");
        Ok(Event::default().event("room_access").data(data))
    });

    // Merge: initial access frame first, then interleave messages + access updates.
    let init_data =
        serde_json::to_string(&initial_access).expect("RoomAccessProjection serializable");
    let init_event = Ok(Event::default().event("room_access").data(init_data));
    let merged = tokio_stream::once(init_event).chain(msg_stream.merge(acc_stream));
    let stream = sse_until_shutdown(merged, state.shutdown.clone());
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL)))
}

// ── S2-P1 access projection SSE tail ─────────────────────────────────────────

/// Run an access-projection tail: on every wake hint (or lag), re-read the
/// durable access projection and send it downstream if changed. Selects
/// `tx.closed()` while idle so client disconnect cleans up.
async fn run_room_access_tail(
    state: AppState,
    room: RoomKey,
    mut last_access: Option<RoomAccessProjection>,
    mut hints: broadcast::Receiver<RoomAccessWakeHint>,
    tx: mpsc::Sender<RoomAccessProjection>,
) {
    loop {
        let should_read = tokio::select! {
            _ = tx.closed() => return,
            res = hints.recv() => match res {
                Ok(hint) => hint.room == room,
                Err(broadcast::error::RecvError::Lagged(_)) => true,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        if !should_read {
            continue;
        }
        let proj = match with_rooms(&state, |store| store.room_access(&room)) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(room = %room, %e, "room access tail read failed");
                return;
            }
        };
        if last_access.as_ref() != Some(&proj) {
            last_access = Some(proj.clone());
            if tx.send(proj).await.is_err() {
                return;
            }
        }
    }
}

// ── S2-P1 outbox retry endpoint ─────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetryOutboxRequest {
    /// Outbox item to retry, identified by client_event_id. Must be non-empty;
    /// empty string is rejected at the handler level.
    pub(super) client_event_id: String,
}

/// Map a `RetryOutboxError` to an HTTP status + typed JSON body.
fn retry_outbox_error_response(
    err: ocean_store::RetryOutboxError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_store::RetryOutboxError::*;
    let (status, code) = match &err {
        RoomNotFound(_) => (StatusCode::NOT_FOUND, "room_not_found"),
        RoomNotFederated(_) => (StatusCode::CONFLICT, "room_not_federated"),
        RoomAccessRevoked(_) => (StatusCode::FORBIDDEN, "room_access_revoked"),
        OutboxItemNotFound { .. } => (StatusCode::NOT_FOUND, "outbox_item_not_found"),
        OutboxItemNotFailed { .. } => (StatusCode::CONFLICT, "outbox_item_not_failed"),
        Store(se) => match se {
            ocean_store::RoomStoreError::BadKey(_) => (StatusCode::BAD_REQUEST, "bad_key"),
            ocean_store::RoomStoreError::UnknownRoom(_) => {
                (StatusCode::NOT_FOUND, "room_not_found")
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        },
    };
    let body = match &err {
        Store(_) => json!({ "ok": false, "code": code, "error": "internal store error" }),
        _ => json!({ "ok": false, "code": code, "error": err.to_string() }),
    };
    (status, Json(body))
}

/// `POST /v1/rooms/persistent/{key}/outbox/retry` — retry a failed outbox item.
///
/// Returns `202 Accepted` with the updated access projection on success.
/// Mapping: 202 (retried), 400 (bad key / malformed body / empty id), 403 (revoked),
/// 404 (room or item not found), 409 (not federated / item not in Failed state), 500 (store).
pub(super) async fn room_retry_outbox(
    State(state): State<AppState>,
    Path(raw_key): Path<String>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: RetryOutboxRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "ok": false, "code": "invalid_retry_request", "error": e.to_string() }),
                ),
            );
        }
    };
    // Reject whitespace-only ids, but pass the original nonempty opaque id.
    let trimmed_id = req.client_event_id.trim();
    if trimmed_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "ok": false, "code": "invalid_retry_request", "error": "client_event_id must be non-empty" }),
            ),
        );
    }
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    let result = with_rooms(&state, |store| {
        store.retry_failed_outbox(&key, &req.client_event_id)
    });
    match result {
        Ok(proj) => {
            publish_room_access_wake(&state, &key);
            (
                StatusCode::ACCEPTED,
                Json(json!({ "ok": true, "access": proj })),
            )
        }
        Err(e) => retry_outbox_error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        fake_convene_state, write_agent_fixture, TestEnvRestore, AUTO_CONVENE_ENV_LOCK,
    };
    use axum::{
        body::{Body, Bytes},
        response::IntoResponse,
    };
    use tower::ServiceExt;

    #[derive(Clone)]
    struct RouteBedrock {
        roster: Arc<tokio::sync::Mutex<serde_json::Value>>,
    }

    impl RouteBedrock {
        fn new() -> Self {
            Self {
                roster: Arc::new(tokio::sync::Mutex::new(json!({"members":[]}))),
            }
        }
    }

    async fn route_control_register(Path(room): Path<String>) -> axum::response::Response {
        (
            StatusCode::CREATED,
            Json(json!({
                "room_id":room,
                "owner":{
                    "member_id":"11111111-1111-4111-8111-111111111111",
                    "actor_type":"user",
                    "role_in_room":"owner",
                    "display_name":"Owner"
                }
            })),
        )
            .into_response()
    }

    async fn route_control_invite(Json(body): Json<serde_json::Value>) -> axum::response::Response {
        let room = body["room_id"].as_str().unwrap();
        (
            StatusCode::CREATED,
            Json(json!({
                "code":"route-share-code",
                "invite":{
                    "role":"contributor",
                    "scopes":[format!("/rooms/{room}")],
                    "expiresAt":"2026-07-18T00:00:00Z"
                }
            })),
        )
            .into_response()
    }

    async fn route_control_redeem() -> axum::response::Response {
        (
            StatusCode::CREATED,
            Json(json!({
                "invite":{
                    "role":"contributor",
                    "scopes":["/rooms/route-redeem"],
                    "expiresAt":"2026-07-18T00:00:00Z"
                },
                "record":{
                    "role":"contributor",
                    "scopes":["/rooms/route-redeem"]
                }
            })),
        )
            .into_response()
    }

    async fn route_control_self_join(
        Path(_room): Path<String>,
        _body: Bytes,
    ) -> axum::response::Response {
        (
            StatusCode::CREATED,
            Json(json!({
                "member":{
                    "member_id":"22222222-2222-4222-8222-222222222222",
                    "actor_type":"user",
                    "role_in_room":"member",
                    "display_name":"Joined Human"
                }
            })),
        )
            .into_response()
    }

    async fn route_control_agents(
        State(fake): State<RouteBedrock>,
        Path(_room): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        let requested = body["agents"].as_array().unwrap();
        let members: Vec<_> = requested
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                json!({
                    "member_id":format!("33333333-3333-4333-8333-{index:012}"),
                    "owner_member_id":"22222222-2222-4222-8222-222222222222",
                    "actor_type":"agent",
                    "role_in_room":"member",
                    "display_name":agent["display_name"],
                    "public_agent_descriptor":{
                        "display_name":agent["display_name"],
                        "skills_count":agent["skills_count"],
                        "subagent_names":agent["subagent_names"]
                    },
                    "joined_at":"2026-07-17T00:00:00Z"
                })
            })
            .collect();
        let mut roster = vec![json!({
            "member_id":"22222222-2222-4222-8222-222222222222",
            "actor_type":"user",
            "role_in_room":"member",
            "display_name":"Joined Human",
            "joined_at":"2026-07-17T00:00:00Z"
        })];
        roster.extend(members.iter().cloned());
        *fake.roster.lock().await = json!({"members":roster});
        (StatusCode::CREATED, Json(json!({"members":members}))).into_response()
    }

    async fn route_control_members(
        State(fake): State<RouteBedrock>,
        Path(_room): Path<String>,
    ) -> axum::response::Response {
        Json(fake.roster.lock().await.clone()).into_response()
    }

    async fn start_route_bedrock() -> (String, tokio::task::JoinHandle<()>) {
        let fake = RouteBedrock::new();
        let app = axum::Router::new()
            .route(
                "/api/v1/rooms/{room}/register",
                axum::routing::post(route_control_register),
            )
            .route("/api/v1/invites", axum::routing::post(route_control_invite))
            .route(
                "/api/v1/invites/redeem",
                axum::routing::post(route_control_redeem),
            )
            .route(
                "/api/v1/rooms/{room}/members/self",
                axum::routing::post(route_control_self_join),
            )
            .route(
                "/api/v1/rooms/{room}/members/agents",
                axum::routing::post(route_control_agents),
            )
            .route(
                "/api/v1/rooms/{room}/members",
                axum::routing::get(route_control_members),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), server)
    }

    fn with_route_supervisor(mut state: AppState, base: &str) -> AppState {
        state.room_federation = crate::room_federation::FederationSupervisor::for_test(
            base,
            state.rooms.clone(),
            state.room_wakes.clone(),
            state.room_access_wakes.clone(),
            state.shutdown.clone(),
            std::time::Duration::from_secs(60),
        );
        state
    }

    fn dispatch_agent_projection(
        member_id: &str,
        owner_member_id: &str,
        display_name: &str,
    ) -> ocean_core::FederatedRoomMemberProjection {
        ocean_core::FederatedRoomMemberProjection {
            member_id: member_id.into(),
            owner_member_id: Some(owner_member_id.into()),
            actor_type: ocean_core::FederatedActorType::Agent,
            role_in_room: ocean_core::FederatedRoomRole::Member,
            display_name: display_name.into(),
            public_agent_descriptor: Some(PublicAgentDescriptor {
                display_name: display_name.into(),
                description: None,
                model_alias: None,
                skills_count: 0,
                subagent_names: vec![],
            }),
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(ocean_core::MemberPresence::Live),
            local_binding_available: Some(true),
        }
    }

    fn dispatch_human_projection(member_id: &str) -> ocean_core::FederatedRoomMemberProjection {
        ocean_core::FederatedRoomMemberProjection {
            member_id: member_id.into(),
            owner_member_id: None,
            actor_type: ocean_core::FederatedActorType::User,
            role_in_room: ocean_core::FederatedRoomRole::Member,
            display_name: "Reclassified Human".into(),
            public_agent_descriptor: None,
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(ocean_core::MemberPresence::Unavailable),
            local_binding_available: None,
        }
    }

    #[test]
    fn g3_author_classification_is_exact_and_fail_closed() {
        let roster = vec![
            RoomParticipant {
                id: "john".into(),
                kind: RoomParticipantKind::Human,
                display_name: "John".into(),
            },
            RoomParticipant {
                id: "helper".into(),
                kind: RoomParticipantKind::Agent,
                display_name: "Helper".into(),
            },
        ];

        assert_eq!(
            classify_local_author(&roster, "john", RoomParticipantKind::Human),
            Ok("john")
        );
        assert_eq!(
            classify_local_author(&roster, " john ", RoomParticipantKind::Human),
            Err(PostRejection::AuthorNotInRoster)
        );
        assert_eq!(
            classify_local_author(&roster, "unknown", RoomParticipantKind::Human),
            Err(PostRejection::AuthorNotInRoster)
        );
        assert_eq!(
            classify_local_author(&roster, "john", RoomParticipantKind::Bot),
            Err(PostRejection::AuthorNotInRoster)
        );
        assert_eq!(
            classify_local_author(&roster, "helper", RoomParticipantKind::Agent),
            Err(PostRejection::ForgedAuthorKind)
        );
        assert_eq!(
            classify_local_author(&roster, "system", RoomParticipantKind::System),
            Err(PostRejection::ForgedAuthorKind)
        );
    }

    #[test]
    fn g3_message_wire_rejects_client_session_attribution() {
        assert!(serde_json::from_value::<RoomMessageRequest>(json!({
            "author_id": "john",
            "author_kind": "human",
            "body": "hello",
            "session_id": "00000000-0000-0000-0000-000000000000"
        }))
        .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn g3_local_post_enforces_author_and_thread_authority_without_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("g3-authority");
        with_rooms(&state, |store| {
            store.create(key.clone(), "G3 Authority", None, Utc::now())?;
            store.add_participant(
                &key,
                RoomParticipant {
                    id: "john".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "John".into(),
                },
                Utc::now(),
            )?;
            store.add_participant(
                &key,
                RoomParticipant {
                    id: "helper".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Helper".into(),
                },
                Utc::now(),
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let initial_len = with_rooms(&state, |store| store.transcript(&key, None))
            .unwrap()
            .len();
        for (author_id, author_kind, expected_error) in [
            ("helper", RoomParticipantKind::Agent, "forged_author_kind"),
            ("system", RoomParticipantKind::System, "forged_author_kind"),
            (
                "unknown",
                RoomParticipantKind::Human,
                "author_not_in_roster",
            ),
            (" john ", RoomParticipantKind::Human, "author_not_in_roster"),
        ] {
            let (status, body) = room_post_message(
                State(state.clone()),
                Path(key.as_str().to_string()),
                Json(RoomMessageRequest {
                    author_id: author_id.into(),
                    author_kind,
                    body: "must not persist".into(),
                    thread_parent_seq: None,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body.0, json!({"ok": false, "error": expected_error}));
            assert_eq!(
                with_rooms(&state, |store| store.transcript(&key, None))
                    .unwrap()
                    .len(),
                initial_len,
                "rejected author {author_id:?} must not write"
            );
        }

        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "valid post".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body.0["message"]["author_id"], "john");
        assert_eq!(body.0["message"]["session_id"], serde_json::Value::Null);

        let before_invalid_parent = with_rooms(&state, |store| store.transcript(&key, None))
            .unwrap()
            .len();
        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "orphan reply".into(),
                thread_parent_seq: Some(u64::MAX),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0,
            json!({"ok": false, "error": "invalid_thread_parent"})
        );
        assert_eq!(
            with_rooms(&state, |store| store.transcript(&key, None))
                .unwrap()
                .len(),
            before_invalid_parent,
            "invalid thread parent must not write"
        );

        // One-level policy, exercised for real (not a nonexistent seq): a reply
        // to a REPLY row is exactly what live QA found silently accepted-or-lost.
        // Build root -> reply, then post against the reply and require the same
        // typed 400 with no write.
        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "thread root".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let root_seq = body.0["message"]["seq"].as_u64().unwrap();
        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "first reply".into(),
                thread_parent_seq: Some(root_seq),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let reply_seq = body.0["message"]["seq"].as_u64().unwrap();
        let before_nested = with_rooms(&state, |store| store.transcript(&key, None))
            .unwrap()
            .len();
        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "nested reply".into(),
                thread_parent_seq: Some(reply_seq),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0,
            json!({"ok": false, "error": "invalid_thread_parent"})
        );
        assert_eq!(
            with_rooms(&state, |store| store.transcript(&key, None))
                .unwrap()
                .len(),
            before_nested,
            "reply-to-a-reply must not write"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn g3_thread_reply_deduplicates_dispatch_and_attributes_agent_reply() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        write_agent_fixture(&agents_root, "helper", "", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let (_replay, mut trigger_rx) = state.agent_events.subscribe_with_replay(None);

        let key = RoomKey::new("g3-thread-dispatch");
        with_rooms(&state, |store| {
            store.create(
                key.clone(),
                "G3 Thread Dispatch",
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    on_thread_reply: true,
                    ..Default::default()
                }),
                Utc::now(),
            )?;
            for participant in [
                RoomParticipant {
                    id: "helper".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Helper".into(),
                },
                RoomParticipant {
                    id: "john".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "John".into(),
                },
            ] {
                store.add_participant(&key, participant, Utc::now())?;
            }
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let root = append_room_agent_reply(&state, &key, "helper", "agent root", None)
            .expect("agent root append");

        // Both the explicit mention and the thread-root author resolve to helper.
        // Policy evaluation reports both, but dispatch must queue helper once.
        let (status, body) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "john".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@helper following up".into(),
                thread_parent_seq: Some(root.seq),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let trigger_seq = body.0["message"]["seq"].as_u64().unwrap();
        assert!(trigger_seq > root.seq, "trigger must be a later reply row");
        assert_eq!(body.0["message"]["thread_parent_seq"], root.seq);
        assert_eq!(body.0["triggers_fired"].as_array().unwrap().len(), 2);

        let mut room_trigger_count = 0;
        while let Ok(event) = trigger_rx.try_recv() {
            if matches!(
                event.event,
                AgentTurnEvent::Extension { ref extension, .. } if extension == "room_trigger"
            ) {
                room_trigger_count += 1;
            }
        }
        assert_eq!(room_trigger_count, 1, "one agent gets one dispatch per row");

        let expected_session = room_agent_session_id(&key, "helper").to_string();
        let mut generated_reply = None;
        for _ in 0..100 {
            generated_reply = with_rooms(&state, |store| store.transcript(&key, None))
                .unwrap()
                .into_iter()
                .find(|message| {
                    message.seq > trigger_seq
                        && message.author_id == "helper"
                        && message.session_id.as_deref() == Some(expected_session.as_str())
                });
            if generated_reply.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let generated_reply = generated_reply.expect("convened agent reply must persist");
        // The convened answer hangs under the thread ROOT — not the reply row
        // that mentioned the agent, which one-level threading would reject.
        assert_eq!(generated_reply.thread_parent_seq, Some(root.seq));
        assert_eq!(
            generated_reply.session_id.as_deref(),
            Some(expected_session.as_str())
        );

        // A reply cannot parent another reply. Agent output degrades to top-level
        // rather than disappearing, while retaining daemon-derived attribution.
        let fallback = append_room_agent_reply(
            &state,
            &key,
            "helper",
            "fallback reply",
            Some(generated_reply.seq),
        )
        .expect("invalid agent parent must fall back");
        assert_eq!(fallback.thread_parent_seq, None);
        assert_eq!(
            fallback.session_id.as_deref(),
            Some(expected_session.as_str())
        );
    }

    #[test]
    fn p2c_registration_key_matches_frozen_known_answer() {
        let key = registration_key(
            "11111111-1111-4111-8111-111111111111",
            &RoomKey::new("room-a"),
            "sage",
        );
        assert!(
            key == "b8b1b37415ebcbcf56fc283df2f49841bd9e06775758115995e66869061ffd34",
            "registration-key known-answer mismatch"
        );
    }

    #[test]
    fn p2c_registration_key_prefixes_utf8_byte_lengths() {
        assert!(
            registration_key("é", &RoomKey::new("room-a"), "sage")
                == "54147903ac5f28cb0a613e96d8d74ed2b0ce053ca3e8c8020634c1abace3befb",
            "UTF-8 byte-length registration-key mismatch"
        );
        assert!(
            registration_key("ab", &RoomKey::new("c"), "d")
                != registration_key("a", &RoomKey::new("bc"), "d"),
            "length prefixes must separate otherwise ambiguous concatenations"
        );
    }

    #[test]
    fn p2c_control_bodies_deny_unknown_fields() {
        assert!(
            serde_json::from_str::<CreateInviteBody>(r#"{"ttl_minutes":1440,"role":"admin"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<RedeemInviteBody>(
            r#"{"code":"secret","token":"forbidden"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<RegisterAgentsBody>(
            r#"{"agent_names":["sage"],"path":"/tmp"}"#
        )
        .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_control_routes_return_frozen_raw_success_envelopes() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let agents_root = tmp.path().join("route-agents");
        write_agent_fixture(&agents_root, "route-agent", r#"model = "fake-ok""#, None);
        let max_agent_names: Vec<_> = (0..32).map(|index| format!("route-max-{index}")).collect();
        for name in &max_agent_names {
            write_agent_fixture(&agents_root, name, r#"model = "fake-ok""#, None);
        }
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let (base, server) = start_route_bedrock().await;

        let invite_state = with_route_supervisor(fake_convene_state(&tmp), &base);
        let invite_key = RoomKey::new("route-invite");
        with_rooms(&invite_state, |store| {
            store.create(invite_key.clone(), "Route Invite", None, Utc::now())
        })
        .unwrap();
        let response = crate::room_routes()
            .with_state(invite_state.clone())
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/route-invite/invites")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"recipient_name":"Peer","ttl_minutes":1440}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let invite: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(invite.as_object().unwrap().len(), 4);
        assert!(invite["code"] == "route-share-code", "invite code mismatch");
        assert_eq!(invite["expires_at"], "2026-07-18T00:00:00Z");
        assert_eq!(invite["room_key"], "route-invite");
        assert_eq!(invite["room_name"], "Route Invite");
        invite_state.room_federation.shutdown().await;

        let redeem_state = with_route_supervisor(fake_convene_state(&tmp), &base);
        let response = crate::room_routes()
            .with_state(redeem_state.clone())
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/invites/redeem")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"code":"route-code"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let redeem: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(redeem.get("ok").is_none());
        assert!(redeem.get("access").is_none());
        assert!(redeem.get("state").is_some());
        let redeem_json = redeem.to_string();
        assert!(!redeem_json.contains("route-code"));
        assert!(!redeem_json.contains("token"));
        redeem_state.room_federation.shutdown().await;

        let agent_state = with_route_supervisor(fake_convene_state(&tmp), &base);
        let agent_key = RoomKey::new("route-agents");
        with_rooms(&agent_state, |store| {
            store.create(agent_key.clone(), "Route Agents", None, Utc::now())?;
            store.install_room_credential(
                &agent_key,
                "agent-route-bearer",
                "22222222-2222-4222-8222-222222222222",
            )?;
            store.update_room_access_safe(
                &agent_key,
                Some(RoomAccessState::Connecting),
                None,
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let response = crate::room_routes()
            .with_state(agent_state.clone())
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/route-agents/members/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"agent_names":["route-agent"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let agents: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(agents.get("ok").is_none());
        assert!(agents.get("access").is_none());
        assert_eq!(agents["members"].as_array().unwrap().len(), 2);
        assert_eq!(
            agents["members"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|member| member["actor_type"] == "agent")
                .count(),
            1
        );
        let agents_json = agents.to_string();
        for private_field in ["registration_key", "bearer", "tools", "path"] {
            assert!(!agents_json.contains(private_field));
        }
        agent_state.room_federation.shutdown().await;

        let max_state = with_route_supervisor(fake_convene_state(&tmp), &base);
        let max_key = RoomKey::new("route-agents-max");
        with_rooms(&max_state, |store| {
            store.create(max_key.clone(), "Route Agents Max", None, Utc::now())?;
            store.install_room_credential(
                &max_key,
                "agent-route-bearer",
                "22222222-2222-4222-8222-222222222222",
            )?;
            store.update_room_access_safe(
                &max_key,
                Some(RoomAccessState::Connecting),
                None,
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let request_body = serde_json::to_vec(&json!({"agent_names":max_agent_names})).unwrap();
        let response = crate::room_routes()
            .with_state(max_state.clone())
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/route-agents-max/members/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let maximum: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 128 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(maximum["members"].as_array().unwrap().len(), 33);
        assert_eq!(
            maximum["members"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|member| member["actor_type"] == "agent")
                .count(),
            32
        );
        max_state.room_federation.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_http_message_ignores_claimed_identity_and_closed_agent_route_is_404() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("p2c-http-room");
        let human = "11111111-1111-4111-8111-111111111111";
        with_rooms(&state, |store| {
            store.create(key.clone(), "P2C HTTP", None, Utc::now())?;
            store.install_room_credential(&key, "private-bearer", human)?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, Json(body)) = room_post_message(
            State(state.clone()),
            Path(key.as_str().into()),
            Json(RoomMessageRequest {
                author_id: "browser-forgery".into(),
                author_kind: RoomParticipantKind::Agent,
                body: "federated intent".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["ok"], true);
        let projection = with_rooms(&state, |store| store.room_access(&key)).unwrap();
        assert_eq!(projection.outbox.len(), 1);
        assert_eq!(projection.outbox[0].author_member_id, human);
        assert!(with_rooms(&state, |store| store.transcript(&key, None))
            .unwrap()
            .is_empty());

        with_rooms(&state, |store| store.close(&key)).unwrap();
        let (status, Json(body)) = room_register_agents(
            State(state),
            Path(key.as_str().into()),
            Ok(Json(RegisterAgentsBody {
                agent_names: vec!["does-not-exist".into()],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "room_not_found");
    }

    #[tokio::test]
    async fn p2c_message_errors_use_frozen_stable_envelopes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let request = || RoomMessageRequest {
            author_id: "human".into(),
            author_kind: RoomParticipantKind::Human,
            body: "intent".into(),
            thread_parent_seq: None,
        };

        let (status, Json(body)) = room_post_message(
            State(state.clone()),
            Path("missing-room".into()),
            Json(request()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"ok":false,"error":"room_not_found"}));

        let closed = RoomKey::new("closed-message-room");
        let corrupt = RoomKey::new("nonlocal-without-credential");
        with_rooms(&state, |store| {
            store.create(closed.clone(), "Closed", None, Utc::now())?;
            store.close(&closed)?;
            store.create(corrupt.clone(), "Corrupt", None, Utc::now())?;
            store.update_room_access_safe(
                &corrupt,
                Some(RoomAccessState::Connecting),
                None,
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let (status, Json(body)) = room_post_message(
            State(state.clone()),
            Path(closed.as_str().into()),
            Json(request()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"ok":false,"error":"room_not_found"}));

        let (status, Json(body)) = room_post_message(
            State(state.clone()),
            Path(corrupt.as_str().into()),
            Json(request()),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"ok":false,"error":"internal_error"}));
        assert!(with_rooms(&state, |store| store.transcript(&corrupt, None))
            .unwrap()
            .is_empty());
        assert!(with_rooms(&state, |store| store.pending_outbox(&corrupt))
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_message_conversion_race_commits_local_or_pending_never_both() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("p2c-conversion-race");
        let human = "11111111-1111-4111-8111-111111111111";
        with_rooms(&state, |store| {
            store.create(key.clone(), "Conversion Race", None, Utc::now())
        })
        .unwrap();
        // G3: the local branch only accepts an admitted author, so the race is
        // still a race between a local commit and a federated hand-off.
        join_participant(
            &state,
            &key,
            "claimed-human",
            RoomParticipantKind::Human,
            "Claimed Human",
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let post = tokio::spawn({
            let state = state.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                room_post_message(
                    State(state),
                    Path(key.as_str().into()),
                    Json(RoomMessageRequest {
                        author_id: "claimed-human".into(),
                        author_kind: RoomParticipantKind::Human,
                        body: "conversion race".into(),
                        thread_parent_seq: None,
                    }),
                )
                .await
            }
        });
        let install = tokio::spawn({
            let state = state.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                with_rooms(&state, |store| {
                    store.install_room_credential(&key, "conversion-bearer", human)?;
                    store.update_room_access_safe(
                        &key,
                        Some(RoomAccessState::Connecting),
                        None,
                        None,
                    )?;
                    Ok::<_, ocean_store::RoomStoreError>(())
                })
                .unwrap();
            }
        });
        barrier.wait().await;
        let (post, install) = tokio::join!(post, install);
        let (status, _) = post.unwrap();
        install.unwrap();
        let transcript = with_rooms(&state, |store| store.transcript(&key, None)).unwrap();
        // Only chat rows are the race's output; the roster join above is fixture
        // setup and is always present.
        let chat: Vec<_> = transcript
            .iter()
            .filter(|m| m.kind == RoomMessageKind::Message)
            .collect();
        let pending = with_rooms(&state, |store| store.pending_outbox(&key)).unwrap();
        match status {
            StatusCode::CREATED => {
                assert_eq!(chat.len(), 1);
                assert!(pending.is_empty());
            }
            StatusCode::ACCEPTED => {
                assert!(chat.is_empty());
                assert_eq!(pending.len(), 1);
            }
            other => panic!("unexpected conversion-race status {other}"),
        }
    }

    #[tokio::test]
    async fn p2c_http_control_validation_is_400_before_network() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let state = fake_convene_state(&tmp);
        let noncanonical = RoomKey::new("Not-Canonical");
        with_rooms(&state, |store| {
            store.create(noncanonical.clone(), "Noncanonical", None, Utc::now())
        })
        .unwrap();
        let (status, Json(body)) = room_create_invite(
            State(state.clone()),
            Path(noncanonical.as_str().into()),
            Ok(Json(CreateInviteBody {
                recipient_name: None,
                ttl_minutes: Some(1440),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");

        let (status, _) = room_create_invite(
            State(state.clone()),
            Path("room".into()),
            Ok(Json(CreateInviteBody {
                recipient_name: None,
                ttl_minutes: Some(0),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = room_redeem_invite(
            State(state.clone()),
            Ok(Json(RedeemInviteBody { code: " ".into() })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = room_register_agents(
            State(state.clone()),
            Path("room".into()),
            Ok(Json(RegisterAgentsBody {
                agent_names: vec![],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let oversized_names: Vec<_> = (0..33).map(|index| format!("agent-{index}")).collect();
        let response = crate::room_routes()
            .with_state(state)
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/room/members/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({"agent_names":oversized_names})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let (status, Json(body)) = intent_error_response(IntentError::InviteForbidden);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "invite_forbidden");
        let (status, Json(body)) = intent_error_response(IntentError::Forbidden);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "federation_forbidden");
        let (status, Json(body)) = intent_error_response(IntentError::Conflict);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "federation_conflict");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_dispatch_uses_local_name_but_agent_reply_reenters_outbox() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        write_agent_fixture(
            &agents_root,
            "bound-agent",
            r#"model = "fake-ok""#,
            Some("FEDERATED_AGENT_INSTRUCTIONS"),
        );
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();

        let key = RoomKey::new("p2c-dispatch-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let member = "33333333-3333-4333-8333-333333333333";
        with_rooms(&state, |store| {
            store.create(key.clone(), "Dispatch", None, Utc::now())?;
            store.install_room_credential(&key, "private-bearer", human)?;
            store.bind_room_agent(&key, member, "bound-agent", "registration-key")?;
            store.update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[dispatch_agent_projection(member, human, "bound-agent")]),
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let dispatcher = tokio::spawn(run_federated_trigger_dispatcher(
            state.clone(),
            rx,
            cancel.clone(),
        ));
        tx.send(FederatedTriggerDispatch {
            room: key.clone(),
            ledger_event_id: "ledger-trigger".into(),
            local_seq: 7,
            target_member_id: member.into(),
        })
        .unwrap();

        let capture = wait_for_turn_capture("bound-agent")
            .await
            .expect("federated dispatch must run the bound local agent");
        // TASK-54: the instructions layer is framed with the folder-as-agent
        // sentinels for display stripping.
        assert!(capture.prompt.starts_with(
            "[folder-agent instructions]\nFEDERATED_AGENT_INSTRUCTIONS\n[end folder-agent instructions]\n\n"
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let outbox = with_rooms(&state, |store| store.pending_outbox(&key)).unwrap();
                if !outbox.is_empty() {
                    assert_eq!(outbox.len(), 1);
                    assert_eq!(outbox[0].author_member_id, member);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("agent reply must become a Pending outbox item");
        assert!(with_rooms(&state, |store| store.transcript(&key, None))
            .unwrap()
            .is_empty());

        // Main follows this order: federation producers stop, the dedicated
        // dispatcher cancellation fires, and the retained JoinHandle is joined.
        drop(tx);
        cancel.cancel();
        dispatcher.await.unwrap();
    }

    #[tokio::test]
    async fn p2c_unresolved_and_stale_dispatches_emit_no_turn_or_audit_row() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("stale-dispatch-agents");
        write_agent_fixture(&agents_root, "stale-agent", r#"model = "fake-ok""#, None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let unresolved = RoomKey::new("p2c-unresolved-dispatch");
        let removed = RoomKey::new("p2c-removed-binding");
        let reclassified = RoomKey::new("p2c-reclassified-binding");
        let remote = RoomKey::new("p2c-remote-binding");
        let member = "33333333-3333-4333-8333-333333333333";
        with_rooms(&state, |store| {
            for (key, agent_name) in [
                (&unresolved, "missing-folder-agent"),
                (&removed, "stale-agent"),
                (&reclassified, "stale-agent"),
                (&remote, "stale-agent"),
            ] {
                store.create(key.clone(), key.as_str(), None, Utc::now())?;
                store.install_room_credential(key, "private-bearer", "human")?;
                store.bind_room_agent(key, member, agent_name, "private-key")?;
            }
            store.update_room_access_safe(
                &unresolved,
                Some(RoomAccessState::Live),
                Some(&[dispatch_agent_projection(
                    member,
                    "human",
                    "missing-folder-agent",
                )]),
                None,
            )?;
            store.update_room_access_safe(
                &removed,
                Some(RoomAccessState::Live),
                Some(&[]),
                None,
            )?;
            store.update_room_access_safe(
                &reclassified,
                Some(RoomAccessState::Live),
                Some(&[dispatch_human_projection(member)]),
                None,
            )?;
            store.update_room_access_safe(
                &remote,
                Some(RoomAccessState::Live),
                Some(&[dispatch_agent_projection(
                    member,
                    "remote-human",
                    "stale-agent",
                )]),
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let dispatcher = tokio::spawn(run_federated_trigger_dispatcher(
            state.clone(),
            rx,
            CancellationToken::new(),
        ));
        for (room, ledger) in [
            (unresolved.clone(), "unresolved-ledger"),
            (removed.clone(), "removed-ledger"),
            (reclassified.clone(), "reclassified-ledger"),
            (remote.clone(), "remote-ledger"),
        ] {
            tx.send(FederatedTriggerDispatch {
                room,
                ledger_event_id: ledger.into(),
                local_seq: 1,
                target_member_id: member.into(),
            })
            .unwrap();
        }
        drop(tx);
        dispatcher.await.unwrap();

        assert!(ROOM_TURN_CAPTURES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        for key in [&unresolved, &removed, &reclassified, &remote] {
            assert!(with_rooms(&state, |store| store.transcript(key, None))
                .unwrap()
                .is_empty());
            assert!(with_rooms(&state, |store| store.pending_outbox(key))
                .unwrap()
                .is_empty());
        }
    }

    fn clear_turn_captures() {
        ROOM_TURN_CAPTURES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    async fn wait_for_turn_capture(agent_id: &str) -> Option<RoomTurnCapture> {
        for _ in 0..200 {
            let capture = ROOM_TURN_CAPTURES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .find(|capture| capture.agent_id == agent_id)
                .cloned();
            if capture.is_some() {
                return capture;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    fn create_mention_room(state: &AppState, key: &RoomKey) {
        with_rooms(state, |store| {
            store
                .create(
                    key.clone(),
                    "Named Agent Seam",
                    Some(RoomTriggerPolicy {
                        on_mention: true,
                        ..Default::default()
                    }),
                    Utc::now(),
                )
                .expect("room fixture");
        });
    }

    fn create_plain_room(state: &AppState, key: &RoomKey) {
        with_rooms(state, |store| {
            store
                .create(key.clone(), key.as_str(), None, Utc::now())
                .expect("room fixture");
        });
    }

    /// Admit a `human` Human participant (G3 author authority): a locally posted
    /// message is refused with 403 unless its `(id, kind)` pair is already on the
    /// roster. The join itself commits a `ParticipantJoined` row, so fixtures that
    /// assert on `seq` or on tail ordering account for it explicitly.
    fn join_human(state: &AppState, key: &RoomKey) {
        join_participant(state, key, "human", RoomParticipantKind::Human, "Human");
    }

    fn join_participant(
        state: &AppState,
        key: &RoomKey,
        id: &str,
        kind: RoomParticipantKind,
        display_name: &str,
    ) {
        with_rooms(state, |store| {
            store
                .add_participant(
                    key,
                    RoomParticipant {
                        id: id.into(),
                        kind,
                        display_name: display_name.into(),
                    },
                    Utc::now(),
                )
                .expect("roster fixture");
        });
    }

    async fn paused_tail(
        state: &AppState,
        key: &RoomKey,
        resume: Option<u64>,
    ) -> (ReceiverStream<RoomMessage>, oneshot::Sender<()>) {
        let hints = state.room_wakes.subscribe();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let stream = room_message_tail(
            state.clone(),
            key.clone(),
            resume,
            hints,
            Some((ready_tx, release_rx)),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), ready_rx)
            .await
            .expect("tail replay ready timeout")
            .expect("tail replay task dropped");
        (stream, release_tx)
    }

    async fn next_message(stream: &mut ReceiverStream<RoomMessage>) -> RoomMessage {
        tokio::time::timeout(std::time::Duration::from_millis(250), stream.next())
            .await
            .expect("room message exceeded 250ms")
            .expect("room tail ended")
    }

    async fn wait_for_wake_receivers(state: &AppState, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.room_wakes.receiver_count() == expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("room tail retained its wake receiver after client disconnect");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_releases_wake_receiver_when_client_disconnects_idle() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("disconnect-release");
        create_plain_room(&state, &key);
        assert_eq!(state.room_wakes.receiver_count(), 0);

        // Disconnect while the test seam is paused: the tail must observe the
        // closed mpsc receiver without waiting forever on the seam release.
        let (paused, release) = paused_tail(&state, &key, None).await;
        assert_eq!(state.room_wakes.receiver_count(), 1);
        drop(paused);
        wait_for_wake_receivers(&state, 0).await;
        assert!(release.send(()).is_err(), "paused tail task still alive");

        // Disconnect again after entering the ordinary idle live wait. No room
        // hint is published, so only `tx.closed()` can release the task.
        let (live, release) = paused_tail(&state, &key, None).await;
        assert_eq!(state.room_wakes.receiver_count(), 1);
        release.send(()).expect("release live tail");
        tokio::task::yield_now().await;
        drop(live);
        wait_for_wake_receivers(&state, 0).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_fans_out_post_once_in_order_under_250ms() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("fanout");
        create_plain_room(&state, &key);
        // The author must be admitted before it may post (G3). Its join row is
        // seq 0, so both tails resume after it and the posts are seq 1 and 2.
        join_human(&state, &key);

        let (mut first, release_first) = paused_tail(&state, &key, Some(0)).await;
        let (mut second, release_second) = paused_tail(&state, &key, Some(0)).await;
        release_first.send(()).unwrap();
        release_second.send(()).unwrap();

        let (status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "first".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let returned_at = tokio::time::Instant::now();
        let first_a = next_message(&mut first).await;
        let first_b = next_message(&mut second).await;
        assert!(returned_at.elapsed() < std::time::Duration::from_millis(250));
        assert_eq!(first_a, first_b);
        assert_eq!(first_a.seq, 1);

        let (status, _) = room_post_message(
            State(state),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "second".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(next_message(&mut first).await.seq, 2);
        assert_eq!(next_message(&mut second).await.seq, 2);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), first.next())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), second.next())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_last_event_resume_has_no_gap_or_duplicate() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("resume");
        create_plain_room(&state, &key);
        for body in ["zero", "one", "two", "three"] {
            append_room_message(
                &state,
                &key,
                "human",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                body,
            )
            .unwrap();
        }

        let (mut resumed, release) = paused_tail(&state, &key, Some(1)).await;
        release.send(()).unwrap();
        assert_eq!(next_message(&mut resumed).await.seq, 2);
        assert_eq!(next_message(&mut resumed).await.seq, 3);
        append_room_message(
            &state,
            &key,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "four",
        )
        .unwrap();
        assert_eq!(next_message(&mut resumed).await.seq, 4);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), resumed.next())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_isolates_other_rooms() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let room_a = RoomKey::new("room-a");
        let room_b = RoomKey::new("room-b");
        create_plain_room(&state, &room_a);
        create_plain_room(&state, &room_b);
        let (mut tail_a, release) = paused_tail(&state, &room_a, None).await;
        release.send(()).unwrap();

        append_room_message(
            &state,
            &room_b,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "private to B",
        )
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(75), tail_a.next())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_join_leave_and_auto_convene_audit_are_live() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let plain = RoomKey::new("roster-live");
        create_plain_room(&state, &plain);
        let (mut roster_tail, release) = paused_tail(&state, &plain, None).await;
        release.send(()).unwrap();

        let (status, _) = room_join(
            State(state.clone()),
            Path(plain.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "amy".into(),
                display_name: "Amy".into(),
                kind: RoomParticipantKind::Human,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            next_message(&mut roster_tail).await.kind,
            RoomMessageKind::ParticipantJoined
        );
        let (status, _) = room_leave(
            State(state.clone()),
            Path((plain.as_str().to_string(), "amy".into())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            next_message(&mut roster_tail).await.kind,
            RoomMessageKind::ParticipantLeft
        );

        let agents_root = tmp.path().join("agents");
        write_agent_fixture(&agents_root, "helper", "", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let convene = RoomKey::new("convene-live");
        create_mention_room(&state, &convene);
        // Author admission (G3) is seq 0 and the agent join is seq 1, so the
        // tail resumes after both and sees only the post + audit rows.
        join_human(&state, &convene);
        let (join_status, _) = room_join(
            State(state.clone()),
            Path(convene.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "helper".into(),
                display_name: "Helper".into(),
                kind: RoomParticipantKind::Agent,
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        let (mut convene_tail, release) = paused_tail(&state, &convene, Some(1)).await;
        release.send(()).unwrap();
        let (post_status, _) = room_post_message(
            State(state),
            Path(convene.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@helper report".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);
        assert_eq!(next_message(&mut convene_tail).await.body, "@helper report");
        let audit = next_message(&mut convene_tail).await;
        assert_eq!(audit.kind, RoomMessageKind::System);
        assert!(audit.body.starts_with("auto-convene: helper"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_buffers_replay_live_seam_hint() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("seam");
        create_plain_room(&state, &key);
        let (mut tail, release) = paused_tail(&state, &key, None).await;
        append_room_message(
            &state,
            &key,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "during seam",
        )
        .unwrap();
        release.send(()).unwrap();
        let message = next_message(&mut tail).await;
        assert_eq!(message.seq, 0);
        assert_eq!(message.body, "during seam");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_recovers_forced_broadcast_lag_from_durable_pages() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut state = fake_convene_state(&tmp);
        state.room_wakes = RoomWakeBus::new(2);
        let key = RoomKey::new("lagged");
        create_plain_room(&state, &key);
        let (mut tail, release) = paused_tail(&state, &key, None).await;
        for i in 0..40 {
            append_room_message(
                &state,
                &key,
                "human",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                &format!("line-{i}"),
            )
            .unwrap();
        }
        release.send(()).unwrap();

        let mut seen = Vec::new();
        for _ in 0..40 {
            seen.push(next_message(&mut tail).await.seq);
        }
        assert_eq!(seen, (0..40).collect::<Vec<_>>());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), tail.next())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_events_http_frame_uses_seq_id_and_exact_room_message_json() {
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("wire-frame");
        create_plain_room(&state, &key);
        let expected = append_room_message(
            &state,
            &key,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "exact JSON",
        )
        .unwrap();
        let app = super::super::room_routes().with_state(state);
        let request = axum::http::Request::builder()
            .uri("/v1/rooms/persistent/wire-frame/events")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut body = response.into_body();
        // First frame is always room_access (S2-P1 contract). Skip it.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("access frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let access_wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame")).unwrap();
        assert!(
            access_wire.contains("event: room_access"),
            "expected room_access first, got: {access_wire:?}"
        );
        // Second frame: room_message.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("message frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame")).unwrap();
        assert!(wire.contains("event: room_message\n"), "wire: {wire:?}");
        assert!(wire.contains("id: 0\n"), "wire: {wire:?}");
        let data = wire
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data line");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_events_http_last_event_id_wins_and_replays_strictly_after() {
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("header-resume");
        create_plain_room(&state, &key);
        for body in ["zero", "one", "two"] {
            append_room_message(
                &state,
                &key,
                "human",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                body,
            )
            .unwrap();
        }
        let app = super::super::room_routes().with_state(state);
        let request = axum::http::Request::builder()
            .uri("/v1/rooms/persistent/header-resume/events?after_seq=0")
            .header("last-event-id", "1")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        // Skip initial room_access frame (S2-P1 contract).
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("access frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let access_wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame")).unwrap();
        assert!(
            access_wire.contains("event: room_access"),
            "expected room_access first, got: {access_wire:?}"
        );
        // Next frame: room_message with resume from id 2.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("resume frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame")).unwrap();
        assert!(wire.contains("id: 2\n"), "wire: {wire:?}");
        assert!(wire.contains("\"body\":\"two\""), "wire: {wire:?}");
        assert!(!wire.contains("\"body\":\"one\""), "wire: {wire:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_events_rejects_invalid_resume_unknown_closed_and_call_rooms() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let open = RoomKey::new("open-room");
        let closed = RoomKey::new("closed-room");
        let call = RoomKey::new("call:excluded");
        for key in [&open, &closed, &call] {
            create_plain_room(&state, key);
        }
        with_rooms(&state, |store| store.close(&closed)).unwrap();

        let mut invalid = HeaderMap::new();
        invalid.insert("last-event-id", "not-a-number".parse().unwrap());
        let result = room_events(
            State(state.clone()),
            Path(open.as_str().to_string()),
            Query(RoomEventsQuery { after_seq: Some(7) }),
            invalid,
        )
        .await;
        let Err((status, Json(body))) = result else {
            panic!("invalid Last-Event-ID unexpectedly opened a stream");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_last_event_id");

        let mut numeric = HeaderMap::new();
        numeric.insert("last-event-id", "11".parse().unwrap());
        assert_eq!(
            room_resume_seq(&numeric, &RoomEventsQuery { after_seq: Some(7) }).unwrap(),
            Some(11),
            "numeric Last-Event-ID must win over after_seq"
        );

        for (key, expected_status, expected_code) in [
            ("missing", StatusCode::NOT_FOUND, "room_not_found"),
            (closed.as_str(), StatusCode::NOT_FOUND, "room_not_found"),
            (
                call.as_str(),
                StatusCode::BAD_REQUEST,
                "call_room_events_unsupported",
            ),
        ] {
            let result = room_events(
                State(state.clone()),
                Path(key.to_string()),
                Query(RoomEventsQuery::default()),
                HeaderMap::new(),
            )
            .await;
            let Err((status, Json(body))) = result else {
                panic!("rejected room unexpectedly opened a stream");
            };
            assert_eq!(status, expected_status);
            assert_eq!(body["code"], expected_code);
        }
    }

    /// The daemon authors every audit row as ("system", System). If a client can
    /// join as System, its ParticipantJoined marker is a System-authored
    /// transcript row indistinguishable from a genuine daemon audit line.
    /// Mutation: delete the System arm in `room_join` -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn system_kind_cannot_join_over_http() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("system-join");
        create_mention_room(&state, &key);

        let (status, Json(body)) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "system".into(),
                display_name: "Ocean System".into(),
                kind: RoomParticipantKind::System,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], json!("forged_participant_kind"));
        let room = with_rooms(&state, |store| store.get(&key))
            .expect("room lookup")
            .expect("room exists");
        assert!(
            room.room.participants.is_empty(),
            "a refused System join must leave no roster row"
        );
        assert!(
            room.transcript.is_empty(),
            "a refused System join must forge no transcript marker"
        );
    }

    /// Join and post must agree on what an id is. `classify_local_author`
    /// refuses an untrimmed id at POST, so accepting one at JOIN strands that
    /// participant forever with no way to discover why.
    /// Mutation: delete the id-normalization arm in `room_join` -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_refuses_the_untrimmed_id_that_post_would_strand() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("untrimmed-join");
        create_mention_room(&state, &key);

        for bad in [" john ", "", "   "] {
            let (status, Json(body)) = room_join(
                State(state.clone()),
                Path(key.as_str().to_string()),
                Json(RoomJoinRequest {
                    id: bad.into(),
                    display_name: "John".into(),
                    kind: RoomParticipantKind::Human,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "id {bad:?} must be refused");
            assert_eq!(body["code"], json!("invalid_participant_id"));
        }

        // The canonical spelling still joins, and can therefore post.
        let (status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "john".into(),
                display_name: "John".into(),
                kind: RoomParticipantKind::Human,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let room = with_rooms(&state, |store| store.get(&key))
            .expect("room lookup")
            .expect("room exists");
        assert_eq!(room.room.participants.len(), 1);
        assert_eq!(room.room.participants[0].id, "john");
    }

    /// An empty display name produces a " joined" marker with no author to read.
    /// Mutation: delete the display_name arm -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_refuses_an_empty_display_name() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("blank-name-join");
        create_mention_room(&state, &key);

        let (status, Json(body)) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "ghost".into(),
                display_name: "   ".into(),
                kind: RoomParticipantKind::Human,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], json!("invalid_display_name"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_agentdef_join_is_rejected() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_root).expect("agents root");
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let key = RoomKey::new("missing-join");
        create_mention_room(&state, &key);

        let (status, Json(body)) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "phantom".into(),
                display_name: "Phantom".into(),
                kind: RoomParticipantKind::Agent,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["code"], json!("agent_unresolved"));
        let room = with_rooms(&state, |store| store.get(&key))
            .expect("room lookup")
            .expect("room exists");
        assert!(room.room.participants.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_phantom_mention_has_no_convene_footprint_or_request() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_root).expect("agents root");
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let (_replay, mut trigger_rx) = state.agent_events.subscribe_with_replay(None);
        let key = RoomKey::new("legacy-phantom");
        create_mention_room(&state, &key);
        join_human(&state, &key);
        with_rooms(&state, |store| {
            store
                .add_participant(
                    &key,
                    RoomParticipant {
                        id: "phantom".into(),
                        kind: RoomParticipantKind::Agent,
                        display_name: "Phantom".into(),
                    },
                    Utc::now(),
                )
                .expect("legacy roster fixture");
        });

        let (status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@phantom report".into(),
                thread_parent_seq: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert!(matches!(
            trigger_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(state.requests.read().await.is_empty());
        assert!(wait_for_turn_capture("phantom").await.is_none());
        let transcript =
            with_rooms(&state, |store| store.transcript(&key, None)).expect("transcript");
        assert!(transcript
            .iter()
            .any(|message| message.author_kind == RoomParticipantKind::System
                && message.kind == RoomMessageKind::System
                && message.body == "agent 'phantom' is not bound; no turn queued"));
        assert!(!transcript
            .iter()
            .any(|message| message.body.starts_with("auto-convene:")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_folder_applies_instructions_model_allowlist_and_caps() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        write_agent_fixture(
            &agents_root,
            "bound-agent",
            r#"model = "fake-ok"
tools = ["read", "glob"]

[[subprocess_capability]]
name = "fixture-cap"
command = "/definitely/missing/ocean-fixture-cap"
args = ["--stdio"]
env = { FIXTURE = "1" }
"#,
            Some("BOUND_AGENT_INSTRUCTIONS"),
        );
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let key = RoomKey::new("bound-agent-profile");
        create_mention_room(&state, &key);
        join_human(&state, &key);

        let (join_status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "bound-agent".into(),
                display_name: "Bound Agent".into(),
                kind: RoomParticipantKind::Agent,
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);

        let (post_status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@bound-agent report".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);

        let capture = wait_for_turn_capture("bound-agent")
            .await
            .expect("resolved room turn must reach runtime dispatch");
        // TASK-54: the instructions layer is framed with the folder-as-agent
        // sentinels so display projections can strip it; the frame encloses the
        // instructions and terminates before the user's prompt.
        assert!(capture
            .prompt
            .starts_with("[folder-agent instructions]\nBOUND_AGENT_INSTRUCTIONS\n[end folder-agent instructions]\n\n"));
        assert_eq!(
            capture.tool_allowlist,
            Some(vec!["read".to_string(), "glob".to_string()])
        );
        assert_eq!(capture.model.as_deref(), Some("fake-ok"));
        let (root, caps) = capture
            .subprocess_caps
            .expect("declared subprocess capability must reach PromptControl");
        assert_eq!(root, agents_root.join("bound-agent"));
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].effective_name(), "fixture-cap");
        assert_eq!(caps[0].command, "/definitely/missing/ocean-fixture-cap");
        assert!(state.requests.read().await.values().any(|request| {
            request.status.session_id == Some(core_sid(room_agent_session_id(&key, "bound-agent")))
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_data_only_agentdef_is_resolved_and_queued() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let agents_root = tmp.path().join("agents");
        write_agent_fixture(&agents_root, "data-only", "", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let (_replay, mut trigger_rx) = state.agent_events.subscribe_with_replay(None);
        let key = RoomKey::new("data-only-profile");
        create_mention_room(&state, &key);
        join_human(&state, &key);

        let (join_status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "data-only".into(),
                display_name: "Data Only".into(),
                kind: RoomParticipantKind::Agent,
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);

        let (post_status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@data-only report".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);
        let event = trigger_rx
            .try_recv()
            .expect("resolved agent emits room_trigger");
        assert!(matches!(
            event.event,
            AgentTurnEvent::Extension { ref extension, .. } if extension == "room_trigger"
        ));

        let capture = wait_for_turn_capture("data-only")
            .await
            .expect("all-None profile must still dispatch");
        assert!(capture.tool_allowlist.is_none());
        assert!(capture.model.is_none());
        assert!(capture.subprocess_caps.is_none());
        assert!(!capture.prompt.starts_with("\n\n"));
        assert!(state.requests.read().await.values().any(|request| {
            request.status.session_id == Some(core_sid(room_agent_session_id(&key, "data-only")))
        }));
    }

    // ── S2-P1: snapshot access, merged SSE via router, outbox/retry ──────────

    use super::super::room_routes;
    use std::time::Duration;

    fn seed_access(state: &AppState, key: &RoomKey, access: RoomAccessProjection) {
        with_rooms(state, |store| {
            store
                .create(key.clone(), key.as_str(), None, Utc::now())
                .expect("room fixture");
            store
                .replace_room_access(key, &access)
                .expect("seed access");
        });
    }

    fn local_access() -> RoomAccessProjection {
        RoomAccessProjection {
            state: RoomAccessState::Local,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_open_room_includes_access() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-open");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["ok"], json!(true));
        assert!(body["room"].is_object());
        // Local access serializes as {"state":"local"} (skip_serializing_if omits defaults).
        assert_eq!(body["access"], json!({"state": "local"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_soft_closed_room_returns_200_with_local_access() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-closed");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), "Closed", None, Utc::now())
                .expect("create");
            store.close(&key).expect("close");
        });

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/snapshot"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["access"], json!({"state": "local"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_get_closed_is_404() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-get-closed");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), "ClosedGet", None, Utc::now())
                .expect("create");
            store.close(&key).expect("close");
        });

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_get_open_includes_access() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-get-open");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["access"], json!({"state": "local"}));
    }

    // ── Merged SSE routed tests ──────────────────────────────────────────────

    /// Read the next SSE frame from a streaming body (non-blocking).
    async fn next_sse_frame(body: &mut Body) -> String {
        use http_body_util::BodyExt as _;
        let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
            .await
            .expect("frame timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let bytes = frame.into_data().unwrap_or_default();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_initial_frame_is_room_access() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-sse-init");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let frame = next_sse_frame(&mut body).await;
        assert!(
            frame.contains("event: room_access"),
            "expected room_access, got: {frame}"
        );
        // No id line on the initial access frame.
        assert!(
            !frame.lines().any(|l| l.starts_with("id:")),
            "initial frame must not carry id"
        );
        let data = frame
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("data line");
        let parsed: serde_json::Value = serde_json::from_str(data).expect("valid JSON");
        assert_eq!(
            parsed,
            json!({"state": "local"}),
            "exact access payload mismatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_unknown_room_is_404() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get("/v1/rooms/persistent/nonexistent/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_room_isolation_message_not_leaked() {
        use http_body_util::BodyExt as _;
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let room_a = RoomKey::new("s2-iso-a");
        let room_b = RoomKey::new("s2-iso-b");
        seed_access(&state, &room_a, local_access());
        seed_access(&state, &room_b, local_access());

        let app_a = room_routes().with_state(state.clone());
        let resp_a = app_a
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{room_a}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);

        let _ = append_room_message(
            &state,
            &room_b,
            "author",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "only in B",
        );

        let mut body = resp_a.into_body();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        let mut saw_b_message = false;
        while tokio::time::Instant::now() < deadline {
            let frame = tokio::time::timeout(Duration::from_millis(50), body.frame()).await;
            if let Ok(Some(Ok(bytes))) = frame {
                let data = bytes.into_data().unwrap_or_default();
                let text = String::from_utf8_lossy(&data);
                if text.contains("only in B") {
                    saw_b_message = true;
                    break;
                }
            }
        }
        assert!(!saw_b_message, "room_a stream leaked room_b's message");
    }

    // ── S2-P1 outbox/retry routed tests ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_returns_202_on_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-ok");
        seed_access(
            &state,
            &key,
            RoomAccessProjection {
                state: RoomAccessState::Live,
                last_confirmed_global_sequence: Some(1),
                members: vec![],
                outbox: vec![RoomOutboxItem {
                    client_event_id: "evt-1".into(),
                    source_id: "src".into(),
                    source_sequence: 10,
                    author_member_id: "auth".into(),
                    event_type: "chat.message".into(),
                    payload: json!({"text": "hi"}),
                    mention_member_ids: vec![],
                    state: OutboxItemState::Failed,
                }],
            },
        );

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["ok"], json!(true));
        let outbox = body["access"]["outbox"].as_array().unwrap();
        let item = outbox
            .iter()
            .find(|i| i["client_event_id"] == "evt-1")
            .unwrap();
        assert_eq!(item["state"], json!("pending"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_local_room_is_409_not_403() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-local");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), "Local", None, Utc::now())
                .expect("create");
        });

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("room_not_federated"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_malformed_json_is_400() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-mal");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("invalid_retry_request"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_empty_id_is_400() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-empty");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("invalid_retry_request"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_unknown_room_is_404() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post("/v1/rooms/persistent/nonexistent/outbox/retry")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("room_not_found"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_unknown_fields_is_400() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-unk");
        seed_access(&state, &key, local_access());

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-1","extra":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("invalid_retry_request"));
    }

    // ── S2-P1 advanced proofs ────────────────────────────────────────────────

    use crate::tests::fake_convene_file_state;

    /// Create a room with Live access and one Failed outbox item.
    fn seed_live_with_failed(state: &AppState, key: &RoomKey, client_event_id: &str) {
        seed_access(
            state,
            key,
            RoomAccessProjection {
                state: RoomAccessState::Live,
                last_confirmed_global_sequence: Some(1),
                members: vec![],
                outbox: vec![RoomOutboxItem {
                    client_event_id: client_event_id.into(),
                    source_id: "src".into(),
                    source_sequence: 10,
                    author_member_id: "auth".into(),
                    event_type: "chat.message".into(),
                    payload: json!({"text": "hi"}),
                    mention_member_ids: vec![],
                    state: OutboxItemState::Failed,
                }],
            },
        );
    }

    /// Read one SSE event+data from a streaming body.
    async fn read_sse_frame(body: &mut Body) -> (String, serde_json::Value) {
        use http_body_util::BodyExt as _;
        let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
            .await
            .expect("frame timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let text = String::from_utf8_lossy(&frame.into_data().unwrap_or_default()).to_string();
        let event_type = text
            .lines()
            .find_map(|line| line.strip_prefix("event: "))
            .unwrap_or("")
            .to_string();
        let data: serde_json::Value = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .map(|d| serde_json::from_str(d).expect("valid JSON"))
            .unwrap_or(serde_json::Value::Null);
        (event_type, data)
    }

    /// Drain the body for `dur` and report whether any `room_access` frame arrived.
    async fn saw_access(body: &mut Body, dur: Duration) -> bool {
        use http_body_util::BodyExt as _;
        let deadline = tokio::time::Instant::now() + dur;
        while tokio::time::Instant::now() < deadline {
            let frame = tokio::time::timeout(Duration::from_millis(50), body.frame()).await;
            if let Ok(Some(Ok(bytes))) = frame {
                let data = bytes.into_data().unwrap_or_default();
                let text = String::from_utf8_lossy(&data);
                if text.contains("event: room_access") {
                    return true;
                }
            }
        }
        false
    }

    // ── Access SSE proofs ─────────────────────────────────────────────────────

    /// (4) Two-subscriber: assert both bodies receive the exact full
    /// committed projection after a retry-triggered wake.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_two_subscribers_both_receive_exact_access_update() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-both-sub");
        seed_live_with_failed(&state, &key, "evt-both");

        let expected_proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(1),
            members: vec![],
            outbox: vec![RoomOutboxItem {
                client_event_id: "evt-both".into(),
                source_id: "src".into(),
                source_sequence: 10,
                author_member_id: "auth".into(),
                event_type: "chat.message".into(),
                payload: json!({"text": "hi"}),
                mention_member_ids: vec![],
                state: OutboxItemState::Pending,
            }],
        };
        let expected = serde_json::to_value(&expected_proj).unwrap();

        let app1 = room_routes().with_state(state.clone());
        let resp1 = app1
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);
        let app2 = room_routes().with_state(state.clone());
        let resp2 = app2
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);

        let mut body1 = resp1.into_body();
        let mut body2 = resp2.into_body();

        // Both start with initial room_access.
        let (ev1, _) = read_sse_frame(&mut body1).await;
        assert_eq!(ev1, "room_access");
        let (ev2, _) = read_sse_frame(&mut body2).await;
        assert_eq!(ev2, "room_access");

        // Retry triggers access change + wake.
        let app_retry = room_routes().with_state(state.clone());
        let resp = app_retry
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-both"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Both must receive a follow-up room_access frame.
        let (ev1b, data1) = read_sse_frame(&mut body1).await;
        assert_eq!(ev1b, "room_access");
        assert_eq!(data1, expected, "subscriber 1 mismatched");
        let (ev2b, data2) = read_sse_frame(&mut body2).await;
        assert_eq!(ev2b, "room_access");
        assert_eq!(data2, expected, "subscriber 2 mismatched");
    }

    /// (3) Dedup: same-room unchanged hint produces no access frame. Also proves
    /// room isolation (cross-room wake does not leak).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_access_dedup_and_room_isolation() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let room_a = RoomKey::new("s2-acc-iso-a");
        let room_b = RoomKey::new("s2-acc-iso-b");
        seed_live_with_failed(&state, &room_a, "evt-a");
        seed_live_with_failed(&state, &room_b, "evt-b");

        let app_a = room_routes().with_state(state.clone());
        let resp_a = app_a
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{room_a}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);
        let mut body_a = resp_a.into_body();

        // Consume initial access frame.
        let (ev, data) = read_sse_frame(&mut body_a).await;
        assert_eq!(ev, "room_access");
        assert_eq!(data["outbox"][0]["client_event_id"], json!("evt-a"));

        // 1) Publish same-room unchanged hint → must produce NO access frame.
        publish_room_access_wake(&state, &room_a);
        assert!(
            !saw_access(&mut body_a, Duration::from_millis(300)).await,
            "same-room unchanged hint produced spurious access frame"
        );

        // 2) Retry on room_b → must NOT deliver access frame to room_a.
        let app_retry = room_routes().with_state(state.clone());
        let resp = app_retry
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{room_b}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(
            !saw_access(&mut body_a, Duration::from_millis(300)).await,
            "room_a received access frame from room_b retry"
        );
    }

    /// (1) Lag recovery (direct mpsc, deterministic): pre-subscribe cap-1
    /// receiver, capture initial=0, store 42, overflow BEFORE spawning tail,
    /// spawn with last_access=0, assert mpsc = exact 42, no second frame.
    #[tokio::test]
    async fn merged_sse_access_lag_recovers_durable_no_rescue_hint() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let mut state = fake_convene_state(&tmp);
        state.room_access_wakes = RoomAccessWakeBus::new(1);
        let key = RoomKey::new("s2-acc-lag");

        // Seed initial: seq=0.
        let initial = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(0),
            members: vec![],
            outbox: vec![],
        };
        seed_access(&state, &key, initial.clone());

        // Pre-subscribe cap-1 receiver BEFORE spawning the tail.
        let hints = state.room_access_wakes.subscribe();

        // Store durable seq=42.
        with_rooms(&state, |store| {
            store
                .replace_room_access(
                    &key,
                    &RoomAccessProjection {
                        state: RoomAccessState::Live,
                        last_confirmed_global_sequence: Some(42),
                        members: vec![],
                        outbox: vec![],
                    },
                )
                .expect("replace");
        });

        // Overflow capacity-1 bus BEFORE spawning tail.
        for _ in 0..10 {
            state.room_access_wakes.publish(&key);
        }

        // Spawn tail with last_access=0 + pre-filled receiver.
        let (tx, mut rx) = mpsc::channel::<RoomAccessProjection>(16);
        tokio::spawn(run_room_access_tail(
            state.clone(),
            key.clone(),
            Some(initial),
            hints,
            tx,
        ));

        // Tail must detect Lagged, re-read durable (seq=42), send it.
        let proj = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(
            proj.last_confirmed_global_sequence,
            Some(42),
            "lag did not recover durable seq=42"
        );

        // No second hint.
        let second = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(second.is_err(), "spurious second projection after lag");
    }

    // ── Dual receiver cleanup ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_sse_drop_releases_both_message_and_access_receivers() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-cleanup");
        seed_access(&state, &key, local_access());

        assert_eq!(state.room_wakes.receiver_count(), 0);
        assert_eq!(state.room_access_wakes.receiver_count(), 0);

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.room_wakes.receiver_count(), 1);
        assert_eq!(state.room_access_wakes.receiver_count(), 1);

        drop(resp);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if state.room_wakes.receiver_count() == 0
                && state.room_access_wakes.receiver_count() == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            state.room_wakes.receiver_count(),
            0,
            "message receiver not released"
        );
        assert_eq!(
            state.room_access_wakes.receiver_count(),
            0,
            "access receiver not released"
        );
    }

    // ── Retry-outbox advanced matrices ────────────────────────────────────────

    fn seed_pending_outbox(state: &AppState, key: &RoomKey) {
        seed_access(
            state,
            key,
            RoomAccessProjection {
                state: RoomAccessState::Live,
                last_confirmed_global_sequence: Some(1),
                members: vec![],
                outbox: vec![RoomOutboxItem {
                    client_event_id: "evt-pending".into(),
                    source_id: "src".into(),
                    source_sequence: 10,
                    author_member_id: "auth".into(),
                    event_type: "chat.message".into(),
                    payload: json!({"text": "p"}),
                    mention_member_ids: vec![],
                    state: OutboxItemState::Pending,
                }],
            },
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_pending_item_is_409() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-pending");
        seed_pending_outbox(&state, &key);
        let app = room_routes().with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-pending"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("outbox_item_not_failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_unknown_item_is_404() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-noitem");
        seed_live_with_failed(&state, &key, "evt-known");
        let app = room_routes().with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("outbox_item_not_found"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_revoked_is_403() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-revoked");
        seed_access(
            &state,
            &key,
            RoomAccessProjection {
                state: RoomAccessState::Revoked,
                last_confirmed_global_sequence: Some(1),
                members: vec![],
                outbox: vec![RoomOutboxItem {
                    client_event_id: "evt-rev".into(),
                    source_id: "src".into(),
                    source_sequence: 10,
                    author_member_id: "auth".into(),
                    event_type: "chat.message".into(),
                    payload: json!({"text": "rev"}),
                    mention_member_ids: vec![],
                    state: OutboxItemState::Failed,
                }],
            },
        );
        let app = room_routes().with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-rev"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("room_access_revoked"));
    }

    /// (8) Opaque-id preservation: whitespace-trim rejects empty,
    /// but original nonempty spaced id passes through and matches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_whitespace_trim_rejects_empty_preserves_opaque() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-ws");
        // Empty trimmed id → 400.
        seed_access(&state, &key, local_access());
        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"   "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], json!("invalid_retry_request"));

        // Spaced nonempty id must be preserved and match stored item.
        let key2 = RoomKey::new("s2-retry-opaque");
        seed_live_with_failed(&state, &key2, "  evt-with-spaces  ");
        let app2 = room_routes().with_state(state);
        let resp2 = app2
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key2}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"  evt-with-spaces  "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::ACCEPTED);
        let body2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let item = body2["access"]["outbox"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["client_event_id"] == "  evt-with-spaces  ")
            .unwrap();
        assert_eq!(item["state"], json!("pending"));
    }

    /// (6) Non-object body: table-driven null / array / string / number / bool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_non_object_bodies_are_400() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-retry-nonobj");
        seed_access(&state, &key, local_access());

        let cases: &[(&str, &str)] = &[
            ("null", "null"),
            ("array", r#"["a","b"]"#),
            ("string", r#""not-an-object""#),
            ("number", "42"),
            ("bool", "true"),
        ];
        for (label, body_str) in cases {
            let app = room_routes().with_state(state.clone());
            let resp = app
                .oneshot(
                    axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                        .header("content-type", "application/json")
                        .body(Body::from(*body_str))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{label}: expected 400, got {}",
                resp.status()
            );
            let body_json: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                body_json["code"],
                json!("invalid_retry_request"),
                "{label}: wrong code"
            );
        }
    }

    /// (5) Exact 202 envelope: compare whole JSON against expected projection.
    /// Also asserts exactly one access wake on success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_success_has_exact_202_envelope_and_one_wake() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-202-env");
        seed_live_with_failed(&state, &key, "evt-env");

        let mut pre_rx = state.room_access_wakes.subscribe();

        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"client_event_id":"evt-env"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        // Build expected from typed projection (matches serde serialization exactly).
        let expected_proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(1),
            members: vec![],
            outbox: vec![RoomOutboxItem {
                client_event_id: "evt-env".into(),
                source_id: "src".into(),
                source_sequence: 10,
                author_member_id: "auth".into(),
                event_type: "chat.message".into(),
                payload: json!({"text": "hi"}),
                mention_member_ids: vec![],
                state: OutboxItemState::Pending,
            }],
        };
        let expected_access = serde_json::to_value(&expected_proj).unwrap();
        let expected = json!({ "ok": true, "access": expected_access });
        assert_eq!(body, expected, "exact 202 envelope mismatch");

        // Exactly one wake, no second.
        let _ = tokio::time::timeout(Duration::from_millis(500), pre_rx.recv())
            .await
            .expect("first wake timeout")
            .expect("first wake not sent");
        let second = tokio::time::timeout(Duration::from_millis(100), pre_rx.recv()).await;
        assert!(second.is_err(), "woke more than once on single success");
    }

    /// (7) No-wake on every error class: prove zero access hints for
    /// 400 / 403 / 404 / 409 / 500 paths.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_outbox_no_access_wake_any_error_class() {
        use tokio::sync::broadcast::error::TryRecvError;
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;

        // --- 400: malformed body ---
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let state = fake_convene_state(&tmp);
            let key = RoomKey::new("s2-nw-400");
            seed_access(&state, &key, local_access());
            let mut rx = state.room_access_wakes.subscribe();
            let _keep = state.clone(); // survives router move
            let app = room_routes().with_state(state);
            let resp = app
                .oneshot(
                    axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                        .header("content-type", "application/json")
                        .body(Body::from("not json"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            // Only Err(Empty) proves no wake was published.
            assert!(
                matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                "wake on 400"
            );
        }

        // --- 403: revoked ---
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let state = fake_convene_state(&tmp);
            let key = RoomKey::new("s2-nw-403");
            seed_access(
                &state,
                &key,
                RoomAccessProjection {
                    state: RoomAccessState::Revoked,
                    last_confirmed_global_sequence: Some(1),
                    members: vec![],
                    outbox: vec![RoomOutboxItem {
                        client_event_id: "evt-403".into(),
                        source_id: "s".into(),
                        source_sequence: 1,
                        author_member_id: "a".into(),
                        event_type: "chat.message".into(),
                        payload: json!({}),
                        mention_member_ids: vec![],
                        state: OutboxItemState::Failed,
                    }],
                },
            );
            let mut rx = state.room_access_wakes.subscribe();
            let _keep = state.clone();
            let app = room_routes().with_state(state);
            let resp = app
                .oneshot(
                    axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"client_event_id":"evt-403"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            assert!(
                matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                "wake on 403"
            );
        }

        // --- 404: unknown room ---
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let state = fake_convene_state(&tmp);
            let mut rx = state.room_access_wakes.subscribe();
            let _keep = state.clone();
            let app = room_routes().with_state(state);
            let resp = app
                .oneshot(
                    axum::http::Request::post("/v1/rooms/persistent/nonexistent/outbox/retry")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"client_event_id":"x"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            assert!(
                matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                "wake on 404"
            );
        }

        // --- 409: local room ---
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let state = fake_convene_state(&tmp);
            let key = RoomKey::new("s2-nw-409");
            with_rooms(&state, |store| {
                store
                    .create(key.clone(), "Local", None, Utc::now())
                    .expect("create");
            });
            let mut rx = state.room_access_wakes.subscribe();
            let _keep = state.clone();
            let app = room_routes().with_state(state);
            let resp = app
                .oneshot(
                    axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"client_event_id":"evt-1"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT);
            assert!(
                matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                "wake on 409"
            );
        }

        // --- 500: real store error via rusqlite corruption ---
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let (state, db_path) = fake_convene_file_state(&tmp);
            let key = RoomKey::new("s2-nw-500");
            seed_live_with_failed(&state, &key, "evt-500");
            let mut rx = state.room_access_wakes.subscribe();

            // Corrupt: drop the room_access table via a separate connection.
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS room_access")
                .unwrap();
            conn.close().ok();

            let _keep = state.clone();
            let app = room_routes().with_state(state);
            let resp = app
                .oneshot(
                    axum::http::Request::post(format!("/v1/rooms/persistent/{key}/outbox/retry"))
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"client_event_id":"evt-500"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "expected 500 from store error, got {}",
                resp.status()
            );
            // Exact sanitized body: Store errors always produce this fixed message.
            let body: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let expected = json!({
                "ok": false,
                "code": "internal_error",
                "error": "internal store error"
            });
            assert_eq!(body, expected, "500 body not exact sanitized form");

            // Zero wake on 500.
            assert!(
                matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                "wake on 500"
            );
        }
    }
}
