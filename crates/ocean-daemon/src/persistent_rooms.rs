use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use chrono::Utc;
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};
use ocean_core::{
    evaluate_trigger_policy, PermissionMode, PromptRequest, RequestState, RoomKey, RoomMessage,
    RoomMessageKind, RoomParticipant, RoomParticipantKind, RoomTriggerEvent, RoomTriggerPolicy,
};
use ocean_store::RoomStore;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use uuid::Uuid;

use super::{
    build_prompt_control, core_sid, record_prompt_result, sdk_sid, sse_until_shutdown, AppState,
    SSE_KEEPALIVE_INTERVAL,
};
use crate::request_control::register_running_request;
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

    fn publish(&self, room: &RoomKey, message: &RoomMessage) {
        let _ = self.tx.send(RoomWakeHint {
            room: room.clone(),
            seq: message.seq,
        });
    }
}

/// Publish only after the store adapter has returned, which means the allocating
/// SQLite transaction has committed. A missing subscriber is harmless: hints
/// are advisory and reconnect/recovery always pages the durable log.
fn publish_room_wake(state: &AppState, room: &RoomKey, message: &RoomMessage) {
    state.room_wakes.publish(room, message);
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

/// `GET /v1/rooms/persistent/{key}` — one persistent room (with its transcript).
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
pub(super) struct RoomMessageRequest {
    /// Author participant id (or a synthetic id like `"system"`).
    pub(super) author_id: String,
    /// Author kind for attribution. Defaults to `human`.
    #[serde(default = "default_participant_kind")]
    pub(super) author_kind: RoomParticipantKind,
    /// Message body. `@id` mentions in the body drive trigger evaluation.
    pub(super) body: String,
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
                let _ = append_room_message(
                    &state,
                    &room,
                    "system",
                    RoomParticipantKind::System,
                    RoomMessageKind::System,
                    &format!("agent '{}' is not bound; no turn queued", agent.id),
                );
                return;
            }
        };
        // Prepend the resolved agent's instructions as a steering layer, exactly
        // as `agent_turn` does for a named folder-as-agent.
        let prompt = match resolved.instructions_layer {
            Some(instr) => format!("{instr}\n\n{prompt}"),
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

        let res = state.runtime.prompt(prompt_req, control).await;
        record_prompt_result(&state, request_id, &res, None).await;

        // Post the agent's reply back into the room as the agent participant.
        // The lock is taken synchronously here, after the await completed.
        if res.ok {
            let body = res.stdout.trim();
            if !body.is_empty() {
                let _ = append_room_message(
                    &state,
                    &room,
                    &agent.id,
                    RoomParticipantKind::Agent,
                    RoomMessageKind::Message,
                    body,
                );
            }
        } else {
            // Surface a failed convene as a system audit line so the room shows
            // the agent was woken but could not answer (e.g. no provider key).
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
        Ok(Some((record, page)))
    });
    match result {
        Ok(Some((rec, page))) => {
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
        let _ = release.await;
    }

    loop {
        match hints.recv().await {
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
/// room-scoped live SSE tail. `Last-Event-ID` wins over `after_seq`. Every frame
/// is `event: room_message`, `id: <seq>`, and the exact existing `RoomMessage`
/// JSON. SQLite is authoritative; the bounded broadcast carries wake hints only.
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

    match with_rooms(&state, |store| store.get(&room)) {
        Ok(Some(_)) => {}
        Ok(None) | Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            return Err(room_events_error(
                StatusCode::NOT_FOUND,
                "room_not_found",
                format!("no open room with key '{room}'"),
            ));
        }
        Err(error) => return Err(room_store_error_response(error)),
    }

    // Subscribe BEFORE the first replay query. Hints arriving during replay stay
    // buffered in this receiver; after replay, every hint (or Lagged signal)
    // pages SQLite from `last_sent_seq`, which closes the seam without trusting
    // channel retention.
    let hints = state.room_wakes.subscribe();
    let stream = room_message_tail(state.clone(), room, resume, hints, None).map(|message| {
        let seq = message.seq.to_string();
        let data = serde_json::to_string(&message)
            .expect("RoomMessage contains only infallibly serializable fields");
        Ok(Event::default().id(seq).event("room_message").data(data))
    });
    let stream = sse_until_shutdown(stream, state.shutdown.clone());
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        fake_convene_state, write_agent_fixture, TestEnvRestore, AUTO_CONVENE_ENV_LOCK,
    };

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_tail_fans_out_post_once_in_order_under_250ms() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("fanout");
        create_plain_room(&state, &key);

        let (mut first, release_first) = paused_tail(&state, &key, None).await;
        let (mut second, release_second) = paused_tail(&state, &key, None).await;
        release_first.send(()).unwrap();
        release_second.send(()).unwrap();

        let (status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "first".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let returned_at = tokio::time::Instant::now();
        let first_a = next_message(&mut first).await;
        let first_b = next_message(&mut second).await;
        assert!(returned_at.elapsed() < std::time::Duration::from_millis(250));
        assert_eq!(first_a, first_b);
        assert_eq!(first_a.seq, 0);

        let (status, _) = room_post_message(
            State(state),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "second".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(next_message(&mut first).await.seq, 1);
        assert_eq!(next_message(&mut second).await.seq, 1);
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
        let (mut convene_tail, release) = paused_tail(&state, &convene, Some(0)).await;
        release.send(()).unwrap();
        let (post_status, _) = room_post_message(
            State(state),
            Path(convene.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@helper report".into(),
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
        let app = crate::room_routes().with_state(state);
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
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("SSE frame exceeded 250ms")
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
        let app = crate::room_routes().with_state(state);
        let request = axum::http::Request::builder()
            .uri("/v1/rooms/persistent/header-resume/events?after_seq=0")
            .header("last-event-id", "1")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
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
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);

        let capture = wait_for_turn_capture("bound-agent")
            .await
            .expect("resolved room turn must reach runtime dispatch");
        assert!(capture.prompt.starts_with("BOUND_AGENT_INSTRUCTIONS\n\n"));
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
}
