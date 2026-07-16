use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};
use ocean_core::{
    evaluate_trigger_policy, PermissionMode, PromptRequest, RequestState, RoomKey, RoomMessageKind,
    RoomParticipant, RoomParticipantKind, RoomTriggerEvent, RoomTriggerPolicy,
};
use ocean_store::RoomStore;
use serde_json::json;
use uuid::Uuid;

use super::{build_prompt_control, core_sid, record_prompt_result, sdk_sid, AppState};
use crate::request_control::register_running_request;
use crate::yolo_settings::effective_permission_mode;

/// Shared handle to the daemon's single durable room store. Every closure is
/// synchronous, and both adapters recover a poisoned mutex without holding the
/// guard across an await.
pub(super) type RoomStoreHandle = Arc<Mutex<ocean_store::SqliteRoomStore>>;

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
        reg.add_participant(&key, participant, Utc::now())
    });
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
pub(super) async fn room_leave(
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
                let _ = with_rooms(&state, |reg| {
                    reg.append_message(
                        &key,
                        "system",
                        RoomParticipantKind::System,
                        RoomMessageKind::System,
                        &format!("agent '{}' is not bound; no turn queued", agent.id),
                        Utc::now(),
                    )
                });
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
                let _ = with_rooms(&state, |reg| {
                    reg.append_message(
                        &room,
                        "system",
                        RoomParticipantKind::System,
                        RoomMessageKind::System,
                        &format!("agent '{}' is not bound; no turn queued", agent.id),
                        Utc::now(),
                    )
                });
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

/// `GET /v1/rooms/persistent/{key}/events?after_seq=N&limit=M` — the live-tail
/// half of the hydrate-then-subscribe pattern: return transcript entries with
/// `seq > N` (omit `after_seq` for the start of the log). The transcript IS the
/// room's event log — chat lines plus join/leave/system markers, each carrying a
/// monotonic `seq` — so this is a thin alias over the same read `room_transcript`
/// serves, shaped as `events` for the client that just snapshotted at `last_seq`
/// and wants only what happened since.
///
/// Bounded + paginated (OCEAN-249): a busy room's event log no longer streams
/// unbounded on each poll. `last_seq` (the last seq in this batch, for the
/// existing tail-resume contract) is retained; `next_seq`/`has_more` are added so
/// a client can drain a large backlog page-by-page before catching up to live.
///
/// Mirrors `room_transcript`'s soft-closed audit fallback so a finished call's
/// frozen room keeps replaying.
pub(super) async fn room_events(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |reg| {
        read_transcript_page(reg, &key, q.after_seq, q.limit)
    });
    match result {
        Ok(page) => {
            let last_seq = page.messages.last().map(|m| m.seq);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "events": page.messages,
                    "last_seq": last_seq,
                    "next_seq": page.next_seq,
                    "has_more": page.has_more,
                })),
            )
        }
        Err(e) => room_store_error_response(e),
    }
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
