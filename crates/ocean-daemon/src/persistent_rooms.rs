use std::{
    collections::HashSet,
    convert::Infallible,
    pin::Pin,
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
    evaluate_trigger_policy, PermissionMode, PromptRequest, PublicAgentDescriptor,
    RoomAccessProjection, RoomAccessState, RoomArtifactKind, RoomArtifactState, RoomKey,
    RoomMessage, RoomMessageKind, RoomParticipant, RoomParticipantKind, RoomReadCursorProjection,
    RoomReadCursorUpdateRequest, RoomTriggerEvent, RoomTriggerPolicy,
};
#[cfg(test)]
use ocean_core::{OutboxItemState, RoomOutboxItem};
use ocean_store::{ContextPolicy, RoomStore, ThreadAppendError};
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
use crate::request_control::{register_room_agent_request_checked, RoomAgentRequestAuthority};
use crate::room_agent_authority::{self, AdmissionTrigger, ApiError, RoomAgentAdmission};
use crate::room_federation::{
    AgentRegistrationInput, FederatedTriggerDispatch, FederatedTriggerKind, IntentError,
};
use crate::room_summary;
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

#[derive(Debug, Clone)]
pub(super) struct RoomReadCursorWakeHint {
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

/// Daemon-wide bounded wake channel for room read-cursor projection changes.
#[derive(Clone)]
pub(super) struct RoomReadCursorWakeBus {
    tx: broadcast::Sender<RoomReadCursorWakeHint>,
}

impl Default for RoomReadCursorWakeBus {
    fn default() -> Self {
        Self::new(64)
    }
}

impl RoomReadCursorWakeBus {
    pub(super) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<RoomReadCursorWakeHint> {
        self.tx.subscribe()
    }

    fn publish(&self, room: &RoomKey) {
        let _ = self.tx.send(RoomReadCursorWakeHint { room: room.clone() });
    }
}

pub(super) fn publish_room_read_cursor_wake(state: &AppState, room: &RoomKey) {
    publish_room_read_cursor_wake_on(&state.room_read_cursor_wakes, room);
}

pub(super) fn publish_room_read_cursor_wake_on(wakes: &RoomReadCursorWakeBus, room: &RoomKey) {
    wakes.publish(room);
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
#[cfg(test)]
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

fn append_authorized_room_agent_reply(
    state: &AppState,
    admission: &RoomAgentAdmission,
    body: &str,
    thread_parent_seq: Option<u64>,
    session_id: AgentSessionId,
) -> Result<RoomMessage, ocean_store::RoomStoreError> {
    let session = session_id.to_string();
    let append = with_rooms(state, |store| {
        store.append_authorized_agent_reply(
            &admission.room,
            &admission.agent_member_id,
            admission.generation,
            &admission.admission_id,
            body,
            Utc::now(),
            thread_parent_seq,
            &session,
        )
    });
    let (reply, audit) = match append {
        Ok(messages) => messages,
        Err(ThreadAppendError::InvalidThreadParent {
            parent_seq, reason, ..
        }) => {
            tracing::warn!(room = %admission.room, agent = %admission.agent_member_id,
                parent_seq, %reason,
                "stale thread parent for authorized agent reply; posting top-level");
            with_rooms(state, |store| {
                store.append_authorized_agent_reply(
                    &admission.room,
                    &admission.agent_member_id,
                    admission.generation,
                    &admission.admission_id,
                    body,
                    Utc::now(),
                    None,
                    &session,
                )
            })
            .map_err(ocean_store::RoomStoreError::from)?
        }
        Err(ThreadAppendError::Store(error)) => return Err(error),
    };
    publish_room_wake(state, &admission.room, &reply);
    publish_room_wake(state, &admission.room, &audit);
    Ok(reply)
}

fn append_authorized_room_agent_failure(
    state: &AppState,
    admission: &RoomAgentAdmission,
    session_id: AgentSessionId,
) -> Result<(), ocean_store::RoomStoreError> {
    let (failure, audit) = with_rooms(state, |store| {
        store.append_authorized_agent_failure(
            &admission.room,
            &admission.agent_member_id,
            admission.generation,
            &admission.admission_id,
            Utc::now(),
            &session_id.to_string(),
        )
    })?;
    publish_room_wake(state, &admission.room, &failure);
    publish_room_wake(state, &admission.room, &audit);
    Ok(())
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
///
/// Records the acquisition into the room-metrics store-lock family (§4.1). This
/// adapter takes only the handle, so it has no `AppState` to reach a registry
/// through and records via the process-global install point instead; see
/// [`crate::metrics::with_process_room_metrics`] for why that indirection
/// exists and what it costs.
pub(super) fn with_rooms_handle<T>(
    rooms: &RoomStoreHandle,
    f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T,
) -> T {
    let waiting_since = std::time::Instant::now();
    let mut guard = match rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let waited = waiting_since.elapsed();
    crate::metrics::with_process_room_metrics(|metrics| metrics.record_store_lock_wait(waited));
    f(&mut guard)
}

/// Run a closure with the locked room store, recovering a poisoned lock the same
/// way the longhouse handlers do (`into_inner`). Synchronous: the guard is
/// dropped before this returns, so no `await` is ever held across the lock.
///
/// Records the acquisition into the room-metrics store-lock family (§4.1).
/// Unlike [`with_rooms_handle`] this one holds an `AppState`, so it records
/// straight into that state's own registry rather than through the process
/// global — which is what makes the store-lock family exact per-`AppState`
/// wherever a caller has one.
pub(super) fn with_rooms<T>(
    state: &AppState,
    f: impl FnOnce(&mut ocean_store::SqliteRoomStore) -> T,
) -> T {
    let waiting_since = std::time::Instant::now();
    let mut guard = match state.rooms.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    state
        .room_metrics
        .record_store_lock_wait(waiting_since.elapsed());
    f(&mut guard)
}

/// Sample the room store WITHOUT blocking on its mutex.
///
/// The `GET /health` liveness probe reads the room-metrics card, and that card's
/// room-derived numbers come from the store. Taking the daemon-wide mutex on the
/// liveness path would make a long store operation able to stall the one probe
/// whose documented contract is that it answers 200 whenever the process is
/// serving HTTP. So the sampler tries, and on contention reports the previous
/// sample as stale rather than waiting. Contributes nothing to the store-lock
/// wait family by construction: a `try_lock` never waits.
pub(super) fn sample_room_metrics(state: &AppState) {
    let guard = match state.rooms.try_lock() {
        Ok(guard) => guard,
        // Poison recovery matches the blocking adapters above: a panicked
        // writer must not also cost the operator their metrics.
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            state.room_metrics.note_sample_skipped();
            return;
        }
    };
    match guard.room_metrics_projection() {
        Ok(projection) => state
            .room_metrics
            .observe_store_sample(&projection, std::time::Instant::now()),
        Err(error) => {
            tracing::warn!(%error, "room metrics sample failed to read the store");
            state.room_metrics.note_sample_skipped();
        }
    }
}

#[derive(Clone)]
struct DurableRoomHistorySource {
    rooms: RoomStoreHandle,
}

fn room_history_row(message: RoomMessage) -> ocean_agent::RoomHistoryRow {
    let author_kind = match message.author_kind {
        RoomParticipantKind::Human => ocean_agent::RoomHistoryAuthorKind::Human,
        RoomParticipantKind::Agent => ocean_agent::RoomHistoryAuthorKind::Agent,
        RoomParticipantKind::System => ocean_agent::RoomHistoryAuthorKind::System,
        RoomParticipantKind::Bot => ocean_agent::RoomHistoryAuthorKind::Bot,
        RoomParticipantKind::Tool => ocean_agent::RoomHistoryAuthorKind::Tool,
    };
    ocean_agent::RoomHistoryRow {
        seq: message.seq,
        author_id: message.author_id,
        author_kind,
        text: room_history_text(message.body),
    }
}

/// A CLOSED whitelist, not a `room.agent.` prefix: an audit `type` that is not
/// one of these four falls through raw to every audience, and no test goes red
/// when it does. A new audit writer adds its `type` here in the same commit.
///
/// `pub(super)` for `room_summary.rs`, which shapes a model PROMPT rather than a
/// response and so has no `RoomMessage` to hand to `projected_room_message`;
/// `build_room_prompt` calls it directly for the same reason. One function on
/// purpose, and every renderer of a room body in this crate is a caller —
/// `room_history_row`, `projected_room_message`, `summary_user_prompt`, and
/// `build_room_prompt`. Four renderers must not become four rules.
pub(super) fn room_history_text(body: String) -> String {
    let audit_type = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match audit_type.as_deref() {
        Some("room.agent.admission") => "[room agent admission audit]".into(),
        Some("room.agent.authority") => "[room agent authority audit]".into(),
        Some("room.agent.bootstrap") => "[room agent bootstrap audit]".into(),
        Some("room.agent.output") => "[room agent output audit]".into(),
        _ => body,
    }
}

/// Collapse a `room.agent.*` audit body to the same summary line the agent path
/// already gets, for a row on its way to a HUMAN client.
///
/// Two things are wrong with handing that body over raw. The dull one is that a
/// human reads a wall of serde_json where an agent reads one line. The sharp one
/// is that the audit interpolates the ids that ARRIVED, and ocean-surface
/// markdown-renders every row body, System included, so a row whose
/// `owner_member_id` reads `[click here](https://evil.co)` lands an
/// attacker-labelled link in a row the UI attributes to the room itself.
///
/// `room_agent_authority::validate_member_id` now refuses that id at both
/// mutation routes, so no NEW row can be minted carrying one. This projection
/// is not thereby redundant: rows written before that guard are permanent, the
/// store still accepts whatever an in-process caller hands it, and the body
/// interpolates the package and operator-principal ids too.
///
/// The repair belongs HERE and not in `ocean-store`: that audit row is a ledger,
/// and a store that quietly repaired `owner_member_id` would report the attempt
/// as something other than what was made (see `crates/ocean-store/AGENTS.md`).
/// It goes at the point each response is SHAPED rather than inside
/// `read_transcript_page`, which stays the one raw paging implementation all of
/// its consumers share. The two MODEL-facing renderers build a prompt rather
/// than a `RoomMessage`, so they call `room_history_text` directly:
/// `summary_user_prompt` for `/summarize`, and `build_room_prompt` for the
/// transcript tail handed to a convened agent, whose window comes from
/// `authorized_room_transcript_context` and is not pre-projected either. Keeping
/// ONE function is the whole point — the human reads, the agent history page,
/// the summarizer, and the convened agent cannot drift into four rules.
///
/// Still open, and this does not close it: `room_history_text` matches four
/// literal `type` values, so a FIFTH audit writer falls through raw on every one
/// of those paths with no test going red. Named in
/// `crates/ocean-store/AGENTS.md`.
fn projected_room_message(mut message: RoomMessage) -> RoomMessage {
    message.body = room_history_text(message.body);
    message
}

/// [`projected_room_message`] across a page, for the handlers that hand back a
/// whole `transcript` array.
fn projected_transcript(messages: Vec<RoomMessage>) -> Vec<RoomMessage> {
    messages.into_iter().map(projected_room_message).collect()
}

/// The wire shape of [`ocean_store::SqliteRoomStore::agent_owners`], shared by
/// every route that reports it. `room_get` and `room_snapshot` both hydrate a
/// room and must not answer with two different shapes for one fact, so the
/// projection lives here rather than being written out twice.
///
/// `owner_present` is kept alongside `owner_id` rather than collapsed into it:
/// a worker can leave and the binding outlives them, so the room says who owns
/// the agent AND whether that worker is still here instead of asserting a live
/// claim it cannot prove.
fn projected_agent_owners(owners: Vec<(String, String, bool)>) -> Vec<serde_json::Value> {
    owners
        .into_iter()
        .map(|(agent, owner, owner_present)| {
            json!({
                "agent_id": agent,
                "owner_id": owner,
                "owner_present": owner_present,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl ocean_agent::RoomHistorySource for DurableRoomHistorySource {
    async fn page(
        &self,
        scope: &ocean_agent::RoomHistoryScope,
        request: ocean_agent::RoomHistoryRequest,
    ) -> Result<ocean_agent::RoomHistoryPage, ocean_agent::RoomHistorySourceError> {
        if scope.room_key().is_empty()
            || scope.agent_member_id().is_empty()
            || scope.generation() == 0
        {
            return Err(ocean_agent::RoomHistorySourceError::AuthorityChanged);
        }
        let room = RoomKey::new(scope.room_key());
        let page = with_rooms_handle(&self.rooms, |store| {
            store.authorized_room_history_page(
                &room,
                scope.agent_member_id(),
                scope.generation(),
                request.before_seq(),
                request.limit(),
            )
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownAgentBinding { .. }
            | ocean_store::RoomStoreError::AgentBindingStatusConflict { .. } => {
                ocean_agent::RoomHistorySourceError::AuthorityChanged
            }
            ocean_store::RoomStoreError::UnknownRoom(_) => {
                ocean_agent::RoomHistorySourceError::Unavailable
            }
            _ => ocean_agent::RoomHistorySourceError::Internal,
        })?;
        Ok(ocean_agent::RoomHistoryPage {
            rows: page.messages.into_iter().map(room_history_row).collect(),
            has_more: page.has_more,
        })
    }
}

/// Map a store error onto an HTTP status + typed JSON body.
pub(super) fn room_store_error_response(
    err: ocean_store::RoomStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ocean_store::RoomStoreError::*;
    let status = match &err {
        BadKey(_) => StatusCode::BAD_REQUEST,
        UnknownRoom(_) | UnknownParticipant { .. } => StatusCode::NOT_FOUND,
        AlreadyExists(_) | RoomNotLocal(_) | LocalRoomOwnerConflict { .. } => StatusCode::CONFLICT,
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
        // The caller read a stale artifact. 409 with the actual version in the
        // body is the whole contract: re-read and retry. Never a silent merge.
        ArtifactVersionConflict { .. } => StatusCode::CONFLICT,
        UnknownArtifact { .. } => StatusCode::NOT_FOUND,
        // A client naming collision is the most ordinary error this endpoint
        // sees. Before this it tripped the PK constraint and surfaced as a 500 —
        // a client mistake reported as a server fault.
        ArtifactAlreadyExists { .. } => StatusCode::CONFLICT,
        // Nothing to change is a client mistake, not a conflict to retry.
        ArtifactUnchanged { .. } => StatusCode::BAD_REQUEST,
        // A write that would leave the artifact untitled. Malformed, and no
        // version the caller could re-read would make it well formed.
        ArtifactTitleBlank { .. } => StatusCode::BAD_REQUEST,
        ParticipantRecordImmutable { .. } => StatusCode::CONFLICT,
        // An artifact attributed to someone not in the room is a lie, not a
        // server fault.
        ArtifactAuthorNotInRoster { .. } => StatusCode::FORBIDDEN,
        // A stale link, or a second delete of something already gone. The
        // caller is working from an out-of-date view of the room.
        UnknownAttachment { .. } => StatusCode::NOT_FOUND,
        // Same rule as an artifact author: a file attributed to somebody who is
        // not in the room is a lie, not a server fault.
        AttachmentUploaderNotInRoster { .. } => StatusCode::FORBIDDEN,
        // Rooms Phase 1. Asking about a binding that was never authorized is a
        // 404, not a 403: the caller is inspecting, and there is nothing there.
        // Admission refusal on an absent binding is a separate path that never
        // reaches this mapping.
        UnknownAgentBinding { .. } => StatusCode::NOT_FOUND,
        // A decision replayed against different content, and any move out of
        // the terminal revoked state, are both "your view of authority is
        // stale" — re-read and issue a new decision.
        DecisionReplayMismatch { .. } | AgentBindingStatusConflict { .. } => StatusCode::CONFLICT,
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
    /// its owning project and `cwd` from it. Absent/empty leaves the room
    /// unbound; agent turns then fail closed with `workspace_unavailable`.
    #[serde(default)]
    pub(super) workspace_root: Option<String>,
}

fn canonical_submitted_workspace_root(
    workspace_root: Option<String>,
) -> Result<Option<String>, ()> {
    let Some(workspace_root) = workspace_root
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let path = std::path::Path::new(&workspace_root);
    if !path.is_absolute() {
        return Err(());
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| ())?;
    if !canonical.is_dir() {
        return Err(());
    }
    canonical
        .to_str()
        .map(|value| Some(value.to_string()))
        .ok_or(())
}

fn persisted_room_workspace(workspace_root: &str) -> Option<String> {
    let stored = std::path::Path::new(workspace_root);
    if !stored.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(stored).ok()?;
    if canonical != stored || !canonical.is_dir() {
        return None;
    }
    canonical.to_str().map(str::to_string)
}

fn invalid_workspace_root_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok": false, "error": "invalid_workspace_root"})),
    )
}

/// The frozen refusal body for a trigger value the daemon will not store: a
/// stable machine `code` and the exact field the caller has to change. Both
/// refusals below share it so the shape can only ever be written once.
fn trigger_unwired_response(
    field: &'static str,
    error: String,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "ok": false,
            "code": "trigger_unwired",
            "field": field,
            "error": error,
        })),
    )
}

/// Refuse a submitted policy that enables a trigger the daemon never fires.
/// Mention, build-failure, and CI-failure events come from real code paths —
/// mention from a local post and a federated inbound alike, the two failure
/// flags from the federation ingest rail alone; nothing emits a schedule tick
/// or a component event, so storing those values would accept configuration
/// that silently never acts. Refuse the VALUE, not the field's presence:
/// clients serialize `"on_component_event": false` into every policy body
/// (bools have no skip-if-default on the wire), so presence-refusal would 400
/// every room write that sets any trigger.
///
/// Neither thread-reply nor the two failure flags is refused here, for
/// opposite reasons. A room is created `Local` and only ever federates later,
/// so enabling a failure flag in a Local room is anticipatory, not inert.
/// Thread-reply runs the asymmetry the other way — live in `Local`, dead the
/// moment a room leaves it — so it is gated on the room's access state by
/// [`dead_thread_reply_transition`] rather than on its value alone.
fn unwired_trigger_response(
    policy: &RoomTriggerPolicy,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    let field = if policy.on_component_event {
        "on_component_event"
    } else if policy.on_schedule.is_some() {
        "on_schedule"
    } else {
        return None;
    };
    Some(trigger_unwired_response(
        field,
        format!("{field} has no runtime yet: the daemon never fires this trigger, so the value would be stored and never act"),
    ))
}

/// Is this policy write switching `on_thread_reply` ON in a room that no
/// longer fires it?
///
/// The daemon builds `RoomTriggerEvent::ThreadReply` on exactly one path — the
/// local post, from the thread root's author — and a federated inbound message
/// carries no thread parent, so the flag is dead from the moment a room leaves
/// `Local`.
///
/// What is refused is the TRANSITION (stored false or absent → requested
/// true), never the value, and that distinction is the whole design.
/// ocean-surface builds a policy PATCH by cloning the room's STORED policy and
/// flipping one field, so a federated room already holding
/// `on_thread_reply: true` re-sends `true` on every unrelated toggle. Refusing
/// the value would 400 all of them and brick the trigger panel for exactly the
/// rooms this rule exists to protect. Refusing the transition is unreachable
/// from that client — which also disables the thread-reply row in a federated
/// room — so it cannot wedge the form.
///
/// Both of those ocean-surface facts are a MANUAL pin, conditional on it and on
/// nothing else: they were read at `rooms_workspace.rs` (`policy_with_toggle`
/// clones the stored policy and flips one field; `trigger_row_is_editable`
/// blocks the row) and hold there today, but no automated cross-repo check
/// exists and nothing in ocean-os reads ocean-surface, so a client that stops
/// cloning the stored policy turns "unreachable" into a 400 nobody re-derived.
/// What does NOT depend on the pin is the direction below: switching the flag
/// off is accepted in every access state, so no client can be locked out of
/// clearing it however its write is composed.
///
/// Switching the flag OFF stays allowed in every access state. It is the only
/// way a room that federated while the flag was set can ever be cleaned up,
/// and the daemon must not be the thing blocking that.
///
/// A room that already stores `true` and has since federated KEEPS its stored
/// value: this path deliberately does not normalize it to false. A PATCH about
/// some other flag silently rewriting a field the caller never named would put
/// the stored policy out of step with the request that wrote it, and the house
/// answer to an unstorable value here is a typed refusal, not a quiet rewrite.
/// Clearing it is blocked on the client, not here — the surface's
/// `trigger_row_is_editable` disables a dead row in BOTH directions, so nobody
/// can uncheck it. That is filed as
/// `surface-dead-trigger-row-cannot-be-unchecked-so-stored-dead-state-is-permanent`;
/// the moment that row can be unchecked, the true→false PATCH it sends is
/// already accepted here.
///
/// `room_create` has no counterpart check: a room is `Local` at creation and
/// federates only later, so a create has no non-Local room to refuse.
fn dead_thread_reply_transition(
    stored: Option<&RoomTriggerPolicy>,
    requested: &RoomTriggerPolicy,
    access: RoomAccessState,
) -> bool {
    requested.on_thread_reply
        && !stored.is_some_and(|p| p.on_thread_reply)
        && access != RoomAccessState::Local
}

/// The update route's error: a store fault, or the daemon refusing the policy
/// on evidence it could only read under the store guard. Keeping them apart is
/// what lets the refusal answer with its own typed 400 while a store fault
/// keeps [`room_store_error_response`]'s existing mapping.
enum RoomUpdateError {
    Store(ocean_store::RoomStoreError),
    DeadThreadReply,
}

impl From<ocean_store::RoomStoreError> for RoomUpdateError {
    fn from(e: ocean_store::RoomStoreError) -> Self {
        Self::Store(e)
    }
}

/// `POST /v1/rooms/persistent` — create a persistent room.
pub(super) async fn room_create(
    State(state): State<AppState>,
    Json(req): Json<RoomCreateRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(refusal) = req
        .trigger_policy
        .as_ref()
        .and_then(unwired_trigger_response)
    {
        return refusal;
    }
    let key = RoomKey::new(req.key.trim());
    // Blank remains explicitly unbound. A non-blank binding must resolve now
    // to one canonical absolute directory, so neither a relative path nor a
    // later process cwd can become execution authority.
    let workspace_root = match canonical_submitted_workspace_root(req.workspace_root) {
        Ok(workspace_root) => workspace_root,
        Err(()) => return invalid_workspace_root_response(),
    };
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

/// Keep "field absent" distinguishable from an explicit `"trigger_policy":
/// null`. Plain `Option` collapses both to `None`, but the store's update
/// contract is `Option<Option<_>>` — absent leaves the policy alone, `null`
/// clears it — and collapsing them would turn "don't touch my policy" into
/// "delete it".
fn double_option_trigger_policy<'de, D>(
    deserializer: D,
) -> Result<Option<Option<RoomTriggerPolicy>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<RoomTriggerPolicy>::deserialize(deserializer).map(Some)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RoomUpdateRequest {
    /// New human-readable room name. Absent ⇒ unchanged.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// Absent ⇒ unchanged; explicit `null` ⇒ clear the policy.
    #[serde(default, deserialize_with = "double_option_trigger_policy")]
    pub(super) trigger_policy: Option<Option<RoomTriggerPolicy>>,
}

/// `PATCH /v1/rooms/persistent/{key}` — update a room's mutable metadata
/// (name and/or trigger policy) after creation. Until this route existed the
/// trigger policy was create-time-only: changing it meant a new room and a
/// lost transcript. Body parsing mirrors the read-cursor PATCH (typed 400,
/// never an extractor rejection), and unknown fields are rejected rather than
/// ignored, so a typo'd field name can never read as "leave everything
/// unchanged".
pub(super) async fn room_update(
    State(state): State<AppState>,
    Path(raw_key): Path<String>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: RoomUpdateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return invalid_request_response(),
    };
    // Same refusal as create: a PATCH must not become the back door that
    // stores a trigger nothing fires. An explicit `null` (clear) is fine.
    if let Some(refusal) = req
        .trigger_policy
        .as_ref()
        .and_then(|p| p.as_ref())
        .and_then(unwired_trigger_response)
    {
        return refusal;
    }
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    let result = with_rooms(&state, |reg| {
        // The thread-reply rule needs the STORED policy and the room's access
        // state, so unlike `unwired_trigger_response` it cannot run before the
        // store is open. Reading both under the SAME guard as the write is the
        // point: a room federating between the access read and the update it
        // gates would otherwise land the flag the read had just cleared.
        if let Some(requested) = req.trigger_policy.as_ref().and_then(|p| p.as_ref()) {
            // Establish the room is WRITABLE before refusing a write to it.
            // `trigger_policy` and `room_access` both answer for any room the
            // store still holds, soft-closed included, while `update` writes
            // only an open one — so without this gate a closed federated room
            // asking for the flag would learn its federation state from a typed
            // 400 where the contract has always been a flat 404. Same gate
            // `room_post_message` opens with, and under the same guard as the
            // write, so it cannot be raced by a close in between.
            if reg.get(&key)?.is_none() {
                return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()).into());
            }
            let stored = reg.trigger_policy(&key)?;
            let access = reg.room_access(&key)?.state;
            if dead_thread_reply_transition(stored.as_ref(), requested, access) {
                return Err(RoomUpdateError::DeadThreadReply);
            }
        }
        reg.update(&key, req.name, req.trigger_policy, Utc::now())
            .map_err(RoomUpdateError::Store)
    });
    match result {
        Ok(rec) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "room": rec.room })),
        ),
        Err(RoomUpdateError::DeadThreadReply) => trigger_unwired_response(
            "on_thread_reply",
            "on_thread_reply has no runtime in a federated room: the daemon raises that trigger only from a local post, and a federated message carries no thread parent, so the value would be stored and never act".to_string(),
        ),
        Err(RoomUpdateError::Store(e)) => room_store_error_response(e),
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

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct PersistentRoomReadState {
    room_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_seq: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_seq: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct PersistentRoomsListResponse {
    ok: bool,
    rooms: Vec<ocean_core::Room>,
    read_states: Vec<PersistentRoomReadState>,
    // Deliberately NOT `skip_serializing_if`: pre-existing pollers rely on the
    // key always being present (`"next_cursor": null` on the final page), so
    // omitting the key on a single-page response would be a silent wire
    // compatibility break.
    next_cursor: Option<String>,
    has_more: bool,
}

/// `GET /v1/rooms/persistent?limit=&cursor=` — list open persistent rooms, one
/// bounded page at a time (OCEAN-250). Rooms are ordered most-recently-updated
/// first; the `rooms` array shape is unchanged, with additive
/// `next_cursor`/`has_more` so a poller doesn't re-serialize every room each call.
pub(super) async fn rooms_list_persistent(
    State(state): State<AppState>,
    Query(q): Query<RoomsListQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match with_rooms(&state, |reg| {
        let page = reg.list_page(q.cursor.as_deref(), q.limit)?;
        let read_states = page
            .rooms
            .iter()
            .map(|room| {
                let key = room.id.clone();
                let access = reg.room_access(&key)?;
                let principal: Option<String> = match access.state {
                    RoomAccessState::Local => Some(local_room_read_cursor_principal().to_string()),
                    RoomAccessState::Live
                    | RoomAccessState::Connecting
                    | RoomAccessState::Recovering
                    | RoomAccessState::Revoked => reg
                        .room_credential(&key)?
                        .map(|credential| credential.local_human_member_id),
                };
                let cursor = match principal.as_deref() {
                    Some(principal) => reg.room_read_cursor(&key, principal)?,
                    None => RoomReadCursorProjection {
                        read_seq: None,
                        mirrored_upstream_read_seq: None,
                    },
                };
                let latest_seq = match access.state {
                    RoomAccessState::Local => reg.room_latest_durable_seq(&key)?,
                    RoomAccessState::Live => access.last_confirmed_global_sequence,
                    RoomAccessState::Connecting
                    | RoomAccessState::Recovering
                    | RoomAccessState::Revoked => access.last_confirmed_global_sequence,
                };
                let read_seq = match access.state {
                    RoomAccessState::Local => cursor.read_seq,
                    RoomAccessState::Live
                    | RoomAccessState::Connecting
                    | RoomAccessState::Recovering
                    | RoomAccessState::Revoked => cursor.mirrored_upstream_read_seq,
                };
                Ok::<_, ocean_store::RoomStoreError>(PersistentRoomReadState {
                    room_id: room.id.to_string(),
                    latest_seq: latest_seq.map(|seq| seq.to_string()),
                    read_seq: read_seq.map(|seq| seq.to_string()),
                })
            })
            .collect::<Result<Vec<_>, ocean_store::RoomStoreError>>()?;
        Ok::<_, ocean_store::RoomStoreError>(PersistentRoomsListResponse {
            ok: true,
            rooms: page.rooms,
            read_states,
            next_cursor: page.next_cursor,
            has_more: page.has_more,
        })
    }) {
        Ok(response) => (
            StatusCode::OK,
            Json(serde_json::to_value(response).unwrap()),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}` — one persistent room (with the first page
/// of its transcript and its access projection). Open rooms only; soft-closed
/// rooms return 404.
///
/// The `transcript` array is a BOUNDED FIRST PAGE (OCEAN-249), like
/// `room_transcript` and `room_snapshot`: at most `MAX_TRANSCRIPT_LIMIT` rows
/// from the start of the log, carrying `next_seq` (replay as
/// `/transcript?after_seq=next_seq`) and `has_more` so a caller can tell a whole
/// transcript from its oldest prefix. Without those two fields a room past the
/// cap answered its oldest thousand messages and presented them as the
/// transcript.
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
        // `get` is the OPEN-room getter, and that choice alone is this route's
        // 404 contract. Widen it to `get_including_closed` and the route quietly
        // starts serving frozen rooms — it becomes `room_snapshot`, whose
        // `closed` boolean exists precisely because that is a different answer.
        let Some(record) = reg.get(&key)? else {
            return Ok(None);
        };
        // This record IS the page, so there is nothing to re-read. `load_record`
        // builds its transcript from `load_transcript_page(key, None,
        // MAX_TRANSCRIPT_LIMIT)` — the same rows, the same cap, the same clamp
        // this route asked `read_transcript_page` for — and it carries that page's
        // own `limit + 1` sentinel as `transcript_has_more`. The paged
        // re-read was justified by only a page being able to tell "exactly the
        // cap" from "the cap, with more behind it"; the record answers that
        // itself now, so the second decode of up to MAX_TRANSCRIPT_LIMIT rows was
        // paying for a fact already in hand. The cursor is not a second field on
        // the record on purpose — it is the last row the record holds, and it is
        // only a cursor when the flag says rows follow it.
        let next_seq = if record.transcript_has_more {
            record.transcript.last().map(|m| m.seq)
        } else {
            None
        };
        let access = reg.room_access(&key)?;
        // Which worker owns which agent in THIS room. Adjacent to the roster,
        // never a field on RoomParticipant (the federated design reserves
        // owner/sovereignty for Bedrock's authenticated principal mapping).
        // Absent key == no local ownership recorded, which is what every
        // pre-existing room reports.
        let owners = reg.agent_owners(&key)?;
        Ok(Some((
            record.room,
            record.transcript,
            record.transcript_has_more,
            next_seq,
            access,
            owners,
        )))
    }) {
        Ok(Some((room, transcript, has_more, next_seq, access, owners))) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "room": room,
                "transcript": projected_transcript(transcript),
                "next_seq": next_seq,
                "has_more": has_more,
                "access": access,
                "agent_owners": projected_agent_owners(owners),
            })),
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
    /// The WORKER who owns this agent. Only meaningful for `kind: Agent`, and
    /// the owner must already be a Human on this room's roster. This is the
    /// local half of "a worker persists alongside their agents": it makes
    /// "my agent" a real relationship in a room with no federation.
    #[serde(default)]
    pub(super) owner_id: Option<String>,
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
    // An owner is only meaningful for an Agent. Refuse it elsewhere rather than
    // silently dropping it — a caller that believed it recorded ownership and
    // did not is the false-success class this work exists to remove.
    if req.owner_id.is_some() && !matches!(req.kind, RoomParticipantKind::Agent) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "owner_requires_agent",
                "error": "owner_id is only valid for a participant of kind 'agent'",
            })),
        );
    }
    let owner_id = req.owner_id.clone();
    let participant = RoomParticipant {
        id: req.id,
        kind: req.kind,
        display_name: req.display_name,
    };
    let result = with_rooms(&state, |reg| match owner_id.as_deref() {
        // The store validates the owner against the live roster INSIDE the same
        // transaction as the insert, so a concurrent leave cannot strand an
        // agent owned by someone who is gone.
        Some(owner) => reg.add_agent_participant_with_owner(&key, participant, owner, Utc::now()),
        None => reg.add_participant_with_message(&key, participant, Utc::now()),
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
pub(super) struct CreateArtifactRequest {
    pub(super) id: String,
    pub(super) kind: RoomArtifactKind,
    pub(super) title: String,
    #[serde(default)]
    pub(super) body: String,
    /// Participant id of the author — human OR agent. Validated against the
    /// roster inside the store transaction.
    pub(super) author_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AmendArtifactRequest {
    /// The version the caller READ. Compare-and-swap: if the artifact has moved
    /// on, the write is refused with the actual version rather than merged.
    pub(super) expected_version: u64,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) body: Option<String>,
    #[serde(default)]
    pub(super) state: Option<RoomArtifactState>,
    pub(super) author_id: String,
}

enum ClientArtifactWriteError {
    ForgedAuthor,
    Store(ocean_store::RoomStoreError),
}

fn forged_artifact_author_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "code": "forged_artifact_author",
            "error": "an agent's artifact is authored by the daemon, not by a client claiming its identity",
        })),
    )
}

/// A browser/client may attribute an artifact only to a human roster member.
/// Agent and System artifacts are daemon-authored; accepting those identities
/// from the wire lets a caller forge a durable artifact and audit line.
///
/// Call only while holding the same room-store guard used for the subsequent
/// mutation, so a concurrent roster replacement cannot race authorization.
fn enforce_client_artifact_author(
    store: &mut ocean_store::SqliteRoomStore,
    key: &RoomKey,
    author_id: &str,
) -> Result<(), ClientArtifactWriteError> {
    let claimed_kind = store
        .get(key)
        .map_err(ClientArtifactWriteError::Store)?
        .and_then(|record| {
            record
                .room
                .participants
                .iter()
                .find(|participant| participant.id == author_id)
                .map(|participant| participant.kind)
        });
    if matches!(
        claimed_kind,
        Some(RoomParticipantKind::Agent) | Some(RoomParticipantKind::System)
    ) {
        return Err(ClientArtifactWriteError::ForgedAuthor);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SummarizeRequest {
    /// Roster participant the summary artifact is attributed to. Required, not
    /// optional: `create_artifact`/`amend_artifact` demand a real roster author
    /// and rooms are created with an EMPTY roster, so there is no daemon
    /// identity to fall back on. The requester owns the write; the model that
    /// actually wrote the words is recorded in the artifact body.
    pub(super) requested_by: String,
    /// Size of the transcript window. Omitted ⇒ the store's default cap; any
    /// value is clamped by `clamp_transcript_limit`, exactly as `/transcript` is.
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Pin an explicit window instead of the newest `limit` rows. Omitted — the
    /// ordinary case — summarizes the tail of the room.
    #[serde(default)]
    pub(super) after_seq: Option<u64>,
}

/// `POST /v1/rooms/persistent/{key}/artifacts` — record something the room
/// produced: a task, a decision, or captured knowledge.
pub(super) async fn room_create_artifact(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<CreateArtifactRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    if req.id.trim().is_empty() || req.id != req.id.trim() {
        return invalid_request_response();
    }
    if req.title.trim().is_empty() {
        return invalid_request_response();
    }
    let result = with_rooms(&state, |store| {
        enforce_client_artifact_author(store, &key, &req.author_id)?;
        store
            .create_artifact(
                &key,
                &req.id,
                req.kind,
                &req.title,
                &req.body,
                &req.author_id,
                Utc::now(),
            )
            .map_err(ClientArtifactWriteError::Store)
    });
    match result {
        Ok((artifact, message)) => {
            // The transcript line is live on the room's SSE, so every client
            // learns the artifact exists without polling.
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::CREATED,
                Json(json!({ "ok": true, "artifact": artifact })),
            )
        }
        Err(ClientArtifactWriteError::ForgedAuthor) => forged_artifact_author_response(),
        Err(ClientArtifactWriteError::Store(e)) => room_store_error_response(e),
    }
}

/// `POST /v1/rooms/persistent/{key}/artifacts/{artifact_id}/amend` — rewrite an
/// artifact in place under compare-and-swap.
pub(super) async fn room_amend_artifact(
    State(state): State<AppState>,
    Path((key, artifact_id)): Path<(String, String)>,
    Json(req): Json<AmendArtifactRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let result = with_rooms(&state, |store| {
        enforce_client_artifact_author(store, &key, &req.author_id)?;
        store
            .amend_artifact(
                &key,
                artifact_id.trim(),
                req.expected_version,
                req.title.as_deref(),
                req.body.as_deref(),
                req.state,
                &req.author_id,
                Utc::now(),
            )
            .map_err(ClientArtifactWriteError::Store)
    });
    match result {
        Ok((artifact, message)) => {
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "artifact": artifact })),
            )
        }
        // A stale write must hand back where to re-read from, not just "409".
        Err(ClientArtifactWriteError::Store(
            ocean_store::RoomStoreError::ArtifactVersionConflict {
                expected, actual, ..
            },
        )) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "code": "artifact_version_conflict",
                "expected_version": expected,
                "actual_version": actual,
                "error": format!("artifact is at version {actual}, not {expected}; re-read and retry"),
            })),
        ),
        // Create refuses a blank title with a bare `invalid_request` 400; an
        // amend that would blank an existing one is the same client mistake and
        // must not be answerable by a different shape depending on which layer
        // caught it. The store is what actually refused — the route no longer
        // needs its own copy of the check.
        Err(ClientArtifactWriteError::Store(ocean_store::RoomStoreError::ArtifactTitleBlank {
            ..
        })) => invalid_request_response(),
        Err(ClientArtifactWriteError::ForgedAuthor) => forged_artifact_author_response(),
        Err(ClientArtifactWriteError::Store(e)) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}/artifacts/{artifact_id}` — one artifact.
///
/// This is the other half of the compare-and-swap contract. A 409 tells a caller
/// their version is stale and hands back the actual one, but without a
/// single-artifact read the only recovery is to re-list the whole room: fine at
/// five artifacts, absurd at two hundred. With this the conflict->re-read->retry
/// loop is one round trip.
pub(super) async fn room_get_artifact(
    State(state): State<AppState>,
    Path((key, artifact_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    match with_rooms(&state, |store| store.artifact(&key, artifact_id.trim())) {
        Ok(Some(artifact)) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "artifact": artifact })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "code": "unknown_artifact",
                "error": format!("room '{key}' has no artifact '{}'", artifact_id.trim()),
            })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}/artifacts` — everything this room produced.
pub(super) async fn room_list_artifacts(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    match with_rooms(&state, |store| store.artifacts(&key)) {
        Ok(artifacts) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "artifacts": artifacts })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `POST /v1/rooms/persistent/{key}/summarize` — read a bounded tail of this
/// room's transcript, run ONE model turn over it, and fold the result into the
/// room's single well-known `room-summary` artifact.
///
/// A long room is unreadable, and the answer is not another wall of chat: the
/// summary lands as a durable thing the room OWNS, versioned by the same
/// compare-and-swap every other artifact uses and announced on the SSE tail
/// every client already listens to. Repeated calls amend that one artifact in
/// place rather than accumulating near-duplicate summaries.
///
/// This adds no provider client. The model turn goes through
/// `AgentRuntime::complete_once` — the same fresh-context, no-session, no-tools
/// seam the post-turn advisor runs on — and the logic lives in `room_summary.rs`
/// behind a closure so it is testable without process-global provider env.
pub(super) async fn room_summarize(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<SummarizeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let trimmed_key = key.trim();
    let requested_by = req.requested_by.trim();
    if trimmed_key.is_empty() || requested_by.is_empty() {
        return invalid_request_response();
    }
    let key = RoomKey::new(trimmed_key);

    // Backpressure, the same gate every other provider-calling route takes
    // (`agent_turn`, `POST /v1/sessions/{id}/compact`): claim a turn permit
    // BEFORE any work and reject immediately at capacity rather than queueing,
    // so a client looping summarize cannot fan out into unbounded concurrent
    // provider calls. The owned permit is held for the whole handler and
    // returned on every exit path, including a panic.
    let _turn_permit = match state.turn_limiter.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::warn!(room = %key, "room summarize: at concurrency cap; rejecting with 429");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "ok": false,
                    "code": "at_capacity",
                    "error": "daemon at concurrent-turn capacity; busy, try again shortly",
                })),
            );
        }
    };

    // A cheap role if the operator configured one, otherwise whatever model the
    // daemon is already bound to — the feature works with zero config rather
    // than being dead by default.
    let alias = room_summary::resolve_summary_alias(&state.roles, &state.runtime.current_model().1);
    let runtime = state.runtime.clone();
    let outcome =
        room_summary::summarize_room(
            &state.rooms,
            room_summary::SummarizeInput {
                key: key.clone(),
                requested_by: requested_by.to_string(),
                limit: req.limit,
                after_seq: req.after_seq,
                alias,
                timeout: room_summary::ROOM_SUMMARY_TIMEOUT,
            },
            move |alias, system, user| async move {
                runtime.complete_once(&alias, &system, &user).await
            },
        )
        .await;

    // Post-commit only: the store adapter has returned, so the artifact and the
    // System transcript line it wrote in the same transaction are both durable
    // before any tail is told to re-read.
    if let room_summary::SummarizeOutcome::Wrote { message, .. } = &outcome {
        publish_room_wake(&state, &key, message);
    }
    summarize_response(outcome)
}

/// Map a summarize outcome onto its HTTP shape. Pure — no `AppState`, no env —
/// so the contract that matters here is unit-testable: a room with nothing to
/// say, a model that returned nothing, and a model that repeated itself are all
/// clean 200s, and a provider failure is a fixed 502 that never carries the
/// provider's own message (which can embed response fragments).
fn summarize_response(
    outcome: room_summary::SummarizeOutcome,
) -> (StatusCode, Json<serde_json::Value>) {
    use room_summary::SummarizeOutcome::*;
    match outcome {
        // 200 for both create and amend so the route has ONE success shape;
        // `created` is what tells the caller which of the two happened.
        Wrote {
            artifact,
            created,
            model,
            messages_summarized,
            from_seq,
            to_seq,
            has_more,
            ..
        } => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "summarized": true,
                "created": created,
                "artifact": artifact,
                "model": model,
                "messages_summarized": messages_summarized,
                "from_seq": from_seq,
                "to_seq": to_seq,
                "has_more": has_more,
            })),
        ),
        // The store refused a no-op amend, which is correct: the model looked at
        // the same conversation and said the same thing. Nothing moved, and the
        // caller gets back the artifact that already stands.
        Unchanged { artifact } => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "summarized": false,
                "code": "unchanged",
                "artifact": artifact,
            })),
        ),
        NoMessages => (
            StatusCode::OK,
            Json(json!({ "ok": true, "summarized": false, "code": "no_messages" })),
        ),
        EmptySummary => (
            StatusCode::OK,
            Json(json!({ "ok": true, "summarized": false, "code": "empty_summary" })),
        ),
        // Same rule and the same code as `room_create_artifact`.
        ForgedAuthor => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "code": "forged_artifact_author",
                "error": "an agent's artifact is authored by the daemon, not by a client claiming its identity",
            })),
        ),
        ProviderError => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "code": "summary_provider_error",
                "error": "the summary model call failed",
            })),
        ),
        Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({
                "ok": false,
                "code": "summary_timeout",
                "error": "the summary model call timed out",
            })),
        ),
        // Unknown room, a soft-closed room (the write requires `room_is_open`),
        // and a non-roster author all already have a truthful mapping.
        Store(e) => room_store_error_response(e),
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

/// Sweep a deleted folder-as-agent out of every local room roster. Returns the
/// number of rooms the agent was removed from.
///
/// A roster row of kind Agent carries the system's invariant that its id IS a
/// resolvable agent folder: `room_join` refuses anything else with a typed 400,
/// and the wake path fail-closes on a vanished folder. So `agent_delete` calls
/// this AFTER the folder removal succeeds — otherwise every room that held the
/// agent keeps a ghost member that renders in rosters, answers every mention
/// with "not bound" noise, and can only be cured by a manual per-room leave.
/// Removal goes through `remove_participant_with_message`, so each swept room
/// gets the same ParticipantLeft marker an explicit leave writes.
///
/// The kind filter is load-bearing: ids are unique within a room, but a Human
/// in some other room may share the deleted agent's name and must survive.
/// Per-room failures are logged and skipped rather than failing the delete —
/// the folder is already gone (the fs delete is not transactional with the
/// store), and a half-swept roster still beats a wholly ghosted one. Federated
/// rooms keep bedrock-authoritative membership, so a roster sync may rewrite a
/// swept row back there; that residual is filed, not handled here.
pub(super) fn sweep_agent_from_local_rosters(state: &AppState, agent_id: &str) -> usize {
    let swept = with_rooms(state, |reg| {
        // Page to the end: `list()` caps at DEFAULT_LIST_LIMIT (OCEAN-250), and
        // a daemon past that many open rooms would silently keep its ghosts.
        // Scan and remove under the one lock hold so a concurrent join cannot
        // interleave; everything here is synchronous, so the guard never
        // crosses an await.
        let mut ghosted: Vec<RoomKey> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = match reg.list_page(cursor.as_deref(), Some(ocean_store::MAX_LIST_LIMIT)) {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(agent = agent_id, error = %e,
                        "agent-delete roster sweep could not list rooms");
                    break;
                }
            };
            ghosted.extend(
                page.rooms
                    .iter()
                    .filter(|room| {
                        room.participants.iter().any(|p| {
                            matches!(p.kind, RoomParticipantKind::Agent) && p.id == agent_id
                        })
                    })
                    .map(|room| room.id.clone()),
            );
            if !page.has_more {
                break;
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut removed = Vec::new();
        for key in ghosted {
            match reg.remove_participant_with_message(&key, agent_id, Utc::now()) {
                Ok((_rec, message)) => removed.push((key, message)),
                Err(e) => {
                    tracing::warn!(agent = agent_id, room = %key, error = %e,
                        "agent-delete roster sweep skipped a room");
                }
            }
        }
        removed
    });
    // Wake hints only after `with_rooms` returns: the transactions have
    // committed and the lock is released — the same post-commit rule as
    // `room_leave`.
    for (key, message) in &swept {
        publish_room_wake(state, key, message);
    }
    swept.len()
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
    /// The body exceeds [`OUTBOUND_MESSAGE_BODY_LIMIT`]. Refused at the door
    /// rather than written, because a row too large to travel the federation
    /// wire is one no peer can read back — see the constant for why an
    /// unreadable row is worth this much trouble to prevent.
    BodyTooLarge,
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
/// [`post_rejection_response`] plus the §4.1 admission-refusal counter.
///
/// The member-level refusals belong in the same family as the room-agent
/// admission arms: both are "this speaker was refused entry to this room", and
/// an operator watching one wants the other. Counted here rather than inside
/// `post_rejection_response` because that function has no `AppState` and is also
/// called from tests; this wrapper is the production door.
fn refuse_local_post(
    state: &AppState,
    rejection: PostRejection,
) -> (StatusCode, Json<serde_json::Value>) {
    let refusal = match rejection {
        PostRejection::ForgedAuthorKind => crate::metrics::AdmissionRefusal::ForgedAuthorKind,
        PostRejection::AuthorNotInRoster => crate::metrics::AdmissionRefusal::AuthorNotInRoster,
        PostRejection::InvalidThreadParent => crate::metrics::AdmissionRefusal::InvalidThreadParent,
        PostRejection::BodyTooLarge => crate::metrics::AdmissionRefusal::BodyTooLarge,
    };
    state.room_metrics.record_admission_refusal(refusal);
    post_rejection_response(rejection)
}

pub(super) fn post_rejection_response(
    rejection: PostRejection,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, code) = match rejection {
        PostRejection::ForgedAuthorKind => (StatusCode::FORBIDDEN, "forged_author_kind"),
        PostRejection::AuthorNotInRoster => (StatusCode::FORBIDDEN, "author_not_in_roster"),
        PostRejection::InvalidThreadParent => (StatusCode::BAD_REQUEST, "invalid_thread_parent"),
        PostRejection::BodyTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
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

/// Fit a daemon-authored body inside [`OUTBOUND_MESSAGE_BODY_LIMIT`].
///
/// A human who writes too much gets a `413` and can split the message. Nobody
/// is standing behind a convened agent to do that, so its reply is trimmed to
/// the limit and marked instead of being dropped on the floor — the room still
/// sees the answer, and the ledger still gets a row every peer can read.
///
/// The cut lands on a UTF-8 boundary, walked by hand because
/// `floor_char_boundary` is still unstable, so the result is never invalid text.
pub(super) fn clamp_room_message_body(body: &str) -> std::borrow::Cow<'_, str> {
    const MARKER: &str = "\n\n[truncated: reply exceeded the room message limit]";
    if body.len() <= crate::room_federation::OUTBOUND_MESSAGE_BODY_LIMIT {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut cut = crate::room_federation::OUTBOUND_MESSAGE_BODY_LIMIT - MARKER.len();
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{MARKER}", &body[..cut]))
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
    // Size is checked before the room is even resolved: the answer is the same
    // for a local room and a federated one, and a local room can be federated
    // later, so an oversized row must not reach the transcript either way.
    if req.body.len() > crate::room_federation::OUTBOUND_MESSAGE_BODY_LIMIT {
        return refuse_local_post(&state, PostRejection::BodyTooLarge);
    }
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
        Err(LocalPostError::Rejected(rejection)) => return refuse_local_post(&state, rejection),
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

            // Phase 1 authority is the first observable convene boundary. A
            // refusal writes only its content-minimal admission decision; it
            // never emits the legacy room_trigger/auto-convene footprint and
            // never reads transcript or attachment context.
            let admission = match room_agent_authority::admit_room_agent(
                &state,
                &key,
                &agent.id,
                &agent.id,
                AdmissionTrigger::from_room_event(&event),
            )
            .await
            {
                Ok(admission) => admission,
                Err(error) => {
                    tracing::info!(room = %key, agent = %agent.id, reason = error.code(),
                        "room-agent admission refused before convene footprint");
                    continue;
                }
            };

            let target = decision.target_participant.clone().unwrap_or_default();
            let reason = decision.reason.clone();
            if let Err(error) = spawn_room_agent_turn(
                state.clone(),
                admission,
                agent,
                msg.seq,
                None,
                Uuid::new_v4(),
                None,
                Some(RoomTurnFootprint {
                    payload: json!({
                        "room": key.as_str(),
                        "target": target,
                        "reason": reason,
                        "triggered_by_seq": msg.seq,
                    }),
                    audit_line: Some(format!("auto-convene: {} ({})", target, reason)),
                }),
            )
            .await
            {
                tracing::info!(room = %key, reason = error.code(),
                    "room-agent convene refused before runtime dispatch");
            }
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InvokeRoomAgentBody {
    invoked_by: String,
    message_seq: u64,
    #[serde(default)]
    decision_token: Option<String>,
}

/// Explicit same-room invocation seam. The caller can point only at an already
/// durable message it authored as a non-agent/non-system roster member; labels
/// and client event ids never create authority.
pub(super) async fn room_agent_invoke(
    State(state): State<AppState>,
    Path((key, agent_member_id)): Path<(String, String)>,
    body: Result<Json<InvokeRoomAgentBody>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return invalid_request_response(),
    };
    let room = RoomKey::new(key.trim());
    let invoked_by = body.invoked_by.trim();
    let agent_member_id = agent_member_id.trim();
    if room.as_str().is_empty()
        || invoked_by.is_empty()
        || agent_member_id.is_empty()
        || body
            .decision_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
    {
        return invalid_request_response();
    }
    let resolved = with_rooms(&state, |store| {
        let record = store
            .get(&room)?
            .ok_or_else(|| ocean_store::RoomStoreError::UnknownRoom(room.clone()))?;
        let binding = store
            .room_agent_binding(&room, agent_member_id)?
            .ok_or_else(|| ocean_store::RoomStoreError::UnknownAgentBinding {
                room: room.clone(),
                agent: agent_member_id.to_string(),
            })?;
        let access = store.room_access(&room)?;
        let local = access.state == RoomAccessState::Local;
        let invoker_is_member = if local {
            record.room.participants.iter().any(|participant| {
                participant.id == invoked_by
                    && !matches!(
                        participant.kind,
                        RoomParticipantKind::Agent | RoomParticipantKind::System
                    )
            })
        } else {
            access.members.iter().any(|member| {
                member.member_id == invoked_by
                    && member.actor_type == ocean_core::FederatedActorType::User
            })
        };
        let target = if local {
            record
                .room
                .participants
                .iter()
                .find(|participant| {
                    participant.id == agent_member_id
                        && participant.kind == RoomParticipantKind::Agent
                })
                .cloned()
        } else {
            access
                .members
                .iter()
                .any(|member| {
                    member.member_id == agent_member_id
                        && member.actor_type == ocean_core::FederatedActorType::Agent
                })
                .then(|| RoomParticipant {
                    id: binding.agent_package_id.clone(),
                    kind: RoomParticipantKind::Agent,
                    display_name: binding.display_name.clone(),
                })
        };
        if !invoker_is_member || target.is_none() {
            return Err(ocean_store::RoomStoreError::Encode(
                "invoke_membership_required".into(),
            ));
        }
        Ok((
            binding.agent_package_id,
            target.expect("checked above"),
            !local,
        ))
    });
    let (package_id, agent, federated) = match resolved {
        Ok(value) => value,
        Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "room_not_found"})),
            );
        }
        Err(ocean_store::RoomStoreError::UnknownAgentBinding { .. }) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"ok": false, "error": "agent_binding_required"})),
            );
        }
        Err(error) => {
            let code = match error.to_string().as_str() {
                value if value.contains("invoke_message_not_found") => "invoke_message_not_found",
                value if value.contains("invoke_author_mismatch") => "invoke_author_mismatch",
                _ => "invoke_membership_required",
            };
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"ok": false, "error": code})),
            );
        }
    };
    let admission = match room_agent_authority::admit_room_agent(
        &state,
        &room,
        agent_member_id,
        &package_id,
        AdmissionTrigger::Explicit,
    )
    .await
    {
        Ok(admission) => admission,
        Err(error) => return error.response(),
    };
    // The binding gate precedes the one authoritative transcript lookup. A
    // client cannot use invoke as a transcript oracle for a room agent that is
    // not currently admissible, and the later checked registration still
    // revalidates the exact authority generation before any runtime context.
    let invocation_message = with_rooms(&state, |store| {
        room_message_at_seq(store, &room, body.message_seq)
            .ok_or_else(|| ocean_store::RoomStoreError::Encode("invoke_message_not_found".into()))
    });
    let invocation_message = match invocation_message {
        Ok(message)
            if message.author_id == invoked_by
                && !matches!(
                    message.author_kind,
                    RoomParticipantKind::Agent | RoomParticipantKind::System
                ) =>
        {
            message
        }
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"ok": false, "error": "invoke_author_mismatch"})),
            );
        }
        Err(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"ok": false, "error": "invoke_message_not_found"})),
            );
        }
    };
    let request_id = Uuid::new_v4();
    let queued = spawn_room_agent_turn(
        state,
        admission,
        agent,
        invocation_message.seq,
        federated.then(|| agent_member_id.to_string()),
        request_id,
        body.decision_token,
        None,
    )
    .await;
    match queued {
        Ok(queued) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "ok": true,
                "status": "queued",
                "admission_id": queued.admission_id,
                "request_id": queued.request_id,
                "generation": queued.generation.to_string(),
                "session_id": queued.session_id,
            })),
        ),
        Err(error) => error.response(),
    }
}

/// The room family's one shape for "your request was malformed". Shared with
/// `room_attachments.rs` so a bad key or a missing field looks identical
/// whichever room route rejected it.
pub(super) fn invalid_request_response() -> (StatusCode, Json<serde_json::Value>) {
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

/// Map one `IntentError` onto its metrics label (§4.1). Kept beside the route
/// that renders the same error to the wire so the counter's reason and the
/// response's error code can never drift apart.
fn redemption_failure_reason(error: IntentError) -> crate::metrics::RedemptionFailure {
    use crate::metrics::RedemptionFailure as R;
    match error {
        IntentError::Invalid => R::Invalid,
        IntentError::NotFound => R::NotFound,
        IntentError::Conflict => R::Conflict,
        IntentError::Forbidden => R::Forbidden,
        IntentError::InviteForbidden => R::InviteForbidden,
        IntentError::Unavailable => R::Unavailable,
        IntentError::Protocol => R::Protocol,
        IntentError::Store => R::Store,
    }
}

pub(super) async fn room_redeem_invite(
    State(state): State<AppState>,
    body: Result<Json<RedeemInviteBody>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    // NOTE (§4.1): the redemption-failure counter is bumped HERE and not on
    // `FederationSupervisor::redeem_invite`'s return, because this route refuses
    // two cases before that call is ever made — a malformed body, and a blank
    // code. Both render the identical `400 {"ok":false,"error":"invalid_request"}`
    // that `IntentError::Invalid` renders through `intent_error_response`, so a
    // counter attached to the supervisor alone would read zero for exactly the
    // refusal an operator is most likely to see. They are counted as `invalid`
    // for the same reason: the wire cannot tell them apart either.
    let Ok(Json(body)) = body else {
        state
            .room_metrics
            .record_redemption_failure(crate::metrics::RedemptionFailure::Invalid);
        return invalid_request_response();
    };
    if body.code.trim().is_empty() {
        state
            .room_metrics
            .record_redemption_failure(crate::metrics::RedemptionFailure::Invalid);
        return invalid_request_response();
    }
    match state.room_federation.redeem_invite(&body.code).await {
        Ok(redeemed) => (
            StatusCode::OK,
            Json(serde_json::to_value(redeemed).expect("RoomRedeemResponse serializes")),
        ),
        Err(error) => {
            state
                .room_metrics
                .record_redemption_failure(redemption_failure_reason(error));
            intent_error_response(error)
        }
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

pub(super) async fn room_remove_member(
    State(state): State<AppState>,
    Path((key, member_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let member_id = member_id.trim();
    if member_id.is_empty() {
        return invalid_request_response();
    }
    let key = RoomKey::new(key.trim());
    match state.room_federation.remove_member(&key, member_id).await {
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
        let agent = RoomParticipant {
            id: agent_name.clone(),
            kind: RoomParticipantKind::Agent,
            display_name: agent_name.clone(),
        };
        let admission = match room_agent_authority::admit_room_agent(
            &state,
            &dispatch.room,
            &dispatch.target_member_id,
            &agent_name,
            match dispatch.trigger_kind {
                FederatedTriggerKind::Mention => AdmissionTrigger::Mention,
                FederatedTriggerKind::ThreadReply => AdmissionTrigger::ThreadReply,
                FederatedTriggerKind::Unknown => AdmissionTrigger::Unknown,
            },
        )
        .await
        {
            Ok(admission) => admission,
            Err(_) => continue,
        };
        if let Err(error) = spawn_room_agent_turn(
            state.clone(),
            admission,
            agent,
            dispatch.local_seq,
            Some(dispatch.target_member_id.clone()),
            Uuid::new_v4(),
            None,
            Some(RoomTurnFootprint {
                payload: json!({
                    "room": dispatch.room.as_str(),
                    "target": dispatch.target_member_id,
                    "agent_name": agent_name,
                    "reason": dispatch.reason,
                    "triggered_by_seq": dispatch.local_seq,
                    "ledger_event_id": dispatch.ledger_event_id,
                }),
                audit_line: None,
            }),
        )
        .await
        {
            tracing::info!(room = %dispatch.room, agent = %dispatch.target_member_id,
                reason = error.code(),
                "federated room-agent turn refused before runtime dispatch");
        }
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
#[cfg(test)]
const ROOM_AGENT_SESSION_NS: Uuid = Uuid::from_u128(0x0ce1_a111_0000_4780_8000_526f_6f6d_4147);

/// Authorized room sessions use a separate domain and include binding
/// generation. This prevents a Phase 1 turn from resuming a legacy room session
/// that may contain operator memory or tools from broader prior authority.
const AUTHORIZED_ROOM_AGENT_SESSION_NS: Uuid =
    Uuid::from_u128(0x0ce1_a112_0000_4780_8000_526f_6f6d_4131);

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
#[cfg(test)]
pub(super) fn room_agent_session_id(room: &RoomKey, participant_id: &str) -> AgentSessionId {
    let seed = format!("{}:{}", room.as_str(), participant_id);
    sdk_sid(Uuid::new_v5(&ROOM_AGENT_SESSION_NS, seed.as_bytes()))
}

pub(super) fn authorized_room_agent_session_id(
    room: &RoomKey,
    agent_member_id: &str,
    generation: u64,
) -> AgentSessionId {
    let mut seed = Vec::with_capacity(room.as_str().len() + agent_member_id.len() + 24);
    for value in [room.as_str().as_bytes(), agent_member_id.as_bytes()] {
        seed.extend_from_slice(&(value.len() as u64).to_be_bytes());
        seed.extend_from_slice(value);
    }
    seed.extend_from_slice(&generation.to_be_bytes());
    sdk_sid(Uuid::new_v5(&AUTHORIZED_ROOM_AGENT_SESSION_NS, &seed))
}

fn room_message_at_seq(
    store: &ocean_store::SqliteRoomStore,
    room: &RoomKey,
    seq: u64,
) -> Option<RoomMessage> {
    store
        .transcript_page(room, Some(seq.saturating_sub(1)), Some(1))
        .ok()?
        .messages
        .into_iter()
        .find(|message| message.seq == seq)
}

/// Read exactly the transcript set authorized by the binding. Admission has
/// already succeeded before this function is reachable.
fn authorized_room_transcript_context(
    state: &AppState,
    admission: &RoomAgentAdmission,
    triggered_by_seq: u64,
) -> Vec<RoomMessage> {
    with_rooms(state, |store| match admission.context_policy {
        ContextPolicy::InvocationOnly => {
            let Some(trigger) = room_message_at_seq(store, &admission.room, triggered_by_seq)
            else {
                return Vec::new();
            };
            let mut rows = trigger
                .thread_parent_seq
                .and_then(|seq| room_message_at_seq(store, &admission.room, seq))
                .into_iter()
                .collect::<Vec<_>>();
            if rows.last().is_none_or(|row| row.seq != trigger.seq) {
                rows.push(trigger);
            }
            rows
        }
        ContextPolicy::RoomRecent | ContextPolicy::RoomHistory => {
            let latest = store
                .room_latest_durable_seq(&admission.room)
                .ok()
                .flatten()
                .unwrap_or(0);
            let after = latest.saturating_sub(ROOM_CONTEXT_TAIL as u64);
            store
                .transcript_page(&admission.room, Some(after), Some(ROOM_CONTEXT_TAIL))
                .map(|page| page.messages)
                .unwrap_or_default()
        }
    })
}

/// Build the prompt handed to a woken agent: a framing header that tells it it's
/// answering a mention in a room, the recent transcript as context, a pointer at
/// the triggering line, and the room's context files. `tail` is oldest→newest.
///
/// `context_files` is the block from `room_context`, or `None` when the room has
/// no attachments — and `None` must reproduce the prompt byte for byte as it was
/// before context files existed. Every room that never uploads a file keeps the
/// prompt it already had, so this feature cannot perturb an unrelated room's
/// agent behavior.
///
/// Bodies run through `room_history_text`, the same collapse the human reads,
/// the agent history page, and `/summarize` apply. `tail` arrives from
/// `authorized_room_transcript_context`, which pages the store raw and filters
/// no kind, so without it a `room.agent.*` audit row inside the last
/// `ROOM_CONTEXT_TAIL` messages hands this model the ids that audit
/// interpolates — an `owner_member_id` among them, which the mutation routes
/// refuse ill-shaped since `room_agent_authority::validate_member_id` but which
/// rows minted before that guard still carry verbatim, because the record is a
/// ledger and is never rewritten. The answer this model shapes is appended to
/// the room and markdown-rendered by
/// ocean-surface, which is the same laundered route the read boundary closes.
/// Under `context_policy:room_history` an unprojected tail would also serve one
/// turn the same row twice, as a label through the bounded history tool and raw
/// through here.
fn build_room_prompt(
    room: &RoomKey,
    agent: &RoomParticipant,
    tail: &[ocean_core::RoomMessage],
    triggered_by_seq: u64,
    context_files: Option<&str>,
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
            body = room_history_text(m.body.clone()),
            marker = marker,
        ));
    }
    out.push_str("--- end transcript ---\n");
    // After the transcript, before the cue to answer: the files are background
    // the reply should be grounded in, not the thing being replied to.
    if let Some(block) = context_files {
        out.push('\n');
        out.push_str(block);
    }
    out.push_str("\nYour reply:");
    out
}

#[derive(Debug, Clone)]
struct RoomTurnFootprint {
    payload: serde_json::Value,
    audit_line: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct QueuedRoomAgentTurn {
    pub(super) request_id: Uuid,
    pub(super) session_id: AgentSessionId,
    pub(super) admission_id: String,
    pub(super) generation: u64,
}

#[derive(Debug)]
enum RoomTurnStartError {
    Authority(ApiError),
    WorkspaceUnavailable,
    RoomHistoryUnavailable,
}

impl RoomTurnStartError {
    fn code(&self) -> &'static str {
        match self {
            Self::Authority(error) => error.code(),
            Self::WorkspaceUnavailable => "workspace_unavailable",
            Self::RoomHistoryUnavailable => "room_history_unavailable",
        }
    }

    fn response(self) -> (StatusCode, Json<serde_json::Value>) {
        match self {
            Self::Authority(error) => error.response(),
            Self::WorkspaceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": "workspace_unavailable"})),
            ),
            Self::RoomHistoryUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": "room_history_unavailable"})),
            ),
        }
    }
}

/// Register a generation-bound turn before any room context read, write its
/// durable allow audit, then emit the legacy convene footprint and start the
/// runtime. Refusals never reach the footprint or request registry.
#[allow(clippy::too_many_arguments)]
async fn spawn_room_agent_turn(
    state: AppState,
    admission: RoomAgentAdmission,
    agent: RoomParticipant,
    triggered_by_seq: u64,
    federated_member_id: Option<String>,
    request_id: Uuid,
    decision_token: Option<String>,
    footprint: Option<RoomTurnFootprint>,
) -> Result<QueuedRoomAgentTurn, RoomTurnStartError> {
    let room = admission.room.clone();
    let room_workspace = with_rooms(&state, |reg| {
        reg.get(&room)
            .ok()
            .flatten()
            .and_then(|record| record.room.workspace_root)
    });
    let Some(cwd) = room_workspace.as_deref().and_then(persisted_room_workspace) else {
        room_agent_authority::append_remote_output_outcome(
            &state,
            &admission,
            "refused",
            "workspace_unavailable",
        )
        .map_err(RoomTurnStartError::Authority)?;
        return Err(RoomTurnStartError::WorkspaceUnavailable);
    };
    let project_id = state
        .runtime
        .project_for_workspace(&cwd)
        .ok()
        .flatten()
        .map(|project| project.id);
    let room_history = if admission.context_policy == ContextPolicy::RoomHistory {
        match state.runtime.admit_room_history(
            &admission,
            Arc::new(DurableRoomHistorySource {
                rooms: state.rooms.clone(),
            }),
        ) {
            Ok(history) => Some(history),
            Err(_) => {
                room_agent_authority::append_remote_output_outcome(
                    &state,
                    &admission,
                    "refused",
                    "room_history_unavailable",
                )
                .map_err(RoomTurnStartError::Authority)?;
                return Err(RoomTurnStartError::RoomHistoryUnavailable);
            }
        }
    } else {
        None
    };
    let session_id =
        authorized_room_agent_session_id(&room, &admission.agent_member_id, admission.generation);
    let is_new = state.runtime.session_detail(core_sid(session_id)).is_err();
    let session_lease = state.runtime.session_operation(core_sid(session_id)).await;
    let permission_mode = effective_permission_mode();
    let mut prompt_req = PromptRequest {
        prompt: String::new(),
        images: None,
        request_id: Some(request_id),
        session_id: Some(core_sid(session_id)),
        create_if_missing: is_new,
        max_turns: None,
        yolo: permission_mode == PermissionMode::SkipAll,
        cwd,
        project_id,
        client_type: Some("room".to_string()),
        decision_token,
    };
    let authority = RoomAgentRequestAuthority {
        room: room.clone(),
        agent_member_id: admission.agent_member_id.clone(),
        generation: admission.generation,
        admission_id: admission.admission_id.clone(),
        decision_id: admission.decision_id.clone(),
        approved_definition_digest: admission.package.definition_digest.clone(),
        session_id: core_sid(session_id),
    };
    let registration = register_room_agent_request_checked(
        &state.requests,
        &mut prompt_req,
        format!("room agent {} in room {}", agent.id, room.as_str()),
        authority,
        || room_agent_authority::append_admission_allow(&state, &admission),
    )
    .await;
    let (_registered_request, cancel) = match registration {
        Ok(value) => value,
        Err(error) => {
            tracing::info!(room = %room, agent = %admission.agent_member_id, reason = error.code(),
                "room-agent admission changed before request registration");
            return Err(RoomTurnStartError::Authority(error));
        }
    };
    emit_session_changed(&state.agent_events, session_id);

    if let Some(footprint) = footprint {
        state.agent_events.emit(AgentTurnEvent::Extension {
            extension: "room_trigger".into(),
            payload: footprint.payload,
            scope: None,
        });
        if let Some(line) = footprint.audit_line {
            let _ = append_room_message(
                &state,
                &room,
                "system",
                RoomParticipantKind::System,
                RoomMessageKind::System,
                &line,
            );
        }
    }

    let queued = QueuedRoomAgentTurn {
        request_id,
        session_id,
        admission_id: admission.admission_id.clone(),
        generation: admission.generation,
    };
    tokio::spawn(async move {
        let tail = authorized_room_transcript_context(&state, &admission, triggered_by_seq);
        let context_files = if admission.context_policy == ContextPolicy::RoomHistory {
            let attachments =
                with_rooms(&state, |store| store.attachments(&room)).unwrap_or_else(|error| {
                    tracing::warn!(room = %room, %error,
                        "room-history attachment context unavailable for this turn");
                    Vec::new()
                });
            crate::room_context::build_attachment_context(
                &attachments,
                |row| {
                    crate::room_attachments::attachment_bytes(
                        state.room_attachments_root.as_path(),
                        &room,
                        row,
                    )
                },
                crate::room_context::ROOM_CONTEXT_BYTE_BUDGET,
            )
        } else {
            None
        };
        let prompt = build_room_prompt(
            &room,
            &agent,
            &tail,
            triggered_by_seq,
            context_files.as_deref(),
        );
        prompt_req.prompt = match admission.package.instructions_layer.as_deref() {
            Some(instructions) => super::compose_folder_agent_prompt(instructions, &prompt),
            None => prompt,
        };
        let control = build_prompt_control(
            &state,
            request_id,
            Some(core_sid(session_id)),
            permission_mode,
            cancel,
            prompt_req.decision_token.clone(),
        );
        let control = room_agent_authority::apply_admission_to_control(control, &admission);
        let control = match room_history {
            Some(history) => control.with_room_history(history),
            None => control,
        };
        #[cfg(test)]
        capture_room_turn(&agent.id, &prompt_req.prompt, &control);

        let result = state
            .runtime
            .prompt_with_lease(prompt_req, control, &session_lease)
            .await;
        record_prompt_result(&state, request_id, &result, None, None).await;
        emit_session_changed(&state.agent_events, session_id);

        if result.ok {
            let body = clamp_room_message_body(result.stdout.trim());
            if !body.is_empty() {
                if let Some(member_id) = federated_member_id.as_deref() {
                    if state
                        .room_federation
                        .enqueue_authorized_federated_agent_message(
                            &room,
                            member_id,
                            admission.generation,
                            &admission.admission_id,
                            body.as_ref(),
                        )
                        .await
                        .is_err()
                    {
                        let reason = if room_agent_authority::admission_generation_is_current(
                            &state, &admission,
                        ) {
                            "remote_enqueue_failed"
                        } else {
                            "authority_changed_before_remote_enqueue"
                        };
                        if let Err(error) = room_agent_authority::append_remote_output_outcome(
                            &state, &admission, "refused", reason,
                        ) {
                            tracing::warn!(room = %room, agent = %agent.id,
                                reason = error.code(),
                                "failed to persist federated room-agent refusal audit");
                        }
                        tracing::warn!(room = %room, outcome = "agent_reply_enqueue_failed",
                            "federated agent reply enqueue failed");
                    }
                } else {
                    let thread_root = with_rooms(&state, |store| {
                        room_message_at_seq(store, &room, triggered_by_seq)
                    })
                    .and_then(|message| message.thread_parent_seq)
                    .unwrap_or(triggered_by_seq);
                    let _ = append_authorized_room_agent_reply(
                        &state,
                        &admission,
                        body.as_ref(),
                        Some(thread_root),
                        session_id,
                    );
                }
            }
        } else if federated_member_id.is_none() {
            if let Err(error) = append_authorized_room_agent_failure(&state, &admission, session_id)
            {
                tracing::warn!(room = %room, agent = %agent.id, %error,
                    "failed room-agent turn lost authority before durable failure row");
            }
        }
    });
    Ok(queued)
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

/// `/snapshot`'s query — [`TranscriptQuery`] plus the backward cursor.
///
/// The field lives here rather than on `TranscriptQuery` because only
/// `/snapshot` serves a backward page; putting it on the shared struct would
/// advertise a parameter `/transcript` silently drops. The two cursors are
/// mutually exclusive and the handler rejects them together rather than
/// inventing a precedence rule a caller would have to know.
#[derive(serde::Deserialize)]
pub(super) struct SnapshotQuery {
    /// Forward cursor: return only entries with `seq > after_seq`.
    #[serde(default)]
    pub(super) after_seq: Option<u64>,
    /// Backward cursor: return the NEWEST `limit` entries with `seq < before_seq`,
    /// still ascending. Present at all ⇒ the read runs from the newest end. A
    /// `before_seq` above every stored seq is therefore how a client opens a room
    /// at its tail without first knowing the last seq — that is the literal
    /// meaning of the parameter, not a sentinel.
    #[serde(default)]
    pub(super) before_seq: Option<u64>,
    /// Max rows to return in this page; same clamping as [`TranscriptQuery`].
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

/// Read one bounded transcript page for a room, open or soft-closed
/// (OCEAN-249 + OCEAN-170).
///
/// One store query for both room states, because the closed room is the one this
/// read used to get wrong. It served a frozen room by windowing the record from
/// `get_including_closed` in memory, and that record IS the oldest
/// `MAX_TRANSCRIPT_LIMIT` rows: `has_more` came out as `msgs.len() >
/// effective_limit` over rows the record had already dropped, so a soft-closed room
/// with twelve thousand messages answered `has_more: false, next_seq: null` on row
/// 999 and a paging client stopped there believing it had the log.
/// `RoomRecord::transcript_has_more` could have told it the answer was short, but
/// the same record still cannot produce row 1000 — the honest fix is the query, not
/// the marker. `transcript_page_including_closed` gates on existence rather than
/// openness, so a room that never existed is still `UnknownRoom` and the handlers
/// keep their 404, and the forward and backward reads are now the same shape:
/// see `read_transcript_tail_page`, which has argued for exactly this since it
/// landed.
///
/// `pub(super)` so `room_summary.rs` reads its bounded window through the SAME
/// paging implementation rather than growing a second one.
pub(super) fn read_transcript_page(
    reg: &ocean_store::SqliteRoomStore,
    key: &RoomKey,
    after_seq: Option<u64>,
    limit: Option<usize>,
) -> Result<ocean_store::TranscriptPage, ocean_store::RoomStoreError> {
    reg.transcript_page_including_closed(key, after_seq, limit)
}

/// Read one bounded transcript page from the NEWEST end, serving a soft-closed
/// room as [`read_transcript_page`] does for the forward read.
///
/// The closed room is the whole reason this exists as a sibling rather than living
/// inside the handler. A finished call's room is closed but still replayable, and
/// if only the open path could read from the tail then a frozen call room would
/// paint its OLDEST page while a live one painted its newest — the same hydration,
/// two different screens, decided by whether the call had ended.
///
/// Like the forward read it goes to the store's rows and not to the frozen record:
/// `get_including_closed` hydrates the oldest `MAX_TRANSCRIPT_LIMIT` of them, so a
/// window over that record answers the newest page of the first thousand and calls
/// it the tail — and a 12,000-message room, the case this window exists for, is
/// exactly where that lands. `transcript_tail_page_including_closed` gates on
/// existence rather than openness, so the open and closed answers are one query
/// with one contract, and a room that never existed is still `UnknownRoom`.
pub(super) fn read_transcript_tail_page(
    reg: &ocean_store::SqliteRoomStore,
    key: &RoomKey,
    before_seq: Option<u64>,
    limit: Option<usize>,
) -> Result<ocean_store::TranscriptTailPage, ocean_store::RoomStoreError> {
    reg.transcript_tail_page_including_closed(key, before_seq, limit)
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
/// Serves a soft-closed room through the same query as an open one: a finished call
/// closes its room on `CallEnded` (OCEAN-170), but its transcript must stay
/// queryable afterwards — that is the whole reason it was persisted. The rows come
/// from the store in both cases and never from the frozen record, which holds only
/// the oldest `MAX_TRANSCRIPT_LIMIT` of them and so cannot answer a page past that.
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
                "transcript": projected_transcript(page.messages),
                "next_seq": page.next_seq,
                "has_more": page.has_more,
            })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// The two paging directions collapsed to what the snapshot body needs, so the
/// response builder answers "which cursors does this page carry?" once instead
/// of branching on direction a second time.
struct SnapshotTranscript {
    messages: Vec<RoomMessage>,
    /// Forward cursor. `None` on a backward read: that read computes no forward
    /// cursor, and `last_seq` is what a tail-opened client replays as `after_seq`
    /// to walk toward the present or to open `/events`.
    next_seq: Option<u64>,
    /// Backward cursor — the oldest row on the page, replayed as `before_seq`.
    /// `None` on a forward read.
    prev_seq: Option<u64>,
    /// Whether more rows exist IN THE DIRECTION THIS PAGE WAS PAGING: newer ones
    /// for a forward read, older ones for a backward one. The direction is set by
    /// which cursor the caller supplied, so a client that knows what it asked for
    /// knows what the flag means.
    has_more: bool,
}

/// `GET /v1/rooms/persistent/{key}/snapshot` — full room hydration in one read:
/// the room entity (id, name, roster, timestamps, trigger policy), one bounded
/// transcript page, and `last_seq` so the caller can immediately tail live updates
/// via `GET /v1/rooms/persistent/{key}/events?after_seq=last_seq`.
///
/// This is the store-backed realization of the collaboration model's "Room
/// hydration / snapshot" step (OCEAN-232): switching into a room must load full
/// state, not just subscribe to future events. Persistent rooms carry everything
/// hydration needs, so this endpoint serves the durable snapshot directly.
///
/// `before_seq` chooses which END of the log that page comes from. Without it the
/// read runs forward from the start, as it always has — which for a room with
/// 12,000 messages means hydration opens at message #1 and the tail, the only part
/// anyone wanted, is reachable only by transferring the whole log. With it the
/// page is the NEWEST `limit` rows before that cursor, still ascending, and
/// `prev_seq` pages further back. A `before_seq` above every stored seq is how a
/// client opens at the tail before it knows the last seq. Supplying both cursors
/// is a typed 400 rather than a precedence rule, because a caller that sent both
/// has two different pages in mind and neither answer would be the one it meant.
///
/// Like `room_transcript`, serves a finished call's frozen room (closed on
/// `CallEnded`, OCEAN-170) so it stays hydratable for replay — in BOTH directions,
/// so a frozen room and a live one hydrate to the same screen. `closed` in the body
/// says which of the two it was: without it a hydrating client cannot tell a frozen
/// room from a live one, so it opens a tail that the events route will never feed
/// and a composer whose every send is rejected.
///
/// `agent_owners` is the same projection `room_get` serves, for the same reason
/// `closed` is here: ocean-surface#185 moved hydration onto this route, and a
/// field only `room_get` sends is a field the surface can no longer read. It
/// annotates the roster in the same body — who owns each agent, and whether that
/// worker is still present.
pub(super) async fn room_snapshot(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<SnapshotQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    if q.after_seq.is_some() && q.before_seq.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "conflicting_transcript_cursors",
                "error": "after_seq and before_seq page in opposite directions; supply at most one",
            })),
        );
    }
    let key = RoomKey::new(trimmed);
    // Hydrate room metadata (entity + roster) and ONE bounded transcript page under
    // one lock. The transcript is not the room's entire log poured into one response
    // (OCEAN-249): a long-lived call room would make every hydration a full-table
    // read. We serve `limit` rows plus the cursor for the direction asked for, so the
    // client immediately knows whether to page (`/transcript?after_seq=next_seq`, or
    // `/snapshot?before_seq=prev_seq` going back) or tail (`/events?after_seq=last_seq`).
    // The metadata read prefers the live room and falls back to the soft-closed
    // audit view (OCEAN-170); the transcript read does not fall back at all, being a
    // single query gated on existence rather than openness. The std mutex guard is
    // dropped inside `with_rooms`; it is never held across an `.await`.
    let result = with_rooms(&state, |reg| {
        // Room metadata: live first, then audit for a soft-closed room. WHICH arm
        // answered is the closedness signal — `get` filters on `closed_at IS NULL`
        // and `get_including_closed` does not — and taking it here, under the same
        // lock as the read, is why the flag cannot disagree with the record it
        // describes. Asking separately would mean a second `with_rooms` call, and a
        // close landing between the two would answer with a flag that contradicts
        // the transcript beside it.
        let (record, closed) = match reg.get(&key) {
            Ok(Some(rec)) => (Some(rec), false),
            Ok(None) => (reg.get_including_closed(&key)?, true),
            Err(e) => return Err(e),
        };
        let Some(record) = record else {
            return Ok(None);
        };
        let page = match q.before_seq {
            Some(before) => {
                let tail = read_transcript_tail_page(reg, &key, Some(before), q.limit)?;
                SnapshotTranscript {
                    messages: tail.messages,
                    next_seq: None,
                    prev_seq: tail.prev_seq,
                    has_more: tail.has_more,
                }
            }
            None => {
                let forward = read_transcript_page(reg, &key, q.after_seq, q.limit)?;
                SnapshotTranscript {
                    messages: forward.messages,
                    next_seq: forward.next_seq,
                    prev_seq: None,
                    has_more: forward.has_more,
                }
            }
        };
        // Access projection (S2-P1): the room's federated state, outbox, and
        // member roster (Local if no access row exists).
        let access = reg.room_access(&key)?;
        // Which worker owns which agent in THIS room, on the same lock as the
        // roster it annotates. A hydrating client renders ownership beside the
        // participants it just read, so the two coming from one acquisition is
        // what stops a join landing between them and painting an agent whose
        // owner the roster does not list.
        let owners = reg.agent_owners(&key)?;
        Ok(Some((record, page, access, closed, owners)))
    });
    match result {
        Ok(Some((rec, page, access, closed, owners))) => {
            let last_seq = page.messages.last().map(|m| m.seq);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "room": rec.room.clone(),
                    "participants": rec.room.participants,
                    "transcript": projected_transcript(page.messages),
                    "last_seq": last_seq,
                    "next_seq": page.next_seq,
                    "prev_seq": page.prev_seq,
                    "has_more": page.has_more,
                    "access": access,
                    "closed": closed,
                    "agent_owners": projected_agent_owners(owners),
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
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RoomReadCursorPatchRequest {
    pub(super) read_seq: u64,
}

/// Canonical `GET`/`PATCH .../read-cursor` response body — used for BOTH
/// Local and Live rooms so callers see one schema regardless of access
/// state. `read_seq` is stringified (matching every other sequence number
/// in this API) because raw `u64` values above 2^53 are not
/// JS-number-precision-safe.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct RoomReadCursorBody {
    room_id: String,
    read_seq: Option<String>,
}

fn local_room_read_cursor_principal() -> &'static str {
    "daemon-local-room-read-cursor"
}

/// The read-cursor mirror principal for a Live room is the local human's
/// per-room federated member id installed with the room credential — NOT a
/// fixed constant. `room_federation::FederationSupervisor::room_get_read_cursor`
/// / `room_patch_read_cursor` always mutate `room_read_cursor_mirrors` keyed
/// by `credential.local_human_member_id`, so any reader here must resolve the
/// exact same key or it will silently observe an always-empty cursor (H1).
fn live_room_read_cursor_principal(
    store: &ocean_store::SqliteRoomStore,
    key: &RoomKey,
) -> Result<Option<String>, ocean_store::RoomStoreError> {
    Ok(store
        .room_credential(key)?
        .map(|credential| credential.local_human_member_id))
}

fn room_read_cursor_unsupported_response(
    state: RoomAccessState,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "ok": false,
            "code": "room_read_cursor_unsupported",
            "error": format!("read cursor unsupported for access state '{state:?}'")
                .to_lowercase()
                .replace("roomaccessstate::", "")
        })),
    )
}

pub(super) async fn room_get_read_cursor(
    State(state): State<AppState>,
    Path(raw_key): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    match with_rooms(&state, |store| {
        let access = store.room_access(&key)?;
        match access.state {
            RoomAccessState::Local => Ok(Ok(store
                .room_read_cursor(&key, local_room_read_cursor_principal())?
                .read_seq)),
            RoomAccessState::Live => Ok(Err(RoomAccessState::Live)),
            other => Ok(Err(other)),
        }
    }) {
        Ok(Ok(read_seq)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "cursor": RoomReadCursorBody {
                    room_id: key.as_str().to_string(),
                    read_seq: read_seq.map(|seq| seq.to_string()),
                }
            })),
        ),
        Ok(Err(RoomAccessState::Live)) => {
            match state.room_federation.room_get_read_cursor(&key).await {
                Ok(cursor) => (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "cursor": RoomReadCursorBody {
                            room_id: key.as_str().to_string(),
                            read_seq: cursor
                                .mirrored_upstream_read_seq
                                .map(|seq| seq.to_string()),
                        }
                    })),
                ),
                Err(crate::room_federation::IntentError::Forbidden) => (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "ok": false, "error": "membership_revoked" })),
                ),
                Err(crate::room_federation::IntentError::Conflict) => {
                    room_read_cursor_unsupported_response(RoomAccessState::Live)
                }
                Err(crate::room_federation::IntentError::NotFound) => (
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({ "ok": false, "error": format!("no open room with key '{}'", key) }),
                    ),
                ),
                Err(_) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "ok": false, "error": "federated read cursor unavailable" })),
                ),
            }
        }
        Ok(Err(state)) => room_read_cursor_unsupported_response(state),
        Err(e) => room_store_error_response(e),
    }
}

pub(super) async fn room_patch_read_cursor(
    State(state): State<AppState>,
    Path(raw_key): Path<String>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: RoomReadCursorPatchRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return invalid_request_response(),
    };
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid room key; must be non-empty" })),
        );
    }
    let key = RoomKey::new(trimmed);
    match with_rooms(&state, |store| {
        let access = store.room_access(&key)?;
        if access.state == RoomAccessState::Local {
            let cursor = store.update_room_read_cursor(
                &key,
                local_room_read_cursor_principal(),
                RoomReadCursorUpdateRequest {
                    read_seq: req.read_seq,
                },
            )?;
            return Ok(Ok(cursor.read_seq));
        }
        if access.state == RoomAccessState::Live {
            return Ok(Err(RoomAccessState::Live));
        }
        Ok(Err(access.state))
    }) {
        Ok(Ok(read_seq)) => {
            publish_room_read_cursor_wake(&state, &key);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "cursor": RoomReadCursorBody {
                        room_id: key.as_str().to_string(),
                        read_seq: read_seq.map(|seq| seq.to_string()),
                    }
                })),
            )
        }
        Ok(Err(RoomAccessState::Live)) => match state
            .room_federation
            .room_patch_read_cursor(&key, req.read_seq)
            .await
        {
            Ok(cursor) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "cursor": RoomReadCursorBody {
                        room_id: key.as_str().to_string(),
                        read_seq: cursor.mirrored_upstream_read_seq.map(|seq| seq.to_string()),
                    }
                })),
            ),
            Err(crate::room_federation::IntentError::Conflict) => {
                room_read_cursor_unsupported_response(RoomAccessState::Live)
            }
            Err(crate::room_federation::IntentError::Forbidden) => (
                StatusCode::FORBIDDEN,
                Json(json!({ "ok": false, "error": "membership_revoked" })),
            ),
            Err(crate::room_federation::IntentError::NotFound) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": format!("no open room with key '{}'", key) })),
            ),
            Err(crate::room_federation::IntentError::Protocol) => invalid_request_response(),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": "federated read cursor unavailable" })),
            ),
        },
        Ok(Err(state)) => room_read_cursor_unsupported_response(state),
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
/// with the existing `RoomMessage` JSON, its body through
/// [`projected_room_message`] like every other human-facing read. SQLite is
/// authoritative; the bounded broadcast carries wake hints only.
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

    // Subscribe to ALL wake buses BEFORE the first replay query. The cursor
    // tail gets its own independent access-bus subscription (in addition to
    // `access_hints` consumed by `run_room_access_tail` below) so an access
    // wake alone — e.g. a federated Connecting/Recovering -> Live transition
    // that does not itself carry a fresh upstream cursor frame — is enough to
    // make the cursor tail re-check and emit (see `run_room_read_cursor_tail`).
    let message_hints = state.room_wakes.subscribe();
    let access_hints = state.room_access_wakes.subscribe();
    let cursor_access_hints = state.room_access_wakes.subscribe();
    let cursor_hints = state.room_read_cursor_wakes.subscribe();

    // Verify room exists (open rooms only) and read initial access snapshot.
    //
    // `initial_cursor` is `None` for the transitional/dead access states
    // (Connecting/Recovering/Revoked) where the read-cursor projection is
    // undefined (F1/F3). The cursor tail below is spawned unconditionally
    // regardless of the *current* access state: it re-reads access fresh on
    // every wake hint, so a connection opened mid-Connecting/Recovering stays
    // subscribed through the transition and still emits the current cursor
    // the moment access becomes Live, without requiring the client to
    // reconnect (see `run_room_read_cursor_tail`).
    let (initial_access, initial_cursor) = match with_rooms(&state, |store| {
        if store.get(&room)?.is_none() {
            return Err(ocean_store::RoomStoreError::UnknownRoom(room.clone()));
        }
        let access = store.room_access(&room)?;
        let cursor = match access.state {
            RoomAccessState::Local => Some(RoomReadCursorBody {
                room_id: room.as_str().to_string(),
                read_seq: store
                    .room_read_cursor(&room, local_room_read_cursor_principal())?
                    .read_seq
                    .map(|seq| seq.to_string()),
            }),
            RoomAccessState::Live => Some(RoomReadCursorBody {
                room_id: room.as_str().to_string(),
                read_seq: match live_room_read_cursor_principal(store, &room)? {
                    Some(principal) => store
                        .room_read_cursor(&room, &principal)?
                        .mirrored_upstream_read_seq
                        .map(|seq| seq.to_string()),
                    None => None,
                },
            }),
            _ => None,
        };
        Ok((access, cursor))
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
            // The live tail is the other half of one client read — a surface
            // hydrates through `/snapshot` and then tails here — so projecting
            // only the paged reads would leave the injection path that matters
            // wide open while reading as closed.
            let data = serde_json::to_string(&projected_room_message(message))
                .expect("RoomMessage serializable");
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

    // Cursor tail: spawned unconditionally, regardless of the access state
    // observed above. If `/events` is opened while a federated room is
    // Connecting or Recovering — normal startup/reconnect states — the tail
    // must stay subscribed on `cursor_hints` for this same long-lived
    // connection rather than being dropped, so it can wake and emit the
    // current cursor the instant access becomes Live without requiring the
    // client to reconnect. `run_room_read_cursor_tail` re-reads access fresh
    // on every wake hint and already suppresses emissions for the
    // unsupported states (Connecting/Recovering/Revoked) on its own.
    let (cursor_tx, cursor_rx) = mpsc::channel::<RoomReadCursorBody>(16);
    tokio::spawn(run_room_read_cursor_tail(
        state.clone(),
        room.clone(),
        initial_cursor.clone(),
        cursor_hints,
        cursor_access_hints,
        cursor_tx,
    ));
    let cursor_stream = ReceiverStream::new(cursor_rx).map(|cursor| -> Result<Event, Infallible> {
        let data = serde_json::to_string(&cursor).expect("RoomReadCursorBody serializable");
        Ok(Event::default().event("room_read_cursor").data(data))
    });

    // Merge: initial access frame first, then interleave messages + access updates.
    let init_data =
        serde_json::to_string(&initial_access).expect("RoomAccessProjection serializable");
    let init_event = Ok(Event::default().event("room_access").data(init_data));
    let cursor_init = initial_cursor
        .map(|cursor| {
            let data = serde_json::to_string(&cursor).expect("RoomReadCursorBody serializable");
            Ok(Event::default().event("room_read_cursor").data(data))
        })
        .into_iter();
    let cursor_events: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(tokio_stream::iter(cursor_init).chain(cursor_stream));
    let merged = tokio_stream::once(init_event)
        .chain(msg_stream.merge(acc_stream))
        .merge(cursor_events);
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

/// Read-cursor tail: on every wake hint (or lag), re-derive the wire-safe
/// `RoomReadCursorBody` and send it downstream if changed. `read_seq` is
/// stringified here (not left as a raw `u64` on the wire) so this event uses
/// the exact same JS-number-precision-safe schema as the REST
/// `GET`/`PATCH .../read-cursor` handlers (`RoomReadCursorBody`) — one shape
/// for both Local and Live rooms, chosen by which store field is
/// authoritative for the current access state (F3).
///
/// Local and Live are the only access states with a defined, trustworthy
/// read-cursor projection (this matches `room_read_cursor_unsupported_response`
/// in the REST handlers, which reject Connecting/Recovering/Revoked the same
/// way). For those three transitional/dead states this tail must NOT fall
/// back to reading under `local_room_read_cursor_principal()` — that principal
/// was never written to for a federated room, so doing so would silently
/// replace a real federated cursor value with an empty one and flicker the
/// client to a cleared "local" cursor on every Connecting/Recovering/Revoked
/// hop. Instead it skips the emission entirely, leaving `last_cursor`
/// (and the client's last-rendered projection) untouched. The federated
/// credential row is never touched by this skip, so the moment the room
/// returns to Live the credential-scoped principal resolves exactly as
/// before and the tail resumes emitting from where it left off (F1).
///
/// Also selects on `access_hints`: the upstream mirror does not necessarily
/// re-emit a `room_read_cursor` federation frame at the exact moment access
/// flips Connecting/Recovering -> Live (that mirror value may already be
/// durable from before the reconnect), so an access wake alone must be
/// enough to re-derive and emit the current cursor for a connection that was
/// opened mid-transition and has been sitting on a stale/absent projection.
async fn run_room_read_cursor_tail(
    state: AppState,
    room: RoomKey,
    mut last_cursor: Option<RoomReadCursorBody>,
    mut hints: broadcast::Receiver<RoomReadCursorWakeHint>,
    mut access_hints: broadcast::Receiver<RoomAccessWakeHint>,
    tx: mpsc::Sender<RoomReadCursorBody>,
) {
    loop {
        let should_read = tokio::select! {
            _ = tx.closed() => return,
            res = hints.recv() => match res {
                Ok(hint) => hint.room == room,
                Err(broadcast::error::RecvError::Lagged(_)) => true,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = access_hints.recv() => match res {
                Ok(hint) => hint.room == room,
                Err(broadcast::error::RecvError::Lagged(_)) => true,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        if !should_read {
            continue;
        }
        let read_seq = match with_rooms(&state, |store| -> Result<_, ocean_store::RoomStoreError> {
            let access = store.room_access(&room)?;
            match access.state {
                RoomAccessState::Local => Ok(Some(
                    store
                        .room_read_cursor(&room, local_room_read_cursor_principal())?
                        .read_seq,
                )),
                RoomAccessState::Live => match live_room_read_cursor_principal(store, &room)? {
                    Some(principal) => Ok(Some(
                        store
                            .room_read_cursor(&room, &principal)?
                            .mirrored_upstream_read_seq,
                    )),
                    None => Ok(Some(None)),
                },
                // Read-cursor is unsupported while the federated link is not
                // confirmed Live — skip the emission (F1) rather than
                // resolving a principal at all.
                RoomAccessState::Connecting
                | RoomAccessState::Recovering
                | RoomAccessState::Revoked => Ok(None),
            }
        }) {
            Ok(read_seq) => read_seq,
            Err(e) => {
                tracing::warn!(room = %room, %e, "room read cursor tail read failed");
                return;
            }
        };
        let Some(read_seq) = read_seq else {
            continue;
        };
        let cursor = RoomReadCursorBody {
            room_id: room.as_str().to_string(),
            read_seq: read_seq.map(|seq| seq.to_string()),
        };
        if last_cursor.as_ref() != Some(&cursor) {
            last_cursor = Some(cursor.clone());
            if tx.send(cursor).await.is_err() {
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
    use ocean_store::ActivationPolicy;

    #[test]
    fn a_short_body_is_borrowed_untouched() {
        let body = "a normal reply";
        assert!(matches!(
            clamp_room_message_body(body),
            std::borrow::Cow::Borrowed("a normal reply")
        ));
    }

    #[test]
    fn an_overlong_agent_reply_is_marked_rather_than_dropped() {
        let limit = crate::room_federation::OUTBOUND_MESSAGE_BODY_LIMIT;
        let long = "x".repeat(limit + 5_000);
        let clamped = clamp_room_message_body(&long);
        assert!(clamped.len() <= limit, "clamped to {} bytes", clamped.len());
        assert!(clamped.starts_with("xxxx"), "the reply itself survives");
        assert!(clamped.ends_with("[truncated: reply exceeded the room message limit]"));
    }

    #[test]
    fn clamping_never_splits_a_character() {
        // A multi-byte body whose natural cut lands mid-codepoint: the walk
        // back to a boundary is what keeps the result valid UTF-8 at all.
        let limit = crate::room_federation::OUTBOUND_MESSAGE_BODY_LIMIT;
        for pad in 0..4 {
            let body = format!("{}{}", "a".repeat(pad), "\u{1f30a}".repeat(limit));
            let clamped = clamp_room_message_body(&body);
            assert!(clamped.len() <= limit);
            // Owning it as a String round-trips only if the bytes are valid.
            assert_eq!(clamped.to_string().len(), clamped.len());
        }
    }

    #[test]
    fn body_too_large_is_a_413_not_a_500() {
        let (status, body) = post_rejection_response(PostRejection::BodyTooLarge);
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body.0["error"], "body_too_large");
    }

    fn prompt_fixture_tail() -> Vec<ocean_core::RoomMessage> {
        vec![ocean_core::RoomMessage {
            seq: 7,
            author_id: "alice".into(),
            author_kind: RoomParticipantKind::Human,
            kind: RoomMessageKind::Message,
            body: "@researcher what does the spec say".into(),
            created_at: Utc::now(),
            federated: None,
            thread_parent_seq: None,
            session_id: None,
            attachment_id: None,
        }]
    }

    fn prompt_fixture_agent() -> RoomParticipant {
        RoomParticipant {
            id: "researcher".into(),
            kind: RoomParticipantKind::Agent,
            display_name: "Researcher".into(),
        }
    }

    /// The no-attachment prompt must be what it was before context files
    /// existed, to the byte. Every room that never uploads a file is entitled to
    /// the agent behavior it already had, and a stray blank line here is a
    /// silent change to every one of them.
    #[test]
    fn a_room_with_no_context_files_gets_the_prompt_it_always_had() {
        let prompt = build_room_prompt(
            &RoomKey::new("spec-room"),
            &prompt_fixture_agent(),
            &prompt_fixture_tail(),
            7,
            None,
        );
        assert!(prompt.ends_with(
            "[#7] alice: @researcher what does the spec say  «— mention\n\
             --- end transcript ---\n\n\
             Your reply:"
        ));
        assert!(!prompt.contains("context files"));
    }

    /// And with files: its own delimited section between the transcript and the
    /// cue to answer, so neither reads as part of the other.
    #[test]
    fn context_files_sit_between_the_transcript_and_the_reply_cue() {
        let block = crate::room_context::build_attachment_context(
            &[ocean_core::RoomAttachment {
                id: "0".repeat(32),
                filename: "spec.md".into(),
                content_type: "text/markdown".into(),
                byte_len: 9,
                sha256: "0".repeat(64),
                uploaded_by: "alice".into(),
                uploaded_at: "2026-08-27T00:00:00Z".into(),
                on_behalf_of: None,
            }],
            |_| Some(b"the spec\n".to_vec()),
            crate::room_context::ROOM_CONTEXT_BYTE_BUDGET,
        )
        .expect("one text attachment must render a block");
        let prompt = build_room_prompt(
            &RoomKey::new("spec-room"),
            &prompt_fixture_agent(),
            &prompt_fixture_tail(),
            7,
            Some(&block),
        );
        assert!(prompt.contains(
            "--- end transcript ---\n\n\
             --- room context files ---\n"
        ));
        assert!(prompt.contains("[file] spec.md (text/markdown, 9 bytes)\nthe spec\n"));
        assert!(prompt.ends_with("--- end context files ---\n\nYour reply:"));
    }

    /// The convened agent's prompt is the other MODEL-facing render of these
    /// rows, and it is the sharper of the two: `/summarize` writes an artifact,
    /// while this answer is appended straight back into the room by
    /// `append_room_agent_reply` and markdown-rendered by ocean-surface. Its
    /// window comes from `authorized_room_transcript_context`, which pages the
    /// store raw and filters no kind, so an unprojected tail would hand the
    /// model the `owner_member_id` a bootstrap audit interpolates. The HTTP
    /// routes refuse an id shaped like this one now
    /// (`room_agent_authority::validate_member_id`), but a renderer may not
    /// assume the guard ran: rows minted before it are permanent, and this
    /// fixture builds its row the way those did, by calling the store in
    /// process. Under `context_policy:room_history` an unprojected tail would
    /// also serve one turn the same row twice, as a label through the bounded
    /// history tool and raw through here.
    ///
    /// The tail is read back through `transcript_page`, the exact call
    /// `authorized_room_transcript_context` makes, so the row under test is the
    /// one the store actually mints rather than a hand-written body.
    #[test]
    fn a_convened_agents_transcript_tail_projects_an_audit_row() {
        const POISON_OWNER: &str = "[click here](https://evil.co)";
        const PACKAGE: &str = "pkg-interpolated-only-into-the-audit";
        const OPERATOR: &str = "operator:only-in-the-audit";

        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let room = RoomKey::new("prompt-tail-projection");
        store
            .create(room.clone(), "Prompt Tail", None, Utc::now())
            .unwrap();
        store
            .add_participant(
                &room,
                RoomParticipant {
                    id: POISON_OWNER.into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "Owner".into(),
                },
                Utc::now(),
            )
            .unwrap();
        store
            .bootstrap_local_room_agent(
                &room,
                POISON_OWNER,
                prompt_fixture_agent(),
                PACKAGE,
                OPERATOR,
                Utc::now(),
            )
            .expect("bootstrap writes the audit row");
        let mention = store
            .append_message(
                &room,
                "alice",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "@researcher what does the spec say",
                Utc::now(),
            )
            .unwrap();

        let tail = store
            .transcript_page(&room, None, Some(ROOM_CONTEXT_TAIL))
            .expect("the tail this prompt is built from")
            .messages;
        let prompt = build_room_prompt(&room, &prompt_fixture_agent(), &tail, mention.seq, None);

        assert!(
            prompt.contains("[room agent bootstrap audit]"),
            "the audit row must reach the model as its fixed label: {prompt}"
        );
        // Every string only the audit body interpolates. The join markers carry
        // the owner id too, so asserting on those would pass for the wrong reason.
        for leaked in [PACKAGE, OPERATOR, "room.agent.bootstrap", "owner_member_id"] {
            assert!(
                !prompt.contains(leaked),
                "`{leaked}` rode into the convened turn: {prompt}"
            );
        }
        assert!(
            prompt.contains("@researcher what does the spec say  «— mention\n"),
            "an ordinary body and the mention marker are untouched: {prompt}"
        );

        // The ledger is untouched: this projects the prompt, never the record.
        let stored = store.transcript(&room, None).expect("transcript");
        assert!(
            stored
                .iter()
                .any(|m| m.body.contains(POISON_OWNER) && m.body.contains(PACKAGE)),
            "the audit row must still hold verbatim what was attempted"
        );
    }

    #[test]
    fn room_history_projection_omits_audit_and_transport_metadata() {
        let projected = room_history_row(RoomMessage {
            seq: 7,
            author_id: "builder".into(),
            author_kind: RoomParticipantKind::Agent,
            kind: RoomMessageKind::Message,
            body: "durable fact".into(),
            created_at: Utc::now(),
            federated: Some(ocean_core::FederatedMessageMeta {
                ledger_event_id: "private-ledger-correlation".into(),
                global_sequence: 99,
                source_id: "private-source".into(),
                source_sequence: 12,
                client_event_id: "private-client-event".into(),
                origin_principal_id: "private-principal".into(),
                origin_member_id: "private-member".into(),
            }),
            thread_parent_seq: Some(3),
            session_id: Some("private-session".into()),
            attachment_id: Some("private-attachment".into()),
        });
        assert_eq!(projected.seq, 7);
        assert_eq!(projected.author_id, "builder");
        assert_eq!(
            projected.author_kind,
            ocean_agent::RoomHistoryAuthorKind::Agent
        );
        assert_eq!(projected.text, "durable fact");
    }

    #[test]
    fn room_history_redacts_structured_audit_without_breaking_backward_cursor() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let room = RoomKey::new("history-redaction");
        store
            .create(room.clone(), "History Redaction", None, Utc::now())
            .unwrap();
        store
            .authorize_room_agent(
                &room,
                ocean_store::AuthorizeAgentInput {
                    agent_member_id: "builder".into(),
                    agent_package_id: "builder".into(),
                    agent_definition_digest: "sha256:def".into(),
                    agent_definition_revision: None,
                    display_name: "Builder".into(),
                    owner_member_id: "human-1".into(),
                    authorized_by: "operator:test".into(),
                    activation_policy: ocean_store::ActivationPolicy::ExplicitOnly,
                    context_policy: ContextPolicy::RoomHistory,
                    memory_scope: ocean_store::MemoryScope::None,
                    requested_capabilities: Vec::new(),
                    room_capability_grants: Vec::new(),
                    decision_id: "decision-secret".into(),
                    request_digest: "request-secret".into(),
                },
                Utc::now(),
            )
            .unwrap();
        let older = store
            .append_message(
                &room,
                "human-1",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "older user fact",
                Utc::now(),
            )
            .unwrap();
        let audit = store
            .append_message(
                &room,
                "system",
                RoomParticipantKind::System,
                RoomMessageKind::System,
                r#"{"type":"room.agent.bootstrap","operator_principal_id":"operator-private","decision_id":"decision-private","agent_member_id":"builder-private"}"#,
                Utc::now(),
            )
            .unwrap();
        let newer = store
            .append_message(
                &room,
                "human-1",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "newer user fact",
                Utc::now(),
            )
            .unwrap();

        let first = store
            .authorized_room_history_page(&room, "builder", 1, None, 2)
            .unwrap();
        assert!(first.has_more);
        let projected = first
            .messages
            .into_iter()
            .map(room_history_row)
            .collect::<Vec<_>>();
        assert_eq!(projected[0].seq, newer.seq);
        assert_eq!(projected[1].seq, audit.seq);
        assert_eq!(projected[1].text, "[room agent bootstrap audit]");
        assert!(!projected[1].text.contains("operator-private"));
        assert!(!projected[1].text.contains("decision-private"));
        assert!(!projected[1].text.contains("builder-private"));

        let second = store
            .authorized_room_history_page(&room, "builder", 1, Some(projected[1].seq), 2)
            .unwrap();
        assert!(second
            .messages
            .iter()
            .any(|message| message.seq == older.seq && message.body == "older user fact"));
    }

    /// The human half of the same read boundary, on all FOUR of its routes.
    ///
    /// A bootstrap audit interpolates `owner_member_id` verbatim, nothing on the
    /// path bounds or shape-checks it, and ocean-surface markdown-renders every
    /// row body including `System` — so an owner id of `[click here](...)` would
    /// otherwise render as an attacker-labelled anchor in a row the UI attributes
    /// to the room itself. A client hydrates through `/snapshot` and then TAILS
    /// through `/events`, which is why closing only the paged reads would leave
    /// the live path open while reading as done.
    ///
    /// The row comes from the real store writer rather than a hand-rolled body:
    /// the ledger keeps the id exactly as it arrived (asserted below) and the READ
    /// is what neutralizes it. An ordinary message rides along to pin that the
    /// projection is surgical and does not rewrite bodies it was not aimed at.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_rows_reach_every_human_route_projected() {
        use http_body_util::BodyExt as _;

        const POISON_OWNER: &str = "[click here](https://evil.co)";
        const PACKAGE: &str = "pkg-interpolated-only-into-the-audit";
        const PLAIN: &str = "they reverted the map change";

        fn audit_and_plain(
            rows: &[serde_json::Value],
            route: &str,
        ) -> (serde_json::Value, serde_json::Value) {
            let audit = rows
                .iter()
                .find(|row| row["kind"] == "system")
                .unwrap_or_else(|| panic!("{route} dropped the audit row"))
                .clone();
            let plain = rows
                .iter()
                .find(|row| row["kind"] == "message")
                .unwrap_or_else(|| panic!("{route} dropped the human row"))
                .clone();
            (audit, plain)
        }

        fn assert_projected(rows: &[serde_json::Value], route: &str) {
            let (audit, plain) = audit_and_plain(rows, route);
            let body = audit["body"]
                .as_str()
                .unwrap_or_else(|| panic!("{route} audit body is not a string"));
            assert_eq!(
                body, "[room agent bootstrap audit]",
                "{route} served it raw"
            );
            assert!(!body.contains("]("), "{route} kept link syntax: {body}");
            assert!(!body.contains("evil.co"), "{route}: {body}");
            assert!(!body.contains(POISON_OWNER), "{route}: {body}");
            assert!(!body.contains(PACKAGE), "{route}: {body}");
            assert_eq!(plain["body"], PLAIN, "{route} rewrote an ordinary body");
        }

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("audit-projection");
        create_plain_room(&state, &key);
        join_participant(
            &state,
            &key,
            POISON_OWNER,
            RoomParticipantKind::Human,
            "Owner",
        );
        with_rooms(&state, |store| {
            store
                .bootstrap_local_room_agent(
                    &key,
                    POISON_OWNER,
                    RoomParticipant {
                        id: "builder".into(),
                        kind: RoomParticipantKind::Agent,
                        display_name: "Builder".into(),
                    },
                    PACKAGE,
                    "operator:test",
                    Utc::now(),
                )
                .expect("bootstrap writes the audit row");
        });
        append_room_message(
            &state,
            &key,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            PLAIN,
        )
        .expect("plain message");

        // The ledger is untouched: this slice fixes the read, not the record, and
        // `crates/ocean-store/AGENTS.md` rules the audit rows records-not-prose.
        let stored = with_rooms(&state, |store| store.transcript(&key, None)).expect("transcript");
        assert!(
            stored.iter().any(|message| {
                message.kind == RoomMessageKind::System && message.body.contains(POISON_OWNER)
            }),
            "the store must still hold the audit exactly as it arrived"
        );

        for (route, path) in [
            ("room_get", format!("/v1/rooms/persistent/{key}")),
            (
                "room_transcript",
                format!("/v1/rooms/persistent/{key}/transcript"),
            ),
            (
                "room_snapshot",
                format!("/v1/rooms/persistent/{key}/snapshot"),
            ),
        ] {
            let app = room_routes().with_state(state.clone());
            let response = app
                .oneshot(axum::http::Request::get(&path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            let value: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let rows: Vec<serde_json::Value> = value["transcript"]
                .as_array()
                .unwrap_or_else(|| panic!("{route} returned no transcript array"))
                .clone();
            assert_projected(&rows, route);
        }

        // The live tail carries the same projection: replay it off the wire.
        let app = room_routes().with_state(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let mut wire = String::new();
        for _ in 0..8 {
            if sse_room_messages(&wire)
                .iter()
                .any(|row| row["kind"] == "message")
            {
                break;
            }
            let frame = tokio::time::timeout(std::time::Duration::from_millis(500), body.frame())
                .await
                .expect("SSE frame exceeded 500ms")
                .expect("SSE body ended before the transcript replayed")
                .expect("SSE body error");
            wire.push_str(std::str::from_utf8(frame.data_ref().expect("SSE data frame")).unwrap());
        }
        assert_projected(&sse_room_messages(&wire), "room_events");
    }

    /// Every `RoomMessage` decoded out of a raw SSE wire, in arrival order. The
    /// whole accumulated wire is re-scanned each pass because frames may batch —
    /// one HTTP body frame is not guaranteed to hold exactly one event.
    fn sse_room_messages(wire: &str) -> Vec<serde_json::Value> {
        wire.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter(|value| value.get("seq").is_some())
            .collect()
    }

    use crate::tests::{
        authorize_room_agent_fixture, authorize_room_agent_package_fixture,
        canonical_test_workspace, fake_convene_state, write_agent_fixture, TestEnvRestore,
        AUTO_CONVENE_ENV_LOCK,
    };
    use axum::{
        body::{Body, Bytes},
        response::IntoResponse,
    };
    use tower::ServiceExt;

    fn summary_artifact() -> ocean_core::RoomArtifact {
        ocean_core::RoomArtifact {
            id: room_summary::ROOM_SUMMARY_ARTIFACT_ID.to_string(),
            kind: RoomArtifactKind::Note,
            title: room_summary::ROOM_SUMMARY_TITLE.to_string(),
            body: "they reverted the map change".into(),
            state: RoomArtifactState::Open,
            created_by: "alice".into(),
            created_at: "2026-08-26T09:00:00Z".into(),
            updated_by: "alice".into(),
            updated_at: "2026-08-26T09:00:00Z".into(),
            on_behalf_of: None,
            version: 4,
        }
    }

    fn summary_system_line() -> RoomMessage {
        RoomMessage {
            seq: 41,
            author_id: "system".into(),
            author_kind: RoomParticipantKind::System,
            kind: RoomMessageKind::System,
            body: "alice updated 'Room summary' (v4)".into(),
            created_at: Utc::now(),
            federated: None,
            thread_parent_seq: None,
            session_id: None,
            attachment_id: None,
        }
    }

    /// The whole point of the route's error contract: a room with nothing to
    /// say, a model that returned nothing, and a model that repeated itself are
    /// ANSWERS, not faults — and a provider failure never leaks the provider's
    /// own message into the body. Pure: no `AppState`, no env, no provider.
    #[test]
    fn summarize_outcomes_map_to_clean_statuses_without_leaking_provider_detail() {
        use room_summary::SummarizeOutcome;

        let (status, body) = summarize_response(SummarizeOutcome::Wrote {
            artifact: summary_artifact(),
            created: false,
            model: "haiku-x".into(),
            messages_summarized: 200,
            from_seq: 1041,
            to_seq: 1240,
            has_more: false,
            message: Box::new(summary_system_line()),
        });
        // 200 for an amend AND for a create: one success shape, `created` says which.
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["summarized"], json!(true));
        assert_eq!(body.0["created"], json!(false));
        assert_eq!(body.0["model"], json!("haiku-x"));
        assert_eq!(body.0["messages_summarized"], json!(200));
        assert_eq!(body.0["from_seq"], json!(1041));
        assert_eq!(body.0["to_seq"], json!(1240));
        assert_eq!(body.0["artifact"]["version"], json!(4));

        for (outcome, code) in [
            (SummarizeOutcome::NoMessages, "no_messages"),
            (SummarizeOutcome::EmptySummary, "empty_summary"),
        ] {
            let (status, body) = summarize_response(outcome);
            assert_eq!(status, StatusCode::OK, "{code} is an answer, not a fault");
            assert_eq!(body.0["ok"], json!(true));
            assert_eq!(body.0["summarized"], json!(false));
            assert_eq!(body.0["code"], json!(code));
            assert!(body.0.get("artifact").is_none());
        }

        let (status, body) = summarize_response(SummarizeOutcome::Unchanged {
            artifact: summary_artifact(),
        });
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["code"], json!("unchanged"));
        assert_eq!(body.0["artifact"]["version"], json!(4));

        let (status, body) = summarize_response(SummarizeOutcome::ForgedAuthor);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.0["code"], json!("forged_artifact_author"));

        let (status, body) = summarize_response(SummarizeOutcome::ProviderError);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body.0["code"], json!("summary_provider_error"));
        assert_eq!(body.0["error"], json!("the summary model call failed"));

        let (status, body) = summarize_response(SummarizeOutcome::Timeout);
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body.0["code"], json!("summary_timeout"));

        // Store errors keep the mapping the rest of the room routes already use.
        let (status, _) = summarize_response(SummarizeOutcome::Store(
            ocean_store::RoomStoreError::UnknownRoom(RoomKey::new("gone")),
        ));
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = summarize_response(SummarizeOutcome::Store(
            ocean_store::RoomStoreError::ArtifactAuthorNotInRoster {
                room: RoomKey::new("room"),
                author: "mallory".into(),
            },
        ));
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

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

    async fn route_room_read_cursor_unauthorized() -> axum::response::Response {
        StatusCode::UNAUTHORIZED.into_response()
    }

    async fn start_route_read_cursor_unauthorized_bedrock() -> (String, tokio::task::JoinHandle<()>)
    {
        let app = axum::Router::new()
            .route(
                "/api/v1/rooms/{room}/read-cursor",
                axum::routing::get(route_room_read_cursor_unauthorized),
            )
            .with_state(RouteBedrock::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), server)
    }

    async fn route_room_read_cursor_ok(Path(room): Path<String>) -> axum::response::Response {
        Json(json!({"room_id": room, "sequence": "77", "clamped": false})).into_response()
    }

    /// M6 regression fixture: an upstream that truthfully answers GET
    /// `.../read-cursor` so the Live-room HTTP handler round trip can be
    /// exercised end to end and checked against the SAME response schema
    /// the Local-room path produces.
    async fn start_route_read_cursor_ok_bedrock() -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .route(
                "/api/v1/rooms/{room}/read-cursor",
                axum::routing::get(route_room_read_cursor_ok),
            )
            .with_state(RouteBedrock::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), server)
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
            state.room_read_cursor_wakes.clone(),
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
            role_in_room: ocean_core::FederatedRoomRole::Owner,
            display_name: "Reclassified Human".into(),
            public_agent_descriptor: None,
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(ocean_core::MemberPresence::Unavailable),
            local_binding_available: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rooms_list_persistent_includes_ordered_read_states_for_local_and_live() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let local = RoomKey::new("list-read-local");
        let live = RoomKey::new("list-read-live");
        with_rooms(&state, |store| {
            store.create(local.clone(), "Local", None, Utc::now())?;
            store.append_message(
                &local,
                "u1",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "local first",
                Utc::now(),
            )?;
            store.update_room_read_cursor(
                &local,
                local_room_read_cursor_principal(),
                RoomReadCursorUpdateRequest { read_seq: 0 },
            )?;

            store.create(live.clone(), "Live", None, Utc::now())?;
            store.update_room_access_safe(
                &live,
                Some(RoomAccessState::Live),
                None,
                Some(u64::MAX),
            )?;
            store.install_room_credential(&live, "bearer-secret", "live-principal")?;
            store.set_room_read_cursor_mirror(
                &live,
                "live-principal",
                None,
                Some((1u64 << 53) + 7),
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) = rooms_list_persistent(
            State(state.clone()),
            Query(RoomsListQuery {
                limit: Some(10),
                cursor: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], json!(true));
        let rooms = body.0["rooms"].as_array().unwrap();
        let read_states = body.0["read_states"].as_array().unwrap();
        assert_eq!(rooms.len(), 2);
        assert_eq!(read_states.len(), 2);
        for (room, read_state) in rooms.iter().zip(read_states.iter()) {
            assert_eq!(read_state["room_id"], room["id"]);
        }
        assert_eq!(
            read_states[0],
            json!({
                "room_id": live.as_str(),
                "latest_seq": u64::MAX.to_string(),
                "read_seq": ((1u64 << 53) + 7).to_string()
            })
        );
        assert_eq!(
            read_states[1],
            json!({
                "room_id": local.as_str(),
                "latest_seq": "0",
                "read_seq": "0"
            })
        );
        let encoded = serde_json::to_string(&body.0).unwrap();
        assert!(encoded.contains(&format!("\"latest_seq\":\"{}\"", u64::MAX)));
        assert!(encoded.contains("\"read_seq\":\"9007199254740999\""));
        assert!(!encoded.contains("bearer-secret"));
        assert!(!encoded.contains("live-principal"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rooms_list_persistent_uses_durable_metadata_for_non_live_federated_states() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("list-read-connecting");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Connecting", None, Utc::now())?;
            store.update_room_access_safe(
                &key,
                Some(RoomAccessState::Connecting),
                None,
                Some(42),
            )?;
            store.install_room_credential(&key, "bearer-secret", "connecting-principal")?;
            store.set_room_read_cursor_mirror(&key, "connecting-principal", None, Some(7))?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) = rooms_list_persistent(
            State(state),
            Query(RoomsListQuery {
                limit: Some(10),
                cursor: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0["read_states"],
            json!([{
                "room_id": key.as_str(),
                "latest_seq": "42",
                "read_seq": "7"
            }])
        );
        let encoded = serde_json::to_string(&body.0).unwrap();
        assert!(!encoded.contains("bearer-secret"));
        assert!(!encoded.contains("connecting-principal"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_handlers_truthfully_distinguish_absent_zero_and_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-handler-local");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Cursor Handler Local", None, Utc::now())?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) =
            room_get_read_cursor(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0,
            json!({"ok": true, "cursor": {"room_id": "cursor-handler-local", "read_seq": null}})
        );

        with_rooms(&state, |store| {
            store.append_message(
                &key,
                "u1",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "first",
                Utc::now(),
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) = room_patch_read_cursor(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"read_seq":0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0,
            json!({"ok": true, "cursor": {"room_id": "cursor-handler-local", "read_seq": "0"}})
        );

        let (status, body) =
            room_get_read_cursor(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0,
            json!({"ok": true, "cursor": {"room_id": "cursor-handler-local", "read_seq": "0"}})
        );

        let (status, body) = room_patch_read_cursor(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"read_seq":0,"extra":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["ok"], json!(false));

        let federated = RoomKey::new("cursor-handler-live");
        with_rooms(&state, |store| {
            store.create(federated.clone(), "Cursor Handler Live", None, Utc::now())?;
            store.update_room_access_safe(&federated, Some(RoomAccessState::Live), None, None)?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) =
            room_get_read_cursor(State(state.clone()), Path(federated.as_str().to_string())).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0["code"], json!("room_read_cursor_unsupported"));

        let (status, body) = room_patch_read_cursor(
            State(state),
            Path(federated.as_str().to_string()),
            Bytes::from_static(br#"{"read_seq":0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0["code"], json!("room_read_cursor_unsupported"));
    }

    /// The PATCH room contract in one pass: a present `trigger_policy`
    /// replaces the stored one, an absent field leaves it untouched, and an
    /// explicit `null` clears it — the three wire shapes the double-Option
    /// deserializer exists to keep apart.
    #[tokio::test]
    async fn room_update_distinguishes_absent_null_and_present_trigger_policy() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("policy-update-room");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Policy Update", None, Utc::now())
        })
        .unwrap();

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_build_failure":true,"on_ci_failure":true}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], json!(true));
        assert_eq!(body.0["room"]["trigger_policy"]["on_mention"], json!(true));
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_build_failure"],
            json!(true)
        );
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_ci_failure"],
            json!(true)
        );

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"name":"Renamed"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["room"]["name"], json!("Renamed"));
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_build_failure"],
            json!(true)
        );
        // Read back through the store's hand-rolled policy codec: a flag that
        // codec drops would read false here while the write response lied.
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_ci_failure"],
            json!(true)
        );

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"trigger_policy":null}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["room"]["trigger_policy"], serde_json::Value::Null);
        let stored = with_rooms(&state, |store| store.get(&key))
            .unwrap()
            .expect("room exists");
        assert!(stored.room.trigger_policy.is_none());
        assert_eq!(stored.room.name, "Renamed");
    }

    /// The failure edges: an unknown room 404s through the store's typed
    /// mapping, and a typo'd field or malformed JSON is the same typed 400
    /// the read-cursor PATCH answers — never a silent "nothing changed".
    #[tokio::test]
    async fn room_update_unknown_room_and_bad_bodies_are_typed_errors() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);

        let (status, body) = room_update(
            State(state.clone()),
            Path("missing-room".to_string()),
            Bytes::from_static(br#"{"name":"New Name"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.0["ok"], json!(false));

        let (status, body) = room_update(
            State(state.clone()),
            Path("missing-room".to_string()),
            Bytes::from_static(br#"{"trigger_polcy":{}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0, json!({"ok": false, "error": "invalid_request"}));

        let (status, body) = room_update(
            State(state),
            Path("missing-room".to_string()),
            Bytes::from_static(b"not json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0, json!({"ok": false, "error": "invalid_request"}));
    }

    /// The two triggers nothing fires — a cron in `on_schedule`, a `true`
    /// `on_component_event` — are refused at create with a typed 400 naming
    /// the field, instead of stored as configuration that silently never
    /// acts. Refusal is by VALUE: clients serialize every bool into the
    /// policy body, so explicit-`false` dead fields must keep passing.
    #[tokio::test]
    async fn room_create_refuses_unwired_trigger_values() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);

        let req = |policy: serde_json::Value| -> RoomCreateRequest {
            serde_json::from_value(json!({
                "key": "unwired-create",
                "name": "Unwired",
                "trigger_policy": policy,
            }))
            .expect("request deserializes")
        };

        let (status, body) = room_create(
            State(state.clone()),
            Json(req(
                json!({"on_mention": true, "on_schedule": "*/5 * * * *"}),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["ok"], json!(false));
        assert_eq!(body.0["code"], json!("trigger_unwired"));
        assert_eq!(body.0["field"], json!("on_schedule"));

        let (status, body) = room_create(
            State(state.clone()),
            Json(req(json!({"on_component_event": true}))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["code"], json!("trigger_unwired"));
        assert_eq!(body.0["field"], json!("on_component_event"));

        // Neither refusal wrote anything.
        let stored =
            with_rooms(&state, |store| store.get(&RoomKey::new("unwired-create"))).unwrap();
        assert!(stored.is_none());

        // The wire shape every client sends: live triggers on, dead bools
        // explicitly false.
        let (status, body) = room_create(
            State(state.clone()),
            Json(req(json!({
                "on_mention": true,
                "on_thread_reply": true,
                "on_component_event": false,
                "on_build_failure": true,
                "on_ci_failure": true,
            }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body.0["room"]["trigger_policy"]["on_mention"], json!(true));
        // Wired, so it is stored rather than refused — the create route must
        // not grow a refusal for a flag the daemon actually fires.
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_ci_failure"],
            json!(true)
        );
    }

    #[tokio::test]
    async fn room_create_requires_a_canonicalizable_absolute_workspace() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let request = |key: &str, workspace_root: String| RoomCreateRequest {
            key: key.to_string(),
            name: "Workspace Boundary".into(),
            trigger_policy: None,
            workspace_root: Some(workspace_root),
        };

        for (key, workspace_root) in [
            ("relative-workspace", ".".to_string()),
            (
                "missing-workspace",
                tmp.path().join("missing").to_string_lossy().into_owned(),
            ),
        ] {
            let (status, body) =
                room_create(State(state.clone()), Json(request(key, workspace_root))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                body.0,
                json!({"ok": false, "error": "invalid_workspace_root"})
            );
            assert!(with_rooms(&state, |store| store.get(&RoomKey::new(key)))
                .unwrap()
                .is_none());
        }

        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let noncanonical = workspace.join("..").join("workspace");
        let expected = std::fs::canonicalize(&workspace)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (status, body) = room_create(
            State(state.clone()),
            Json(request(
                "canonical-workspace",
                noncanonical.to_string_lossy().into_owned(),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body.0["room"]["workspace_root"], expected);

        assert!(persisted_room_workspace(".").is_none());
        assert!(persisted_room_workspace(noncanonical.to_str().unwrap()).is_none());
        assert_eq!(
            persisted_room_workspace(&expected).as_deref(),
            Some(expected.as_str())
        );

        #[cfg(unix)]
        {
            let moved = tmp.path().join("workspace-moved");
            std::fs::rename(&workspace, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &workspace).unwrap();
            assert!(
                persisted_room_workspace(&expected).is_none(),
                "a symlink replacement must not silently retarget the Room"
            );
        }
    }

    /// The same refusal on PATCH: the update route must not be the back door
    /// that stores an unwired trigger. A refused PATCH leaves the stored
    /// policy untouched, and the normal wire shape (dead bools false) passes.
    #[tokio::test]
    async fn room_update_refuses_unwired_trigger_values() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("unwired-update");
        let created_policy = RoomTriggerPolicy {
            on_mention: true,
            ..Default::default()
        };
        with_rooms(&state, |store| {
            store.create(
                key.clone(),
                "Unwired Update",
                Some(created_policy.clone()),
                Utc::now(),
            )
        })
        .unwrap();

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"trigger_policy":{"on_schedule":"0 * * * *"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["code"], json!("trigger_unwired"));
        assert_eq!(body.0["field"], json!("on_schedule"));

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(br#"{"trigger_policy":{"on_component_event":true}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["field"], json!("on_component_event"));

        // Both refusals wrote nothing: the policy is still what create stored.
        let stored = with_rooms(&state, |store| store.get(&key))
            .unwrap()
            .expect("room exists");
        assert_eq!(stored.room.trigger_policy, Some(created_policy));

        let (status, body) = room_update(
            State(state),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_thread_reply":true,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_thread_reply"],
            json!(true)
        );
    }

    /// The transition rule in one table, with neither store nor route in the
    /// way: only a false-or-absent → true flip in a room that has left `Local`
    /// is refused.
    #[test]
    fn dead_thread_reply_transition_refuses_only_the_flip_out_of_a_local_room() {
        let on = RoomTriggerPolicy {
            on_thread_reply: true,
            ..Default::default()
        };
        let off = RoomTriggerPolicy::default();

        // The flip, in every room state that no longer fires the trigger. An
        // absent stored policy reads the same as a stored `false`.
        for access in [
            RoomAccessState::Connecting,
            RoomAccessState::Live,
            RoomAccessState::Recovering,
            RoomAccessState::Revoked,
        ] {
            assert!(dead_thread_reply_transition(Some(&off), &on, access));
            assert!(dead_thread_reply_transition(None, &on, access));
        }

        // Not a flip: the room already stores the value, so every later write
        // that carries it back through is accepted.
        assert!(!dead_thread_reply_transition(
            Some(&on),
            &on,
            RoomAccessState::Live
        ));
        // Switching it OFF is allowed everywhere — it is the only way a stored
        // dead value ever goes away.
        assert!(!dead_thread_reply_transition(
            Some(&on),
            &off,
            RoomAccessState::Live
        ));
        // A Local room fires the trigger, so nothing is refused there.
        assert!(!dead_thread_reply_transition(
            Some(&off),
            &on,
            RoomAccessState::Local
        ));
        assert!(!dead_thread_reply_transition(
            None,
            &on,
            RoomAccessState::Local
        ));
    }

    /// A room that federates after creation stops firing thread-reply: the
    /// daemon builds that event only on the local post path, from the thread
    /// root's author, and a federated message carries no thread parent. So
    /// switching the flag ON there is refused with the same typed body every
    /// other unwired trigger gets, and nothing is written.
    #[tokio::test]
    async fn room_update_refuses_enabling_thread_reply_once_the_room_federates() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("thread-reply-federated");
        let created_policy = RoomTriggerPolicy {
            on_mention: true,
            ..Default::default()
        };
        with_rooms(&state, |store| {
            store.create(
                key.clone(),
                "Thread Reply Federated",
                Some(created_policy.clone()),
                Utc::now(),
            )?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "test-bearer", "member-a")?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_thread_reply":true,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["ok"], json!(false));
        assert_eq!(body.0["code"], json!("trigger_unwired"));
        assert_eq!(body.0["field"], json!("on_thread_reply"));

        // The refusal wrote nothing: the policy is still what create stored.
        let stored = with_rooms(&state, |store| store.get(&key))
            .unwrap()
            .expect("room exists");
        assert_eq!(stored.room.trigger_policy, Some(created_policy));
    }

    /// The refusal answers only for rooms that can be WRITTEN. `trigger_policy`
    /// and `room_access` read any room the store still holds, soft-closed
    /// included, while `update` writes only an open one — so the check had to be
    /// gated or it would have answered for a room no PATCH can reach, changing a
    /// documented 404 into a typed 400 that also discloses the closed room's
    /// federation state. Same room as the refusal test above, closed first.
    #[tokio::test]
    async fn room_update_404s_a_closed_room_rather_than_refusing_its_dead_trigger() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("thread-reply-closed");
        with_rooms(&state, |store| {
            store.create(
                key.clone(),
                "Thread Reply Closed",
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                Utc::now(),
            )?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "test-bearer", "member-a")?;
            store.close(&key)?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        // Exactly the body the open federated room is refused for.
        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_thread_reply":true,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.0["ok"], json!(false));
        // Not `trigger_unwired`: a closed room is answered as absent, and is
        // told nothing about whether it had federated.
        assert_eq!(body.0["code"], json!(serde_json::Value::Null));
    }

    /// The property the refusal turns on, and the reason it is a TRANSITION
    /// rule and not a value rule. ocean-surface builds a policy PATCH by
    /// cloning the room's STORED policy and flipping one field, so a federated
    /// room already holding `on_thread_reply: true` re-sends `true` on every
    /// unrelated toggle. Refusing the value would 400 all of them and brick the
    /// trigger panel for exactly the rooms the rule protects. Clearing the flag
    /// has to keep working too: that is the write the surface will send once
    /// its dead row can be unchecked, and the daemon must not be what blocks
    /// it. Once cleared, switching it back on is a real flip and is refused.
    #[tokio::test]
    async fn room_update_accepts_a_federated_room_resending_a_thread_reply_it_already_stores() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("thread-reply-already-stored");
        with_rooms(&state, |store| {
            store.create(
                key.clone(),
                "Thread Reply Already Stored",
                Some(RoomTriggerPolicy {
                    on_thread_reply: true,
                    ..Default::default()
                }),
                Utc::now(),
            )?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "test-bearer", "member-a")?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        // Exactly what the surface sends to turn `on_mention` on: the stored
        // policy, one field flipped, the dead flag carried through untouched.
        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_thread_reply":true,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["room"]["trigger_policy"]["on_mention"], json!(true));
        // Still stored, still dead. The daemon does not quietly normalize it.
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_thread_reply"],
            json!(true)
        );

        // The clearing write.
        let (status, body) = room_update(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_thread_reply":false,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0["room"]["trigger_policy"]["on_thread_reply"],
            json!(false)
        );

        // And now it is a flip again, so the room cannot re-acquire the dead
        // value it just gave up.
        let (status, body) = room_update(
            State(state),
            Path(key.as_str().to_string()),
            Bytes::from_static(
                br#"{"trigger_policy":{"on_mention":true,"on_thread_reply":true,"on_component_event":false}}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["field"], json!("on_thread_reply"));
    }

    /// M6 + H1 regression: the Live-room `GET .../read-cursor` response uses
    /// the SAME `{room_id, read_seq}` schema as the Local-room response
    /// (previously it returned `{room_id, sequence}`, a different shape),
    /// and the value it reports is read back from the SAME store principal
    /// the SSE read-cursor tail resolves for that room — the per-credential
    /// `local_human_member_id`, not a fixed placeholder string that nothing
    /// ever writes to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_live_get_matches_local_schema_and_sse_principal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-handler-live-ok");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Cursor Handler Live Ok", None, Utc::now())?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "bearer", "member-live-ok")?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (base, server) = start_route_read_cursor_ok_bedrock().await;
        state = with_route_supervisor(state, &base);

        let (status, body) =
            room_get_read_cursor(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.0,
            json!({"ok": true, "cursor": {"room_id": key.as_str(), "read_seq": "77"}})
        );

        // The federation client durably mirrors what the upstream reported
        // (77) keyed by the credential's `local_human_member_id`.
        let cursor = with_rooms(&state, |store| {
            store.room_read_cursor(&key, "member-live-ok")
        })
        .unwrap();
        assert_eq!(cursor.mirrored_upstream_read_seq, Some(77));

        // H1: both the initial SSE snapshot logic and the read-cursor tail
        // must resolve that exact same principal, or they would silently
        // observe an always-empty cursor for this (and every) Live room.
        let resolved = with_rooms(&state, |store| {
            let access = store.room_access(&key)?;
            assert_eq!(access.state, RoomAccessState::Live);
            match live_room_read_cursor_principal(store, &key)? {
                Some(principal) => store.room_read_cursor(&key, &principal),
                None => panic!("expected a room credential principal for a Live room"),
            }
        })
        .unwrap();
        assert_eq!(resolved.mirrored_upstream_read_seq, Some(77));

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hints = state.room_read_cursor_wakes.subscribe();
        let access_hints = state.room_access_wakes.test_subscribe();
        tokio::spawn(run_room_read_cursor_tail(
            state.clone(),
            key.clone(),
            Some(RoomReadCursorBody {
                room_id: key.as_str().to_string(),
                read_seq: None,
            }),
            hints,
            access_hints,
            tx,
        ));
        publish_room_read_cursor_wake(&state, &key);
        let tailed = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tailed.room_id, key.as_str());
        assert_eq!(tailed.read_seq, Some("77".to_string()));

        server.abort();
        state.room_federation.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_live_get_truthfully_reports_revoked_without_mutating_mirror() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-handler-live-revoked");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Cursor Handler Live Revoked", None, Utc::now())?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "bearer", "principal")?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (base, server) = start_route_read_cursor_unauthorized_bedrock().await;
        state = with_route_supervisor(state, &base);

        let (status, body) =
            room_get_read_cursor(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.0, json!({"ok": false, "error": "membership_revoked"}));

        let cursor = with_rooms(&state, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(cursor.read_seq, None);
        assert_eq!(cursor.mirrored_upstream_read_seq, None);

        server.abort();
        state.room_federation.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_tail_emits_absent_then_zero_on_wake() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-tail-local");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Cursor Tail Local", None, Utc::now())?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hints = state.room_read_cursor_wakes.subscribe();
        let access_hints = state.room_access_wakes.test_subscribe();
        tokio::spawn(run_room_read_cursor_tail(
            state.clone(),
            key.clone(),
            Some(RoomReadCursorBody {
                room_id: key.as_str().to_string(),
                read_seq: None,
            }),
            hints,
            access_hints,
            tx,
        ));

        with_rooms(&state, |store| {
            store.append_message(
                &key,
                "u1",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "first",
                Utc::now(),
            )?;
            store.update_room_read_cursor(
                &key,
                local_room_read_cursor_principal(),
                RoomReadCursorUpdateRequest { read_seq: 0 },
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        publish_room_read_cursor_wake(&state, &key);

        let cursor = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor.room_id, key.as_str());
        assert_eq!(cursor.read_seq, Some("0".to_string()));
    }

    /// F1 regression: while the federated link is not confirmed Live
    /// (Connecting/Recovering/Revoked), the read-cursor tail must skip
    /// emissions entirely rather than falling back to
    /// `local_room_read_cursor_principal()` — that principal is never
    /// written to for a federated room, so reading it would flicker the
    /// client from the last real federated cursor value to a fabricated
    /// cleared/local one on every transient hop. The federated credential
    /// principal must also still resolve correctly once the room returns to
    /// Live, proving it was never disturbed by the skip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_tail_retains_federated_principal_and_skips_unsupported_transitions() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-tail-transition");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Cursor Tail Transition", None, Utc::now())?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, Some(1))?;
            store.install_room_credential(&key, "bearer-secret", "federated-principal")?;
            store.set_room_read_cursor_mirror(&key, "federated-principal", None, Some(5))?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hints = state.room_read_cursor_wakes.subscribe();
        let access_hints = state.room_access_wakes.test_subscribe();
        tokio::spawn(run_room_read_cursor_tail(
            state.clone(),
            key.clone(),
            None,
            hints,
            access_hints,
            tx,
        ));

        publish_room_read_cursor_wake(&state, &key);
        let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.read_seq, Some("5".to_string()));

        // Connecting: read-cursor unsupported. Must skip, not flicker.
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
        })
        .unwrap();
        publish_room_read_cursor_wake(&state, &key);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "expected no room_read_cursor emission while access is Connecting"
        );

        // Revoked: same — still skip, credential row untouched.
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Revoked), None, None)
        })
        .unwrap();
        publish_room_read_cursor_wake(&state, &key);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "expected no room_read_cursor emission while access is Revoked"
        );

        // Back to Live: the federated credential principal must still
        // resolve correctly (never cleared to the local principal), so the
        // mirrored value is read again without any manual re-installation.
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, Some(2))?;
            store.set_room_read_cursor_mirror(&key, "federated-principal", Some(5), Some(9))
        })
        .unwrap();
        publish_room_read_cursor_wake(&state, &key);
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.read_seq, Some("9".to_string()));
    }

    /// PR #366 review comment 3727657446 regression: `/events` opened while a
    /// federated room is `Connecting` must not drop the cursor tail. The
    /// handler used to gate spawning `run_room_read_cursor_tail` on
    /// `read_cursor_supported`, so a connection opened mid-Connecting never
    /// started a tail at all and would never observe a `room_read_cursor`
    /// frame later, even after access became `Live`, without the client
    /// reconnecting. Proves over the real HTTP SSE handler, on the same
    /// still-open connection:
    /// - the initial snapshot is `room_access: Connecting` with no
    ///   `room_read_cursor` bootstrap frame (projection undefined);
    /// - no `room_read_cursor` frame arrives while still Connecting;
    /// - once access flips to Live — mirroring
    ///   `room_federation::commit_access`, which publishes only an access
    ///   wake, not a read-cursor wake — the same connection still emits the
    ///   current cursor, proving the tail stayed subscribed through the
    ///   transition and reacted to the access wake alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_events_http_cursor_tail_survives_connecting_and_emits_on_live_without_reconnect()
    {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-transition-live");
        create_plain_room(&state, &key);
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)?;
            store.install_room_credential(&key, "bearer-secret", "transition-principal")?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let app = super::super::room_routes().with_state(state.clone());
        let request = axum::http::Request::builder()
            .uri(format!("/v1/rooms/persistent/{key}/events"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();

        // Only one frame up front: the initial room_access snapshot
        // (Connecting). No room_read_cursor bootstrap frame — the projection
        // is undefined while access is unsupported.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), body.frame())
            .await
            .expect("frame timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let text = String::from_utf8_lossy(&frame.into_data().unwrap_or_default()).to_string();
        assert!(
            text.contains("event: room_access\n"),
            "expected initial room_access frame, got: {text:?}"
        );
        assert!(
            text.contains("\"state\":\"connecting\""),
            "expected Connecting access snapshot, got: {text:?}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), body.frame())
                .await
                .is_err(),
            "expected no room_read_cursor frame while access is Connecting"
        );

        // Same still-open connection observes a reconnect completion: access
        // flips straight to Live and the federated mirror already carries a
        // durable cursor value from before the reconnect (no fresh upstream
        // `room_read_cursor` federation frame arrives).
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.set_room_read_cursor_mirror(&key, "transition-principal", None, Some(42))?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        publish_room_access_wake(&state, &key);

        let mut saw_access_live = false;
        let mut saw_cursor = false;
        for _ in 0..4 {
            if saw_access_live && saw_cursor {
                break;
            }
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
                .await
                .expect("frame timeout")
                .expect("SSE body ended")
                .expect("SSE body error");
            let text = String::from_utf8_lossy(&frame.into_data().unwrap_or_default()).to_string();
            if text.contains("event: room_access\n") && text.contains("\"state\":\"live\"") {
                saw_access_live = true;
            }
            if text.contains("event: room_read_cursor\n") {
                saw_cursor = true;
                let data = text
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("data line");
                let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
                assert_eq!(parsed, json!({ "room_id": key.as_str(), "read_seq": "42" }));
            }
        }
        assert!(saw_access_live, "expected a Live room_access frame");
        assert!(
            saw_cursor,
            "expected the still-open connection to emit the current room_read_cursor without reconnecting"
        );
    }

    /// F3 regression: the SSE `room_read_cursor` wire uses the same
    /// JS-number-precision-safe decimal-string schema as REST
    /// (`RoomReadCursorBody`) — a `read_seq` above 2^53 must serialize as a
    /// quoted string, never a bare JS-unsafe number, for both the initial
    /// bootstrap frame and subsequent tail emissions.
    ///
    /// Uses a Live room's federated mirror (`set_room_read_cursor_mirror`)
    /// rather than the Local `room_read_cursors` path: `update_room_read_cursor`
    /// clamps the requested value to the room's message high-water seq, so a
    /// fixture with no messages could never durably persist a raw >2^53
    /// value there. The federated mirror has no such clamp (it stores
    /// whatever the upstream reports), matching how
    /// `rooms_list_persistent_includes_ordered_read_states_for_local_and_live`
    /// already proves this exact value round-trips through the REST list
    /// endpoint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_read_cursor_sse_wire_uses_js_safe_decimal_strings_above_2_53() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("cursor-precision-live");
        create_plain_room(&state, &key);
        let huge = (1u64 << 53) + 11;
        with_rooms(&state, |store| {
            store.update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)?;
            store.install_room_credential(&key, "bearer-secret", "huge-principal")?;
            store.set_room_read_cursor_mirror(&key, "huge-principal", None, Some(huge))?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let app = super::super::room_routes().with_state(state);
        let request = axum::http::Request::builder()
            .uri(format!("/v1/rooms/persistent/{key}/events"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();

        // Only two frames expected up front (no messages, no access churn):
        // the S2-P1 initial room_access frame and the read-cursor bootstrap.
        // Order between them is not contractually fixed (they're merged
        // concurrently), so read both and match by event name.
        let frame_a = tokio::time::timeout(std::time::Duration::from_millis(500), body.frame())
            .await
            .expect("frame a timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let text_a = String::from_utf8_lossy(&frame_a.into_data().unwrap_or_default()).to_string();
        let frame_b = tokio::time::timeout(std::time::Duration::from_millis(500), body.frame())
            .await
            .expect("frame b timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let text_b = String::from_utf8_lossy(&frame_b.into_data().unwrap_or_default()).to_string();

        let cursor_wire = if text_a.contains("event: room_read_cursor\n") {
            text_a
        } else {
            assert!(
                text_b.contains("event: room_read_cursor\n"),
                "expected a room_read_cursor bootstrap frame, got: {text_a:?} / {text_b:?}"
            );
            text_b
        };
        let data = cursor_wire
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data line");
        // Wire-level proof: the huge value must appear as a quoted decimal
        // string, never a bare JSON number (which would silently lose
        // precision in JS's IEEE-754 f64 doubles above 2^53).
        assert!(
            data.contains(&format!("\"read_seq\":\"{huge}\"")),
            "expected JS-safe quoted decimal string in: {data:?}"
        );
        assert!(
            !data.contains(&format!("\"read_seq\":{huge}")),
            "read_seq must never be a bare unsafe number: {data:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            parsed,
            json!({ "room_id": key.as_str(), "read_seq": huge.to_string() })
        );
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
            store.create_in_workspace(
                key.clone(),
                "G3 Thread Dispatch",
                Some(canonical_test_workspace(tmp.path())),
                Some(RoomTriggerPolicy {
                    on_mention: true,
                    on_thread_reply: true,
                    ..Default::default()
                }),
                Utc::now(),
            )?;
            store.add_participant(
                &key,
                RoomParticipant {
                    id: "john".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "John".into(),
                },
                Utc::now(),
            )?;
            store.add_agent_participant_with_owner(
                &key,
                RoomParticipant {
                    id: "helper".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Helper".into(),
                },
                "john",
                Utc::now(),
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        let generation = authorize_room_agent_fixture(
            &state,
            &key,
            "helper",
            ActivationPolicy::TaskAndThread,
            ContextPolicy::InvocationOnly,
        );
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

        let expected_session =
            authorized_room_agent_session_id(&key, "helper", generation).to_string();
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
        let legacy_session = room_agent_session_id(&key, "helper").to_string();
        assert_eq!(
            fallback.session_id.as_deref(),
            Some(legacy_session.as_str())
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
    fn authorized_room_session_is_generation_bound_and_collision_safe() {
        let room = RoomKey::new("ab");
        let first = authorized_room_agent_session_id(&room, "c", 1);
        assert_eq!(first, authorized_room_agent_session_id(&room, "c", 1));
        assert_ne!(first, authorized_room_agent_session_id(&room, "c", 2));
        assert_ne!(
            first,
            authorized_room_agent_session_id(&RoomKey::new("a"), "bc", 1),
            "length-prefixing must distinguish concatenation-equivalent pairs"
        );
        assert_ne!(
            first,
            room_agent_session_id(&room, "c"),
            "Phase 1 must never resume a legacy room session"
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

    #[tokio::test]
    async fn member_remove_rejects_blank_ids_and_maps_intent_errors() {
        let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
        let state = fake_convene_state(&tmp);

        let (status, Json(body)) =
            room_remove_member(State(state.clone()), Path(("room".into(), "   ".into()))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");

        // Through the mounted route, so a router mis-mount cannot hide
        // behind the handler's own 404: the body must be the typed error.
        let response = crate::room_routes()
            .with_state(state.clone())
            .oneshot(
                axum::http::Request::delete(
                    "/v1/rooms/persistent/missing-room/members/some-member",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], "room_not_found");

        let key = RoomKey::new("member-remove-room");
        with_rooms(&state, |store| {
            store.create(key.clone(), "Member Remove", None, Utc::now())
        })
        .unwrap();
        let (status, Json(body)) = room_remove_member(
            State(state.clone()),
            Path((key.as_str().into(), "member-1".into())),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "federation_conflict");

        with_rooms(&state, |store| {
            store.install_room_credential(&key, "bearer", "11111111-1111-4111-8111-111111111111")
        })
        .unwrap();
        let (status, Json(body)) =
            room_remove_member(State(state), Path((key.as_str().into(), "member-1".into()))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "federation_unavailable");
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
            store.create_in_workspace(
                key.clone(),
                "Dispatch",
                Some(canonical_test_workspace(tmp.path())),
                None,
                Utc::now(),
            )?;
            store.install_room_credential(&key, "private-bearer", human)?;
            store.bind_room_agent(&key, member, "bound-agent", "registration-key")?;
            store.update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    dispatch_agent_projection(member, human, "bound-agent"),
                    dispatch_human_projection(human),
                ]),
                None,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();
        authorize_room_agent_package_fixture(
            &state,
            &key,
            member,
            "bound-agent",
            ActivationPolicy::Mention,
            ContextPolicy::InvocationOnly,
        );
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
            trigger_kind: FederatedTriggerKind::Mention,
            reason: format!("on_mention: @{member} mentioned"),
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
            .iter()
            .all(|row| row.kind == RoomMessageKind::System));

        // Main follows this order: federation producers stop, the dedicated
        // dispatcher cancellation fires, and the retained JoinHandle is joined.
        drop(tx);
        cancel.cancel();
        dispatcher.await.unwrap();
    }

    #[tokio::test]
    async fn p2c_unresolved_and_stale_dispatches_emit_no_turn_and_only_denial_audits() {
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
                trigger_kind: FederatedTriggerKind::Mention,
                reason: format!("on_mention: @{member} mentioned"),
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
            let transcript = with_rooms(&state, |store| store.transcript(key, None)).unwrap();
            assert!(transcript.iter().all(|row| {
                row.kind == RoomMessageKind::System && row.body.contains("\"outcome\":\"refused\"")
            }));
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
        let workspace = canonical_test_workspace(
            state
                .room_attachments_root
                .parent()
                .expect("test attachment root has a tempdir parent"),
        );
        with_rooms(state, |store| {
            store
                .create_in_workspace(
                    key.clone(),
                    "Named Agent Seam",
                    Some(workspace),
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
                owner_id: None,
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
                owner_id: Some("human".into()),
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        let _generation = authorize_room_agent_fixture(
            &state,
            &convene,
            "helper",
            ActivationPolicy::Mention,
            ContextPolicy::InvocationOnly,
        );
        let authority_seq = with_rooms(&state, |store| store.transcript(&convene, None))
            .unwrap()
            .last()
            .expect("authority audit")
            .seq;
        let (mut convene_tail, release) = paused_tail(&state, &convene, Some(authority_seq)).await;
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
        let admission = next_message(&mut convene_tail).await;
        assert_eq!(admission.kind, RoomMessageKind::System);
        assert!(admission.body.contains("admission_id") && admission.body.contains("admitted"));
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
        // Optional Local-room read cursor bootstrap may arrive before transcript replay.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("next frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let mut wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame"))
            .unwrap()
            .to_string();
        if wire.contains("event: room_read_cursor\n") {
            let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
                .await
                .expect("message frame exceeded 250ms")
                .expect("SSE body ended")
                .expect("SSE body error");
            wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame"))
                .unwrap()
                .to_string();
        }
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
        // Optional Local-room read cursor bootstrap may arrive before replay.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
            .await
            .expect("resume frame exceeded 250ms")
            .expect("SSE body ended")
            .expect("SSE body error");
        let mut wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame"))
            .unwrap()
            .to_string();
        if wire.contains("event: room_read_cursor\n") {
            let frame = tokio::time::timeout(std::time::Duration::from_millis(250), body.frame())
                .await
                .expect("resume message frame exceeded 250ms")
                .expect("SSE body ended")
                .expect("SSE body error");
            wire = std::str::from_utf8(frame.data_ref().expect("SSE data frame"))
                .unwrap()
                .to_string();
        }
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

    /// END TO END: a worker adds their agent over HTTP, and the room reports
    /// that the agent is THEIRS. This is the whole point of the feature — the
    /// store gates prove the write, this proves it is reachable and projected.
    /// Mutation: make `room_join` ignore `owner_id` (always take the
    /// non-owner store path) -> agent_owners comes back empty -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_records_agent_ownership_and_room_get_projects_it() {
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
        write_agent_fixture(&agents_root, "researcher", "model = \"fake-ok\"\n", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let key = RoomKey::new("owned-room");
        create_mention_room(&state, &key);

        // The worker joins first — an agent cannot be owned by someone absent.
        let (status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "alice".into(),
                display_name: "Alice".into(),
                kind: RoomParticipantKind::Human,
                owner_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Then adds THEIR agent.
        let (status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "researcher".into(),
                display_name: "Researcher".into(),
                kind: RoomParticipantKind::Agent,
                owner_id: Some("alice".into()),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, Json(body)) =
            room_get(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["agent_owners"],
            json!([{ "agent_id": "researcher", "owner_id": "alice", "owner_present": true }]),
            "the room must report whose agent this is"
        );
    }

    /// An owner named for a participant who is not on the roster is refused,
    /// and the refusal writes nothing — no roster row, no join marker.
    /// Mutation: delete the store's `None =>` owner arm -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_with_an_absent_owner_is_refused_and_writes_nothing() {
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
        write_agent_fixture(&agents_root, "researcher", "model = \"fake-ok\"\n", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let key = RoomKey::new("absent-owner");
        create_mention_room(&state, &key);

        let (status, _body) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "researcher".into(),
                display_name: "Researcher".into(),
                kind: RoomParticipantKind::Agent,
                owner_id: Some("nobody".into()),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let room = with_rooms(&state, |store| store.get(&key))
            .expect("room lookup")
            .expect("room exists");
        assert!(
            room.room.participants.is_empty(),
            "a refused owner must leave no roster row"
        );
        assert!(
            room.transcript.is_empty(),
            "a refused owner must forge no join marker"
        );
    }

    /// Only an Agent may carry an owner; anything else is refused rather than
    /// silently dropped. A caller that believed it recorded ownership and did
    /// not is the false-success class.
    /// Mutation: delete the `owner_requires_agent` arm -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owner_id_on_a_non_agent_is_refused() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&[
            "OCEAN_CONFIG_DIR",
            "OCEAN_MODEL",
            "OCEAN_YOLO",
            "OCEAN_AGENTS_DIR",
        ]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("owner-on-human");
        create_mention_room(&state, &key);

        let (status, Json(body)) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "bob".into(),
                display_name: "Bob".into(),
                kind: RoomParticipantKind::Human,
                owner_id: Some("alice".into()),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], json!("owner_requires_agent"));
    }

    /// Finding B (pro-adversary): `author_id` is caller-supplied and only
    /// roster-checked, so a hostile local caller could author an artifact AS
    /// somebody's agent. An agent's artifact is produced by the daemon's convene
    /// path, never by a client claiming its identity over the wire.
    /// Mutation: delete the forged_artifact_author arm -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_cannot_author_an_artifact_as_an_agent() {
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
        write_agent_fixture(&agents_root, "researcher", "model = \"fake-ok\"\n", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        let key = RoomKey::new("forge-artifact");
        create_mention_room(&state, &key);
        for (id, name, kind) in [
            ("alice", "Alice", RoomParticipantKind::Human),
            ("researcher", "Researcher", RoomParticipantKind::Agent),
        ] {
            let (status, _) = room_join(
                State(state.clone()),
                Path(key.as_str().to_string()),
                Json(RoomJoinRequest {
                    id: id.into(),
                    display_name: name.into(),
                    kind,
                    owner_id: None,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        let (status, Json(body)) = room_create_artifact(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(CreateArtifactRequest {
                id: "forged".into(),
                kind: RoomArtifactKind::Task,
                title: "I am the agent".into(),
                body: String::new(),
                author_id: "researcher".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("forged_artifact_author"));

        let (status, Json(list)) =
            room_list_artifacts(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            list["artifacts"].as_array().map(|a| a.len()),
            Some(0),
            "a forged artifact must not exist"
        );

        // A human author on the same route still works.
        let (status, _) = room_create_artifact(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(CreateArtifactRequest {
                id: "real".into(),
                kind: RoomArtifactKind::Task,
                title: "Real task".into(),
                body: String::new(),
                author_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, Json(body)) = room_amend_artifact(
            State(state.clone()),
            Path((key.as_str().to_string(), "real".into())),
            Json(AmendArtifactRequest {
                expected_version: 1,
                title: Some("Forged rewrite".into()),
                body: None,
                state: None,
                author_id: "researcher".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("forged_artifact_author"));

        let (status, Json(body)) = room_get_artifact(
            State(state.clone()),
            Path((key.as_str().to_string(), "real".into())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["artifact"]["title"], json!("Real task"));
        assert_eq!(body["artifact"]["version"], json!(1));
    }

    /// Create refuses a blank title; amend used to pass one straight through to
    /// the store, which erased the title and then minted a System line calling
    /// the erasure an update. Both halves answer the same bare 400 now, so a
    /// caller cannot tell which layer refused — and the amend's refusal is the
    /// store's, not a second route-side copy of the check.
    /// Mutation: delete the ArtifactTitleBlank arm in `room_amend_artifact` ->
    /// the store's typed error falls through to `room_store_error_response`,
    /// which answers 400 carrying the Display text instead of `invalid_request`
    /// -> RED. Delete the store guard -> the amend returns 200 -> RED.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_amend_cannot_blank_an_artifact_title_over_http() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _env = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("blank-title");
        create_plain_room(&state, &key);
        join_participant(&state, &key, "alice", RoomParticipantKind::Human, "Alice");

        // The shape create answers with, captured from the live route so the
        // comparison below cannot drift out of date.
        let (create_status, Json(create_refusal)) = room_create_artifact(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(CreateArtifactRequest {
                id: "t1".into(),
                kind: RoomArtifactKind::Task,
                title: "   ".into(),
                body: String::new(),
                author_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(create_status, StatusCode::BAD_REQUEST);

        let (status, _) = room_create_artifact(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(CreateArtifactRequest {
                id: "t1".into(),
                kind: RoomArtifactKind::Task,
                title: "Ship it".into(),
                body: String::new(),
                author_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let before = with_rooms(&state, |store| store.get(&key))
            .expect("room")
            .expect("room exists")
            .transcript
            .len();

        for blank in ["", "   "] {
            let (status, Json(body)) = room_amend_artifact(
                State(state.clone()),
                Path((key.as_str().to_string(), "t1".into())),
                Json(AmendArtifactRequest {
                    expected_version: 1,
                    title: Some(blank.into()),
                    body: Some("a body the caller meant to keep".into()),
                    state: None,
                    author_id: "alice".into(),
                }),
            )
            .await;
            assert_eq!(status, create_status, "amend must refuse as create does");
            assert_eq!(body, create_refusal, "and in the same shape, to the byte");
        }

        let (status, Json(body)) = room_get_artifact(
            State(state.clone()),
            Path((key.as_str().to_string(), "t1".into())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["artifact"]["title"], json!("Ship it"));
        assert_eq!(body["artifact"]["body"], json!(""));
        assert_eq!(body["artifact"]["version"], json!(1));
        assert_eq!(
            with_rooms(&state, |store| store.get(&key))
                .expect("room")
                .expect("room exists")
                .transcript
                .len(),
            before,
            "a refused amend must not mint a transcript line"
        );

        // An amend that leaves the title alone is untouched by the guard — this
        // is the shape `room_summary`'s body-only upsert issues.
        let (status, Json(body)) = room_amend_artifact(
            State(state.clone()),
            Path((key.as_str().to_string(), "t1".into())),
            Json(AmendArtifactRequest {
                expected_version: 1,
                title: None,
                body: Some("new body".into()),
                state: None,
                author_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["artifact"]["title"], json!("Ship it"));
        assert_eq!(body["artifact"]["version"], json!(2));
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
                owner_id: None,
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
                    owner_id: None,
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "id {bad:?} must be refused"
            );
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
                owner_id: None,
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
                owner_id: None,
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
                owner_id: None,
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
        assert!(transcript.iter().any(|message| message.author_kind
            == RoomParticipantKind::System
            && message.kind == RoomMessageKind::System
            && message.body.contains("\"outcome\":\"refused\"")
            && message.body.contains("agent_package_not_found")));
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
                owner_id: Some("human".into()),
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        let generation = authorize_room_agent_fixture(
            &state,
            &key,
            "bound-agent",
            ActivationPolicy::Mention,
            ContextPolicy::InvocationOnly,
        );

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
            Some(vec!["glob".to_string(), "read".to_string()])
        );
        assert_eq!(capture.model.as_deref(), Some("fake-ok"));
        assert!(
            capture.subprocess_caps.is_none(),
            "the Phase 1 node ceiling must remove ambient subprocess access"
        );
        assert!(state.requests.read().await.values().any(|request| {
            request.status.session_id
                == Some(core_sid(authorized_room_agent_session_id(
                    &key,
                    "bound-agent",
                    generation,
                )))
        }));
    }

    /// Index one attachment and write its bytes, the way an upload does. The
    /// hash is real because `attachment_bytes` re-verifies it: a fixture that
    /// recorded a plausible-looking digest would read back as missing bytes and
    /// pass the wrong assertion.
    fn attach_file(
        state: &AppState,
        key: &RoomKey,
        id: &str,
        filename: &str,
        content_type: &str,
        bytes: &[u8],
    ) {
        crate::room_attachments::write_blob_for_test(
            state.room_attachments_root.as_path(),
            key,
            id,
            bytes,
        );
        with_rooms(state, |store| {
            store
                .add_attachment(
                    key,
                    id,
                    filename,
                    content_type,
                    bytes.len() as u64,
                    &format!("{:x}", Sha256::digest(bytes)),
                    "human",
                    Utc::now(),
                )
                .expect("attachment fixture");
        });
    }

    /// `room_history` admits an ordinary turn while durable older transcript
    /// retrieval stays behind the opaque exact-generation source.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_history_turn_is_admitted_with_room_context() {
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
        write_agent_fixture(&agents_root, "reader-agent", "model = \"fake-ok\"\n", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let key = RoomKey::new("context-files");
        create_mention_room(&state, &key);
        join_human(&state, &key);
        attach_file(
            &state,
            &key,
            &"a".repeat(32),
            "brief.md",
            "text/markdown",
            b"ship the narrow slice\n",
        );
        // Declared `text/plain` and unmistakably not text: what gets inlined is
        // decided by the bytes, and this is the room-level proof of it.
        attach_file(
            &state,
            &key,
            &"b".repeat(32),
            "logo.png",
            "text/plain",
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR",
        );

        let (join_status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "reader-agent".into(),
                display_name: "Reader".into(),
                kind: RoomParticipantKind::Agent,
                owner_id: Some("human".into()),
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        authorize_room_agent_fixture(
            &state,
            &key,
            "reader-agent",
            ActivationPolicy::Mention,
            ContextPolicy::RoomHistory,
        );

        let (post_status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@reader-agent what are we shipping".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);

        let capture = wait_for_turn_capture("reader-agent")
            .await
            .expect("room-history turn reaches runtime with its opaque tool authority");
        assert!(capture
            .prompt
            .contains("[file] brief.md (text/markdown, 22 bytes)\nship the narrow slice\n"));
        assert!(capture
            .prompt
            .contains("[file] logo.png (text/plain, 16 bytes) — binary, not inlined"));
        assert!(!capture.prompt.contains("IHDR"));
        assert!(!state.requests.read().await.is_empty());
        let transcript = with_rooms(&state, |store| store.transcript(&key, None)).unwrap();
        assert!(transcript
            .iter()
            .all(|row| !row.body.contains("room_history_unavailable")));
        assert!(transcript.iter().all(|row| !row.body.contains("IHDR")));
    }

    /// The other half of the same guarantee: a room with no files is not paying
    /// for the feature, not even an empty section.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_convened_agent_in_a_fileless_room_gets_no_context_section() {
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
        write_agent_fixture(&agents_root, "bare-agent", "model = \"fake-ok\"\n", None);
        std::env::set_var("OCEAN_AGENTS_DIR", &agents_root);
        clear_turn_captures();
        let key = RoomKey::new("no-context-files");
        create_mention_room(&state, &key);
        join_human(&state, &key);

        let (join_status, _) = room_join(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomJoinRequest {
                id: "bare-agent".into(),
                display_name: "Bare".into(),
                kind: RoomParticipantKind::Agent,
                owner_id: Some("human".into()),
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        authorize_room_agent_fixture(
            &state,
            &key,
            "bare-agent",
            ActivationPolicy::Mention,
            ContextPolicy::RoomRecent,
        );

        let (post_status, _) = room_post_message(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Json(RoomMessageRequest {
                author_id: "human".into(),
                author_kind: RoomParticipantKind::Human,
                body: "@bare-agent status".into(),
                thread_parent_seq: None,
            }),
        )
        .await;
        assert_eq!(post_status, StatusCode::CREATED);

        let capture = wait_for_turn_capture("bare-agent")
            .await
            .expect("a convened turn must reach runtime dispatch");
        assert!(!capture.prompt.contains("context files"));
        assert!(capture
            .prompt
            .ends_with("--- end transcript ---\n\nYour reply:"));
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
                owner_id: Some("human".into()),
            }),
        )
        .await;
        assert_eq!(join_status, StatusCode::OK);
        let generation = authorize_room_agent_fixture(
            &state,
            &key,
            "data-only",
            ActivationPolicy::Mention,
            ContextPolicy::InvocationOnly,
        );

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
        let event = loop {
            let event = trigger_rx
                .try_recv()
                .expect("resolved agent emits room_trigger");
            if matches!(event.event, AgentTurnEvent::Extension { ref extension, .. } if extension == "room_trigger")
            {
                break event;
            }
        };
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
            request.status.session_id
                == Some(core_sid(authorized_room_agent_session_id(
                    &key,
                    "data-only",
                    generation,
                )))
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
            self_member_id: None,
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
        assert_eq!(body["closed"], json!(true));
    }

    /// The mirror of the test above: identical fixture minus the `close`, so the
    /// only thing that can move `closed` is closedness itself. Alone, either test
    /// passes against a hardcoded constant; the pair is what makes the field a
    /// discriminator, and a hydrating client is trusting it to be one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_open_room_reports_closed_false() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-not-closed");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), "Open", None, Utc::now())
                .expect("create");
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
        assert_eq!(body["closed"], json!(false));
    }

    /// `agent_owners` on the route the surface actually hydrates through.
    /// ocean-surface#185 moved hydration off `room_get`, so the projection only
    /// `room_get` served became unreachable from the UI.
    ///
    /// Four states on ONE fixture, because a hardcoded value would satisfy any of
    /// them alone: a roster with no owned agent answers `[]`; an owned agent whose
    /// worker is present answers `owner_present: true`; the same binding after that
    /// worker leaves answers `false` WITHOUT dropping the row; and the room closed
    /// answers that row still. The first three are also compared against
    /// `room_get`, so the two hydration routes cannot grow two shapes for one fact.
    /// The fourth cannot be compared, and that is exactly why it is asserted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_reports_agent_owners_exactly_as_room_get_does() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-owners");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), "Owned", None, Utc::now())
                .expect("create");
            // An agent cannot be owned by someone absent, so the worker lands first.
            store
                .add_participant_with_message(
                    &key,
                    RoomParticipant {
                        id: "alice".into(),
                        kind: RoomParticipantKind::Human,
                        display_name: "Alice".into(),
                    },
                    Utc::now(),
                )
                .expect("seed owner");
        });

        let (status, body) = snapshot_response(&state, &key, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["agent_owners"],
            json!([]),
            "a room with no owned agent still answers the field"
        );
        assert_eq!(
            body["agent_owners"],
            room_get_body(&state, &key).await["agent_owners"]
        );

        with_rooms(&state, |store| {
            store
                .add_agent_participant_with_owner(
                    &key,
                    RoomParticipant {
                        id: "researcher".into(),
                        kind: RoomParticipantKind::Agent,
                        display_name: "Researcher".into(),
                    },
                    "alice",
                    Utc::now(),
                )
                .expect("own the agent");
        });

        let (status, body) = snapshot_response(&state, &key, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["agent_owners"],
            json!([{ "agent_id": "researcher", "owner_id": "alice", "owner_present": true }]),
            "hydration must say whose agent this is"
        );
        assert_eq!(
            body["agent_owners"],
            room_get_body(&state, &key).await["agent_owners"],
            "the two hydration routes must report one shape for one fact"
        );

        // The worker leaves. The binding outlives them, so the flag moves and the
        // row does not — an agent that is unclaimed now, not one that never was.
        with_rooms(&state, |store| {
            store
                .remove_participant_with_message(&key, "alice", Utc::now())
                .expect("owner leaves");
        });

        let (status, body) = snapshot_response(&state, &key, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["agent_owners"],
            json!([{ "agent_id": "researcher", "owner_id": "alice", "owner_present": false }]),
            "a departed owner is reported, not erased"
        );
        assert_eq!(
            body["agent_owners"],
            room_get_body(&state, &key).await["agent_owners"]
        );

        // Freezing the room does not erase who owned what: `close` is a soft
        // `UPDATE rooms SET closed_at` that retains the roster and the ownership
        // rows, and the store read carries no openness guard. This stage exists
        // because hydration moving here made `/snapshot` the ONLY route that can
        // report ownership for a frozen room — `room_get` 404s — so giving that
        // read the `room_is_open` check its neighbours carry would empty every
        // audit view's annotation with nothing red: the key-set pin in `main.rs`
        // sees a present key, and the stages above never close the room.
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let (status, body) = snapshot_response(&state, &key, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["closed"], json!(true), "the audit view");
        assert_eq!(
            body["agent_owners"],
            json!([{ "agent_id": "researcher", "owner_id": "alice", "owner_present": false }]),
            "a frozen room still reports the binding it froze with"
        );

        let app = room_routes().with_state(state.clone());
        let closed_get = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            closed_get.status(),
            StatusCode::NOT_FOUND,
            "the other hydration route cannot answer this state at all"
        );
    }

    // ── Snapshot tail paging (`before_seq`) ──────────────────────────────────

    /// GET a snapshot with a raw query string and return `(status, body)`.
    async fn snapshot_response(
        state: &AppState,
        key: &RoomKey,
        query: &str,
    ) -> (StatusCode, serde_json::Value) {
        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/snapshot{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        (status, body)
    }

    /// Seq values in a snapshot's `transcript` array, in the order served.
    fn transcript_seqs(body: &serde_json::Value) -> Vec<u64> {
        body["transcript"]
            .as_array()
            .expect("transcript array")
            .iter()
            .map(|m| m["seq"].as_u64().expect("seq"))
            .collect()
    }

    /// A `before_seq` above every storable seq: "before the end of everything",
    /// which is how a client opens at the tail without knowing the last seq.
    const BEYOND_END: u64 = u64::MAX;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_before_seq_serves_the_newest_page() {
        // The bug this slice closes: a 12-message room hydrated at message #1. With
        // before_seq the same room opens at its last four, ascending.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-tail");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 12);

        let (status, body) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transcript_seqs(&body), vec![8, 9, 10, 11]);
        assert_eq!(body["last_seq"], json!(11), "the tail is the newest row");
        assert_eq!(body["has_more"], json!(true), "older rows remain");
        // Backward cursor is the OLDEST row on the page, replayed as before_seq.
        assert_eq!(body["prev_seq"], json!(8));
        // A backward read computes no forward cursor; last_seq is that cursor.
        assert_eq!(body["next_seq"], json!(null));
    }

    /// The mirror of the test above: the identical fixture read WITHOUT
    /// `before_seq` must still open at the oldest page. Alone, either test passes
    /// against a handler hardwired to one end of the log; the pair is what makes
    /// the parameter a discriminator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_without_before_seq_still_serves_the_oldest_page() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-head");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 12);

        let (status, body) = snapshot_response(&state, &key, "?limit=4").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transcript_seqs(&body), vec![0, 1, 2, 3]);
        assert_eq!(body["next_seq"], json!(3), "forward cursor is unchanged");
        assert_eq!(body["prev_seq"], json!(null));
        assert_eq!(body["has_more"], json!(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_before_seq_pages_backward_to_the_start() {
        // Replaying prev_seq walks toward the oldest page and terminates there with
        // has_more false and no cursor — a client scrolling up has an end.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-tail-page");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 9);

        let (_, first) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(transcript_seqs(&first), vec![5, 6, 7, 8]);
        assert_eq!(first["prev_seq"], json!(5));

        let (_, second) = snapshot_response(&state, &key, "?before_seq=5&limit=4").await;
        assert_eq!(transcript_seqs(&second), vec![1, 2, 3, 4]);
        assert_eq!(second["prev_seq"], json!(1));
        assert_eq!(second["has_more"], json!(true));

        let (_, third) = snapshot_response(&state, &key, "?before_seq=1&limit=4").await;
        assert_eq!(transcript_seqs(&third), vec![0]);
        assert_eq!(third["has_more"], json!(false));
        assert_eq!(
            third["prev_seq"],
            json!(null),
            "the oldest page has no cursor"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_before_seq_zero_is_an_empty_page() {
        // Nothing precedes the first message (seq 0). The answer this rules out is
        // the whole log, which is what a 0 read as "no bound" would return.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-before-zero");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 12);

        let (status, body) = snapshot_response(&state, &key, "?before_seq=0&limit=4").await;
        assert_eq!(status, StatusCode::OK);
        assert!(transcript_seqs(&body).is_empty());
        assert_eq!(body["has_more"], json!(false));
        assert_eq!(body["prev_seq"], json!(null));
        assert_eq!(body["last_seq"], json!(null));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_rejects_both_cursors_together() {
        // Two directions in one request: a caller that sent both has two different
        // pages in mind, so neither silent precedence would be the one it meant.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-both-cursors");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 5);

        let (status, body) =
            snapshot_response(&state, &key, "?after_seq=1&before_seq=4&limit=2").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["code"], json!("conflicting_transcript_cursors"));
        assert!(body["transcript"].is_null(), "no page is served either way");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_before_seq_serves_a_soft_closed_room_identically() {
        // A frozen call room must paint the SAME screen a live one paints. Before
        // the audit arm mirrored the window, this room answered its oldest page
        // while `snapshot_before_seq_serves_the_newest_page` answered its newest —
        // the same hydration, two screens, decided by whether the call had ended.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-tail-closed");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 12);
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let (status, body) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["closed"], json!(true), "still the audit view");
        assert_eq!(transcript_seqs(&body), vec![8, 9, 10, 11]);
        assert_eq!(body["has_more"], json!(true));
        assert_eq!(body["prev_seq"], json!(8));
        assert_eq!(body["last_seq"], json!(11));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_closed_room_backward_paging_terminates_at_the_start() {
        // The audit arm's own boundary: replaying prev_seq into a frozen room must
        // reach a final page with no cursor, not repeat the same window forever.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-closed-page");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 6);
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let (_, first) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(transcript_seqs(&first), vec![2, 3, 4, 5]);
        assert_eq!(first["prev_seq"], json!(2));

        let (_, second) = snapshot_response(&state, &key, "?before_seq=2&limit=4").await;
        assert_eq!(transcript_seqs(&second), vec![0, 1]);
        assert_eq!(second["has_more"], json!(false));
        assert_eq!(second["prev_seq"], json!(null));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_closed_room_past_the_record_cap_still_serves_its_true_tail() {
        // Parity has to hold for the LONG room too, and that is exactly where the
        // frozen record cannot supply it: `get_including_closed` hydrates the oldest
        // MAX_TRANSCRIPT_LIMIT rows, so a window applied to that answered 996..999 —
        // the newest page of the first thousand — with `has_more`, `prev_seq`,
        // `last_seq` and `closed` all looking right and rows 1000..1004 reachable by
        // nothing on the wire. The 12,000-message room is the case this slice exists
        // for, so the audit arm reads the store's rows and not the record's copy.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-closed-long");
        seed_access(&state, &key, local_access());
        let seeded = ocean_store::MAX_TRANSCRIPT_LIMIT + 5;
        seed_transcript(&state, &key, seeded);
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let (status, body) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["closed"], json!(true), "still the audit view");
        let newest = (seeded - 1) as u64;
        assert_eq!(
            transcript_seqs(&body),
            vec![newest - 3, newest - 2, newest - 1, newest],
            "the true tail, not the newest page of the first thousand"
        );
        assert_eq!(body["last_seq"], json!(newest));
        assert_eq!(body["prev_seq"], json!(newest - 3));
        assert_eq!(body["has_more"], json!(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_closed_room_page_exactly_at_the_limit_has_no_cursor() {
        // The closed room's boundary, which every other frozen fixture steps over —
        // 12 rows against a limit of 4, or 2 against 4, never exactly 4. A page that
        // is full has not thereby got more behind it, and reporting `has_more` when
        // it has not costs the client a round trip to learn that a "load older"
        // affordance had nothing under it. The mirror of the store's
        // `transcript_tail_page_exact_boundary_page_has_no_cursor`.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-closed-exact");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 4);
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let (status, body) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["closed"], json!(true));
        assert_eq!(transcript_seqs(&body), vec![0, 1, 2, 3]);
        assert_eq!(body["has_more"], json!(false), "nothing older remains");
        assert_eq!(body["prev_seq"], json!(null));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_before_seq_on_unknown_room_is_still_404() {
        // The tail read must not widen visibility: a room that never existed is a
        // 404 on this path exactly as it is on the forward one.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-snap-tail-missing");

        let (status, body) =
            snapshot_response(&state, &key, &format!("?before_seq={BEYOND_END}&limit=4")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["ok"], json!(false));
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

    /// Append `n` chat rows bodied `msg-{i}` to an existing room, so a test can
    /// assert exactly which slice of the log came back.
    fn seed_transcript(state: &AppState, key: &RoomKey, n: usize) {
        with_rooms(state, |store| {
            for i in 0..n {
                store
                    .append_message(
                        key,
                        "john",
                        RoomParticipantKind::Human,
                        RoomMessageKind::Message,
                        &format!("msg-{i}"),
                        Utc::now(),
                    )
                    .expect("seed transcript");
            }
        });
    }

    /// GET a room and return its parsed body.
    async fn room_get_body(state: &AppState, key: &RoomKey) -> serde_json::Value {
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
        serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_get_under_the_cap_says_there_is_no_more() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-get-short");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 3);

        let body = room_get_body(&state, &key).await;
        // The rows this route already answered, unchanged.
        let rows = body["transcript"].as_array().expect("transcript array");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["body"], json!("msg-0"));
        assert_eq!(rows[2]["body"], json!("msg-2"));
        // Plus the two fields that say it is the WHOLE log, not a prefix.
        assert_eq!(body["has_more"], json!(false));
        assert_eq!(body["next_seq"], json!(null));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_get_past_the_cap_admits_it_is_a_prefix() {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-get-long");
        seed_access(&state, &key, local_access());
        // One past the cap is the case a bare row count could not see: the record's
        // transcript is ALSO 1000 rows here, and only the page's `limit + 1`
        // sentinel — carried on the record as `transcript_has_more` — separates it
        // from a room that ends exactly on the cap.
        seed_transcript(&state, &key, ocean_store::MAX_TRANSCRIPT_LIMIT + 1);

        let body = room_get_body(&state, &key).await;
        let rows = body["transcript"].as_array().expect("transcript array");
        assert_eq!(rows.len(), ocean_store::MAX_TRANSCRIPT_LIMIT);
        assert_eq!(body["has_more"], json!(true));
        // The cursor is the last row actually returned, replayable as after_seq.
        let last_seq = rows.last().unwrap()["seq"].clone();
        assert_eq!(body["next_seq"], last_seq);
        assert_eq!(
            body["next_seq"],
            json!(ocean_store::MAX_TRANSCRIPT_LIMIT as u64 - 1)
        );
    }

    /// GET a room's `/transcript` with a raw query string, returning `(status, body)`.
    async fn transcript_response(
        state: &AppState,
        key: &RoomKey,
        query: &str,
    ) -> (StatusCode, serde_json::Value) {
        let app = room_routes().with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::get(format!("/v1/rooms/persistent/{key}/transcript{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        (status, body)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transcript_of_a_closed_room_pages_forward_past_the_record_cap() {
        // The forward twin of `snapshot_closed_room_past_the_record_cap_...`, and the
        // case the audit arm answered wrong for as long as it windowed the frozen
        // RECORD: that record is the oldest MAX_TRANSCRIPT_LIMIT rows, so
        // `msgs.len() > effective_limit` was false at the cap however long the room
        // was. A soft-closed 1005-row room served its first full page with
        // `has_more: false, next_seq: null` at seq 999 and a paging client stopped
        // there believing it had the log — rows 1000..1004 reachable by nothing on
        // this route. The arm reads the store's rows now, so the walk finishes.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-transcript-closed-long");
        seed_access(&state, &key, local_access());
        let seeded = ocean_store::MAX_TRANSCRIPT_LIMIT + 5;
        seed_transcript(&state, &key, seeded);
        with_rooms(&state, |store| store.close(&key).expect("close"));

        let cap = ocean_store::MAX_TRANSCRIPT_LIMIT;
        let (status, first) = transcript_response(&state, &key, &format!("?limit={cap}")).await;
        assert_eq!(status, StatusCode::OK, "a frozen room stays queryable");
        assert_eq!(transcript_seqs(&first).len(), cap);
        let cap_edge = cap as u64 - 1;
        assert_eq!(
            first["has_more"],
            json!(true),
            "five rows lie past the record's last one"
        );
        assert_eq!(first["next_seq"], json!(cap_edge));

        let (_, rest) =
            transcript_response(&state, &key, &format!("?after_seq={cap_edge}&limit={cap}")).await;
        let newest = (seeded - 1) as u64;
        assert_eq!(
            transcript_seqs(&rest),
            (cap_edge + 1..=newest).collect::<Vec<u64>>(),
            "the rows past the record's cap, not an empty terminal page"
        );
        assert_eq!(rest["has_more"], json!(false));
        assert_eq!(rest["next_seq"], json!(null));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_get_closed_room_with_a_transcript_is_still_404() {
        // A closed room whose audit view HAS rows to serve is still 404.
        // `room_get_closed_is_404` pins the empty one; this pins the case that
        // makes serving those rows tempting, since `/snapshot` reads exactly this
        // room and answers 200. Both go red on the one mutation that matters —
        // `reg.get` widened to `get_including_closed`.
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("s2-get-closed-with-rows");
        seed_access(&state, &key, local_access());
        seed_transcript(&state, &key, 4);
        with_rooms(&state, |store| store.close(&key).expect("close"));

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
                self_member_id: None,
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

    fn seed_live_with_failed_projection(client_event_id: &str) -> RoomAccessProjection {
        RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(1),
            members: vec![],
            self_member_id: None,
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
        }
    }

    /// Create a room with Live access and one Failed outbox item.
    fn seed_live_with_failed(state: &AppState, key: &RoomKey, client_event_id: &str) {
        seed_access(
            state,
            key,
            seed_live_with_failed_projection(client_event_id),
        );
    }

    /// Read one SSE event frame from a streaming body.
    async fn read_sse_frame(body: &mut Body) -> SseFrame {
        use http_body_util::BodyExt as _;
        let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
            .await
            .expect("frame timeout")
            .expect("SSE body ended")
            .expect("SSE body error");
        let text = String::from_utf8_lossy(&frame.into_data().unwrap_or_default()).to_string();
        parse_sse_frame(&text)
    }

    #[derive(Debug)]
    struct SseFrame {
        event: String,
        id: Option<String>,
        data: serde_json::Value,
    }

    fn parse_sse_frame(text: &str) -> SseFrame {
        let event = text
            .lines()
            .find_map(|line| line.strip_prefix("event: "))
            .unwrap_or("")
            .to_string();
        let id = text
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .map(str::to_string);
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .map(|d| serde_json::from_str(d).expect("valid JSON"))
            .unwrap_or(serde_json::Value::Null);
        SseFrame { event, id, data }
    }

    async fn read_until_access_frame(body: &mut Body, dur: Duration) -> SseFrame {
        use http_body_util::BodyExt as _;
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for room_access SSE frame"
            );
            let frame = tokio::time::timeout(Duration::from_millis(50), body.frame())
                .await
                .expect("frame timeout")
                .expect("SSE body ended")
                .expect("SSE body error");
            let parsed = parse_sse_frame(&String::from_utf8_lossy(
                &frame.into_data().unwrap_or_default(),
            ));
            if parsed.event == "room_access" {
                return parsed;
            }
            assert_eq!(
                parsed.event, "room_read_cursor",
                "unexpected interleaved SSE event before room_access"
            );
            assert!(
                parsed.id.is_none(),
                "room_read_cursor bootstrap must not consume or alter Last-Event-ID"
            );
        }
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
            self_member_id: None,
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
        let initial_expected =
            serde_json::to_value(seed_live_with_failed_projection("evt-both")).unwrap();
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

        // Both subscribers must observe the exact initial access projection, but
        // a no-id cursor bootstrap may interleave first.
        let init1 = read_until_access_frame(&mut body1, Duration::from_millis(500)).await;
        assert_eq!(init1.data, initial_expected);
        let init2 = read_until_access_frame(&mut body2, Duration::from_millis(500)).await;
        assert_eq!(init2.data, initial_expected);

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

        // Both must receive exactly one follow-up room_access frame; any
        // interleaved bootstrap/read-cursor frames must be no-id and not affect
        // message Last-Event-ID semantics.
        let sub1 = read_until_access_frame(&mut body1, Duration::from_secs(1)).await;
        assert_eq!(sub1.data, expected, "subscriber 1 mismatched");
        let sub2 = read_until_access_frame(&mut body2, Duration::from_secs(1)).await;
        assert_eq!(sub2.data, expected, "subscriber 2 mismatched");
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
        let frame = read_sse_frame(&mut body_a).await;
        assert_eq!(frame.event, "room_access");
        assert_eq!(frame.data["outbox"][0]["client_event_id"], json!("evt-a"));

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
            self_member_id: None,
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
                        self_member_id: None,
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
        // Two independent subscriptions: one for the access-projection tail
        // and one dedicated to the cursor tail (so an access wake alone can
        // make the cursor tail re-check on a federated Connecting/Recovering
        // -> Live transition; see `run_room_read_cursor_tail`).
        assert_eq!(state.room_access_wakes.receiver_count(), 2);

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
                self_member_id: None,
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
                self_member_id: None,
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
            self_member_id: None,
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
                    self_member_id: None,
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
