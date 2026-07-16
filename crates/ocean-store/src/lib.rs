//! `ocean-store` — SQLite-backed durable storage for Ocean OS (OCEAN-86).
//!
//! This crate is the persistent counterpart to the in-memory
//! [`RoomRegistry`](https://github.com/) that lives in `ocean-agent`
//! (`crates/ocean-agent/src/rooms.rs`, OCEAN-65). OCEAN-65 built the in-memory
//! store and explicitly deferred SQLite persistence to a future ticket; this is
//! that ticket.
//!
//! # What it is
//!
//! A single [`SqliteRoomStore`] that mirrors the `RoomRegistry` API one method
//! for one method — `create` / `get` / `list` / `update` / `close`,
//! `add_participant` / `remove_participant` (with the same auto join/leave
//! transcript markers the in-memory store writes), `append_message`,
//! `transcript` / `transcript_page` (bounded `after_seq` tailing with a
//! `LIMIT` + cursor, OCEAN-249), and `trigger_policy`. The
//! [`RoomStore`] trait captures that shared shape so the in-memory registry and
//! this SQLite store are interchangeable behind a `dyn RoomStore`.
//!
//! Operations are **synchronous** and the store is held behind its own
//! `rusqlite::Connection`. This deliberately matches the daemon's room registry,
//! which is a plain struct behind a `std::sync::Mutex` whose guard is always
//! dropped before any `.await`. No async runtime is coupled in.
//!
//! # DB library choice — `rusqlite` (bundled)
//!
//! Nothing in the workspace used a database before this crate, so there was no
//! existing async-DB convention to match. We chose `rusqlite` with the
//! `bundled` feature because:
//!
//! * **Sync, no runtime coupling.** The room registry it replaces is sync and
//!   `Mutex`-guarded; a sync DB drops straight into the same call sites without
//!   dragging `sqlx`/`tokio` into the storage layer.
//! * **Bundled SQLite.** No system `libsqlite3` dependency — the C library is
//!   compiled in, so builds are hermetic on any operator machine.
//! * **Simplicity.** Local-first single-writer storage is exactly SQLite's
//!   sweet spot; an async pool buys nothing here.
//!
//! # Seq semantics
//!
//! Transcript messages are keyed by `(room, seq)` where `seq` is a per-room
//! monotonically increasing counter assigned by the store, identical to the
//! in-memory registry. The counter is derived as `MAX(seq) + 1` recomputed from
//! stored rows, so it survives restarts and never reuses a value.
//!
//! Every write path that allocates a seq (`add_participant`, `remove_participant`,
//! `append_message`) runs the `SELECT MAX(seq) + 1` and its dependent `INSERT`
//! inside a single `IMMEDIATE` SQLite transaction (OCEAN-201). IMMEDIATE takes the
//! write lock at `BEGIN`, so a second connection on the same DB file cannot
//! interleave a commit between the seq read and the message insert — that race
//! used to tear the transcript (a roster row with no join marker, or a seq gap).
//! On any failure the transaction rolls back as a unit, so a partial write is
//! never observable.
//!
//! # How the daemon would adopt this (deferred follow-up)
//!
//! **This crate is additive only.** The in-memory `RoomRegistry` is untouched
//! and the daemon is not wired to use this store — that is a separate follow-up
//! ticket. When the daemon does adopt it:
//!
//! 1. Open a store once at startup:
//!    `SqliteRoomStore::open(state_dir.join("rooms.db"))?` (or
//!    `open_in_memory()` for tests). `open` runs [`SqliteRoomStore::migrate`]
//!    idempotently, so it is safe on an existing DB.
//! 2. Replace the `Mutex<RoomRegistry>` field on the daemon's `AppState` with a
//!    `Mutex<SqliteRoomStore>` (or store it as `Mutex<Box<dyn RoomStore>>` to
//!    keep both backends selectable). Every method name and signature already
//!    matches, so the room HTTP handlers change only their field type.
//! 3. Because the methods are sync and `&mut self`, keep the existing pattern:
//!    lock the `Mutex`, call the method, drop the guard before any `.await`.
//! 4. `close` here **soft-closes** (marks `closed_at`) rather than deleting, so
//!    transcripts survive an audit. `list` and `get` hide soft-closed rooms by
//!    default, matching the in-memory `close` (which removed the room from
//!    `get`/`list`). Use [`SqliteRoomStore::get_including_closed`] for audit
//!    views.
//!
//! No `ocean-daemon` code is modified by this crate.

use std::path::Path;

use chrono::{DateTime, Utc};
use ocean_core::{
    FederatedMessageMeta, FederatedRoomMemberProjection, OutboxItemState, Room,
    RoomAccessProjection, RoomAccessState, RoomKey, RoomMessage, RoomMessageKind, RoomOutboxItem,
    RoomParticipant, RoomParticipantKind, RoomTriggerPolicy,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// A persistent room plus its transcript. Mirror of `ocean_agent::rooms::RoomRecord`
/// so callers can move between the in-memory and SQLite stores without changing
/// their handling of returned records.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomRecord {
    /// The persistent room entity (id, name, roster, timestamps, trigger policy).
    pub room: Room,
    /// Append-only transcript of room events, in `seq` order. Bounded by
    /// [`MAX_TRANSCRIPT_LIMIT`] — a record never hydrates an unbounded transcript
    /// (OCEAN-249). For a transcript longer than that cap, page with
    /// [`RoomStore::transcript_page`].
    pub transcript: Vec<RoomMessage>,
}

/// Default number of transcript rows returned when a caller does not specify a
/// limit (OCEAN-249). Transcript reads used to be unbounded — a long-lived call
/// room would re-read its entire log on every hydration (O(n) per poll, O(n²)
/// over a session). This cap makes the default read bounded; callers that need
/// more page through with the returned cursor.
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 200;

/// Hard ceiling on a single transcript page (OCEAN-249). A caller-supplied limit
/// is clamped to this so no single query can be coerced into a full-table scan.
pub const MAX_TRANSCRIPT_LIMIT: usize = 1000;

/// One bounded page of a room transcript (OCEAN-249).
///
/// `messages` holds at most the effective limit of rows in ascending `seq`
/// order. `next_seq` is the cursor a client replays as the next `after_seq` to
/// fetch the following page; it is `Some(last_returned_seq)` when more rows exist
/// and `None` when the page reached the end of the transcript. `has_more` is the
/// same signal as a bool for callers that only need to know whether to keep
/// paging.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptPage {
    /// This page's messages, ascending by `seq`, at most the effective limit.
    pub messages: Vec<RoomMessage>,
    /// Cursor for the next page (`after_seq`), or `None` at the end.
    pub next_seq: Option<u64>,
    /// Whether at least one more row exists beyond this page.
    pub has_more: bool,
}

/// Clamp a caller-supplied transcript limit into the allowed range. `None` (no
/// limit given) maps to [`DEFAULT_TRANSCRIPT_LIMIT`]; any value is capped at
/// [`MAX_TRANSCRIPT_LIMIT`] and floored at 1 so a `0` can never request an empty
/// page that also reports `has_more = true`.
pub fn clamp_transcript_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_TRANSCRIPT_LIMIT)
        .clamp(1, MAX_TRANSCRIPT_LIMIT)
}

/// Default number of rows returned by a *collection* list (rooms, sessions,
/// projects) when a caller does not specify a limit (OCEAN-250). The list
/// endpoints used to return everything — a daemon with thousands of historical
/// rows answered a multi-MB JSON blob on every poll. This caps the default read;
/// callers that need more page through with the returned cursor. (Distinct from
/// [`DEFAULT_TRANSCRIPT_LIMIT`]: a transcript tail wants a larger default window
/// than a list of room/session cards.)
pub const DEFAULT_LIST_LIMIT: usize = 100;

/// Hard ceiling on a single collection-list page (OCEAN-250). A caller-supplied
/// limit is clamped to this so no single list request can be coerced into an
/// unbounded scan + serialize.
pub const MAX_LIST_LIMIT: usize = 1000;

/// One bounded page of a room list (OCEAN-250).
///
/// `rooms` holds at most the effective limit of rooms in the store's stable list
/// order (`updated_at DESC, id ASC`). `next_cursor` is the room key a client
/// replays as the next `after` to fetch the following page; it is
/// `Some(last_returned_key)` when more rows exist and `None` at the end.
/// `has_more` is the same signal as a bool.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomPage {
    /// This page's rooms, in list order, at most the effective limit.
    pub rooms: Vec<Room>,
    /// Cursor for the next page (the `after` room key), or `None` at the end.
    pub next_cursor: Option<String>,
    /// Whether at least one more room exists beyond this page.
    pub has_more: bool,
}

/// Clamp a caller-supplied collection-list limit into the allowed range. `None`
/// (no limit given) maps to [`DEFAULT_LIST_LIMIT`]; any value is capped at
/// [`MAX_LIST_LIMIT`] and floored at 1 so a `0` can never request an empty page
/// that also reports `has_more = true`. The sibling of [`clamp_transcript_limit`]
/// for list endpoints.
pub fn clamp_list_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

/// Error returned by store operations.
///
/// The caller-input variants (`BadKey`, `UnknownRoom`, `AlreadyExists`,
/// `UnknownParticipant`) are identical to `ocean_agent::rooms::RoomStoreError`;
/// `Db` and `Encode` are added because a durable backend can fail on I/O and
/// (de)serialization, which an in-memory map cannot.
#[derive(Debug)]
pub enum RoomStoreError {
    /// A room key was empty or otherwise malformed.
    BadKey(String),
    /// No room exists for the given key.
    UnknownRoom(RoomKey),
    /// A room with this key already exists (on create).
    AlreadyExists(RoomKey),
    /// No participant with the given id is in the room (on remove).
    UnknownParticipant { room: RoomKey, participant: String },
    /// No outbox item found for the given client_event_id.
    UnknownOutboxItem {
        room: RoomKey,
        client_event_id: String,
    },
    /// Outbox item exists but is not in a failed state.
    OutboxItemNotFailed {
        room: RoomKey,
        client_event_id: String,
        current_state: String,
    },
    /// The room exists but is in a state that rejects the operation (local / revoked).
    RoomStateRejected {
        room: RoomKey,
        state: RoomAccessState,
    },
    /// An underlying SQLite error.
    Db(rusqlite::Error),
    /// A stored value could not be (de)serialized.
    Encode(String),
}

impl std::fmt::Display for RoomStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey(k) => write!(f, "invalid room key '{k}'; must be non-empty"),
            Self::UnknownRoom(k) => write!(f, "no room with key '{k}'"),
            Self::AlreadyExists(k) => write!(f, "room '{k}' already exists"),
            Self::UnknownParticipant { room, participant } => {
                write!(f, "room '{room}' has no participant '{participant}'")
            }
            Self::UnknownOutboxItem {
                room,
                client_event_id,
            } => {
                write!(f, "room '{room}' has no outbox item '{client_event_id}'")
            }
            Self::OutboxItemNotFailed {
                room,
                client_event_id,
                current_state,
            } => {
                write!(
                    f,
                    "outbox item '{client_event_id}' in room '{room}' is in state '{current_state}', not failed"
                )
            }
            Self::RoomStateRejected { room, state } => {
                write!(
                    f,
                    "room '{room}' is in state '{}', rejecting operation",
                    serde_json::to_string(state).unwrap_or_else(|_| format!("{:?}", state))
                )
            }
            Self::Db(e) => write!(f, "sqlite error: {e}"),
            Self::Encode(e) => write!(f, "encode error: {e}"),
        }
    }
}

impl std::error::Error for RoomStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for RoomStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

type Result<T> = std::result::Result<T, RoomStoreError>;

/// The common room-store operations, shared by the in-memory `RoomRegistry`
/// (OCEAN-65) and [`SqliteRoomStore`]. Capturing the shape as a trait lets the
/// daemon hold a `Box<dyn RoomStore>` and swap backends. The in-memory registry
/// in `ocean-agent` is a candidate to implement this in the daemon-wiring
/// follow-up; this crate provides the SQLite implementation today.
pub trait RoomStore {
    /// Create a new persistent room. Fails if the key is empty or already taken.
    fn create(
        &mut self,
        key: RoomKey,
        name: &str,
        trigger_policy: Option<RoomTriggerPolicy>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord>;

    /// Create a new persistent room bound to a workspace directory (OCEAN-260).
    ///
    /// Identical to [`create`](Self::create) but persists `workspace_root` on the
    /// room so a room-bound agent turn can resolve its owning project (via the
    /// reverse map `AgentRuntime::project_for_workspace`, OCEAN-228) and set the
    /// turn's `cwd`. `None` is equivalent to plain `create` — the room has no
    /// project binding. A blanket provided impl forwards to `create` for stores
    /// that have no workspace column yet, so this is additive for implementors.
    fn create_in_workspace(
        &mut self,
        key: RoomKey,
        name: &str,
        workspace_root: Option<String>,
        trigger_policy: Option<RoomTriggerPolicy>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        // Default: ignore the binding and fall back to the unbound create. The
        // SQLite store overrides this to actually persist `workspace_root`.
        let _ = workspace_root;
        self.create(key, name, trigger_policy, now)
    }

    /// One room record (room + transcript) by key.
    fn get(&self, key: &RoomKey) -> Result<Option<RoomRecord>>;

    /// All open rooms, most-recently-updated first, ties broken by key.
    ///
    /// **Bounded (OCEAN-250).** This returns at most [`DEFAULT_LIST_LIMIT`]
    /// rooms — it is no longer an unbounded list. Callers that need to page or
    /// that want the cursor/`has_more` signal should use [`RoomStore::list_page`];
    /// this method is kept as the convenience "first page with the default cap"
    /// form.
    fn list(&self) -> Result<Vec<Room>>;

    /// One bounded page of the open-room list (OCEAN-250).
    ///
    /// Returns open rooms in `updated_at DESC, id ASC` order starting *after* the
    /// `after` room key (or from the top when `None`), at most `limit` rooms.
    /// `limit` is clamped by [`clamp_list_limit`]: `None` ⇒ [`DEFAULT_LIST_LIMIT`],
    /// any value capped at [`MAX_LIST_LIMIT`]. The returned [`RoomPage`] carries
    /// `next_cursor` (the room key to replay as the next `after`) and `has_more`.
    /// Page to the end by repeating with `after = next_cursor` until `has_more` is
    /// false. An `after` key that is not in the list (closed/never-existed) simply
    /// yields rows that sort after it — paging is resilient to a stale cursor.
    fn list_page(&self, after: Option<&str>, limit: Option<usize>) -> Result<RoomPage>;

    /// Update a room's mutable metadata (name and/or trigger policy). `None`
    /// leaves a field unchanged; `Some(None)` clears the trigger policy.
    fn update(
        &mut self,
        key: &RoomKey,
        name: Option<String>,
        trigger_policy: Option<Option<RoomTriggerPolicy>>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord>;

    /// Close a room. Returns the record as it was at close time.
    fn close(&mut self, key: &RoomKey) -> Result<RoomRecord>;

    /// Add a participant and append a `ParticipantJoined` marker. Idempotent on
    /// id (re-adding replaces). Bumps `updated_at`.
    fn add_participant(
        &mut self,
        key: &RoomKey,
        participant: RoomParticipant,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord>;

    /// Add a participant and return both the unchanged room result and the
    /// committed `ParticipantJoined` transcript row. This additive adapter lets
    /// live-tail publishers issue a wake hint only after the allocating
    /// transaction commits, without querying for a possibly-raced latest row.
    fn add_participant_with_message(
        &mut self,
        key: &RoomKey,
        participant: RoomParticipant,
        now: DateTime<Utc>,
    ) -> Result<(RoomRecord, RoomMessage)>;

    /// Remove a participant by id and append a `ParticipantLeft` marker. Fails
    /// if the participant isn't present. Bumps `updated_at`.
    fn remove_participant(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord>;

    /// Remove a participant and return both the unchanged room result and the
    /// committed `ParticipantLeft` transcript row. See
    /// [`RoomStore::add_participant_with_message`].
    fn remove_participant_with_message(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(RoomRecord, RoomMessage)>;

    /// Append a chat/system message, assigning the next room-scoped `seq`.
    fn append_message(
        &mut self,
        key: &RoomKey,
        author_id: &str,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage>;

    /// Read a room's transcript, optionally only entries with `seq > after_seq`.
    ///
    /// **Bounded (OCEAN-249).** This returns at most [`DEFAULT_TRANSCRIPT_LIMIT`]
    /// rows — it is no longer an unbounded full-table read. Callers that need to
    /// page or that want the cursor/`has_more` signal should use
    /// [`RoomStore::transcript_page`]; this method is kept as the convenience
    /// "first page with the default cap" form for the many call sites that only
    /// ever read a recent tail.
    fn transcript(&self, key: &RoomKey, after_seq: Option<u64>) -> Result<Vec<RoomMessage>>;

    /// Read one bounded page of a room's transcript (OCEAN-249).
    ///
    /// Returns entries with `seq > after_seq` (or from the start when `None`) in
    /// ascending `seq` order, at most `limit` rows. `limit` is clamped by
    /// [`clamp_transcript_limit`]: `None` ⇒ [`DEFAULT_TRANSCRIPT_LIMIT`], and any
    /// value is capped at [`MAX_TRANSCRIPT_LIMIT`]. The returned
    /// [`TranscriptPage`] carries `next_seq` (the cursor to replay as the next
    /// `after_seq`) and `has_more`. Page to the end by repeating with
    /// `after_seq = next_seq` until `has_more` is false.
    fn transcript_page(
        &self,
        key: &RoomKey,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<TranscriptPage>;

    /// The room's current trigger policy, if any.
    fn trigger_policy(&self, key: &RoomKey) -> Result<Option<RoomTriggerPolicy>>;

    // ── S2-P1 federation: access projection + outbox ──────────────────────

    /// Upsert the room's access projection (S2-P1).
    ///
    /// Stores the given state, confirmed sequence, and member list. This is the
    /// single writer path by which the daemon persists an access snapshot. Called
    /// after every successful federation RPC and on local-state transitions.
    fn upsert_access_projection(
        &mut self,
        key: &RoomKey,
        state: RoomAccessState,
        confirmed_sequence: Option<u64>,
        members: &[FederatedRoomMemberProjection],
    ) -> Result<()>;

    /// Read the room's access projection (S2-P1).
    ///
    /// Returns `None` when the room exists but no access projection has ever been
    /// upserted (fresh local room). Returns `UnknownRoom` when the room itself
    /// does not exist.
    fn read_access_projection(&self, key: &RoomKey) -> Result<Option<RoomAccessProjection>>;

    /// Append an item to the room's outbox (S2-P1).
    ///
    /// Outbox items are isolated from the transcript (`seq` column) — they are
    /// a separate table keyed by `(room_id, client_event_id)`. Fails if the room
    /// does not exist or if an item with the same `client_event_id` already exists.
    fn append_outbox_item(&mut self, key: &RoomKey, item: &RoomOutboxItem) -> Result<()>;

    /// Read outbox items for a room filtered by state (S2-P1).
    ///
    /// If `state` is `None`, returns all items regardless of state. Returns an
    /// empty vec when the room is unknown (not an error).
    fn read_outbox_items(
        &self,
        key: &RoomKey,
        state: Option<OutboxItemState>,
    ) -> Result<Vec<RoomOutboxItem>>;

    /// Retry a failed outbox item, setting its state back to `Pending` (S2-P1).
    ///
    /// Returns `UnknownOutboxItem` if the item does not exist, and
    /// `OutboxItemNotFailed` if it is not in the `Failed` state.  Only the
    /// `state` field is altered — every other column is left untouched.
    fn retry_outbox_item(&mut self, key: &RoomKey, client_event_id: &str) -> Result<()>;
}

/// SQLite-backed durable room store.
pub struct SqliteRoomStore {
    conn: Connection,
}

impl SqliteRoomStore {
    /// Open (or create) a store at `path`, running migrations idempotently.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory store (for tests). Migrations run on open.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Create the schema if it does not already exist. Safe to call repeatedly —
    /// every statement is `IF NOT EXISTS`, so re-opening an existing DB is a
    /// no-op. Also enables foreign keys so transcript/participant rows are
    /// cascade-deleted with their room.
    pub fn migrate(&mut self) -> Result<()> {
        self.conn.pragma_update(None, "foreign_keys", true)?;
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS rooms (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                trigger_policy TEXT,                -- JSON RoomTriggerPolicy, NULL = none
                workspace_root TEXT,                -- OCEAN-260 bound workspace dir, NULL = unbound
                created_at     TEXT NOT NULL,       -- RFC3339
                updated_at     TEXT NOT NULL,       -- RFC3339
                closed_at      TEXT                 -- RFC3339, NULL = open
            );

            CREATE TABLE IF NOT EXISTS participants (
                room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                id           TEXT NOT NULL,
                kind         TEXT NOT NULL,          -- RoomParticipantKind, snake_case
                display_name TEXT NOT NULL,
                position     INTEGER NOT NULL,       -- preserves roster order
                PRIMARY KEY (room_id, id)
            );

            CREATE TABLE IF NOT EXISTS messages (
                room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                seq         INTEGER NOT NULL,        -- per-room monotonic
                author_id   TEXT NOT NULL,
                author_kind TEXT NOT NULL,           -- RoomParticipantKind, snake_case
                kind        TEXT NOT NULL,           -- RoomMessageKind, snake_case
                body        TEXT NOT NULL,
                created_at  TEXT NOT NULL,           -- RFC3339
                federated   TEXT,                    -- JSON FederatedMessageMeta, NULL = local
                PRIMARY KEY (room_id, seq)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_room_seq ON messages(room_id, seq);
            CREATE INDEX IF NOT EXISTS idx_participants_room ON participants(room_id, position);

            CREATE TABLE IF NOT EXISTS room_access (
                room_id             TEXT PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
                state               TEXT NOT NULL,    -- RoomAccessState, snake_case
                confirmed_sequence  TEXT,             -- canonical decimal u64, NULL = none
                member_projection   TEXT NOT NULL     -- JSON [FederatedRoomMemberProjection]
            );

            CREATE TABLE IF NOT EXISTS outbox (
                room_id            TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                client_event_id    TEXT NOT NULL,
                source_id          TEXT NOT NULL,
                source_sequence    TEXT NOT NULL,     -- canonical decimal u64
                author_member_id   TEXT NOT NULL,
                event_type         TEXT NOT NULL,
                payload            TEXT NOT NULL,     -- JSON Value
                mention_member_ids TEXT NOT NULL,     -- JSON [String]
                state              TEXT NOT NULL,     -- OutboxItemState, snake_case
                PRIMARY KEY (room_id, client_event_id)
            );

            CREATE INDEX IF NOT EXISTS idx_outbox_room_state ON outbox(room_id, state);
            "#,
        )?;
        // Backfill columns on DBs created before they existed.
        // workspace_root (OCEAN-260) — on the `rooms` table.
        match self
            .conn
            .execute("ALTER TABLE rooms ADD COLUMN workspace_root TEXT", [])
        {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }
        // federated (S2-P1) — on the `messages` table.
        match self
            .conn
            .execute("ALTER TABLE messages ADD COLUMN federated TEXT", [])
        {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Like [`get`](Self::get) but also returns soft-closed rooms (audit view).
    pub fn get_including_closed(&self, key: &RoomKey) -> Result<Option<RoomRecord>> {
        self.load_record(key, true)
    }

    // ---- internal helpers ---------------------------------------------------

    /// Does an open room exist for this key?
    fn room_is_open(&self, key: &RoomKey) -> Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1 AND closed_at IS NULL",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// Does any room (open or closed) exist for this key? (S2-P1)
    fn room_exists(&self, key: &RoomKey) -> Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// Load a full record (room + roster + transcript). `include_closed` decides
    /// whether soft-closed rooms are visible.
    fn load_record(&self, key: &RoomKey, include_closed: bool) -> Result<Option<RoomRecord>> {
        let sql = if include_closed {
            "SELECT id, name, trigger_policy, workspace_root, created_at, updated_at FROM rooms WHERE id = ?1"
        } else {
            "SELECT id, name, trigger_policy, workspace_root, created_at, updated_at FROM rooms WHERE id = ?1 AND closed_at IS NULL"
        };
        let room = self
            .conn
            .query_row(sql, params![key.as_str()], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let policy_json: Option<String> = row.get(2)?;
                let workspace_root: Option<String> = row.get(3)?;
                let created_at: String = row.get(4)?;
                let updated_at: String = row.get(5)?;
                Ok((
                    id,
                    name,
                    policy_json,
                    workspace_root,
                    created_at,
                    updated_at,
                ))
            })
            .optional()?;
        let Some((id, name, policy_json, workspace_root, created_at, updated_at)) = room else {
            return Ok(None);
        };

        let trigger_policy = decode_policy(policy_json.as_deref())?;
        let participants = self.load_participants(key)?;
        // A record's transcript is bounded (OCEAN-249): even the audit/closed-room
        // view caps at MAX_TRANSCRIPT_LIMIT rather than hydrating an unbounded log.
        // Callers needing the full history of a very long room page via
        // `transcript_page`. This reads the first (oldest) page; the `has_more`
        // signal is available through `transcript_page` for callers that care.
        let transcript = self
            .load_transcript_page(key, None, MAX_TRANSCRIPT_LIMIT)?
            .messages;

        let room = Room {
            id: RoomKey::new(id),
            name,
            participants,
            created_at: parse_ts(&created_at)?,
            updated_at: parse_ts(&updated_at)?,
            trigger_policy,
            workspace_root,
        };
        Ok(Some(RoomRecord { room, transcript }))
    }

    fn load_participants(&self, key: &RoomKey) -> Result<Vec<RoomParticipant>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, display_name FROM participants WHERE room_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![key.as_str()], |row| {
            let id: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let display_name: String = row.get(2)?;
            Ok((id, kind, display_name))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, kind, display_name) = r?;
            out.push(RoomParticipant {
                id,
                kind: decode_participant_kind(&kind)?,
                display_name,
            });
        }
        Ok(out)
    }

    /// Read a bounded slice of the transcript and report whether more rows exist
    /// (OCEAN-249).
    ///
    /// The query carries a `LIMIT` so a long-lived room never triggers a
    /// full-table scan. To know whether a *next* page exists without a second
    /// `COUNT(*)` query, we ask SQLite for one extra row (`LIMIT limit + 1`): if
    /// that sentinel comes back we drop it, keep exactly `limit` rows, and report
    /// `has_more = true`. `effective_limit` is already clamped by the callers via
    /// [`clamp_transcript_limit`]; it is passed in (not re-clamped here) so this
    /// private helper has a single, predictable contract.
    fn load_transcript_page(
        &self,
        key: &RoomKey,
        after_seq: Option<u64>,
        effective_limit: usize,
    ) -> Result<TranscriptPage> {
        let after = after_seq.map(|s| s as i64).unwrap_or(-1);
        // Fetch one extra row as the "is there a next page?" sentinel. Guard the
        // `+ 1` against overflow on a pathological usize::MAX (clamp prevents it,
        // but stay total) and bind as i64 for SQLite.
        let fetch = effective_limit.saturating_add(1) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT seq, author_id, author_kind, kind, body, created_at, federated
             FROM messages WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![key.as_str(), after, fetch], |row| {
            let seq: i64 = row.get(0)?;
            let author_id: String = row.get(1)?;
            let author_kind: String = row.get(2)?;
            let kind: String = row.get(3)?;
            let body: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            let federated: Option<String> = row.get(6)?;
            Ok((
                seq,
                author_id,
                author_kind,
                kind,
                body,
                created_at,
                federated,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (seq, author_id, author_kind, kind, body, created_at, federated) = r?;
            let federated_meta: Option<FederatedMessageMeta> =
                match federated {
                    Some(json) => Some(serde_json::from_str(&json).map_err(|e| {
                        RoomStoreError::Encode(format!("invalid federated JSON: {e}"))
                    })?),
                    None => None,
                };
            out.push(RoomMessage {
                seq: seq as u64,
                author_id,
                author_kind: decode_participant_kind(&author_kind)?,
                kind: decode_message_kind(&kind)?,
                body,
                created_at: parse_ts(&created_at)?,
                federated: federated_meta,
            });
        }
        // If we got the sentinel row back, there is at least one more page. Drop
        // it so the page holds exactly `effective_limit` rows, then expose the
        // last *kept* row's seq as the next cursor.
        let has_more = out.len() > effective_limit;
        if has_more {
            out.truncate(effective_limit);
        }
        let next_seq = if has_more {
            out.last().map(|m| m.seq)
        } else {
            None
        };
        Ok(TranscriptPage {
            messages: out,
            next_seq,
            has_more,
        })
    }

    /// Assign the next per-room seq and insert a message in one go. Caller must
    /// ensure the room exists.
    ///
    /// Takes the connection explicitly (rather than `self.conn`) so it can run on
    /// a [`rusqlite::Transaction`], which derefs to `&Connection`. The
    /// `SELECT MAX(seq)+1` and the dependent `INSERT` are two statements that MUST
    /// run inside the same transaction as the caller's other writes — otherwise a
    /// concurrent writer can steal the seq between them and tear the row
    /// (OCEAN-201). The IMMEDIATE transaction the callers open also serializes the
    /// seq allocation across connections.
    fn insert_message_on(
        conn: &Connection,
        key: &RoomKey,
        author_id: &str,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage> {
        // MAX(seq)+1, recomputed from stored rows so it survives restarts.
        let next_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at, federated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                key.as_str(),
                next_seq,
                author_id,
                encode_participant_kind(author_kind),
                encode_message_kind(kind),
                body,
                fmt_ts(now),
            ],
        )?;
        Ok(RoomMessage {
            seq: next_seq as u64,
            author_id: author_id.to_string(),
            author_kind,
            kind,
            body: body.to_string(),
            created_at: now,
            federated: None,
        })
    }

    /// Bump `updated_at`. Takes the connection explicitly so it can run on a
    /// transaction alongside the caller's other writes.
    fn touch_on(conn: &Connection, key: &RoomKey, now: DateTime<Utc>) -> Result<()> {
        conn.execute(
            "UPDATE rooms SET updated_at = ?2 WHERE id = ?1",
            params![key.as_str(), fmt_ts(now)],
        )?;
        Ok(())
    }
}

impl RoomStore for SqliteRoomStore {
    fn create(
        &mut self,
        key: RoomKey,
        name: &str,
        trigger_policy: Option<RoomTriggerPolicy>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        // Unbound create == workspace create with no binding. Single insert path.
        self.create_in_workspace(key, name, None, trigger_policy, now)
    }

    fn create_in_workspace(
        &mut self,
        key: RoomKey,
        name: &str,
        workspace_root: Option<String>,
        trigger_policy: Option<RoomTriggerPolicy>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        if key.as_str().trim().is_empty() {
            return Err(RoomStoreError::BadKey(key.0));
        }
        // The existence check and the INSERT are a check-then-act pair. Run them in
        // an IMMEDIATE transaction so two concurrent creates of the same key can't
        // both pass the SELECT and race the INSERT — IMMEDIATE serializes the
        // writers, so the loser sees the winner's committed row and reports
        // AlreadyExists cleanly instead of a raw PK violation (OCEAN-201).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Treat an existing row (open or closed) as a collision, matching the
        // in-memory store's "key already taken".
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            return Err(RoomStoreError::AlreadyExists(key));
        }
        tx.execute(
            "INSERT INTO rooms (id, name, trigger_policy, workspace_root, created_at, updated_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL)",
            params![
                key.as_str(),
                name,
                encode_policy(trigger_policy.as_ref())?,
                workspace_root,
                fmt_ts(now),
            ],
        )?;
        tx.commit()?;
        Ok(self
            .load_record(&key, false)?
            .expect("just inserted the room"))
    }

    fn get(&self, key: &RoomKey) -> Result<Option<RoomRecord>> {
        self.load_record(key, false)
    }

    fn list(&self) -> Result<Vec<Room>> {
        // Bounded by default (OCEAN-250): delegate to the paged read with the
        // default cap and hand back just the rooms. A daemon with thousands of
        // rows no longer serializes all of them on every poll.
        Ok(self.list_page(None, None)?.rooms)
    }

    fn list_page(&self, after: Option<&str>, limit: Option<usize>) -> Result<RoomPage> {
        let effective_limit = clamp_list_limit(limit);
        // Keyset pagination over the stable `updated_at DESC, id ASC` order. The
        // cursor is just the last returned room key; we resolve its `updated_at`
        // (an indexed point lookup) so the WHERE clause can express "comes strictly
        // after the cursor in this ordering" without an OFFSET (which would still
        // scan all skipped rows). A cursor key that no longer exists (room closed
        // since) yields no anchor row, so we fall back to the unanchored first page
        // rather than 404 — paging stays resilient to a stale cursor.
        let anchor: Option<(String, String)> = match after {
            Some(k) => self
                .conn
                .query_row(
                    "SELECT updated_at, id FROM rooms WHERE id = ?1 AND closed_at IS NULL",
                    params![k],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?,
            None => None,
        };

        // Fetch one extra row as the "is there a next page?" sentinel, then drop it.
        let fetch = effective_limit.saturating_add(1) as i64;
        let keys: Vec<String> = match &anchor {
            // Strictly-after predicate for `updated_at DESC, id ASC`:
            //   updated_at < u_c  OR  (updated_at = u_c AND id > id_c)
            Some((u_c, id_c)) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM rooms
                     WHERE closed_at IS NULL
                       AND (updated_at < ?1 OR (updated_at = ?1 AND id > ?2))
                     ORDER BY updated_at DESC, id ASC
                     LIMIT ?3",
                )?;
                let keys = stmt
                    .query_map(params![u_c, id_c, fetch], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<_, _>>()?;
                keys
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM rooms WHERE closed_at IS NULL
                     ORDER BY updated_at DESC, id ASC LIMIT ?1",
                )?;
                let keys = stmt
                    .query_map(params![fetch], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<_, _>>()?;
                keys
            }
        };

        // If we got the sentinel back, there is at least one more page. Drop it so
        // the page holds exactly `effective_limit` keys.
        let has_more = keys.len() > effective_limit;
        let kept = if has_more {
            &keys[..effective_limit]
        } else {
            &keys[..]
        };
        let next_cursor = if has_more { kept.last().cloned() } else { None };

        let mut rooms = Vec::with_capacity(kept.len());
        for k in kept {
            let key = RoomKey::new(k.clone());
            if let Some(rec) = self.load_record(&key, false)? {
                rooms.push(rec.room);
            }
        }
        Ok(RoomPage {
            rooms,
            next_cursor,
            has_more,
        })
    }

    fn update(
        &mut self,
        key: &RoomKey,
        name: Option<String>,
        trigger_policy: Option<Option<RoomTriggerPolicy>>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        // name/policy/touch are separate UPDATEs to the same room row; wrap them so
        // a partial failure can't leave the row half-updated (e.g. new name but
        // stale policy) (OCEAN-201).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(name) = name {
            tx.execute(
                "UPDATE rooms SET name = ?2 WHERE id = ?1",
                params![key.as_str(), name],
            )?;
        }
        if let Some(policy) = trigger_policy {
            tx.execute(
                "UPDATE rooms SET trigger_policy = ?2 WHERE id = ?1",
                params![key.as_str(), encode_policy(policy.as_ref())?],
            )?;
        }
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        Ok(self.load_record(key, false)?.expect("room exists"))
    }

    fn close(&mut self, key: &RoomKey) -> Result<RoomRecord> {
        // Snapshot the open record first (so the return value matches the
        // pre-close state), then soft-close.
        let record = self
            .load_record(key, false)?
            .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))?;
        self.conn.execute(
            "UPDATE rooms SET closed_at = ?2 WHERE id = ?1",
            params![key.as_str(), fmt_ts(Utc::now())],
        )?;
        Ok(record)
    }

    fn add_participant(
        &mut self,
        key: &RoomKey,
        participant: RoomParticipant,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        self.add_participant_with_message(key, participant, now)
            .map(|(record, _)| record)
    }

    fn add_participant_with_message(
        &mut self,
        key: &RoomKey,
        participant: RoomParticipant,
        now: DateTime<Utc>,
    ) -> Result<(RoomRecord, RoomMessage)> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        // All four statements (delete-existing, MAX(position)+1, insert
        // participant, insert join marker via insert_message_on) are dependent and
        // MUST be atomic. IMMEDIATE takes the write lock at BEGIN so a concurrent
        // writer on the same DB file can't steal the seq between the
        // SELECT MAX(seq)+1 and the message INSERT (the torn-row bug, OCEAN-201).
        // Any `?` failure drops `tx` → rollback → no orphan participant row.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Idempotent on id: replace any existing entry, appending at the end of
        // the roster ordering (MAX(position)+1) to mirror the Vec push.
        tx.execute(
            "DELETE FROM participants WHERE room_id = ?1 AND id = ?2",
            params![key.as_str(), participant.id],
        )?;
        let next_pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM participants WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO participants (room_id, id, kind, display_name, position)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.as_str(),
                participant.id,
                encode_participant_kind(participant.kind),
                participant.display_name,
                next_pos,
            ],
        )?;
        let message = Self::insert_message_on(
            &tx,
            key,
            &participant.id,
            participant.kind,
            RoomMessageKind::ParticipantJoined,
            &format!("{} joined", participant.display_name),
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        let record = self.load_record(key, false)?.expect("room exists");
        Ok((record, message))
    }

    fn remove_participant(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
        self.remove_participant_with_message(key, participant_id, now)
            .map(|(record, _)| record)
    }

    fn remove_participant_with_message(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(RoomRecord, RoomMessage)> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let found: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT kind, display_name FROM participants WHERE room_id = ?1 AND id = ?2",
                params![key.as_str(), participant_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((kind, display_name)) = found else {
            return Err(RoomStoreError::UnknownParticipant {
                room: key.clone(),
                participant: participant_id.to_string(),
            });
        };
        // Delete + join-marker insert (which itself does SELECT MAX(seq)+1 then
        // INSERT) are dependent and must be atomic. IMMEDIATE serializes seq
        // allocation across connections; any `?` failure rolls the whole thing
        // back, so we never leave a removed-roster row without its ParticipantLeft
        // marker — or vice versa (OCEAN-201).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM participants WHERE room_id = ?1 AND id = ?2",
            params![key.as_str(), participant_id],
        )?;
        let message = Self::insert_message_on(
            &tx,
            key,
            participant_id,
            decode_participant_kind(&kind)?,
            RoomMessageKind::ParticipantLeft,
            &format!("{display_name} left"),
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        let record = self.load_record(key, false)?.expect("room exists");
        Ok((record, message))
    }

    fn append_message(
        &mut self,
        key: &RoomKey,
        author_id: &str,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        // SELECT MAX(seq)+1, the message INSERT, and the updated_at touch are
        // dependent statements. Wrap them in an IMMEDIATE transaction so a
        // concurrent writer can't interleave a commit at the same seq and tear the
        // transcript (OCEAN-201). On a PK collision the `?` rolls the whole thing
        // back rather than leaving a half-written row.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let msg = Self::insert_message_on(&tx, key, author_id, author_kind, kind, body, now)?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        Ok(msg)
    }

    fn transcript(&self, key: &RoomKey, after_seq: Option<u64>) -> Result<Vec<RoomMessage>> {
        // Bounded by default (OCEAN-249): delegate to the paged read with the
        // default cap and hand back just the rows. Same open-room precondition as
        // before — a closed room is still `UnknownRoom` here (the audit fallback
        // lives in the daemon's handler).
        Ok(self.transcript_page(key, after_seq, None)?.messages)
    }

    fn transcript_page(
        &self,
        key: &RoomKey,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<TranscriptPage> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let effective_limit = clamp_transcript_limit(limit);
        self.load_transcript_page(key, after_seq, effective_limit)
    }

    fn trigger_policy(&self, key: &RoomKey) -> Result<Option<RoomTriggerPolicy>> {
        let policy_json: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT trigger_policy FROM rooms WHERE id = ?1 AND closed_at IS NULL",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        match policy_json {
            Some(json) => decode_policy(json.as_deref()),
            None => Ok(None),
        }
    }

    fn upsert_access_projection(
        &mut self,
        key: &RoomKey,
        state: RoomAccessState,
        confirmed_sequence: Option<u64>,
        members: &[FederatedRoomMemberProjection],
    ) -> Result<()> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let state_str = serde_json::to_string(&state)
            .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
        let state_str = state_str.trim_matches('"'); // "local" → local
        let seq_text = confirmed_sequence.map(write_u64_text);
        let member_json = serde_json::to_string(members)
            .map_err(|e| RoomStoreError::Encode(format!("members serialize: {e}")))?;
        self.conn.execute(
            "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
               state = excluded.state,
               confirmed_sequence = excluded.confirmed_sequence,
               member_projection = excluded.member_projection",
            params![key.as_str(), state_str, seq_text, member_json],
        )?;
        Ok(())
    }

    fn read_access_projection(&self, key: &RoomKey) -> Result<Option<RoomAccessProjection>> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let row = self
            .conn
            .query_row(
                "SELECT state, confirmed_sequence, member_projection
                 FROM room_access WHERE room_id = ?1",
                params![key.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((state_str, seq_text, member_json)) = row else {
            return Ok(None);
        };
        let state: RoomAccessState =
            serde_json::from_value(serde_json::Value::String(state_str))
                .map_err(|e| RoomStoreError::Encode(format!("bad access state: {e}")))?;
        let confirmed_sequence: Option<u64> = match seq_text {
            Some(t) => Some(read_u64_text(Some(t))?),
            None => None,
        };
        let members: Vec<FederatedRoomMemberProjection> = serde_json::from_str(&member_json)
            .map_err(|e| RoomStoreError::Encode(format!("bad member projection: {e}")))?;
        Ok(Some(RoomAccessProjection {
            state,
            last_confirmed_global_sequence: confirmed_sequence,
            members,
            outbox: Vec::new(),
        }))
    }

    fn append_outbox_item(&mut self, key: &RoomKey, item: &RoomOutboxItem) -> Result<()> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let state_str = serde_json::to_string(&item.state)
            .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
        let state_str = state_str.trim_matches('"');
        let payload_json = item.payload.to_string();
        let mentions_json = serde_json::to_string(&item.mention_member_ids)
            .map_err(|e| RoomStoreError::Encode(format!("mentions serialize: {e}")))?;
        let src_seq = write_u64_text(item.source_sequence);
        self.conn.execute(
            "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                 author_member_id, event_type, payload, mention_member_ids, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                key.as_str(),
                item.client_event_id,
                item.source_id,
                src_seq,
                item.author_member_id,
                item.event_type,
                payload_json,
                mentions_json,
                state_str,
            ],
        )?;
        Ok(())
    }

    fn read_outbox_items(
        &self,
        key: &RoomKey,
        state: Option<OutboxItemState>,
    ) -> Result<Vec<RoomOutboxItem>> {
        // Silently return empty when room is unknown (not an error per spec).
        if !self.room_exists(key)? {
            return Ok(Vec::new());
        }
        let (where_clause, param) = match state {
            Some(ref s) => {
                let s_str = serde_json::to_string(s)
                    .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
                let s_str = s_str.trim_matches('"').to_string();
                ("WHERE room_id = ?1 AND state = ?2".to_string(), Some(s_str))
            }
            None => ("WHERE room_id = ?1".to_string(), None),
        };
        let sql = format!(
            "SELECT client_event_id, source_id, source_sequence, author_member_id,
                    event_type, payload, mention_member_ids, state
             FROM outbox {} ORDER BY rowid",
            where_clause,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<_> = if let Some(ref s_str) = param {
            stmt.query_map(params![key.as_str(), s_str], |row| {
                let client_event_id: String = row.get(0)?;
                let source_id: String = row.get(1)?;
                let source_sequence: String = row.get(2)?;
                let author_member_id: String = row.get(3)?;
                let event_type: String = row.get(4)?;
                let payload: String = row.get(5)?;
                let mention_member_ids: String = row.get(6)?;
                let state: String = row.get(7)?;
                Ok((
                    client_event_id,
                    source_id,
                    source_sequence,
                    author_member_id,
                    event_type,
                    payload,
                    mention_member_ids,
                    state,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![key.as_str()], |row| {
                let client_event_id: String = row.get(0)?;
                let source_id: String = row.get(1)?;
                let source_sequence: String = row.get(2)?;
                let author_member_id: String = row.get(3)?;
                let event_type: String = row.get(4)?;
                let payload: String = row.get(5)?;
                let mention_member_ids: String = row.get(6)?;
                let state: String = row.get(7)?;
                Ok((
                    client_event_id,
                    source_id,
                    source_sequence,
                    author_member_id,
                    event_type,
                    payload,
                    mention_member_ids,
                    state,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut out = Vec::new();
        for (
            client_event_id,
            source_id,
            source_sequence,
            author_member_id,
            event_type,
            payload,
            mention_member_ids,
            state,
        ) in rows
        {
            let source_sequence = read_u64_text(Some(source_sequence))?;
            let payload: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|e| RoomStoreError::Encode(format!("bad payload JSON: {e}")))?;
            let mention_member_ids: Vec<String> = serde_json::from_str(&mention_member_ids)
                .map_err(|e| RoomStoreError::Encode(format!("bad mentions JSON: {e}")))?;
            let state_str = format!("\"{state}\"");
            let state: OutboxItemState = serde_json::from_str(&state_str)
                .map_err(|e| RoomStoreError::Encode(format!("bad outbox state: {e}")))?;
            out.push(RoomOutboxItem {
                client_event_id,
                source_id,
                source_sequence,
                author_member_id,
                event_type,
                payload,
                mention_member_ids,
                state,
            });
        }
        Ok(out)
    }

    fn retry_outbox_item(&mut self, key: &RoomKey, client_event_id: &str) -> Result<()> {
        if !self.room_exists(key)? {
            return Err(RoomStoreError::UnknownOutboxItem {
                room: key.clone(),
                client_event_id: client_event_id.to_string(),
            });
        }
        // Read current state — if it doesn't exist or isn't Failed, error.
        let current: Option<(String,)> = self
            .conn
            .query_row(
                "SELECT state FROM outbox WHERE room_id = ?1 AND client_event_id = ?2",
                params![key.as_str(), client_event_id],
                |r| Ok((r.get::<_, String>(0)?,)),
            )
            .optional()?;
        let Some((state_str,)) = current else {
            return Err(RoomStoreError::UnknownOutboxItem {
                room: key.clone(),
                client_event_id: client_event_id.to_string(),
            });
        };
        if state_str != "failed" {
            return Err(RoomStoreError::OutboxItemNotFailed {
                room: key.clone(),
                client_event_id: client_event_id.to_string(),
                current_state: state_str,
            });
        }
        // Only mutate the state column — every other field is preserved.
        self.conn.execute(
            "UPDATE outbox SET state = 'pending' WHERE room_id = ?1 AND client_event_id = ?2",
            params![key.as_str(), client_event_id],
        )?;
        Ok(())
    }
}

// ---- (de)serialization helpers ---------------------------------------------
//
// `RoomTriggerPolicy` derives serde, so it is stored as JSON. The two small
// enums have a fixed, stable wire form (snake_case, matching their serde attrs)
// so they are stored as plain strings rather than dragging serde_json into
// every column. Timestamps use RFC3339.

fn encode_policy(policy: Option<&RoomTriggerPolicy>) -> Result<Option<String>> {
    match policy {
        None => Ok(None),
        Some(p) => serialize_policy(p).map(Some),
    }
}

fn serialize_policy(p: &RoomTriggerPolicy) -> Result<String> {
    // Hand-rolled JSON: avoids a serde_json dependency for one small, fixed
    // struct. Fields mirror the serde contract in ocean-core.
    let mut parts = vec![
        format!("\"on_mention\":{}", p.on_mention),
        format!("\"on_thread_reply\":{}", p.on_thread_reply),
        format!("\"on_component_event\":{}", p.on_component_event),
    ];
    if let Some(cron) = &p.on_schedule {
        parts.push(format!("\"on_schedule\":{}", json_string(cron)));
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

fn decode_policy(json: Option<&str>) -> Result<Option<RoomTriggerPolicy>> {
    let Some(json) = json else { return Ok(None) };
    Ok(Some(parse_policy(json)?))
}

/// Minimal flat-object JSON parser for the four `RoomTriggerPolicy` fields. The
/// only writer of this column is [`serialize_policy`], so the input shape is
/// known: a flat object of booleans plus an optional string. Kept deliberately
/// small rather than pulling in serde_json.
fn parse_policy(json: &str) -> Result<RoomTriggerPolicy> {
    let mut policy = RoomTriggerPolicy::default();
    let body = json.trim();
    let body = body
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| RoomStoreError::Encode(format!("not a JSON object: {json}")))?;
    for field in split_top_level(body) {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (raw_key, raw_val) = field
            .split_once(':')
            .ok_or_else(|| RoomStoreError::Encode(format!("bad field: {field}")))?;
        let k = raw_key.trim().trim_matches('"');
        let v = raw_val.trim();
        match k {
            "on_mention" => policy.on_mention = v == "true",
            "on_thread_reply" => policy.on_thread_reply = v == "true",
            "on_component_event" => policy.on_component_event = v == "true",
            "on_schedule" => policy.on_schedule = Some(unquote(v)?),
            _ => {} // forward-compat: ignore unknown fields
        }
    }
    Ok(policy)
}

/// Split a JSON object body on top-level commas (commas not inside a string).
fn split_top_level(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in body.chars() {
        if in_str {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            cur.push(c);
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(v: &str) -> Result<String> {
    let v = v.trim();
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| RoomStoreError::Encode(format!("expected JSON string: {v}")))?;
    // Unescape the small set our writer emits.
    Ok(inner
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
        .replace("\\n", "\n"))
}

fn json_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn encode_participant_kind(k: RoomParticipantKind) -> &'static str {
    match k {
        RoomParticipantKind::Human => "human",
        RoomParticipantKind::Agent => "agent",
        RoomParticipantKind::Bot => "bot",
        RoomParticipantKind::Tool => "tool",
        RoomParticipantKind::System => "system",
    }
}

fn decode_participant_kind(s: &str) -> Result<RoomParticipantKind> {
    Ok(match s {
        "human" => RoomParticipantKind::Human,
        "agent" => RoomParticipantKind::Agent,
        "bot" => RoomParticipantKind::Bot,
        "tool" => RoomParticipantKind::Tool,
        "system" => RoomParticipantKind::System,
        other => {
            return Err(RoomStoreError::Encode(format!(
                "unknown participant kind: {other}"
            )))
        }
    })
}

fn encode_message_kind(k: RoomMessageKind) -> &'static str {
    match k {
        RoomMessageKind::Message => "message",
        RoomMessageKind::ParticipantJoined => "participant_joined",
        RoomMessageKind::ParticipantLeft => "participant_left",
        RoomMessageKind::System => "system",
    }
}

fn decode_message_kind(s: &str) -> Result<RoomMessageKind> {
    Ok(match s {
        "message" => RoomMessageKind::Message,
        "participant_joined" => RoomMessageKind::ParticipantJoined,
        "participant_left" => RoomMessageKind::ParticipantLeft,
        "system" => RoomMessageKind::System,
        other => {
            return Err(RoomStoreError::Encode(format!(
                "unknown message kind: {other}"
            )))
        }
    })
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| RoomStoreError::Encode(format!("bad timestamp '{s}': {e}")))
}

// ── canonical decimal u64 sequence helpers (S2-P1) ────────────────────────

/// Read a `u64` from its canonical decimal TEXT column.
///
/// Federation sequence numbers are stored as TEXT so they never lose precision
/// across producers (SQLite integers are signed i64). Returns `0` for a NULL
/// column.  Rejects non-decimal text including hex, leading zeros (`"01"`),
/// empty strings, and overflow (`">18446744073709551615"`).
fn read_u64_text(s: Option<String>) -> Result<u64> {
    let raw = match s {
        Some(v) => v,
        None => return Ok(0_u64),
    };
    if raw.is_empty() || raw.as_bytes()[0] == b'0' && raw.len() > 1 || raw.as_bytes()[0] == b'-' {
        return Err(RoomStoreError::Encode(format!("invalid u64 text: '{raw}'")));
    }
    let v: u128 = raw
        .parse()
        .map_err(|_| RoomStoreError::Encode(format!("invalid u64 decimal: '{raw}'")))?;
    if v > u64::MAX as u128 {
        return Err(RoomStoreError::Encode(format!("u64 overflow: '{raw}'")));
    }
    Ok(v as u64)
}

/// Write a `u64` to its canonical decimal TEXT form.
fn write_u64_text(v: u64) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::{evaluate_trigger_policy, RoomTriggerEvent};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn human(id: &str, name: &str) -> RoomParticipant {
        RoomParticipant {
            id: id.into(),
            kind: RoomParticipantKind::Human,
            display_name: name.into(),
        }
    }

    fn store() -> SqliteRoomStore {
        SqliteRoomStore::open_in_memory().unwrap()
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut s = store();
        // Re-running migrate over an existing schema must not error and must not
        // drop data.
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap();
        assert!(s.get(&key).unwrap().is_some());
    }

    #[test]
    fn open_existing_db_runs_migrations_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("persisted");
        {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            s.create(key.clone(), "Persisted", None, now()).unwrap();
            s.append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "hello",
                now(),
            )
            .unwrap();
        }
        // Re-open: migrate() runs again on an existing DB, data survives.
        let s = SqliteRoomStore::open(&path).unwrap();
        let rec = s.get(&key).unwrap().unwrap();
        assert_eq!(rec.room.name, "Persisted");
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].body, "hello");
    }

    #[test]
    fn create_in_workspace_persists_and_returns_binding() {
        // OCEAN-260: a room created WITH a workspace_root carries it on the
        // returned record and on subsequent reads.
        let mut s = store();
        let key = RoomKey::new("bound-room");
        let rec = s
            .create_in_workspace(
                key.clone(),
                "Bound",
                Some("/dev/ocean-os".into()),
                None,
                now(),
            )
            .unwrap();
        assert_eq!(rec.room.workspace_root.as_deref(), Some("/dev/ocean-os"));
        // And a fresh read sees the same binding.
        let got = s.get(&key).unwrap().unwrap();
        assert_eq!(got.room.workspace_root.as_deref(), Some("/dev/ocean-os"));
    }

    #[test]
    fn plain_create_leaves_workspace_unbound() {
        // OCEAN-260 backward-compat: the legacy `create` path binds no workspace,
        // so existing room creators keep their None semantics unchanged.
        let mut s = store();
        let key = RoomKey::new("unbound-room");
        let rec = s.create(key.clone(), "Unbound", None, now()).unwrap();
        assert_eq!(rec.room.workspace_root, None);
        assert_eq!(s.get(&key).unwrap().unwrap().room.workspace_root, None);
    }

    #[test]
    fn workspace_binding_survives_reopen() {
        // The binding is durable: it survives dropping and re-opening the store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("durable-bound");
        {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            s.create_in_workspace(
                key.clone(),
                "Durable",
                Some("/work/repo".into()),
                None,
                now(),
            )
            .unwrap();
        }
        let s = SqliteRoomStore::open(&path).unwrap();
        assert_eq!(
            s.get(&key).unwrap().unwrap().room.workspace_root.as_deref(),
            Some("/work/repo")
        );
    }

    #[test]
    fn migrate_backfills_workspace_root_on_preexisting_db() {
        // OCEAN-260 migration: a DB whose `rooms` table predates the
        // workspace_root column must gain the column on the next open, with old
        // rows reading back as unbound (None) — not a hard error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-rooms.db");
        let key = RoomKey::new("legacy");
        {
            // Build the OLD schema by hand (no workspace_root column) and seed a
            // room the pre-OCEAN-260 way.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE rooms (
                    id             TEXT PRIMARY KEY,
                    name           TEXT NOT NULL,
                    trigger_policy TEXT,
                    created_at     TEXT NOT NULL,
                    updated_at     TEXT NOT NULL,
                    closed_at      TEXT
                );
                "#,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO rooms (id, name, trigger_policy, created_at, updated_at, closed_at)
                 VALUES (?1, ?2, NULL, ?3, ?3, NULL)",
                params![key.as_str(), "Legacy", fmt_ts(now())],
            )
            .unwrap();
        }
        // Opening with the current store runs migrate(), which ALTERs in the new
        // column. The legacy room reads back cleanly as unbound.
        let s = SqliteRoomStore::open(&path).unwrap();
        let rec = s.get(&key).unwrap().unwrap();
        assert_eq!(rec.room.name, "Legacy");
        assert_eq!(rec.room.workspace_root, None);
        // Re-opening again must be a no-op (ADD COLUMN swallowed as duplicate).
        let _ = SqliteRoomStore::open(&path).unwrap();
    }

    #[test]
    fn create_get_round_trip() {
        let mut s = store();
        let key = RoomKey::new("map-fix");
        let policy = RoomTriggerPolicy {
            on_mention: true,
            on_schedule: Some("0 9 * * *".into()),
            ..Default::default()
        };
        let created = s
            .create(key.clone(), "Map Fix", Some(policy.clone()), now())
            .unwrap();
        assert_eq!(created.room.name, "Map Fix");
        assert!(created.transcript.is_empty());
        assert_eq!(created.room.created_at, created.room.updated_at);

        let got = s.get(&key).unwrap().unwrap();
        assert_eq!(got.room.id, key);
        assert_eq!(got.room.name, "Map Fix");
        assert_eq!(got.room.trigger_policy, Some(policy));

        // Duplicate create fails.
        assert!(matches!(
            s.create(key.clone(), "Dup", None, now()),
            Err(RoomStoreError::AlreadyExists(_))
        ));

        // Empty key rejected.
        assert!(matches!(
            s.create(RoomKey::new("   "), "x", None, now()),
            Err(RoomStoreError::BadKey(_))
        ));
    }

    #[test]
    fn list_orders_by_updated_then_key() {
        let mut s = store();
        let t0 = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let t1 = "2026-01-02T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        s.create(RoomKey::new("docs"), "Docs", None, t0).unwrap();
        s.create(RoomKey::new("map-fix"), "Map", None, t1).unwrap();
        let list = s.list().unwrap();
        assert_eq!(list.len(), 2);
        // Most recently updated first.
        assert_eq!(list[0].id, RoomKey::new("map-fix"));
        assert_eq!(list[1].id, RoomKey::new("docs"));
    }

    /// Create `n` open rooms with strictly-increasing `updated_at` so the
    /// newest-first list order (`updated_at DESC`) is deterministic: keys are
    /// `room-000`, `room-001`, … and the list returns them in REVERSE (newest
    /// created first). Returns the store. (OCEAN-250)
    fn store_with_rooms(n: usize) -> SqliteRoomStore {
        let mut s = store();
        let base = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        for i in 0..n {
            // i seconds past base → later i sorts earlier in the DESC list.
            let ts = base + chrono::Duration::seconds(i as i64);
            s.create(RoomKey::new(format!("room-{i:03}")), "R", None, ts)
                .unwrap();
        }
        s
    }

    /// The newest-first list order as room-key strings, for asserting paging
    /// reconstructs exactly the full order.
    fn expected_room_order(n: usize) -> Vec<String> {
        (0..n).rev().map(|i| format!("room-{i:03}")).collect()
    }

    #[test]
    fn list_page_caps_rows_and_returns_cursor() {
        // 10 rooms, ask for a page of 4.
        let s = store_with_rooms(10);
        let page = s.list_page(None, Some(4)).unwrap();
        assert_eq!(page.rooms.len(), 4, "page is capped at the limit");
        // Newest-first: room-009 … room-006.
        assert_eq!(page.rooms[0].id, RoomKey::new("room-009"));
        assert_eq!(page.rooms[3].id, RoomKey::new("room-006"));
        assert!(page.has_more, "6 rooms remain, so has_more is true");
        // Cursor is the last returned key, to be replayed as the next `after`.
        assert_eq!(page.next_cursor.as_deref(), Some("room-006"));
    }

    #[test]
    fn list_page_paging_with_cursor_retrieves_all_rooms() {
        // Walk the whole list in pages of 3 using the returned cursor; assert we
        // see every room exactly once, in the store's order, no gaps/dupes.
        let total = 17;
        let s = store_with_rooms(total);

        let mut collected: Vec<String> = Vec::new();
        let mut after: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = s.list_page(after.as_deref(), Some(3)).unwrap();
            pages += 1;
            assert!(pages <= total + 2, "paging must terminate");
            for r in &page.rooms {
                collected.push(r.id.as_str().to_string());
            }
            if page.has_more {
                after = Some(page.next_cursor.clone().expect("has_more implies a cursor"));
            } else {
                assert_eq!(page.next_cursor, None, "final page has no cursor");
                break;
            }
        }
        assert_eq!(
            collected,
            expected_room_order(total),
            "every room retrieved once, in list order"
        );
    }

    #[test]
    fn list_page_last_page_has_no_cursor() {
        // Exactly `limit` rooms total: the single full page must NOT claim
        // has_more (the +1 sentinel row simply doesn't exist).
        let s = store_with_rooms(5);
        let page = s.list_page(None, Some(5)).unwrap();
        assert_eq!(page.rooms.len(), 5);
        assert!(!page.has_more, "a full final page is not 'has_more'");
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn list_default_cap_applies_when_no_limit_given() {
        // More rooms than the default cap: both the convenience `list()` and
        // `list_page(.., None)` must bound at DEFAULT_LIST_LIMIT, NOT return
        // everything (the OCEAN-250 regression guard).
        let over = DEFAULT_LIST_LIMIT + 15;
        let s = store_with_rooms(over);

        let rooms = s.list().unwrap();
        assert_eq!(
            rooms.len(),
            DEFAULT_LIST_LIMIT,
            "list() is bounded by the default cap, not unbounded"
        );

        let page = s.list_page(None, None).unwrap();
        assert_eq!(page.rooms.len(), DEFAULT_LIST_LIMIT);
        assert!(page.has_more, "rooms beyond the cap mean more pages");
    }

    #[test]
    fn list_page_limit_is_clamped_to_max() {
        // An absurd caller limit is clamped to MAX_LIST_LIMIT. With fewer rooms
        // than the cap we still get them all and has_more is false; the point is
        // the request can't be coerced into an unbounded scan.
        let s = store_with_rooms(3);
        let page = s.list_page(None, Some(usize::MAX)).unwrap();
        assert_eq!(page.rooms.len(), 3);
        assert!(!page.has_more);
        assert_eq!(clamp_list_limit(Some(usize::MAX)), MAX_LIST_LIMIT);
        assert_eq!(clamp_list_limit(None), DEFAULT_LIST_LIMIT);
        // A 0 limit floors to 1 so it can never report an empty-yet-has_more page.
        assert_eq!(clamp_list_limit(Some(0)), 1);
    }

    #[test]
    fn list_page_stale_cursor_falls_back_to_first_page() {
        // A cursor key that isn't an open room (closed/never existed) must not
        // 404 or panic — paging resumes from the top (resilient to a stale or
        // since-closed cursor).
        let s = store_with_rooms(4);
        let page = s.list_page(Some("no-such-room"), Some(2)).unwrap();
        assert_eq!(page.rooms.len(), 2);
        assert_eq!(page.rooms[0].id, RoomKey::new("room-003"));
        assert!(page.has_more);
    }

    #[test]
    fn list_page_excludes_closed_rooms() {
        // Closed rooms never appear in the page (the list is open-rooms-only).
        let mut s = store_with_rooms(3); // room-000..room-002
        s.close(&RoomKey::new("room-001")).unwrap();
        let page = s.list_page(None, Some(10)).unwrap();
        let ids: Vec<String> = page
            .rooms
            .iter()
            .map(|r| r.id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["room-002".to_string(), "room-000".to_string()]);
        assert!(!page.has_more);
    }

    #[test]
    fn participant_join_leave_writes_transcript_markers() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        let rec = s
            .add_participant(&key, human("john", "John"), now())
            .unwrap();
        assert_eq!(rec.room.participants.len(), 1);
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].seq, 0);
        assert_eq!(rec.transcript[0].kind, RoomMessageKind::ParticipantJoined);
        assert_eq!(rec.transcript[0].body, "John joined");

        // Re-adding same id does not duplicate the roster entry.
        s.add_participant(&key, human("john", "John"), now())
            .unwrap();
        assert_eq!(s.get(&key).unwrap().unwrap().room.participants.len(), 1);

        let rec = s.remove_participant(&key, "john", now()).unwrap();
        assert!(rec.room.participants.is_empty());
        let last = rec.transcript.last().unwrap();
        assert_eq!(last.kind, RoomMessageKind::ParticipantLeft);
        assert_eq!(last.body, "John left");

        // Removing a non-member fails.
        assert!(matches!(
            s.remove_participant(&key, "ghost", now()),
            Err(RoomStoreError::UnknownParticipant { .. })
        ));
    }

    #[test]
    fn participant_mutation_adapters_return_committed_marker_rows() {
        let mut s = store();
        let key = RoomKey::new("marker-return");
        s.create(key.clone(), "Marker Return", None, now()).unwrap();

        let (joined_room, joined) = s
            .add_participant_with_message(&key, human("john", "John"), now())
            .unwrap();
        assert_eq!(joined.kind, RoomMessageKind::ParticipantJoined);
        assert_eq!(joined.seq, 0);
        assert_eq!(joined_room.transcript, vec![joined.clone()]);

        let (left_room, left) = s
            .remove_participant_with_message(&key, "john", now())
            .unwrap();
        assert_eq!(left.kind, RoomMessageKind::ParticipantLeft);
        assert_eq!(left.seq, 1);
        assert_eq!(left_room.transcript.last(), Some(&left));
        assert_eq!(s.transcript(&key, Some(0)).unwrap(), vec![left]);
    }

    #[test]
    fn append_message_and_transcript_after_seq_tailing() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        let m0 = s
            .append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "hello",
                now(),
            )
            .unwrap();
        let m1 = s
            .append_message(
                &key,
                "ocean",
                RoomParticipantKind::Agent,
                RoomMessageKind::Message,
                "on it",
                now(),
            )
            .unwrap();
        assert_eq!(m0.seq, 0);
        assert_eq!(m1.seq, 1);

        let all = s.transcript(&key, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].body, "hello");
        assert_eq!(all[1].author_kind, RoomParticipantKind::Agent);

        // after_seq tail returns only later entries.
        let tail = s.transcript(&key, Some(0)).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 1);
        assert_eq!(tail[0].body, "on it");

        // Tailing past the end is empty, not an error.
        assert!(s.transcript(&key, Some(99)).unwrap().is_empty());
    }

    /// Append `n` chat messages and return the store + key. Bodies are `msg-{i}`
    /// so a test can assert exact ordering and contents.
    fn store_with_messages(n: usize) -> (SqliteRoomStore, RoomKey) {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        for i in 0..n {
            s.append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                &format!("msg-{i}"),
                now(),
            )
            .unwrap();
        }
        (s, key)
    }

    #[test]
    fn transcript_page_caps_rows_and_returns_cursor() {
        // 10 messages (seq 0..=9), ask for a page of 4.
        let (s, key) = store_with_messages(10);
        let page = s.transcript_page(&key, None, Some(4)).unwrap();
        assert_eq!(page.messages.len(), 4, "page is capped at the limit");
        assert_eq!(page.messages[0].seq, 0);
        assert_eq!(page.messages[3].seq, 3);
        assert!(page.has_more, "6 rows remain, so has_more is true");
        // Cursor is the last *returned* seq, to be replayed as the next after_seq.
        assert_eq!(page.next_seq, Some(3));
    }

    #[test]
    fn transcript_page_paging_with_cursor_retrieves_all_rows() {
        // Walk the whole transcript in pages of 3 using the returned cursor and
        // assert we see every seq exactly once, in order, with no gaps/dupes.
        let total = 17;
        let (s, key) = store_with_messages(total);

        let mut collected: Vec<u64> = Vec::new();
        let mut after: Option<u64> = None;
        let mut pages = 0;
        loop {
            let page = s.transcript_page(&key, after, Some(3)).unwrap();
            pages += 1;
            assert!(pages <= total + 2, "paging must terminate");
            for m in &page.messages {
                collected.push(m.seq);
            }
            if page.has_more {
                // has_more ⇒ a usable cursor.
                after = Some(page.next_seq.expect("has_more implies a cursor"));
            } else {
                assert_eq!(page.next_seq, None, "final page has no cursor");
                break;
            }
        }
        let expected: Vec<u64> = (0..total as u64).collect();
        assert_eq!(
            collected, expected,
            "every row retrieved once, in seq order"
        );
    }

    #[test]
    fn transcript_page_last_page_has_no_cursor() {
        // Exactly `limit` rows total: the single page must NOT claim has_more even
        // though it is full (the +1 sentinel row simply doesn't exist).
        let (s, key) = store_with_messages(5);
        let page = s.transcript_page(&key, None, Some(5)).unwrap();
        assert_eq!(page.messages.len(), 5);
        assert!(!page.has_more, "a full final page is not 'has_more'");
        assert_eq!(page.next_seq, None);
    }

    #[test]
    fn transcript_default_cap_applies_when_no_limit_given() {
        // More rows than the default cap: both the convenience `transcript()` and
        // `transcript_page(.., None)` must bound at DEFAULT_TRANSCRIPT_LIMIT, NOT
        // return everything (the OCEAN-249 regression guard).
        let over = DEFAULT_TRANSCRIPT_LIMIT + 25;
        let (s, key) = store_with_messages(over);

        let rows = s.transcript(&key, None).unwrap();
        assert_eq!(
            rows.len(),
            DEFAULT_TRANSCRIPT_LIMIT,
            "transcript() is bounded by the default cap, not unbounded"
        );

        let page = s.transcript_page(&key, None, None).unwrap();
        assert_eq!(page.messages.len(), DEFAULT_TRANSCRIPT_LIMIT);
        assert!(page.has_more, "rows beyond the cap mean more pages");
        assert_eq!(page.next_seq, Some(DEFAULT_TRANSCRIPT_LIMIT as u64 - 1));
    }

    #[test]
    fn transcript_page_limit_is_clamped_to_max() {
        // An absurd caller limit is clamped to MAX_TRANSCRIPT_LIMIT. With fewer
        // rows than the cap we still get them all and has_more is false; the point
        // is the request can't be coerced into an unbounded scan.
        let (s, key) = store_with_messages(3);
        let page = s.transcript_page(&key, None, Some(usize::MAX)).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert!(!page.has_more);
        assert_eq!(
            clamp_transcript_limit(Some(usize::MAX)),
            MAX_TRANSCRIPT_LIMIT
        );
        assert_eq!(clamp_transcript_limit(None), DEFAULT_TRANSCRIPT_LIMIT);
        // A 0 limit floors to 1 so it can never report an empty-yet-has_more page.
        assert_eq!(clamp_transcript_limit(Some(0)), 1);
    }

    #[test]
    fn transcript_page_after_seq_combines_with_limit() {
        // after_seq and limit compose: skip the first 2, take 3 of the remaining.
        let (s, key) = store_with_messages(10); // seq 0..=9
        let page = s.transcript_page(&key, Some(1), Some(3)).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].seq, 2, "starts just after after_seq");
        assert_eq!(page.messages[2].seq, 4);
        assert!(page.has_more);
        assert_eq!(page.next_seq, Some(4));
    }

    #[test]
    fn transcript_page_on_closed_room_is_unknown() {
        // The open-room precondition is unchanged: a closed room is UnknownRoom on
        // the page API too (the daemon handler is what falls back to the audit
        // view). Pins that transcript_page didn't accidentally widen visibility.
        let (mut s, key) = store_with_messages(2);
        s.close(&key).unwrap();
        assert!(matches!(
            s.transcript_page(&key, None, Some(10)),
            Err(RoomStoreError::UnknownRoom(_))
        ));
    }

    #[test]
    fn seq_is_monotonic_across_mixed_operations() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.add_participant(&key, human("john", "John"), now())
            .unwrap(); // seq 0
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "hi",
            now(),
        )
        .unwrap(); // seq 1
        s.remove_participant(&key, "john", now()).unwrap(); // seq 2
        let seqs: Vec<u64> = s
            .transcript(&key, None)
            .unwrap()
            .iter()
            .map(|m| m.seq)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn update_changes_name_and_policy_and_bumps_updated_at() {
        let mut s = store();
        let key = RoomKey::new("r1");
        let created = s.create(key.clone(), "Old", None, now()).unwrap();

        let updated = s
            .update(
                &key,
                Some("New".into()),
                Some(Some(RoomTriggerPolicy {
                    on_thread_reply: true,
                    ..Default::default()
                })),
                now(),
            )
            .unwrap();
        assert_eq!(updated.room.name, "New");
        assert!(updated.room.trigger_policy.unwrap().on_thread_reply);
        assert!(updated.room.updated_at >= created.room.updated_at);

        // Clearing the policy with Some(None).
        let cleared = s.update(&key, None, Some(None), now()).unwrap();
        assert!(cleared.room.trigger_policy.is_none());
        assert_eq!(cleared.room.name, "New"); // name untouched

        // Update of unknown room errors.
        assert!(matches!(
            s.update(&RoomKey::new("nope"), Some("x".into()), None, now()),
            Err(RoomStoreError::UnknownRoom(_))
        ));
    }

    #[test]
    fn close_hides_room_from_get_and_list() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "bye",
            now(),
        )
        .unwrap();

        let closed = s.close(&key).unwrap();
        assert_eq!(closed.transcript.len(), 1);

        // Soft-closed: hidden from get/list, but recoverable for audit.
        assert!(s.get(&key).unwrap().is_none());
        assert!(s.list().unwrap().is_empty());
        assert!(s.get_including_closed(&key).unwrap().is_some());

        // Closing again errors (no open room).
        assert!(matches!(s.close(&key), Err(RoomStoreError::UnknownRoom(_))));
    }

    #[test]
    fn unknown_room_errors_on_transcript() {
        let s = store();
        assert!(matches!(
            s.transcript(&RoomKey::new("nope"), None),
            Err(RoomStoreError::UnknownRoom(_))
        ));
    }

    // ---- OCEAN-200: rollback-on-failure + FK cascade coverage ---------------

    /// Count rows in a table for a room (test helper that reaches into the
    /// store's connection so assertions can inspect raw persisted state).
    fn count(s: &SqliteRoomStore, table: &str, room: &RoomKey) -> i64 {
        s.conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE room_id = ?1"),
                params![room.as_str()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn create_collision_leaves_store_unchanged() {
        // A duplicate `create` must fail AND leave the existing room's rows
        // exactly as they were — no partial overwrite, no extra room row.
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "Original", None, now()).unwrap();
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "hello",
            now(),
        )
        .unwrap();

        let rooms_before: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM rooms", [], |r| r.get(0))
            .unwrap();
        let msgs_before = count(&s, "messages", &key);

        // Colliding create with a different name + policy must NOT mutate the row.
        let err = s.create(
            key.clone(),
            "Hijacked",
            Some(RoomTriggerPolicy {
                on_mention: true,
                ..Default::default()
            }),
            now(),
        );
        assert!(matches!(err, Err(RoomStoreError::AlreadyExists(_))));

        let rooms_after: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM rooms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rooms_before, rooms_after, "no extra/leaked room row");
        assert_eq!(
            msgs_before,
            count(&s, "messages", &key),
            "transcript intact"
        );

        // The name + (absent) policy of the original survive untouched.
        let rec = s.get(&key).unwrap().unwrap();
        assert_eq!(rec.room.name, "Original");
        assert!(rec.room.trigger_policy.is_none());
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].body, "hello");
    }

    #[test]
    fn failed_remove_of_unknown_participant_is_a_clean_noop() {
        // remove_participant on a non-member must error BEFORE any write — no
        // stray ParticipantLeft marker, no seq advance, roster unchanged.
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.add_participant(&key, human("john", "John"), now())
            .unwrap(); // seq 0

        let msgs_before = count(&s, "messages", &key);
        let parts_before = count(&s, "participants", &key);

        let err = s.remove_participant(&key, "ghost", now());
        assert!(matches!(
            err,
            Err(RoomStoreError::UnknownParticipant { .. })
        ));

        assert_eq!(msgs_before, count(&s, "messages", &key), "no leaked marker");
        assert_eq!(
            parts_before,
            count(&s, "participants", &key),
            "roster intact"
        );
        // seq did not skip: next real append is seq 1, not 2.
        let m = s
            .append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "still here",
                now(),
            )
            .unwrap();
        assert_eq!(m.seq, 1, "failed op must not consume a seq");
    }

    #[test]
    fn append_to_closed_room_does_not_torn_write() {
        // append/add/remove on a soft-closed room must fail with UnknownRoom and
        // write nothing — the closed transcript stays frozen.
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "bye",
            now(),
        )
        .unwrap();
        s.close(&key).unwrap();

        let msgs_before = count(&s, "messages", &key);

        assert!(matches!(
            s.append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "after close",
                now(),
            ),
            Err(RoomStoreError::UnknownRoom(_))
        ));
        assert!(matches!(
            s.add_participant(&key, human("late", "Late"), now()),
            Err(RoomStoreError::UnknownRoom(_))
        ));

        assert_eq!(
            msgs_before,
            count(&s, "messages", &key),
            "closed transcript must stay frozen"
        );
        // The single original message is still all there is, via audit view.
        let rec = s.get_including_closed(&key).unwrap().unwrap();
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].body, "bye");
    }

    #[test]
    fn pragma_foreign_keys_is_enabled_on_the_live_connection() {
        // SQLite enforces FOREIGN KEY clauses ONLY when this per-connection
        // pragma is ON. migrate() sets it; assert it actually stuck on the
        // connection the store keeps and uses for every query.
        let s = store();
        let fk_on: i64 = s
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            fk_on, 1,
            "foreign_keys pragma must be ON or FK clauses are inert"
        );
    }

    #[test]
    fn fk_cascade_deletes_children_when_a_room_row_is_deleted() {
        // The schema declares participants/messages with
        // `REFERENCES rooms(id) ON DELETE CASCADE`. With the pragma ON, deleting
        // the parent room row must cascade-delete its roster + transcript.
        //
        // NOTE: the public API never hard-deletes a room (`close` is a soft
        // UPDATE), so this exercises the cascade directly to prove the schema +
        // pragma are wired correctly — i.e. that the ON DELETE CASCADE is real
        // and not silently inert.
        let s = store();
        let key = RoomKey::new("r1");

        // Build a room with roster + transcript directly (mut not needed: raw SQL).
        s.conn
            .execute(
                "INSERT INTO rooms (id, name, created_at, updated_at) VALUES (?1, 'R1', ?2, ?2)",
                params![key.as_str(), fmt_ts(now())],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO participants (room_id, id, kind, display_name, position)
                 VALUES (?1, 'john', 'human', 'John', 0)",
                params![key.as_str()],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
                 VALUES (?1, 0, 'john', 'human', 'message', 'hi', ?2)",
                params![key.as_str(), fmt_ts(now())],
            )
            .unwrap();
        assert_eq!(count(&s, "participants", &key), 1);
        assert_eq!(count(&s, "messages", &key), 1);

        // Hard-delete the parent room row.
        s.conn
            .execute("DELETE FROM rooms WHERE id = ?1", params![key.as_str()])
            .unwrap();

        // Children must be gone — proving the cascade fired (only true with the
        // pragma ON; this test fails loudly if FK enforcement regresses).
        assert_eq!(
            count(&s, "participants", &key),
            0,
            "participants must cascade"
        );
        assert_eq!(count(&s, "messages", &key), 0, "messages must cascade");
    }

    #[test]
    fn orphan_message_insert_is_rejected_by_fk() {
        // A message referencing a non-existent room must be rejected by the FK
        // (again, only with the pragma ON). This is the "referencing a
        // nonexistent room id" failure mode — proving the constraint enforces.
        let s = store();
        let res = s.conn.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
             VALUES ('ghost-room', 0, 'a', 'human', 'message', 'x', ?1)",
            params![fmt_ts(now())],
        );
        assert!(res.is_err(), "FK must reject a message with no parent room");
    }

    #[test]
    fn close_is_soft_and_retains_all_rows() {
        // close() is a soft-close (UPDATE closed_at), NOT a delete: roster and
        // transcript rows must be retained for audit, not cascaded away.
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.add_participant(&key, human("john", "John"), now())
            .unwrap();
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "hi",
            now(),
        )
        .unwrap();

        let parts_before = count(&s, "participants", &key);
        let msgs_before = count(&s, "messages", &key);
        assert!(parts_before > 0 && msgs_before > 0);

        s.close(&key).unwrap();

        // Hidden from the open view...
        assert!(s.get(&key).unwrap().is_none());
        // ...but every row is retained (soft-close, no cascade).
        assert_eq!(
            count(&s, "participants", &key),
            parts_before,
            "roster retained"
        );
        assert_eq!(
            count(&s, "messages", &key),
            msgs_before,
            "transcript retained"
        );
        // closed_at is set.
        let closed_at: Option<String> = s
            .conn
            .query_row(
                "SELECT closed_at FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(closed_at.is_some(), "closed_at must be set on soft-close");
    }

    /// Assert the room invariant on a connection: every participant has exactly
    /// one `participant_joined` marker, and the transcript seqs are a dense
    /// `0..N` range (no gaps, no duplicates). A torn row violates one of these.
    fn assert_no_torn_row(s: &SqliteRoomStore, key: &RoomKey) {
        let parts = count(s, "participants", key);
        let join_markers: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE room_id = ?1 AND kind = 'participant_joined'",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            parts, join_markers,
            "every roster row must have its join marker (no torn row)"
        );

        // Seqs must be a dense 0..N range — no gaps and no duplicates.
        let mut stmt = s
            .conn
            .prepare("SELECT seq FROM messages WHERE room_id = ?1 ORDER BY seq")
            .unwrap();
        let seqs: Vec<i64> = stmt
            .query_map(params![key.as_str()], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for (i, seq) in seqs.iter().enumerate() {
            assert_eq!(*seq, i as i64, "seq gap/dup detected: {seqs:?}");
        }
    }

    /// REGRESSION (OCEAN-201, inverts OCEAN-200's `#[ignore]`d repro): the
    /// multi-statement write paths (`add_participant`, `remove_participant`,
    /// `append_message`) are now wrapped in an `IMMEDIATE` SQLite transaction. The
    /// old un-wrapped code auto-committed each statement independently, so a
    /// concurrent writer on the same DB file could steal the seq between the
    /// participant INSERT and the join-marker INSERT — leaving a roster row with
    /// no matching marker (a torn row) once the marker hit the `(room_id, seq)` PK.
    ///
    /// This replays OCEAN-200's exact interleave, but with `s2` running
    /// `add_participant`'s statements on an `IMMEDIATE` transaction (mirroring the
    /// fix). IMMEDIATE takes the write lock at `BEGIN`, so `s1` can no longer steal
    /// the seq mid-operation: its colliding commit fails with `SQLITE_BUSY` while
    /// `s2` holds the lock. `s2` then commits a consistent, paired
    /// (participant, join-marker) at a fresh seq. Even if `s2`'s marker insert
    /// *had* failed, dropping the transaction rolls back the participant insert too
    /// — no orphan. Under the OLD auto-commit code the participant insert would
    /// commit independently and the marker collide, tearing the row; that path is
    /// what `assert_no_torn_row` would have caught.
    #[test]
    fn concurrent_seq_collision_rolls_back_with_no_torn_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("r1");

        let mut s1 = SqliteRoomStore::open(&path).unwrap();
        s1.create(key.clone(), "R1", None, now()).unwrap();
        let mut s2 = SqliteRoomStore::open(&path).unwrap();

        // Drive add_participant's statement sequence on s2 through an IMMEDIATE
        // transaction (exactly what the fixed method does), pausing mid-op to let
        // s1 attempt the OCEAN-200 seq steal.
        {
            let tx = s2
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();

            // 1. s2 inserts the participant — now INSIDE the uncommitted tx, not
            //    auto-committed. (The torn-row leak point under the old code.)
            tx.execute(
                "INSERT INTO participants (room_id, id, kind, display_name, position)
                 VALUES (?1, 'p', 'human', 'P', 0)",
                params![key.as_str()],
            )
            .unwrap();

            // 2. s1 tries to commit a message at seq 0 — the steal that tore the
            //    row in OCEAN-200. With s2 holding the IMMEDIATE write lock, this
            //    MUST be refused (SQLITE_BUSY), so the seq can't be stolen.
            let s1_steal = s1.conn.execute(
                "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
                 VALUES (?1, 0, 'a', 'human', 'message', 'x', ?2)",
                params![key.as_str(), fmt_ts(now())],
            );
            assert!(
                s1_steal.is_err(),
                "IMMEDIATE lock must block the concurrent seq steal (got {s1_steal:?})"
            );

            // 3. s2 allocates its seq (MAX(seq)+1) and writes the paired join
            //    marker — no collision, because the steal was blocked. Inlined
            //    (rather than calling the private helper) so this test pins the
            //    behaviour independent of internal refactors.
            let next_seq: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE room_id = ?1",
                    params![key.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            tx.execute(
                "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
                 VALUES (?1, ?2, 'p', 'human', 'participant_joined', 'P joined', ?3)",
                params![key.as_str(), next_seq, fmt_ts(now())],
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Invariant: the participant committed WITH its join marker, and seqs are a
        // dense range — no torn row, no gap. This is the inversion of the old
        // assertion (which asserted parts==1 but markers==0).
        assert_eq!(count(&s2, "participants", &key), 1, "participant committed");
        assert_no_torn_row(&s2, &key);
    }

    /// REGRESSION (OCEAN-201) — method-level inversion through the REAL
    /// `add_participant` API. Forces the join-marker INSERT to fail mid-method via
    /// a temporary trigger that ABORTs message inserts, then asserts the whole
    /// operation rolled back: NO orphan participant row, NO seq advance.
    ///
    /// Under the OLD auto-commit code the participant DELETE/INSERT committed
    /// independently before the marker insert ran, so a marker failure left a
    /// roster row with no join marker — the exact torn row. Under the fix the
    /// participant insert lives in the same transaction as the failing marker
    /// insert, so the `?` drops the tx and rolls both back. This test FAILS on the
    /// old code (orphan participant remains) and PASSES on the fix.
    #[test]
    fn marker_insert_failure_rolls_back_participant_insert() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        // Trigger: abort any INSERT of a participant_joined marker, simulating the
        // (room_id, seq) PK collision the concurrent-writer race would cause —
        // deterministically, without needing a second connection.
        s.conn
            .execute_batch(
                "CREATE TRIGGER fail_join_marker
                 BEFORE INSERT ON messages
                 WHEN NEW.kind = 'participant_joined'
                 BEGIN SELECT RAISE(ABORT, 'forced marker failure'); END;",
            )
            .unwrap();

        let res = s.add_participant(&key, human("p", "P"), now());
        assert!(res.is_err(), "marker insert must fail (trigger aborts it)");

        // The whole op rolled back: no orphan participant, no leaked marker.
        assert_eq!(
            count(&s, "participants", &key),
            0,
            "participant insert must roll back with the failed marker (no orphan/torn row)"
        );
        assert_no_torn_row(&s, &key);

        // And no seq was consumed: drop the trigger, a real add now starts at seq 0.
        s.conn
            .execute_batch("DROP TRIGGER fail_join_marker;")
            .unwrap();
        let rec = s.add_participant(&key, human("p", "P"), now()).unwrap();
        assert_eq!(
            rec.transcript[0].seq, 0,
            "rolled-back op must not consume a seq"
        );
        assert_no_torn_row(&s, &key);
    }

    #[test]
    fn trigger_policy_round_trips_and_evaluates() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(
            key.clone(),
            "R1",
            Some(RoomTriggerPolicy {
                on_mention: true,
                ..Default::default()
            }),
            now(),
        )
        .unwrap();

        let policy = s.trigger_policy(&key).unwrap();
        let decision = evaluate_trigger_policy(
            policy.as_ref(),
            &RoomTriggerEvent::Mention {
                participant_id: "ocean".into(),
            },
        );
        assert!(decision.should_convene);
        assert_eq!(decision.target_participant.as_deref(), Some("ocean"));
    }

    // ── S2-P1 federation store tests ──────────────────────────────────────

    fn member_proj(member_id: &str, display_name: &str) -> FederatedRoomMemberProjection {
        FederatedRoomMemberProjection {
            member_id: member_id.into(),
            owner_member_id: None,
            actor_type: ocean_core::FederatedActorType::User,
            role_in_room: ocean_core::FederatedRoomRole::Member,
            display_name: display_name.into(),
            public_agent_descriptor: None,
            joined_at: "2026-07-16T18:00:00Z".into(),
            derived_presence: None,
            local_binding_available: None,
        }
    }

    // ── reopen + migration ────────────────────────────────────────────────

    #[test]
    fn reopen_preserves_access_projection_and_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let key = RoomKey::new("r1");

        // First open: create room + upsert access projection + append outbox.
        {
            let mut s = SqliteRoomStore::open(&db_path).unwrap();
            s.create(key.clone(), "R1", None, now()).unwrap();
            let members = vec![member_proj("m1", "Alice")];
            s.upsert_access_projection(&key, RoomAccessState::Live, Some(42), &members)
                .unwrap();
            let item = RoomOutboxItem {
                client_event_id: "evt-1".into(),
                source_id: key.as_str().into(),
                source_sequence: 1,
                author_member_id: "m1".into(),
                event_type: "message".into(),
                payload: serde_json::json!({"body": "hi"}),
                mention_member_ids: vec![],
                state: OutboxItemState::Pending,
            };
            s.append_outbox_item(&key, &item).unwrap();
        }

        // Reopen: projection and outbox survive.
        {
            let s = SqliteRoomStore::open(&db_path).unwrap();
            let proj = s.read_access_projection(&key).unwrap().unwrap();
            assert_eq!(proj.state, RoomAccessState::Live);
            assert_eq!(proj.last_confirmed_global_sequence, Some(42));
            assert_eq!(proj.members.len(), 1);
            assert_eq!(proj.members[0].display_name, "Alice");

            let items = s.read_outbox_items(&key, None).unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].client_event_id, "evt-1");
            assert_eq!(items[0].state, OutboxItemState::Pending);
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let mut s = store();
        s.migrate().unwrap();
        s.migrate().unwrap(); // second call must not error or duplicate
                              // Prove tables still work after double migrate.
        let key = RoomKey::new("r2");
        s.create(key.clone(), "R2", None, now()).unwrap();
        assert!(s.get(&key).unwrap().is_some());
    }

    // ── transcript paging metadata ────────────────────────────────────────

    #[test]
    fn transcript_page_returns_federated_metadata() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        // Append a message, then inject federated metadata directly via SQL.
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "hello",
            now(),
        )
        .unwrap();

        let meta = FederatedMessageMeta {
            ledger_event_id: "evt_x".into(),
            global_sequence: 99,
            source_id: "room:r1:member:m1:producer:p1".into(),
            source_sequence: 5,
            client_event_id: "cli-1".into(),
            origin_principal_id: "princ-1".into(),
            origin_member_id: "mem-1".into(),
        };
        s.conn
            .execute(
                "UPDATE messages SET federated = ?1 WHERE room_id = ?2 AND seq = 0",
                params![serde_json::to_string(&meta).unwrap(), key.as_str()],
            )
            .unwrap();

        let page = s.transcript_page(&key, None, None).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert!(!page.has_more);
        let msg = &page.messages[0];
        assert_eq!(msg.seq, 0);
        assert_eq!(msg.body, "hello");
        let fed = msg.federated.as_ref().unwrap();
        assert_eq!(fed.global_sequence, 99);
        assert_eq!(fed.source_sequence, 5);
        assert_eq!(fed.ledger_event_id, "evt_x");
    }

    // ── outbox excluded from transcript ───────────────────────────────────

    #[test]
    fn outbox_items_do_not_appear_in_transcript() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        s.append_message(
            &key,
            "john",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "real message",
            now(),
        )
        .unwrap();

        let item = RoomOutboxItem {
            client_event_id: "evt-1".into(),
            source_id: key.as_str().into(),
            source_sequence: 1,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "outbox msg"}),
            mention_member_ids: vec![],
            state: OutboxItemState::Pending,
        };
        s.append_outbox_item(&key, &item).unwrap();

        // Transcript must contain only the "real message" (1 row), NOT the outbox item.
        let page = s.transcript_page(&key, None, None).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].body, "real message");
    }

    // ── u64 TEXT edge cases ───────────────────────────────────────────────

    #[test]
    fn u64_max_roundtrips_through_access_projection() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let members = vec![member_proj("m1", "Alice")];
        s.upsert_access_projection(&key, RoomAccessState::Live, Some(u64::MAX), &members)
            .unwrap();
        let proj = s.read_access_projection(&key).unwrap().unwrap();
        assert_eq!(
            proj.last_confirmed_global_sequence,
            Some(u64::MAX),
            "u64::MAX must survive roundtrip"
        );
    }

    #[test]
    fn u64_max_roundtrips_through_outbox() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let item = RoomOutboxItem {
            client_event_id: "evt-max".into(),
            source_id: key.as_str().into(),
            source_sequence: u64::MAX,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "max"}),
            mention_member_ids: vec![],
            state: OutboxItemState::Pending,
        };
        s.append_outbox_item(&key, &item).unwrap();
        let items = s.read_outbox_items(&key, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_sequence, u64::MAX);
    }

    #[test]
    fn corrupt_u64_text_is_fail_closed() {
        // Inject a non-decimal value directly into the TEXT sequence column.
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let members = vec![member_proj("m1", "Alice")];
        s.upsert_access_projection(&key, RoomAccessState::Live, Some(1), &members)
            .unwrap();

        // Corrupt the confirmed_sequence column directly.
        s.conn
            .execute(
                "UPDATE room_access SET confirmed_sequence = '0xDEAD' WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();

        let err = s.read_access_projection(&key).unwrap_err();
        assert!(
            err.to_string().contains("invalid u64"),
            "corrupt hex u64 must fail closed — got: {err}"
        );
    }

    #[test]
    fn leading_zero_u64_text_is_fail_closed() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let members = vec![member_proj("m1", "Alice")];
        s.upsert_access_projection(&key, RoomAccessState::Live, Some(1), &members)
            .unwrap();

        // Inject leading-zero text directly.
        s.conn
            .execute(
                "UPDATE room_access SET confirmed_sequence = '00042' WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();

        let err = s.read_access_projection(&key).unwrap_err();
        assert!(
            err.to_string().contains("invalid u64"),
            "leading-zero u64 must fail closed — got: {err}"
        );
    }

    // ── stable outbox order ───────────────────────────────────────────────

    #[test]
    fn outbox_items_are_returned_in_stable_insertion_order() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        for i in 0..5 {
            let item = RoomOutboxItem {
                client_event_id: format!("evt-{i}"),
                source_id: key.as_str().into(),
                source_sequence: i,
                author_member_id: "m1".into(),
                event_type: "message".into(),
                payload: serde_json::json!({"i": i}),
                mention_member_ids: vec![],
                state: OutboxItemState::Pending,
            };
            s.append_outbox_item(&key, &item).unwrap();
        }
        let items = s.read_outbox_items(&key, None).unwrap();
        assert_eq!(items.len(), 5);
        let ids: Vec<_> = items.iter().map(|i| &i.client_event_id as &str).collect();
        assert_eq!(ids, vec!["evt-0", "evt-1", "evt-2", "evt-3", "evt-4"]);
    }

    // ── retry preserves every non-state field ─────────────────────────────

    #[test]
    fn retry_preserves_all_non_state_fields() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let item = RoomOutboxItem {
            client_event_id: "evt-1".into(),
            source_id: "room:r1:member:m1:producer:p1".into(),
            source_sequence: 7,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "retry me", "extra": true}),
            mention_member_ids: vec!["m2".into(), "m3".into()],
            state: OutboxItemState::Failed,
        };
        s.append_outbox_item(&key, &item).unwrap();

        s.retry_outbox_item(&key, "evt-1").unwrap();

        let items = s.read_outbox_items(&key, None).unwrap();
        assert_eq!(items.len(), 1);
        let retried = &items[0];
        assert_eq!(retried.state, OutboxItemState::Pending); // only this changed
        assert_eq!(retried.client_event_id, "evt-1");
        assert_eq!(retried.source_id, "room:r1:member:m1:producer:p1");
        assert_eq!(retried.source_sequence, 7);
        assert_eq!(retried.author_member_id, "m1");
        assert_eq!(retried.event_type, "message");
        assert_eq!(
            retried.payload,
            serde_json::json!({"body": "retry me", "extra": true})
        );
        assert_eq!(retried.mention_member_ids, vec!["m2", "m3"]);
    }

    // ── distinct outcomes ─────────────────────────────────────────────────

    #[test]
    fn unknown_room_rejected_on_access_ops() {
        let s = store();
        let key = RoomKey::new("nonexistent");
        let err = s.read_access_projection(&key).unwrap_err();
        assert!(
            matches!(err, RoomStoreError::UnknownRoom(_)),
            "unknown room must be UnknownRoom — got: {err}"
        );
    }

    #[test]
    fn local_room_returns_none_access_projection() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        // Never upserted — fresh local room.
        let proj = s.read_access_projection(&key).unwrap();
        assert!(
            proj.is_none(),
            "fresh local room must return None projection"
        );
    }

    #[test]
    fn unknown_outbox_item_rejected_on_retry() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let err = s.retry_outbox_item(&key, "nonexistent").unwrap_err();
        assert!(
            matches!(err, RoomStoreError::UnknownOutboxItem { .. }),
            "retry on unknown item must be UnknownOutboxItem — got: {err}"
        );
    }

    #[test]
    fn not_failed_outbox_item_rejected_on_retry() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();
        let item = RoomOutboxItem {
            client_event_id: "evt-1".into(),
            source_id: key.as_str().into(),
            source_sequence: 1,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "pending"}),
            mention_member_ids: vec![],
            state: OutboxItemState::Pending,
        };
        s.append_outbox_item(&key, &item).unwrap();
        let err = s.retry_outbox_item(&key, "evt-1").unwrap_err();
        assert!(
            matches!(err, RoomStoreError::OutboxItemNotFailed { .. }),
            "retry on pending item must be OutboxItemNotFailed — got: {err}"
        );
    }

    #[test]
    fn read_outbox_returns_empty_on_unknown_room() {
        let s = store();
        let key = RoomKey::new("nonexistent");
        let items = s.read_outbox_items(&key, None).unwrap();
        assert!(
            items.is_empty(),
            "unknown room must return empty outbox, not error"
        );
    }

    #[test]
    fn outbox_state_filter_works() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        let pending = RoomOutboxItem {
            client_event_id: "evt-pending".into(),
            source_id: key.as_str().into(),
            source_sequence: 1,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "pending"}),
            mention_member_ids: vec![],
            state: OutboxItemState::Pending,
        };
        let failed = RoomOutboxItem {
            client_event_id: "evt-failed".into(),
            source_id: key.as_str().into(),
            source_sequence: 2,
            author_member_id: "m1".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body": "failed"}),
            mention_member_ids: vec![],
            state: OutboxItemState::Failed,
        };
        s.append_outbox_item(&key, &pending).unwrap();
        s.append_outbox_item(&key, &failed).unwrap();

        let all = s.read_outbox_items(&key, None).unwrap();
        assert_eq!(all.len(), 2);

        let only_pending = s
            .read_outbox_items(&key, Some(OutboxItemState::Pending))
            .unwrap();
        assert_eq!(only_pending.len(), 1);
        assert_eq!(only_pending[0].client_event_id, "evt-pending");

        let only_failed = s
            .read_outbox_items(&key, Some(OutboxItemState::Failed))
            .unwrap();
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].client_event_id, "evt-failed");
    }
}
