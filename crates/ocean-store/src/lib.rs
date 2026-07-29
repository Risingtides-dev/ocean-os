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
//! # How the daemon uses this store
//!
//! The daemon holds one `SqliteRoomStore` behind the `Mutex` on its `AppState`
//! (`ocean-daemon/src/persistent_rooms.rs`), opened once at startup with
//! `SqliteRoomStore::open(config_dir.join("rooms.db"))`. `open` enforces
//! owner-only `0600` on the DB and sidecars BEFORE any DB work (Unix), then
//! runs [`SqliteRoomStore::migrate`] idempotently, so it is safe on an
//! existing DB. Methods are sync and `&mut self`: lock the `Mutex`, call, and
//! drop the guard before any `.await`. `close` **soft-closes** (marks
//! `closed_at`) rather than deleting, so transcripts survive an audit; use
//! [`SqliteRoomStore::get_including_closed`] for audit views. The daemon maps
//! [`RoomStoreError`] onto HTTP responses in
//! `persistent_rooms.rs::room_store_error_response`, so new error variants
//! here require a matching arm there (the match is deliberately exhaustive).
//!
//! # Federation core (S2 P2-A)
//!
//! Beyond the `RoomRegistry`-shaped API, this crate owns the restart-safe
//! federation state for Bedrock-connected rooms: credential custody
//! (`room_federation`, `pending_redemptions`), member→agent bindings,
//! per-producer outbox sequence counters, the confirmed-event dedup/order
//! index, and the at-most-once trigger-claim journal. Every multi-row
//! federation mutation commits in one IMMEDIATE transaction, u64 counters and
//! cursors persist as canonical decimal TEXT (noncanonical text fails
//! closed), and secrets (bearer tokens, invite codes, registration keys) are
//! never serialized into projections, transcripts, logs, or error messages.
//! See `crates/ocean-store/AGENTS.md` for the binding invariants.

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
/// (de)serialization, which an in-memory map cannot. The federation core
/// (P2-A) adds three more: `RoomNotFederated` (a federation-only operation on
/// a room with no access row), `FederationCorruption` (a fail-closed integrity
/// stop — dedup/order violations, exhausted counters, promote-state
/// mismatches; its message never carries secrets), and `Io` (filesystem
/// errors from the owner-only DB-mode enforcement). The daemon's
/// `room_store_error_response` maps them to 409/500/500 respectively.
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
    /// The room exists but has no federation access projection row (P2-A).
    RoomNotFederated(RoomKey),
    /// Confirmed-ingest ordering/dedup violation: persisted state disagrees
    /// with the incoming row. Carries opaque ids/sequences only — never a
    /// bearer, registration key, or any secret material (P2-A).
    FederationCorruption(String),
    /// An underlying SQLite error.
    Db(rusqlite::Error),
    /// A stored value could not be (de)serialized.
    Encode(String),
    /// A filesystem error (e.g. enforcing the owner-only DB file mode).
    Io(std::io::Error),
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
            Self::RoomNotFederated(k) => {
                write!(f, "room '{k}' is not federated (no access projection)")
            }
            Self::FederationCorruption(m) => write!(f, "federation corruption: {m}"),
            Self::Db(e) => write!(f, "sqlite error: {e}"),
            Self::Encode(e) => write!(f, "encode error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for RoomStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for RoomStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

// ── S2-P1 retry-outbox error (inherent API, never widened on RoomStore) ────

/// Errors specific to [`SqliteRoomStore::retry_failed_outbox`] (S2-P1).
///
/// This is a separate type — not widened onto [`RoomStoreError`] — so the
/// daemon's exhaustive match on `RoomStoreError` is never broken.
#[derive(Debug)]
pub enum RetryOutboxError {
    /// The room does not exist at all.
    RoomNotFound(RoomKey),
    /// The room exists but has no access projection (local room, not federated).
    RoomNotFederated(RoomKey),
    /// The room's access state is `Revoked` — retry is forbidden.
    RoomAccessRevoked(RoomKey),
    /// No outbox item exists for the given `client_event_id` in this room.
    OutboxItemNotFound {
        room: RoomKey,
        client_event_id: String,
    },
    /// The outbox item exists but is not in `Failed` state.
    OutboxItemNotFailed {
        room: RoomKey,
        client_event_id: String,
        current_state: String,
    },
    /// An underlying store error.
    Store(RoomStoreError),
}

impl std::fmt::Display for RetryOutboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoomNotFound(k) => write!(f, "room '{k}' not found"),
            Self::RoomNotFederated(k) => {
                write!(f, "room '{k}' is not federated (no access projection)")
            }
            Self::RoomAccessRevoked(k) => write!(f, "room '{k}' access is revoked"),
            Self::OutboxItemNotFound {
                room,
                client_event_id,
            } => {
                write!(
                    f,
                    "outbox item '{client_event_id}' not found in room '{room}'"
                )
            }
            Self::OutboxItemNotFailed {
                room,
                client_event_id,
                current_state,
            } => {
                write!(
                    f,
                    "outbox item '{client_event_id}' in room '{room}' is '{current_state}', not failed"
                )
            }
            Self::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl std::error::Error for RetryOutboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RoomStoreError> for RetryOutboxError {
    fn from(e: RoomStoreError) -> Self {
        Self::Store(e)
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
}

/// SQLite-backed durable room store.
pub struct SqliteRoomStore {
    conn: Connection,
}

impl SqliteRoomStore {
    /// Open (or create) a store at `path`, running migrations idempotently.
    ///
    /// The DB holds federation bearer tokens (P2-A), so on Unix every
    /// create/reopen also enforces owner-only `0600` on the database file and
    /// its SQLite sidecars — a previously loosened mode is repaired, not just
    /// asserted.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // Enforce BEFORE any DB work: a pre-existing loosened DB (and any
        // sidecars) is repaired before a single byte is read through it.
        enforce_owner_only_db_mode(path.as_ref())?;
        let conn = Connection::open(path.as_ref())?;
        let mut store = Self { conn };
        store.migrate()?;
        // Re-enforce after create: a freshly created DB file (and sidecars
        // SQLite spawned during migration) must leave open() owner-only.
        enforce_owner_only_db_mode(path.as_ref())?;
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
                thread_parent_seq INTEGER,           -- NULL = top-level; G1-B threads
                session_id  TEXT,                    -- NULL = unattributed; G1-B agent import
                PRIMARY KEY (room_id, seq)
            );

            -- G1-B: add thread_parent_seq and session_id columns to existing
            -- databases without rewriting. SQLite ALTER TABLE ADD COLUMN is
            -- cheap (no table copy) and NULL defaults are implicit.
            -- These are safe to run on new databases too: we catch "duplicate
            -- column" errors silently below rather than failing the batch.
            "#,
        )?;
        // G1-B migrations: safe on both fresh and existing databases.
        for col in &["thread_parent_seq INTEGER", "session_id TEXT"] {
            let sql = format!("ALTER TABLE messages ADD COLUMN {col}");
            if let Err(e) = self.conn.execute(&sql, []) {
                // SQLite error: "duplicate column name" means the column already
                // exists (e.g. CREATE TABLE IF NOT EXISTS just created it).
                // Any other error is a real problem.
                if !e.to_string().contains("duplicate column") {
                    return Err(e.into());
                }
            }
        }
        self.conn.execute_batch(
            r#"
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
                position           INTEGER NOT NULL,  -- stable ordering, never rowid
                PRIMARY KEY (room_id, client_event_id)
            );

            CREATE INDEX IF NOT EXISTS idx_outbox_room_state ON outbox(room_id, state);

            -- ── P2-A federation durability (private tables) ──────────────
            -- Bearer tokens and registration keys below are PRIVATE: they are
            -- never serialized into projections, transcripts, logs, or errors.

            CREATE TABLE IF NOT EXISTS federation_instance (
                singleton   INTEGER PRIMARY KEY CHECK (singleton = 1),
                instance_id TEXT NOT NULL         -- one stable daemon UUID
            );

            CREATE TABLE IF NOT EXISTS room_federation (
                room_id               TEXT PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
                bearer_token          TEXT NOT NULL,  -- PRIVATE: never projected
                local_human_member_id TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS room_member_bindings (
                room_id          TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                member_id        TEXT NOT NULL,       -- opaque Bedrock member id
                agent_name       TEXT NOT NULL,       -- local folder-agent reference (never a snapshot)
                registration_key TEXT NOT NULL,       -- PRIVATE: never projected
                PRIMARY KEY (room_id, member_id)
            );

            -- One opaque member per local agent name within a room.
            CREATE UNIQUE INDEX IF NOT EXISTS idx_room_member_bindings_agent
                ON room_member_bindings(room_id, agent_name);

            CREATE TABLE IF NOT EXISTS producer_counters (
                room_id          TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                author_member_id TEXT NOT NULL,
                next_sequence    TEXT NOT NULL,       -- canonical decimal u64
                PRIMARY KEY (room_id, author_member_id)
            );

            CREATE TABLE IF NOT EXISTS federated_events (
                room_id          TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                ledger_event_id  TEXT NOT NULL,       -- Bedrock ledger id (dedup key)
                global_sequence  TEXT NOT NULL,       -- canonical decimal u64, gaps allowed
                local_seq        INTEGER NOT NULL,    -- transcript seq assigned at ingest
                source_id        TEXT NOT NULL,
                source_sequence  TEXT NOT NULL,       -- canonical decimal u64
                client_event_id  TEXT NOT NULL,
                PRIMARY KEY (room_id, ledger_event_id)
            );

            -- Backstop for the strict per-room monotonic rule (enforced in the
            -- ingest transaction; canonical text must never be ORDER BY'd in SQL).
            CREATE UNIQUE INDEX IF NOT EXISTS idx_federated_events_global
                ON federated_events(room_id, global_sequence);

            CREATE TABLE IF NOT EXISTS processed_room_triggers (
                room_id          TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                ledger_event_id  TEXT NOT NULL,
                target_member_id TEXT NOT NULL,
                claimed_at       TEXT NOT NULL,       -- RFC3339; claim commits with the message
                PRIMARY KEY (room_id, ledger_event_id, target_member_id)
            );

            -- Pre-room redemption custody (v1.2 amendment, table 7). Rows are
            -- daemon-owned state for the B1 idempotent-redeem exchange: they
            -- exist BEFORE any local room and are therefore keyed by the
            -- redemption exchange, not by room_id. bearer_token AND
            -- invite_code are PRIVATE (custody class of room_federation).
            -- UNIQUE(invite_code): one invite code can never fork into two
            -- triples — retries get-or-insert by code.
            CREATE TABLE IF NOT EXISTS pending_redemptions (
                redemption_id TEXT PRIMARY KEY,       -- daemon-minted UUID, lowercase
                bearer_token  TEXT NOT NULL,          -- PRIVATE: locally generated secret
                invite_code   TEXT NOT NULL UNIQUE,   -- PRIVATE: needed to retry the exact request
                created_at    TEXT NOT NULL           -- RFC3339
            );
            "#,
        )?;
        // Backfill columns on DBs created before they existed.
        // position (S2-P1) — on the `outbox` table. The column *and* its
        // index are created inside the `execute_batch` above for fresh DBs but
        // must be ALTER'd for pre-existing outbox tables BEFORE the index is
        // created below, otherwise `CREATE INDEX … (position)` fails.
        //
        // If the ALTER succeeds (the column didn't exist), we backfill
        // deterministic per-room positions using stable client_event_id
        // ordering so legacy rows don't all land at position 0.
        let position_just_added = match self.conn.execute(
            "ALTER TABLE outbox ADD COLUMN position INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            Ok(_) => true,
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") =>
            {
                false
            }
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
                false
            }
            Err(e) => return Err(e.into()),
        };
        if position_just_added {
            self.conn.execute(
                "UPDATE outbox SET position = (
                     SELECT COUNT(*) FROM outbox AS o2
                     WHERE o2.room_id = outbox.room_id
                       AND o2.client_event_id < outbox.client_event_id
                 )",
                [],
            )?;
        }
        // UNIQUE per-room position index — runs AFTER the ALTER + backfill so
        // it always sees the column with distinct values.
        self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_outbox_room_position ON outbox(room_id, position)",
            [],
        )?;
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
        // position (S2-P1) — already handled first (above).
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
            "SELECT seq, author_id, author_kind, kind, body, created_at, federated, thread_parent_seq, session_id
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
            let thread_parent_seq: Option<i64> = row.get(7)?;
            let session_id: Option<String> = row.get(8)?;
            Ok((
                seq,
                author_id,
                author_kind,
                kind,
                body,
                created_at,
                federated,
                thread_parent_seq,
                session_id,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (
                seq,
                author_id,
                author_kind,
                kind,
                body,
                created_at,
                federated,
                thread_parent_seq,
                session_id,
            ) = r?;
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
                thread_parent_seq: thread_parent_seq.map(|s| s as u64),
                session_id,
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
        thread_parent_seq: Option<u64>,
        session_id: Option<&str>,
    ) -> Result<RoomMessage> {
        // MAX(seq)+1, recomputed from stored rows so it survives restarts.
        let next_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        let tps: Option<i64> = thread_parent_seq.map(|s| s as i64);
        conn.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at, federated, thread_parent_seq, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9)",
            params![
                key.as_str(),
                next_seq,
                author_id,
                encode_participant_kind(author_kind),
                encode_message_kind(kind),
                body,
                fmt_ts(now),
                tps,
                session_id,
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
            thread_parent_seq,
            session_id: session_id.map(|s| s.to_string()),
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
            None,
            None,
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
            None,
            None,
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
        self.append_message_threaded(key, author_id, author_kind, kind, body, now, None, None)
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
}

// ── G1-B: real threads + session attribution (not on RoomStore trait) ─────

impl SqliteRoomStore {
    /// Append a chat/system message with optional thread and session
    /// attribution (G1-B). `thread_parent_seq`, when `Some`, marks this as a
    /// reply to an existing message's `seq` in the same room — a real,
    /// durable parent/child relationship, not a CSS-only visual grouping.
    /// `session_id`, when `Some`, records the Ocean session that produced
    /// this message, so imported user-owned agents and humans posting
    /// through a session are attributable. The plain
    /// [`RoomStore::append_message`] delegates here with both `None`.
    #[allow(clippy::too_many_arguments)]
    pub fn append_message_threaded(
        &mut self,
        key: &RoomKey,
        author_id: &str,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: &str,
        now: DateTime<Utc>,
        thread_parent_seq: Option<u64>,
        session_id: Option<&str>,
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
        let msg = Self::insert_message_on(
            &tx,
            key,
            author_id,
            author_kind,
            kind,
            body,
            now,
            thread_parent_seq,
            session_id,
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        Ok(msg)
    }

    /// Count direct replies (`thread_parent_seq = root_seq`) to a message
    /// (G1-B). Used to materialize a root's reply count without loading the
    /// whole transcript, mirroring Buzz's root `reply_count` pattern.
    pub fn thread_reply_count(&self, key: &RoomKey, root_seq: u64) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE room_id = ?1 AND thread_parent_seq = ?2",
            params![key.as_str(), root_seq as i64],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    /// Read every reply to a root message (`thread_parent_seq = root_seq`),
    /// in ascending `seq` order (G1-B). Independently addressable, mirroring
    /// Buzz's thread-panel read path rather than deriving replies from
    /// in-memory transcript scanning on every render.
    pub fn thread_replies(&self, key: &RoomKey, root_seq: u64) -> Result<Vec<RoomMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, author_id, author_kind, kind, body, created_at, federated, thread_parent_seq, session_id
             FROM messages WHERE room_id = ?1 AND thread_parent_seq = ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![key.as_str(), root_seq as i64], |row| {
            let seq: i64 = row.get(0)?;
            let author_id: String = row.get(1)?;
            let author_kind: String = row.get(2)?;
            let kind: String = row.get(3)?;
            let body: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            let federated: Option<String> = row.get(6)?;
            let thread_parent_seq: Option<i64> = row.get(7)?;
            let session_id: Option<String> = row.get(8)?;
            Ok((
                seq,
                author_id,
                author_kind,
                kind,
                body,
                created_at,
                federated,
                thread_parent_seq,
                session_id,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (
                seq,
                author_id,
                author_kind,
                kind,
                body,
                created_at,
                federated,
                thread_parent_seq,
                session_id,
            ) = r?;
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
                thread_parent_seq: thread_parent_seq.map(|s| s as u64),
                session_id,
            });
        }
        Ok(out)
    }
}

// ── S2-P1 federation: inherent APIs (not on RoomStore trait) ───────────────

impl SqliteRoomStore {
    /// Read the room's access projection (S2-P1).
    ///
    /// Uses any-room existence (including soft-closed). Returns an exact
    /// `RoomAccessProjection` with `state: Local` when the room exists but no
    /// access row is present. Returns `UnknownRoom` only when the room does not
    /// exist at all.
    pub fn room_access(&self, key: &RoomKey) -> Result<RoomAccessProjection> {
        if !self.room_exists(key)? {
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
            // Room exists but no access row → exact Local projection.
            return Ok(RoomAccessProjection {
                state: RoomAccessState::Local,
                last_confirmed_global_sequence: None,
                members: Vec::new(),
                outbox: Vec::new(),
            });
        };
        let state: RoomAccessState =
            serde_json::from_value(serde_json::Value::String(state_str))
                .map_err(|e| RoomStoreError::Encode(format!("bad access state: {e}")))?;
        let confirmed_sequence: Option<u64> = match seq_text {
            Some(ref t) => Some(parse_canonical_u64_text(t)?),
            None => None,
        };
        let members: Vec<FederatedRoomMemberProjection> = serde_json::from_str(&member_json)
            .map_err(|e| RoomStoreError::Encode(format!("bad member projection: {e}")))?;
        let outbox = self.load_outbox_for_room(key)?;
        Ok(RoomAccessProjection {
            state,
            last_confirmed_global_sequence: confirmed_sequence,
            members,
            outbox,
        })
    }

    /// Replace the room's access projection atomically (S2-P1).
    ///
    /// One `IMMEDIATE` transaction: verifies room exists, upserts state /
    /// confirmed sequence / member JSON, deletes all existing outbox rows and
    /// re-inserts from the input projection (preserving `position` order),
    /// commits, returns the full projection.
    ///
    /// **Test seeding only (P2-A):** this destructively replaces outbox rows.
    /// Production roster/cursor/state refresh goes through
    /// [`SqliteRoomStore::update_room_access_safe`], which never touches the
    /// outbox.
    pub fn replace_room_access(
        &mut self,
        key: &RoomKey,
        proj: &RoomAccessProjection,
    ) -> Result<RoomAccessProjection> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Verify room existence inside the transaction.
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let state_str = serde_json::to_string(&proj.state)
            .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
        let state_str = state_str.trim_matches('"');
        let seq_text = proj.last_confirmed_global_sequence.map(write_u64_text);
        let member_json = serde_json::to_string(&proj.members)
            .map_err(|e| RoomStoreError::Encode(format!("members serialize: {e}")))?;
        tx.execute(
            "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
               state = excluded.state,
               confirmed_sequence = excluded.confirmed_sequence,
               member_projection = excluded.member_projection",
            params![key.as_str(), state_str, seq_text, member_json],
        )?;
        // Delete all existing outbox rows, then re-insert from input order.
        tx.execute(
            "DELETE FROM outbox WHERE room_id = ?1",
            params![key.as_str()],
        )?;
        for (pos, item) in proj.outbox.iter().enumerate() {
            Self::insert_outbox_item_on(&tx, key, item, pos)?;
        }
        tx.commit()?;
        // Reload to return the committed state.
        self.room_access(key)
    }

    /// Append a federated message to the transcript (S2-P1).
    ///
    /// Writes a transcript row with the given [`FederatedMessageMeta`] in the
    /// `federated` column. This is a real transactional writer — the message
    /// lands in the transcript table and is visible to all subsequent reads.
    #[allow(clippy::too_many_arguments)]
    pub fn append_federated_message(
        &mut self,
        key: &RoomKey,
        author_id: &str,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: &str,
        meta: &FederatedMessageMeta,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage> {
        if !self.room_is_open(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        let federated_json = serde_json::to_string(meta)
            .map_err(|e| RoomStoreError::Encode(format!("federated serialize: {e}")))?;
        tx.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at, federated, thread_parent_seq, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
            params![
                key.as_str(),
                next_seq,
                author_id,
                encode_participant_kind(author_kind),
                encode_message_kind(kind),
                body,
                fmt_ts(now),
                federated_json,
            ],
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        Ok(RoomMessage {
            seq: next_seq as u64,
            author_id: author_id.to_string(),
            author_kind,
            kind,
            body: body.to_string(),
            created_at: now,
            federated: Some(meta.clone()),
            thread_parent_seq: None,
            session_id: None,
        })
    }

    /// Retry a failed outbox item (S2-P1).
    ///
    /// One transaction, six distinct outcomes:
    ///
    /// | Condition | Error |
    /// |---|---|
    /// | Room does not exist | `RoomNotFound` |
    /// | Room exists, no access row | `RoomNotFederated` |
    /// | Access state is `Revoked` | `RoomAccessRevoked` |
    /// | Outbox item not found | `OutboxItemNotFound` |
    /// | Item exists but state != `Failed` | `OutboxItemNotFailed` |
    /// | Success | Full ordered projection |
    ///
    /// Only the `state` column is changed (`Failed` → `Pending`); every other
    /// field is preserved. No network calls.
    pub fn retry_failed_outbox(
        &mut self,
        key: &RoomKey,
        client_event_id: &str,
    ) -> std::result::Result<RoomAccessProjection, RetryOutboxError> {
        // One IMMEDIATE transaction from the start — all reads, checks, and
        // mutation happen inside it so no interleaving writer can change state.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| RetryOutboxError::Store(e.into()))?;
        // 1. Room must exist.
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| RetryOutboxError::Store(e.into()))?;
        if room_exists.is_none() {
            return Err(RetryOutboxError::RoomNotFound(key.clone()));
        }
        // 2. Access projection must exist AND not be Local/Revoked.
        let access_row = tx
            .query_row(
                "SELECT state FROM room_access WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| RetryOutboxError::Store(e.into()))?;
        let Some(state_str) = access_row else {
            return Err(RetryOutboxError::RoomNotFederated(key.clone()));
        };
        // Decode and validate state before mutation.
        let access_state: RoomAccessState =
            serde_json::from_value(serde_json::Value::String(state_str.clone())).map_err(|e| {
                RetryOutboxError::Store(RoomStoreError::Encode(format!("bad access state: {e}")))
            })?;
        match access_state {
            RoomAccessState::Local => {
                return Err(RetryOutboxError::RoomNotFederated(key.clone()));
            }
            RoomAccessState::Revoked => {
                return Err(RetryOutboxError::RoomAccessRevoked(key.clone()));
            }
            // Connecting / Live / Recovering → retry is allowed.
            _ => {}
        }
        // 3. Outbox item must exist and be in Failed state.
        let current: Option<(String,)> = tx
            .query_row(
                "SELECT state FROM outbox WHERE room_id = ?1 AND client_event_id = ?2",
                params![key.as_str(), client_event_id],
                |r| Ok((r.get::<_, String>(0)?,)),
            )
            .optional()
            .map_err(|e| RetryOutboxError::Store(e.into()))?;
        let Some((item_state_raw,)) = current else {
            return Err(RetryOutboxError::OutboxItemNotFound {
                room: key.clone(),
                client_event_id: client_event_id.to_string(),
            });
        };
        let item_state: OutboxItemState = serde_json::from_value(serde_json::Value::String(
            item_state_raw.clone(),
        ))
        .map_err(|e| {
            RetryOutboxError::Store(RoomStoreError::Encode(format!(
                "bad outbox state '{item_state_raw}': {e}"
            )))
        })?;
        if item_state != OutboxItemState::Failed {
            return Err(RetryOutboxError::OutboxItemNotFailed {
                room: key.clone(),
                client_event_id: client_event_id.to_string(),
                current_state: item_state_raw.trim_matches('"').to_string(),
            });
        }
        // 4. Mutate only the state column.
        tx.execute(
            "UPDATE outbox SET state = 'pending' WHERE room_id = ?1 AND client_event_id = ?2",
            params![key.as_str(), client_event_id],
        )
        .map_err(|e| RetryOutboxError::Store(e.into()))?;
        tx.commit().map_err(|e| RetryOutboxError::Store(e.into()))?;
        // Return full ordered projection.
        Ok(self.room_access(key)?)
    }

    // ── outbox helpers ────────────────────────────────────────────────────

    /// Load outbox rows ordered by `position` for a room.
    fn load_outbox_for_room(&self, key: &RoomKey) -> Result<Vec<RoomOutboxItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT client_event_id, source_id, source_sequence, author_member_id,
                    event_type, payload, mention_member_ids, state
             FROM outbox WHERE room_id = ?1 ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![key.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
            let source_sequence = parse_canonical_u64_text(&source_sequence)?;
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

    /// Insert a single outbox item on the given connection with an explicit
    /// `position`. Caller is responsible for the transaction.
    fn insert_outbox_item_on(
        conn: &Connection,
        key: &RoomKey,
        item: &RoomOutboxItem,
        position: usize,
    ) -> Result<()> {
        let state_str = serde_json::to_string(&item.state)
            .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
        let state_str = state_str.trim_matches('"');
        let payload_json = item.payload.to_string();
        let mentions_json = serde_json::to_string(&item.mention_member_ids)
            .map_err(|e| RoomStoreError::Encode(format!("mentions serialize: {e}")))?;
        let src_seq = write_u64_text(item.source_sequence);
        let pos: i64 = i64::try_from(position)
            .map_err(|_| RoomStoreError::Encode(format!("position overflow: {position}")))?;
        conn.execute(
            "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                 author_member_id, event_type, payload, mention_member_ids,
                                 state, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                pos,
            ],
        )?;
        Ok(())
    }
}

// ── P2-A federation durability: restart-safe store core ──────────────────
//
// Freeze: `gate2-s2-bridge-freeze-v1` §P2-A. Six private tables hold what the
// daemon's federation supervisor (P2-B) and sovereign intent routes (P2-C)
// need to survive restarts: one stable instance id, per-room bearer
// credentials, opaque-member→local-agent bindings, producer counters,
// confirmed-event dedup, and the trigger claim journal.
//
// Credential custody (pinned by the freeze):
//   * `room_federation.bearer_token` and
//     `room_member_bindings.registration_key` are PRIVATE. They never enter
//     `RoomAccessProjection`, transcript rows, logs, errors, debug output,
//     browser storage, or SSE. [`RoomCredential`] has a redacting `Debug` and
//     deliberately no `Serialize` impl; `FederationCorruption` messages carry
//     opaque ids/sequences only.
//   * [`SqliteRoomStore::open`] enforces owner-only `0600` on the DB and its
//     SQLite sidecars on every create/reopen (Unix; tests pin the mode).
//
// All u64 counters/cursors/sequences are stored as strict canonical decimal
// TEXT via `write_u64_text`/`parse_canonical_u64_text`; corrupt or
// noncanonical text fails closed on read. Canonical decimal TEXT must never
// be `ORDER BY`'d or `MAX()`'d in SQL (lexicographic ≠ numeric); monotonic
// reads go through the `local_seq` INTEGER column instead.

/// A room's private federation credential (P2-A). Read for daemon network
/// use only — the bearer must never be logged, projected, or serialized.
pub struct RoomCredential {
    /// The room this credential authorizes.
    pub room_id: RoomKey,
    /// Daemon-private Bedrock bearer token.
    pub bearer_token: String,
    /// The authenticated local human member id for this room.
    pub local_human_member_id: String,
}

impl std::fmt::Debug for RoomCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomCredential")
            .field("room_id", &self.room_id)
            .field("bearer_token", &"[redacted]")
            .field("local_human_member_id", &self.local_human_member_id)
            .finish()
    }
}

/// One durable pre-room redemption triple (v1.2 amendment, table 7): the
/// daemon persists `{redemption_id, bearer, invite_code}` BEFORE calling
/// Bedrock redeem so a crash between redeem and promote can replay the exact
/// request. Same custody class as [`RoomCredential`]: `Debug` redacts BOTH
/// secrets, and the type deliberately does not implement `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingRedemption {
    /// Daemon-minted lowercase UUID identifying one redemption exchange.
    pub redemption_id: String,
    /// Daemon-private locally generated bearer secret.
    pub bearer_token: String,
    /// Daemon-private invite code — required to retry the exact request.
    pub invite_code: String,
    /// When the triple was first persisted.
    pub created_at: DateTime<Utc>,
}

impl std::fmt::Debug for PendingRedemption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRedemption")
            .field("redemption_id", &self.redemption_id)
            .field("bearer_token", &"[redacted]")
            .field("invite_code", &"[redacted]")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// One Bedrock-confirmed ledger row, normalized for ingest (P2-A). The
/// caller (P2-B supervisor) maps author identity from the current member
/// projection before calling; the store records exactly what it is given.
#[derive(Debug, Clone)]
pub struct ConfirmedEvent {
    /// Bedrock ledger event id — the ingest dedup key.
    pub ledger_event_id: String,
    /// Bedrock global ledger sequence; gaps allowed, must strictly increase
    /// within the room.
    pub global_sequence: u64,
    /// Producer stream id —
    /// `room:<room_id>:member:<member_id>:producer:<instance>`.
    pub source_id: String,
    /// Monotonic counter within that producer stream.
    pub source_sequence: u64,
    /// Client-assigned idempotency key set by the posting daemon.
    pub client_event_id: String,
    /// Non-secret public attribution id of the author's owning human.
    pub origin_principal_id: String,
    /// Opaque Bedrock member id of the author.
    pub origin_member_id: String,
    /// Author id mapped from the current member projection by the caller.
    pub author_id: String,
    /// Author kind derived from the same projection (drives trigger gating).
    pub author_kind: RoomParticipantKind,
    /// Transcript message kind (usually `Message`).
    pub kind: RoomMessageKind,
    /// Message body.
    pub body: String,
    /// Candidate opaque target member ids for trigger claims. Only targets
    /// with a current local binding are claimed; agent-authored rows produce
    /// no claims regardless of this list.
    pub trigger_targets: Vec<String>,
}

/// The committed result of a successful confirmed ingest (P2-A).
#[derive(Debug)]
pub struct IngestedCommit {
    /// The appended federated transcript message.
    pub message: RoomMessage,
    /// Opaque target member ids whose claim row committed now (first time
    /// only — replay/reconnect cannot re-claim).
    pub claimed_trigger_targets: Vec<String>,
}

/// Outcome of [`SqliteRoomStore::ingest_confirmed_event`].
#[derive(Debug)]
pub enum IngestOutcome {
    /// The row committed: exactly one federated transcript message plus the
    /// trigger targets newly claimed in the same transaction — the caller
    /// dispatches each claimed target once. Boxed to keep the enum small.
    Ingested(Box<IngestedCommit>),
    /// The ledger id was already ingested with identical metadata: no new
    /// transcript row, no cursor move, no outbox change, no trigger claims.
    Duplicate,
}

impl SqliteRoomStore {
    /// Read the stable daemon instance id, minting it on first use (P2-A).
    /// One row guarded by `CHECK (singleton = 1)`; the id is a random v4 UUID
    /// minted inside the same `IMMEDIATE` transaction that proves absence.
    pub fn federation_instance_id(&mut self) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = Self::federation_instance_id_on(&tx)?;
        tx.commit()?;
        Ok(id)
    }

    /// Shared instance-id read/mint on an existing transaction/connection.
    fn federation_instance_id_on(conn: &Connection) -> Result<String> {
        let existing: Option<String> = conn
            .query_row(
                "SELECT instance_id FROM federation_instance WHERE singleton = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = new_uuid_v4(conn)?;
        conn.execute(
            "INSERT INTO federation_instance (singleton, instance_id) VALUES (1, ?1)",
            params![id],
        )?;
        Ok(id)
    }

    /// Install (or replace) the room's one federation credential (P2-A). The
    /// bearer is stored but never projected; the room must exist.
    pub fn install_room_credential(
        &mut self,
        key: &RoomKey,
        bearer_token: &str,
        local_human_member_id: &str,
    ) -> Result<()> {
        if bearer_token.is_empty() {
            return Err(RoomStoreError::Encode("empty bearer token".into()));
        }
        if local_human_member_id.is_empty() {
            return Err(RoomStoreError::Encode("empty local human member id".into()));
        }
        if !self.room_exists(key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        self.conn.execute(
            "INSERT INTO room_federation (room_id, bearer_token, local_human_member_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(room_id) DO UPDATE SET
               bearer_token = excluded.bearer_token,
               local_human_member_id = excluded.local_human_member_id",
            params![key.as_str(), bearer_token, local_human_member_id],
        )?;
        Ok(())
    }

    /// Read the room's federation credential for daemon network use (P2-A).
    /// `None` when the room has no credential row. Never log, project, or
    /// serialize the returned bearer.
    pub fn room_credential(&self, key: &RoomKey) -> Result<Option<RoomCredential>> {
        let row = self
            .conn
            .query_row(
                "SELECT bearer_token, local_human_member_id
                 FROM room_federation WHERE room_id = ?1",
                params![key.as_str()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(
            row.map(|(bearer_token, local_human_member_id)| RoomCredential {
                room_id: key.clone(),
                bearer_token,
                local_human_member_id,
            }),
        )
    }

    /// Revoke (delete) the room's federation credential (P2-A). Returns
    /// `true` when a credential existed. Does not touch the access
    /// projection — persisting `Revoked` there is the supervisor's separate
    /// step (`update_room_access_safe`).
    pub fn revoke_room_credential(&mut self, key: &RoomKey) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM room_federation WHERE room_id = ?1",
            params![key.as_str()],
        )?;
        Ok(n > 0)
    }

    /// List every credentialed room for startup recovery (P2-A). The result
    /// carries private bearers — daemon-internal use only; never serialize.
    pub fn list_credentialed_rooms(&self) -> Result<Vec<RoomCredential>> {
        let mut stmt = self.conn.prepare(
            "SELECT room_id, bearer_token, local_human_member_id
             FROM room_federation ORDER BY room_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, bearer_token, local_human_member_id)| RoomCredential {
                room_id: RoomKey::new(id),
                bearer_token,
                local_human_member_id,
            })
            .collect())
    }

    /// Atomic get-or-insert of one pre-room redemption triple, keyed by
    /// `invite_code` (v1.2 amendment §1). One IMMEDIATE transaction:
    ///
    /// - no row for this code ⇒ persist the caller-supplied triple and
    ///   return it with `fresh = true`;
    /// - existing row for this code ⇒ return the STORED exact triple with
    ///   `fresh = false`; the caller-supplied `redemption_id`/`bearer` are
    ///   discarded — one invite code can never fork into two triples.
    ///
    /// A duplicate `redemption_id` under a DIFFERENT code still fails closed
    /// (primary key).
    pub fn get_or_insert_pending_redemption(
        &mut self,
        invite_code: &str,
        redemption_id: &str,
        bearer_token: &str,
        now: DateTime<Utc>,
    ) -> Result<(PendingRedemption, bool)> {
        if invite_code.is_empty() || redemption_id.is_empty() || bearer_token.is_empty() {
            return Err(RoomStoreError::Encode(
                "empty invite code, redemption id, or bearer token".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT redemption_id, bearer_token, created_at
                 FROM pending_redemptions WHERE invite_code = ?1",
                params![invite_code],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((stored_id, stored_bearer, created)) = existing {
            tx.commit()?;
            return Ok((
                PendingRedemption {
                    redemption_id: stored_id,
                    bearer_token: stored_bearer,
                    invite_code: invite_code.to_string(),
                    created_at: parse_ts(&created)?,
                },
                false,
            ));
        }
        tx.execute(
            "INSERT INTO pending_redemptions (redemption_id, bearer_token, invite_code, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![redemption_id, bearer_token, invite_code, fmt_ts(now)],
        )?;
        tx.commit()?;
        Ok((
            PendingRedemption {
                redemption_id: redemption_id.to_string(),
                bearer_token: bearer_token.to_string(),
                invite_code: invite_code.to_string(),
                created_at: now,
            },
            true,
        ))
    }

    /// All pending redemptions, for startup recovery (v1.1 amendment). Full
    /// private rows: restart must recover the exact `{code, redemption_id,
    /// token}` triple to replay the redeem request.
    pub fn list_pending_redemptions(&self) -> Result<Vec<PendingRedemption>> {
        let mut stmt = self.conn.prepare(
            "SELECT redemption_id, bearer_token, invite_code, created_at
             FROM pending_redemptions ORDER BY redemption_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(redemption_id, bearer_token, invite_code, created)| {
                Ok(PendingRedemption {
                    redemption_id,
                    bearer_token,
                    invite_code,
                    created_at: parse_ts(&created)?,
                })
            })
            .collect()
    }

    /// Promote one pending redemption to a room credential (v1.2 amendment
    /// §2). Exact inputs: `(redemption_id, room, bearer, local_human_member_id)`.
    /// One IMMEDIATE transaction, all-or-nothing:
    ///
    /// - pending row exists AND its redemption_id+bearer match ⇒ install the
    ///   room credential and delete the pending row; returns `true`;
    /// - pending row missing AND the room is credentialed with the SAME
    ///   bearer AND same member ⇒ idempotent no-op (a prior promote committed
    ///   but the response/process was lost); returns `false`;
    /// - any other state ⇒ corruption, fail closed, no partial write. Error
    ///   messages never carry the bearer or invite code.
    pub fn promote_pending_redemption(
        &mut self,
        redemption_id: &str,
        key: &RoomKey,
        bearer_token: &str,
        local_human_member_id: &str,
    ) -> Result<bool> {
        if redemption_id.is_empty() || bearer_token.is_empty() || local_human_member_id.is_empty() {
            return Err(RoomStoreError::Encode(
                "empty redemption id, bearer token, or local human member id".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let pending_bearer: Option<String> = tx
            .query_row(
                "SELECT bearer_token FROM pending_redemptions WHERE redemption_id = ?1",
                params![redemption_id],
                |r| r.get(0),
            )
            .optional()?;
        let credential: Option<(String, String)> = tx
            .query_row(
                "SELECT bearer_token, local_human_member_id
                 FROM room_federation WHERE room_id = ?1",
                params![key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match pending_bearer {
            Some(stored_bearer) => {
                if stored_bearer != bearer_token {
                    return Err(RoomStoreError::FederationCorruption(format!(
                        "pending redemption {redemption_id} bearer does not match promote input"
                    )));
                }
                if let Some((cred_bearer, cred_member)) = credential {
                    if cred_bearer != bearer_token || cred_member != local_human_member_id {
                        return Err(RoomStoreError::FederationCorruption(format!(
                            "room {} already credentialed with different values",
                            key.as_str()
                        )));
                    }
                }
                tx.execute(
                    "INSERT INTO room_federation (room_id, bearer_token, local_human_member_id)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(room_id) DO UPDATE SET
                       bearer_token = excluded.bearer_token,
                       local_human_member_id = excluded.local_human_member_id",
                    params![key.as_str(), bearer_token, local_human_member_id],
                )?;
                tx.execute(
                    "DELETE FROM pending_redemptions WHERE redemption_id = ?1",
                    params![redemption_id],
                )?;
                tx.commit()?;
                Ok(true)
            }
            None => match credential {
                Some((cred_bearer, cred_member))
                    if cred_bearer == bearer_token && cred_member == local_human_member_id =>
                {
                    tx.commit()?;
                    Ok(false)
                }
                Some(_) => Err(RoomStoreError::FederationCorruption(format!(
                    "room {} credentialed with different values than redemption {redemption_id}",
                    key.as_str()
                ))),
                None => Err(RoomStoreError::FederationCorruption(format!(
                    "redemption {redemption_id} has no pending row and room {} has no credential",
                    key.as_str()
                ))),
            },
        }
    }

    /// Remove one pending redemption without touching any room credential
    /// (v1.1 amendment) — terminal 403 / second-consumer cleanup. Returns
    /// `true` when a row existed.
    pub fn remove_pending_redemption(&mut self, redemption_id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM pending_redemptions WHERE redemption_id = ?1",
            params![redemption_id],
        )?;
        Ok(n > 0)
    }

    /// Replace the room's SAFE projection fields — state, member roster,
    /// confirmed cursor — WITHOUT touching outbox rows (P2-A). This is the
    /// production heartbeat/reconnect refresh path; `replace_room_access`
    /// remains test seeding only because it rewrites the outbox.
    ///
    /// Upserts when no access row exists yet (supervisor bootstrap): missing
    /// fields default to `Connecting`, empty roster, no cursor. `cursor` only
    /// ever advances — a lower or equal value is ignored, because the
    /// confirmed-ingest transaction is the cursor authority.
    pub fn update_room_access_safe(
        &mut self,
        key: &RoomKey,
        state: Option<RoomAccessState>,
        members: Option<&[FederatedRoomMemberProjection]>,
        cursor: Option<u64>,
    ) -> Result<RoomAccessProjection> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let row = tx
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
        let (cur_state, cur_seq, cur_members) = match row {
            Some((s, q, m)) => (s, q, m),
            None => ("connecting".to_string(), None, "[]".to_string()),
        };
        let new_state = match state {
            Some(st) => {
                let s = serde_json::to_string(&st)
                    .map_err(|e| RoomStoreError::Encode(format!("state serialize: {e}")))?;
                s.trim_matches('"').to_string()
            }
            None => cur_state,
        };
        let new_members = match members {
            Some(ms) => serde_json::to_string(ms)
                .map_err(|e| RoomStoreError::Encode(format!("members serialize: {e}")))?,
            None => cur_members,
        };
        let new_seq = match (cursor, cur_seq) {
            (Some(c), Some(cur)) => {
                let cur_v = parse_canonical_u64_text(&cur)?;
                if c > cur_v {
                    Some(write_u64_text(c))
                } else {
                    Some(cur)
                }
            }
            (Some(c), None) => Some(write_u64_text(c)),
            (None, cur) => cur,
        };
        tx.execute(
            "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
               state = excluded.state,
               confirmed_sequence = excluded.confirmed_sequence,
               member_projection = excluded.member_projection",
            params![key.as_str(), new_state, new_seq, new_members],
        )?;
        tx.commit()?;
        self.room_access(key)
    }

    /// Bind one opaque Bedrock member to one local folder-agent name (P2-A).
    /// `registration_key` is stored privately and never projected — P2-A
    /// stores the column opaquely; the deterministic derivation is frozen for
    /// P2-C (v1.2 amendment §3). Retried registration semantics (v1.1 §1b):
    /// an upsert with the identical `(room_id, member_id, agent_name,
    /// registration_key)` tuple is an idempotent no-op; the same
    /// `(room_id, member_id)` with a different agent or key fails closed.
    /// Binding a second member to an already-bound agent name fails (unique
    /// per room). Rebinding requires an explicit unbind first.
    pub fn bind_room_agent(
        &mut self,
        key: &RoomKey,
        member_id: &str,
        agent_name: &str,
        registration_key: &str,
    ) -> Result<()> {
        if member_id.is_empty() || agent_name.is_empty() {
            return Err(RoomStoreError::Encode(
                "empty member id or agent name".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT agent_name, registration_key FROM room_member_bindings
                 WHERE room_id = ?1 AND member_id = ?2",
                params![key.as_str(), member_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((cur_agent, cur_key)) = existing {
            if cur_agent == agent_name && cur_key == registration_key {
                tx.commit()?;
                return Ok(()); // identical tuple: idempotent retried registration
            }
            return Err(RoomStoreError::FederationCorruption(format!(
                "member {member_id} already bound with a different agent or key in room {}",
                key.as_str()
            )));
        }
        tx.execute(
            "INSERT INTO room_member_bindings (room_id, member_id, agent_name, registration_key)
             VALUES (?1, ?2, ?3, ?4)",
            params![key.as_str(), member_id, agent_name, registration_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Remove one opaque-member binding (P2-A). Returns `true` when a row
    /// existed.
    pub fn unbind_room_agent(&mut self, key: &RoomKey, member_id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM room_member_bindings WHERE room_id = ?1 AND member_id = ?2",
            params![key.as_str(), member_id],
        )?;
        Ok(n > 0)
    }

    /// Resolve one opaque member to its local folder-agent name (P2-A).
    /// `None` when unbound. Never returns the registration key.
    pub fn resolve_room_agent(&self, key: &RoomKey, member_id: &str) -> Result<Option<String>> {
        let name = self
            .conn
            .query_row(
                "SELECT agent_name FROM room_member_bindings
                 WHERE room_id = ?1 AND member_id = ?2",
                params![key.as_str(), member_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(name)
    }

    /// Allocate the next producer sequence and insert one Pending outbox row
    /// in a single `IMMEDIATE` transaction (P2-A). No transcript row is
    /// written and no trigger fires — federation intents live in the outbox
    /// until Bedrock confirms them through the ordered ingest rail.
    ///
    /// The allocated item's `source_id` is exactly
    /// `room:<room_id>:member:<member_id>:producer:<instance_uuid>`. The
    /// counter is one canonical-decimal u64 per (room, producer member),
    /// starting at 1; exhausting `u64::MAX` fails closed rather than reusing
    /// a value. Concurrent callers on the same DB file are serialized by the
    /// `IMMEDIATE` write lock, so no sequence is ever allocated twice.
    pub fn allocate_outbox_pending(
        &mut self,
        key: &RoomKey,
        author_member_id: &str,
        client_event_id: &str,
        event_type: &str,
        payload: serde_json::Value,
        mention_member_ids: Vec<String>,
    ) -> Result<RoomOutboxItem> {
        if author_member_id.is_empty() || client_event_id.is_empty() {
            return Err(RoomStoreError::Encode(
                "empty author member id or client event id".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let instance_id = Self::federation_instance_id_on(&tx)?;
        let cur: Option<String> = tx
            .query_row(
                "SELECT next_sequence FROM producer_counters
                 WHERE room_id = ?1 AND author_member_id = ?2",
                params![key.as_str(), author_member_id],
                |r| r.get(0),
            )
            .optional()?;
        let next = match cur {
            None => 1u64,
            Some(t) => parse_canonical_u64_text(&t)?,
        };
        let after = next.checked_add(1).ok_or_else(|| {
            RoomStoreError::FederationCorruption("producer counter exhausted at u64::MAX".into())
        })?;
        tx.execute(
            "INSERT INTO producer_counters (room_id, author_member_id, next_sequence)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(room_id, author_member_id) DO UPDATE SET
               next_sequence = excluded.next_sequence",
            params![key.as_str(), author_member_id, write_u64_text(after)],
        )?;
        let pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM outbox WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        let item = RoomOutboxItem {
            client_event_id: client_event_id.to_string(),
            source_id: format!(
                "room:{}:member:{}:producer:{}",
                key.as_str(),
                author_member_id,
                instance_id
            ),
            source_sequence: next,
            author_member_id: author_member_id.to_string(),
            event_type: event_type.to_string(),
            payload,
            mention_member_ids,
            state: OutboxItemState::Pending,
        };
        Self::insert_outbox_item_on(&tx, key, &item, pos as usize)?;
        tx.commit()?;
        Ok(item)
    }

    /// List the room's Pending outbox rows in stable producer order (P2-A) —
    /// durable `position` order, which is allocation order.
    pub fn pending_outbox(&self, key: &RoomKey) -> Result<Vec<RoomOutboxItem>> {
        let mut out = self.load_outbox_for_room(key)?;
        out.retain(|i| i.state == OutboxItemState::Pending);
        Ok(out)
    }

    /// Mark one Pending outbox row Failed (P2-A). Only the state column
    /// changes; the producer tuple and content are preserved exactly.
    /// Returns `true` when a Pending row transitioned; `false` when the row
    /// is missing or not Pending (an already-Failed row stays Failed).
    pub fn fail_outbox_pending(&mut self, key: &RoomKey, client_event_id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE outbox SET state = 'failed'
             WHERE room_id = ?1 AND client_event_id = ?2 AND state = 'pending'",
            params![key.as_str(), client_event_id],
        )?;
        Ok(n > 0)
    }

    /// Atomically ingest one Bedrock-confirmed row (P2-A) — the ONLY writer
    /// of federated transcript rows. One `IMMEDIATE` transaction performs,
    /// in order:
    ///
    /// 1. dedup on `ledger_event_id` — identical metadata ⇒ `Duplicate`;
    ///    same id with different metadata ⇒ corruption;
    /// 2. strict monotonic `global_sequence` within the room — a lower or
    ///    equal sequence under a new ledger id ⇒ corruption (gaps allowed);
    /// 3. append exactly one federated transcript row;
    /// 4. record the confirmed-event dedup/index row;
    /// 5. delete the matching local outbox row — matched on the FULL
    ///    producer tuple (`client_event_id` + `source_id` +
    ///    `source_sequence`), never on `client_event_id` alone;
    /// 6. advance the confirmed cursor in `room_access` (the room must be
    ///    federated — no access row fails closed and rolls everything back);
    /// 7. claim candidate trigger targets that have a current local binding.
    ///    Claims and the message commit together; agent-authored rows claim
    ///    nothing; the PK on `(room, ledger_event_id, target_member_id)`
    ///    makes replay/reconnect claims no-ops.
    ///
    /// Any failure rolls back every step as a unit.
    pub fn ingest_confirmed_event(
        &mut self,
        key: &RoomKey,
        event: &ConfirmedEvent,
        now: DateTime<Utc>,
    ) -> Result<IngestOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let room_exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if room_exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }

        // The access row is the federation marker: no row ⇒ not federated ⇒
        // fail closed before any write. Its persisted cursor also joins the
        // ordering baseline below (recovery/bootstrap can set it ahead of the
        // local index).
        let access_cursor: Option<Option<String>> = tx
            .query_row(
                "SELECT confirmed_sequence FROM room_access WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(cursor_txt) = access_cursor else {
            return Err(RoomStoreError::RoomNotFederated(key.clone()));
        };
        let cursor = match cursor_txt {
            None => None,
            Some(t) => Some(parse_canonical_u64_text(&t)?),
        };

        let meta = FederatedMessageMeta {
            ledger_event_id: event.ledger_event_id.clone(),
            global_sequence: event.global_sequence,
            source_id: event.source_id.clone(),
            source_sequence: event.source_sequence,
            client_event_id: event.client_event_id.clone(),
            origin_principal_id: event.origin_principal_id.clone(),
            origin_member_id: event.origin_member_id.clone(),
        };

        // 1. Dedup on ledger id against BOTH persisted copies: the index row
        //    tuple AND the transcript's parsed `FederatedMessageMeta` — every
        //    field, never raw JSON bytes, never a column subset. Duplicate is
        //    returned only when index == transcript == incoming; an index row
        //    that disagrees with its own transcript row (either side
        //    corrupted), or transcript metadata that is missing or unreadable,
        //    ⇒ corruption.
        type IndexTuple = (i64, String, String, String, String);
        let prior: Option<IndexTuple> = tx
            .query_row(
                "SELECT local_seq, global_sequence, source_id, source_sequence, client_event_id
                 FROM federated_events
                 WHERE room_id = ?1 AND ledger_event_id = ?2",
                params![key.as_str(), event.ledger_event_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        if let Some((local_seq, idx_gs, idx_sid, idx_sseq, idx_ceid)) = prior {
            let idx_gs = parse_canonical_u64_text(&idx_gs)?;
            let idx_sseq = parse_canonical_u64_text(&idx_sseq)?;
            let stored_json: Option<Option<String>> = tx
                .query_row(
                    "SELECT federated FROM messages WHERE room_id = ?1 AND seq = ?2",
                    params![key.as_str(), local_seq],
                    |r| r.get(0),
                )
                .optional()?;
            let stored_json = stored_json.flatten().ok_or_else(|| {
                RoomStoreError::FederationCorruption(format!(
                    "indexed confirmed event {} has no persisted federated transcript metadata",
                    event.ledger_event_id
                ))
            })?;
            let stored: FederatedMessageMeta = serde_json::from_str(&stored_json).map_err(|e| {
                RoomStoreError::FederationCorruption(format!(
                    "persisted federated metadata for ledger event {} is unreadable: {e}",
                    event.ledger_event_id
                ))
            })?;
            let index_matches_transcript = idx_gs == stored.global_sequence
                && idx_sid == stored.source_id
                && idx_sseq == stored.source_sequence
                && idx_ceid == stored.client_event_id;
            if !index_matches_transcript {
                return Err(RoomStoreError::FederationCorruption(format!(
                    "index and transcript metadata diverge for ledger event {}",
                    event.ledger_event_id
                )));
            }
            if stored == meta {
                return Ok(IngestOutcome::Duplicate);
            }
            return Err(RoomStoreError::FederationCorruption(format!(
                "ledger event {} re-ingested with different metadata",
                event.ledger_event_id
            )));
        }

        // 2. Strict monotonic global sequence against BOTH baselines: the
        //    last indexed row (found by ORDER BY local_seq — never ORDER/MAX
        //    the canonical-decimal TEXT column, lexicographic ≠ numeric) and
        //    the persisted access cursor. A cursor set ahead of the local
        //    index (bootstrap/recovery) must reject stale lower sequences.
        let last_gs: Option<String> = tx
            .query_row(
                "SELECT global_sequence FROM federated_events
                 WHERE room_id = ?1 ORDER BY local_seq DESC LIMIT 1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let last_indexed = match last_gs {
            None => None,
            Some(t) => Some(parse_canonical_u64_text(&t)?),
        };
        let baseline = match (last_indexed, cursor) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if let Some(last) = baseline {
            if event.global_sequence <= last {
                return Err(RoomStoreError::FederationCorruption(format!(
                    "global sequence {} not after last confirmed {last}",
                    event.global_sequence
                )));
            }
        }

        // 3. Append exactly one federated transcript row.
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE room_id = ?1",
            params![key.as_str()],
            |r| r.get(0),
        )?;
        let federated_json = serde_json::to_string(&meta)
            .map_err(|e| RoomStoreError::Encode(format!("federated serialize: {e}")))?;
        tx.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at, federated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                key.as_str(),
                next_seq,
                event.author_id,
                encode_participant_kind(event.author_kind),
                encode_message_kind(event.kind),
                event.body,
                fmt_ts(now),
                federated_json,
            ],
        )?;

        // 4. Record the confirmed-event dedup/index row. The UNIQUE index on
        //    (room_id, global_sequence) backstops the monotonic rule.
        tx.execute(
            "INSERT INTO federated_events (room_id, ledger_event_id, global_sequence,
                                           local_seq, source_id, source_sequence, client_event_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key.as_str(),
                event.ledger_event_id,
                write_u64_text(event.global_sequence),
                next_seq,
                event.source_id,
                write_u64_text(event.source_sequence),
                event.client_event_id,
            ],
        )?;

        // 5. Delete the matching local outbox row — FULL producer tuple only.
        tx.execute(
            "DELETE FROM outbox
             WHERE room_id = ?1 AND client_event_id = ?2 AND source_id = ?3
               AND source_sequence = ?4",
            params![
                key.as_str(),
                event.client_event_id,
                event.source_id,
                write_u64_text(event.source_sequence),
            ],
        )?;

        // 6. Advance the confirmed cursor. The access row was proven to exist
        //    up front, and the ordering gate guarantees gs > cursor, so this
        //    can never regress. The zero-row check is defense in depth.
        let cursor_rows = tx.execute(
            "UPDATE room_access SET confirmed_sequence = ?2 WHERE room_id = ?1",
            params![key.as_str(), write_u64_text(event.global_sequence)],
        )?;
        if cursor_rows == 0 {
            return Err(RoomStoreError::RoomNotFederated(key.clone()));
        }
        Self::touch_on(&tx, key, now)?;

        // 7. Claim locally-bound trigger targets; agent-authored rows claim
        //    nothing. INSERT OR IGNORE + PK ⇒ replay cannot claim twice.
        let mut claimed = Vec::new();
        if event.author_kind != RoomParticipantKind::Agent {
            for target in &event.trigger_targets {
                let bound: Option<String> = tx
                    .query_row(
                        "SELECT agent_name FROM room_member_bindings
                         WHERE room_id = ?1 AND member_id = ?2",
                        params![key.as_str(), target],
                        |r| r.get(0),
                    )
                    .optional()?;
                if bound.is_none() {
                    continue;
                }
                let n = tx.execute(
                    "INSERT OR IGNORE INTO processed_room_triggers
                       (room_id, ledger_event_id, target_member_id, claimed_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![key.as_str(), event.ledger_event_id, target, fmt_ts(now)],
                )?;
                if n > 0 {
                    claimed.push(target.clone());
                }
            }
        }

        tx.commit()?;
        let message = RoomMessage {
            seq: next_seq as u64,
            author_id: event.author_id.clone(),
            author_kind: event.author_kind,
            kind: event.kind,
            body: event.body.clone(),
            created_at: now,
            federated: Some(meta),
            thread_parent_seq: None,
            session_id: None,
        };
        Ok(IngestOutcome::Ingested(Box::new(IngestedCommit {
            message,
            claimed_trigger_targets: claimed,
        })))
    }
}

/// Mint a random v4 UUID using SQLite's CSPRNG (`randomblob`) — no new crate
/// dependencies (the frozen P2-A file scope excludes Cargo manifests).
fn new_uuid_v4(conn: &Connection) -> Result<String> {
    let blob: Vec<u8> = conn.query_row("SELECT randomblob(16)", [], |r| r.get(0))?;
    let mut b = [0u8; 16];
    b.copy_from_slice(&blob[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 0b10
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

/// Enforce owner-only `0600` on the DB file and every SQLite sidecar (`-wal`,
/// `-shm`, `-journal`) — the credential-custody pin (P2-A). Runs BEFORE any
/// DB work on open and again after create/migration, so a previously loosened
/// mode is repaired, never merely observed. Fails closed on any filesystem
/// error except `NotFound` (a sidecar may legitimately not exist). Unix only;
/// other platforms are a no-op (the freeze pins Unix behavior).
#[cfg(unix)]
fn enforce_owner_only_db_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut candidates = vec![path.to_path_buf()];
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    for suffix in ["wal", "shm", "journal"] {
        candidates.push(path.with_file_name(format!("{file_name}-{suffix}")));
    }
    for p in candidates {
        match std::fs::metadata(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(RoomStoreError::Io(e)),
            Ok(md) => {
                if md.is_file() && md.permissions().mode() & 0o777 != 0o600 {
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
                        .map_err(RoomStoreError::Io)?;
                }
            }
        }
    }
    Ok(())
}

/// Non-Unix platforms: the credential-custody mode pin is Unix-specific.
#[cfg(not(unix))]
fn enforce_owner_only_db_mode(_path: &Path) -> Result<()> {
    Ok(())
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

/// Parse a u64 from its strict canonical decimal TEXT form.
///
/// Accepts ONLY `"0"` or a string matching `[1-9][0-9]*` that fits in `u64`.
/// Rejects: leading zeros (`"01"`), hex (`"0xFF"`), leading `+`/space,
/// empty strings, negative, and overflow (`> 18446744073709551615`).
/// SQL NULL stays `None` at the caller — this function never receives it.
fn parse_canonical_u64_text(raw: &str) -> Result<u64> {
    if raw.is_empty() {
        return Err(RoomStoreError::Encode("empty u64 text".into()));
    }
    let bytes = raw.as_bytes();
    if bytes[0] == b'-' {
        return Err(RoomStoreError::Encode(format!(
            "invalid u64 text: '{raw}' (negative)"
        )));
    }
    if bytes[0] == b'+' || bytes[0] == b' ' {
        return Err(RoomStoreError::Encode(format!(
            "invalid u64 text: '{raw}' (leading +/space)"
        )));
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return Err(RoomStoreError::Encode(format!(
            "invalid u64 text: '{raw}' (leading zero)"
        )));
    }
    // Every byte must be ASCII 0-9.
    for &b in bytes {
        if !b.is_ascii_digit() {
            return Err(RoomStoreError::Encode(format!(
                "invalid u64 text: '{raw}' (non-digit)"
            )));
        }
    }
    // Parse as u128 to catch overflow.
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

    // ── S2-P1 federation store tests (inherent APIs, tempfile proofs) ──────

    use std::error::Error;

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

    fn outbox_item(id: &str, state: OutboxItemState) -> RoomOutboxItem {
        RoomOutboxItem {
            client_event_id: id.into(),
            source_id: "src-1".into(),
            source_sequence: 1,
            author_member_id: "m1".into(),
            event_type: "chat.message".into(),
            payload: serde_json::json!({"text": "hello"}),
            mention_member_ids: vec![],
            state,
        }
    }

    /// Create a tempfile DB matching bab60e98: rooms (with workspace_root),
    /// participants, messages WITHOUT federated column; NO room_access or
    /// outbox tables. Inserts one room, participant, and message row that must
    /// survive migration with `federated` = NULL.
    fn bab60e98_tempfile_db() -> (tempfile::TempPath, RoomKey) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE rooms (
                 id             TEXT PRIMARY KEY,
                 name           TEXT NOT NULL,
                 trigger_policy TEXT,
                 workspace_root TEXT,
                 created_at     TEXT NOT NULL,
                 updated_at     TEXT NOT NULL,
                 closed_at      TEXT
             );
             CREATE TABLE participants (
                 room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                 id           TEXT NOT NULL,
                 kind         TEXT NOT NULL,
                 display_name TEXT NOT NULL,
                 position     INTEGER NOT NULL,
                 PRIMARY KEY (room_id, id)
             );
             CREATE TABLE messages (
                 room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                 seq         INTEGER NOT NULL,
                 author_id   TEXT NOT NULL,
                 author_kind TEXT NOT NULL,
                 kind        TEXT NOT NULL,
                 body        TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 PRIMARY KEY (room_id, seq)
             );
             ",
        )
        .unwrap();
        let key = RoomKey::new("legacy-room");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO rooms (id, name, created_at, updated_at) VALUES (?1, 'Legacy', ?2, ?2)",
            params![key.as_str(), now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO participants (room_id, id, kind, display_name, position)
             VALUES (?1, 'alice', 'human', 'Alice', 0)",
            params![key.as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
             VALUES (?1, 0, 'alice', 'human', 'message', 'hello legacy', ?2)",
            params![key.as_str(), now],
        )
        .unwrap();
        conn.close().unwrap();
        (tmp.into_temp_path(), key)
    }

    // ── migration: bab60e98 base survives open → migrate → reopen ──────────

    #[test]
    fn bab60e98_db_opens_migrates_and_preserves_all_rows() {
        let (path, key) = bab60e98_tempfile_db();

        // First open + migrate.
        let mut s = SqliteRoomStore::open(&path).unwrap();
        s.migrate().unwrap();
        // Second migrate must be idempotent.
        s.migrate().unwrap();

        // Room survived.
        let rec = s.get(&key).unwrap().expect("room must survive migration");
        assert_eq!(rec.room.name, "Legacy");

        // Participant survived.
        assert_eq!(rec.room.participants.len(), 1);
        assert_eq!(rec.room.participants[0].id, "alice");
        assert_eq!(rec.room.participants[0].display_name, "Alice");

        // Message survived with federated = None (column added, value NULL).
        let tx_page = s.transcript_page(&key, None, Some(10)).unwrap();
        assert_eq!(tx_page.messages.len(), 1);
        assert_eq!(tx_page.messages[0].body, "hello legacy");
        assert!(tx_page.messages[0].federated.is_none());

        // Close and reopen — everything must still be intact.
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();

        let rec2 = s2.get(&key).unwrap().expect("room must survive reopen");
        assert_eq!(rec2.room.name, "Legacy");

        let tx_page2 = s2.transcript_page(&key, None, Some(10)).unwrap();
        assert_eq!(tx_page2.messages.len(), 1);
        assert_eq!(tx_page2.messages[0].body, "hello legacy");
        assert!(tx_page2.messages[0].federated.is_none());
    }

    // ── no-position outbox: intermediate pre-S2-P1 schema ──────────────────

    /// Simulates a DB created after bab60e98 but before S2-P1: has
    /// room_access + outbox tables but outbox lacks the `position` column.
    fn no_position_outbox_tempfile_db() -> (tempfile::TempPath, RoomKey) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE rooms (
                 id             TEXT PRIMARY KEY,
                 name           TEXT NOT NULL,
                 trigger_policy TEXT,
                 workspace_root TEXT,
                 created_at     TEXT NOT NULL,
                 updated_at     TEXT NOT NULL,
                 closed_at      TEXT
             );
             CREATE TABLE participants (
                 room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                 id           TEXT NOT NULL,
                 kind         TEXT NOT NULL,
                 display_name TEXT NOT NULL,
                 position     INTEGER NOT NULL,
                 PRIMARY KEY (room_id, id)
             );
             CREATE TABLE messages (
                 room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                 seq         INTEGER NOT NULL,
                 author_id   TEXT NOT NULL,
                 author_kind TEXT NOT NULL,
                 kind        TEXT NOT NULL,
                 body        TEXT NOT NULL,
                 created_at  TEXT NOT NULL,
                 federated   TEXT,
                 PRIMARY KEY (room_id, seq)
             );
             CREATE TABLE room_access (
                 room_id             TEXT PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
                 state               TEXT NOT NULL,
                 confirmed_sequence  TEXT,
                 member_projection   TEXT NOT NULL
             );
             -- outbox WITHOUT position column
             CREATE TABLE outbox (
                 room_id            TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                 client_event_id    TEXT NOT NULL,
                 source_id          TEXT NOT NULL,
                 source_sequence    TEXT NOT NULL,
                 author_member_id   TEXT NOT NULL,
                 event_type         TEXT NOT NULL,
                 payload            TEXT NOT NULL,
                 mention_member_ids TEXT NOT NULL,
                 state              TEXT NOT NULL,
                 PRIMARY KEY (room_id, client_event_id)
             );
             CREATE INDEX idx_outbox_room_state ON outbox(room_id, state);
             ",
        )
        .unwrap();
        let key = RoomKey::new("no-pos-room");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO rooms (id, name, created_at, updated_at) VALUES (?1, 'NoPos', ?2, ?2)",
            params![key.as_str(), now],
        )
        .unwrap();
        // Insert a room_access row so the outbox is visible in the projection.
        conn.execute(
            "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
             VALUES (?1, 'live', NULL, '[]')",
            params![key.as_str()],
        )
        .unwrap();
        // Insert 3 outbox rows with distinct client_event_ids so the
        // deterministic position backfill can be proven.
        conn.execute(
            "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                 author_member_id, event_type, payload, mention_member_ids, state)
             VALUES (?1, 'evt-a', 'src-a', '10', 'm-a', 'chat.message',
                     '{\"text\":\"a\"}', '[]', 'pending')",
            params![key.as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                 author_member_id, event_type, payload, mention_member_ids, state)
             VALUES (?1, 'evt-b', 'src-b', '20', 'm-b', 'chat.message',
                     '{\"text\":\"b\"}', '[]', 'failed')",
            params![key.as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                 author_member_id, event_type, payload, mention_member_ids, state)
             VALUES (?1, 'evt-c', 'src-c', '30', 'm-c', 'chat.message',
                     '{\"text\":\"c\"}', '[]', 'pending')",
            params![key.as_str()],
        )
        .unwrap();
        conn.close().unwrap();
        (tmp.into_temp_path(), key)
    }

    #[test]
    fn no_position_outbox_db_migrates_and_preserves_rows() {
        let (path, key) = no_position_outbox_tempfile_db();

        let mut s = SqliteRoomStore::open(&path).unwrap();
        s.migrate().unwrap();
        s.migrate().unwrap(); // idempotent

        let rec = s.get(&key).unwrap().expect("room must survive migration");
        assert_eq!(rec.room.name, "NoPos");

        // All 3 outbox rows survived with deterministic positions
        // (client_event_id ordering: evt-a → 0, evt-b → 1, evt-c → 2).
        let proj = s.room_access(&key).unwrap();
        assert_eq!(proj.outbox.len(), 3);
        assert_eq!(proj.outbox[0].client_event_id, "evt-a");
        assert_eq!(proj.outbox[0].source_sequence, 10);
        assert_eq!(proj.outbox[0].state, OutboxItemState::Pending);
        assert_eq!(proj.outbox[1].client_event_id, "evt-b");
        assert_eq!(proj.outbox[1].state, OutboxItemState::Failed);
        assert_eq!(proj.outbox[2].client_event_id, "evt-c");
        assert_eq!(proj.outbox[2].state, OutboxItemState::Pending);

        // Close and reopen — all 3 rows, same deterministic order.
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();

        let reopened = s2.room_access(&key).unwrap();
        assert_eq!(reopened.outbox.len(), 3);
        assert_eq!(reopened.outbox[0].client_event_id, "evt-a");
        assert_eq!(reopened.outbox[1].client_event_id, "evt-b");
        assert_eq!(reopened.outbox[2].client_event_id, "evt-c");
        assert_eq!(reopened.outbox[1].state, OutboxItemState::Failed);
    }

    // ── tempfile close/reopen: full projection + u64::MAX round-trips ──────

    fn tempfile_store() -> (tempfile::TempPath, SqliteRoomStore) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut s = SqliteRoomStore::open(tmp.path()).unwrap();
        s.migrate().unwrap();
        (tmp.into_temp_path(), s)
    }

    #[test]
    fn projection_survives_close_reopen_u64_max() {
        let (path, mut s) = tempfile_store();
        let key = RoomKey::new("r-persist");
        s.create(key.clone(), "Persist", None, now()).unwrap();

        let proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(u64::MAX),
            members: vec![member_proj("m1", "Alice")],
            outbox: vec![RoomOutboxItem {
                client_event_id: "evt-max".into(),
                source_id: "src-max".into(),
                source_sequence: u64::MAX,
                author_member_id: "auth-max".into(),
                event_type: "chat.message".into(),
                payload: serde_json::json!({"n": 42}),
                mention_member_ids: vec!["m2".into()],
                state: OutboxItemState::Pending,
            }],
        };
        s.replace_room_access(&key, &proj).unwrap();

        // Close and reopen.
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();

        let loaded = s2.room_access(&key).unwrap();
        assert_eq!(loaded.state, RoomAccessState::Live);
        assert_eq!(loaded.last_confirmed_global_sequence, Some(u64::MAX));
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(loaded.members[0].display_name, "Alice");
        assert_eq!(loaded.outbox.len(), 1);
        let item = &loaded.outbox[0];
        assert_eq!(item.client_event_id, "evt-max");
        assert_eq!(item.source_sequence, u64::MAX);
        assert_eq!(item.author_member_id, "auth-max");
        assert_eq!(item.payload, serde_json::json!({"n": 42}));
        assert_eq!(item.mention_member_ids, vec!["m2"]);
        assert_eq!(item.state, OutboxItemState::Pending);
    }

    // ── federated writer: multi-page metadata persistence ──────────────────

    #[test]
    fn federated_messages_span_pages_with_metadata() {
        let (path, mut s) = tempfile_store();
        let key = RoomKey::new("r-fed-pages");
        s.create(key.clone(), "FedPages", None, now()).unwrap();

        // Write 25 federated messages — enough to span multiple transcript
        // pages (default cap is 20).
        for i in 0..25u64 {
            let meta = FederatedMessageMeta {
                ledger_event_id: format!("ledger-{i}"),
                global_sequence: 1000 + i,
                source_id: format!("src-{i}"),
                source_sequence: i,
                client_event_id: format!("cevt-{i}"),
                origin_principal_id: "principal-1".into(),
                origin_member_id: format!("m-{i}"),
            };
            s.append_federated_message(
                &key,
                &format!("author-{i}"),
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                &format!("message {i}"),
                &meta,
                now(),
            )
            .unwrap();
        }
        // Read page 1.
        let p1 = s.transcript_page(&key, None, Some(10)).unwrap();
        assert_eq!(p1.messages.len(), 10);
        assert!(p1.next_seq.is_some());
        for (j, msg) in p1.messages.iter().enumerate() {
            let fm = msg.federated.as_ref().unwrap();
            assert_eq!(fm.global_sequence, 1000 + j as u64);
            assert_eq!(fm.origin_member_id, format!("m-{j}"));
        }
        // Read page 2.
        let p2 = s.transcript_page(&key, p1.next_seq, Some(10)).unwrap();
        assert_eq!(p2.messages.len(), 10);
        for (j, msg) in p2.messages.iter().enumerate() {
            let fm = msg.federated.as_ref().unwrap();
            assert_eq!(fm.global_sequence, 1010 + j as u64);
        }
        // Page 3 (last 5).
        let p3 = s.transcript_page(&key, p2.next_seq, Some(10)).unwrap();
        assert_eq!(p3.messages.len(), 5);
        assert!(p3.next_seq.is_none());

        // Close and reopen — metadata must survive.
        drop(s);
        let s2 = SqliteRoomStore::open(&path).unwrap();
        let p1_reopen = s2.transcript_page(&key, None, Some(10)).unwrap();
        assert_eq!(p1_reopen.messages.len(), 10);
        assert_eq!(
            p1_reopen.messages[0]
                .federated
                .as_ref()
                .unwrap()
                .global_sequence,
            1000
        );
        assert_eq!(
            p1_reopen.messages[9]
                .federated
                .as_ref()
                .unwrap()
                .global_sequence,
            1009
        );
    }

    // ── corrupt access sequence TEXT ───────────────────────────────────────

    #[test]
    fn corrupt_confirmed_sequence_is_store_error() {
        let mut s = store();
        let key = RoomKey::new("r-badseq");
        s.create(key.clone(), "BadSeq", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'live', 'garbage', '[]')",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.room_access(&key).unwrap_err();
        assert!(
            matches!(&err, RoomStoreError::Encode(msg) if msg.contains("invalid")
                 || msg.contains("u64")),
            "expected encode error, got: {err:?}"
        );
    }

    // ── corrupt outbox source_sequence TEXT ────────────────────────────────

    #[test]
    fn corrupt_outbox_sequence_is_store_error() {
        let mut s = store();
        let key = RoomKey::new("r-badoutboxseq");
        s.create(key.clone(), "BadObSeq", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'live', NULL, '[]')",
                params![key.as_str()],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                     author_member_id, event_type, payload, mention_member_ids,
                                     state, position)
                 VALUES (?1, 'evt-bad', 'src-x', 'not-a-number', 'm1', 'chat.message',
                         '{}', '[]', 'pending', 0)",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.room_access(&key).unwrap_err();
        assert!(
            matches!(&err, RoomStoreError::Encode(msg) if msg.contains("invalid")
                 || msg.contains("u64")),
            "expected encode error for corrupt outbox sequence, got: {err:?}"
        );
    }

    // ── corrupt outbox state TEXT ──────────────────────────────────────────

    #[test]
    fn corrupt_outbox_state_is_store_error() {
        let mut s = store();
        let key = RoomKey::new("r-badstate");
        s.create(key.clone(), "BadState", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'live', NULL, '[]')",
                params![key.as_str()],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                     author_member_id, event_type, payload, mention_member_ids,
                                     state, position)
                 VALUES (?1, 'evt-corrupt', 'src-x', '1', 'm1', 'chat.message',
                         '{}', '[]', 'not_a_valid_state', 0)",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.room_access(&key).unwrap_err();
        assert!(
            matches!(&err, RoomStoreError::Encode(msg) if msg.contains("bad outbox state")
                 || msg.contains("outbox")),
            "expected encode error for corrupt outbox state, got: {err:?}"
        );
    }

    // ── exact {"state":"local"} open + closed ──────────────────────────────

    #[test]
    fn local_access_state_open_and_closed() {
        let mut s = store();
        let key = RoomKey::new("r-local-state");
        s.create(key.clone(), "LocalState", None, now()).unwrap();
        // Insert an explicit {"state":"local"} access row via SQL;
        // verify the exact JSON round-trips through the store.
        let local_json = serde_json::json!("local");
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, ?2, NULL, '[]')",
                params![
                    key.as_str(),
                    serde_json::to_string(&local_json)
                        .unwrap()
                        .trim_matches('"'),
                ],
            )
            .unwrap();

        // Open room — state must decode as Local.
        let proj = s.room_access(&key).unwrap();
        assert_eq!(proj.state, RoomAccessState::Local);
        // Prove the stored state serializes back to the exact JSON value.
        let roundtripped = serde_json::to_value(proj.state).unwrap();
        assert_eq!(roundtripped, local_json);

        // Soft-close the room; access projection must still decode.
        s.close(&key).unwrap();
        let proj_closed = s.room_access(&key).unwrap();
        assert_eq!(proj_closed.state, RoomAccessState::Local);
        let closed_rt = serde_json::to_value(proj_closed.state).unwrap();
        assert_eq!(closed_rt, local_json);
    }

    // ── replace_room_access reorders an existing vector ────────────────────

    #[test]
    fn replace_room_access_reorders_existing_outbox() {
        let (path, mut s) = tempfile_store();
        let key = RoomKey::new("r-reorder");
        s.create(key.clone(), "Reorder", None, now()).unwrap();

        let orig = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![
                outbox_item("a", OutboxItemState::Pending),
                outbox_item("b", OutboxItemState::Failed),
                outbox_item("c", OutboxItemState::Pending),
            ],
        };
        s.replace_room_access(&key, &orig).unwrap();

        let reordered = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![
                outbox_item("c", OutboxItemState::Pending),
                outbox_item("a", OutboxItemState::Pending),
                outbox_item("b", OutboxItemState::Failed),
            ],
        };
        s.replace_room_access(&key, &reordered).unwrap();

        // In-memory assertion.
        let loaded = s.room_access(&key).unwrap();
        assert_eq!(loaded, reordered);

        // Close and reopen — reordered projection must survive.
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();

        let reopened = s2.room_access(&key).unwrap();
        assert_eq!(reopened, reordered);
    }

    // ── multi-item retry: order + every non-state field ────────────────────

    #[test]
    fn retry_preserves_order_and_all_non_state_fields() {
        let (path, mut s) = tempfile_store();
        let key = RoomKey::new("r-multi-retry");
        s.create(key.clone(), "MultiRetry", None, now()).unwrap();

        let proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(7),
            members: vec![member_proj("m-a", "A"), member_proj("m-b", "B")],
            outbox: vec![
                RoomOutboxItem {
                    client_event_id: "evt-pending".into(),
                    source_id: "src-1".into(),
                    source_sequence: 10,
                    author_member_id: "auth-1".into(),
                    event_type: "type-1".into(),
                    payload: serde_json::json!({"k": "v1"}),
                    mention_member_ids: vec!["ref-1".into()],
                    state: OutboxItemState::Pending,
                },
                RoomOutboxItem {
                    client_event_id: "evt-failed".into(),
                    source_id: "src-2".into(),
                    source_sequence: 20,
                    author_member_id: "auth-2".into(),
                    event_type: "type-2".into(),
                    payload: serde_json::json!({"k": "v2"}),
                    mention_member_ids: vec!["ref-2".into(), "ref-3".into()],
                    state: OutboxItemState::Failed,
                },
                RoomOutboxItem {
                    client_event_id: "evt-pending2".into(),
                    source_id: "src-3".into(),
                    source_sequence: 30,
                    author_member_id: "auth-3".into(),
                    event_type: "type-3".into(),
                    payload: serde_json::json!({"k": "v3"}),
                    mention_member_ids: vec![],
                    state: OutboxItemState::Pending,
                },
            ],
        };
        s.replace_room_access(&key, &proj).unwrap();

        // Build the expected projection: only evt-failed → Pending.
        let mut expected = proj.clone();
        expected.outbox[1].state = OutboxItemState::Pending;

        // Retry and assert full structural equality.
        let result = s.retry_failed_outbox(&key, "evt-failed").unwrap();
        assert_eq!(result, expected);

        // Close and reopen — retried state must survive.
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();

        let reopened = s2.room_access(&key).unwrap();
        assert_eq!(reopened, expected);
    }

    // ── u64 TEXT parser: Unicode, trailing whitespace, edge cases ──────────

    #[test]
    fn parse_canonical_u64_text_max_roundtrip() {
        let max_str = write_u64_text(u64::MAX);
        let val = parse_canonical_u64_text(&max_str).unwrap();
        assert_eq!(val, u64::MAX);

        let zero = parse_canonical_u64_text("0").unwrap();
        assert_eq!(zero, 0);

        let one = parse_canonical_u64_text("1").unwrap();
        assert_eq!(one, 1);
    }

    #[test]
    fn parse_canonical_u64_text_rejects_corrupt() {
        assert!(parse_canonical_u64_text("").is_err());
        assert!(parse_canonical_u64_text("+1").is_err());
        assert!(parse_canonical_u64_text(" 1").is_err());
        assert!(parse_canonical_u64_text("01").is_err());
        assert!(parse_canonical_u64_text("-1").is_err());
        assert!(parse_canonical_u64_text("0xFF").is_err());
        assert!(parse_canonical_u64_text("18446744073709551616").is_err());
        // Unicode full-width digits.
        assert!(parse_canonical_u64_text("\u{ff10}").is_err()); // U+FF10 = '０'
        assert!(parse_canonical_u64_text("4\u{ff12}").is_err()); // mixed ASCII + full-width '２'
                                                                 // Trailing whitespace.
        assert!(parse_canonical_u64_text("42\n").is_err());
        assert!(parse_canonical_u64_text("42 ").is_err());
        assert!(parse_canonical_u64_text("42\t").is_err());
        // Trailing non-digit.
        assert!(parse_canonical_u64_text("42x").is_err());
    }

    // ── exact Local projection, semantics ──────────────────────────────────

    #[test]
    fn room_access_unknown_room_is_error() {
        let s = store();
        let key = RoomKey::new("r-nonexistent");
        let err = s.room_access(&key).unwrap_err();
        assert!(matches!(err, RoomStoreError::UnknownRoom(_)));
    }

    #[test]
    fn room_access_local_on_existing_room_without_row() {
        let (path, mut s) = tempfile_store();
        let key = RoomKey::new("r-no-row");
        s.create(key.clone(), "NoRow", None, now()).unwrap();
        let proj = s.room_access(&key).unwrap();
        assert_eq!(proj.state, RoomAccessState::Local);
        assert!(proj.last_confirmed_global_sequence.is_none());
        assert!(proj.members.is_empty());
        assert!(proj.outbox.is_empty());
        // Exact projection JSON — no access row means {"state":"local"}.
        assert_eq!(
            serde_json::to_value(&proj).unwrap(),
            serde_json::json!({"state":"local"})
        );

        // Close and reopen — projection must survive.
        s.close(&key).unwrap();
        drop(s);
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        s2.migrate().unwrap();
        let reopened = s2.room_access(&key).unwrap();
        assert_eq!(reopened.state, RoomAccessState::Local);
        assert_eq!(
            serde_json::to_value(&reopened).unwrap(),
            serde_json::json!({"state":"local"})
        );
    }

    // ── replace_room_access: unknown room inside transaction ───────────────

    #[test]
    fn replace_room_access_unknown_room_errors() {
        let mut s = store();
        let key = RoomKey::new("r-nonexistent");
        let proj = RoomAccessProjection {
            state: RoomAccessState::Local,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![],
        };
        let err = s.replace_room_access(&key, &proj).unwrap_err();
        assert!(matches!(err, RoomStoreError::UnknownRoom(_)));
    }

    // ── retry_failed_outbox: all six outcomes ──────────────────────────────

    #[test]
    fn retry_unknown_room_returns_room_not_found() {
        let mut s = store();
        let key = RoomKey::new("r-unknown");
        let err = s.retry_failed_outbox(&key, "evt-1").unwrap_err();
        assert!(matches!(err, RetryOutboxError::RoomNotFound(_)));
    }

    #[test]
    fn retry_local_no_access_row_returns_room_not_federated() {
        let mut s = store();
        let key = RoomKey::new("r-local-norow");
        s.create(key.clone(), "LocalNoRow", None, now()).unwrap();
        let err = s.retry_failed_outbox(&key, "evt-1").unwrap_err();
        assert!(matches!(err, RetryOutboxError::RoomNotFederated(_)));
    }

    #[test]
    fn retry_explicit_local_row_returns_room_not_federated() {
        let mut s = store();
        let key = RoomKey::new("r-local-row");
        s.create(key.clone(), "LocalRow", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'local', NULL, '[]')",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.retry_failed_outbox(&key, "evt-1").unwrap_err();
        assert!(matches!(err, RetryOutboxError::RoomNotFederated(_)));
    }

    #[test]
    fn retry_revoked_returns_room_access_revoked() {
        let mut s = store();
        let key = RoomKey::new("r-revoked");
        s.create(key.clone(), "Revoked", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'revoked', '5', '[]')",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.retry_failed_outbox(&key, "evt-1").unwrap_err();
        assert!(matches!(err, RetryOutboxError::RoomAccessRevoked(_)));
    }

    #[test]
    fn retry_unknown_item_returns_outbox_item_not_found() {
        let mut s = store();
        let key = RoomKey::new("r-unknown-item");
        s.create(key.clone(), "UnknownItem", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'live', '1', '[]')",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.retry_failed_outbox(&key, "no-such-item").unwrap_err();
        assert!(matches!(err, RetryOutboxError::OutboxItemNotFound { .. }));
    }

    #[test]
    fn retry_not_failed_returns_outbox_item_not_failed() {
        let mut s = store();
        let key = RoomKey::new("r-not-failed");
        s.create(key.clone(), "NotFailed", None, now()).unwrap();
        let proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![outbox_item("evt-pending", OutboxItemState::Pending)],
        };
        s.replace_room_access(&key, &proj).unwrap();
        let err = s.retry_failed_outbox(&key, "evt-pending").unwrap_err();
        assert!(matches!(err, RetryOutboxError::OutboxItemNotFailed { .. }));
    }

    // ── outbox excluded from transcript ────────────────────────────────────

    #[test]
    fn outbox_items_do_not_appear_in_transcript() {
        let mut s = store();
        let key = RoomKey::new("r-outbox-txn");
        s.create(key.clone(), "OutboxTxn", None, now()).unwrap();

        s.append_message(
            &key,
            "alice",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "real message",
            now(),
        )
        .unwrap();

        let proj = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: None,
            members: vec![],
            outbox: vec![
                outbox_item("ob-1", OutboxItemState::Pending),
                outbox_item("ob-2", OutboxItemState::Failed),
            ],
        };
        s.replace_room_access(&key, &proj).unwrap();

        let page = s.transcript_page(&key, None, Some(10)).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].body, "real message");
    }

    // ── Display + Error impls for RetryOutboxError ─────────────────────────

    #[test]
    fn retry_outbox_error_display_and_source() {
        let e = RetryOutboxError::RoomNotFound(RoomKey::new("r1"));
        assert!(e.to_string().contains("r1"));
        assert!(e.source().is_none());

        let e = RetryOutboxError::RoomNotFederated(RoomKey::new("r2"));
        assert!(e.to_string().contains("r2"));

        let e = RetryOutboxError::RoomAccessRevoked(RoomKey::new("r3"));
        assert!(e.to_string().contains("r3"));

        let e = RetryOutboxError::OutboxItemNotFound {
            room: RoomKey::new("r4"),
            client_event_id: "evt-x".into(),
        };
        assert!(e.to_string().contains("evt-x"));

        let e = RetryOutboxError::OutboxItemNotFailed {
            room: RoomKey::new("r5"),
            client_event_id: "evt-y".into(),
            current_state: "pending".into(),
        };
        assert!(e.to_string().contains("pending"));

        let store_err = RoomStoreError::UnknownRoom(RoomKey::new("r6"));
        let e = RetryOutboxError::Store(store_err);
        assert!(e.source().is_some());
    }

    #[test]
    fn retry_corrupt_outbox_state_is_retry_error() {
        let mut s = store();
        let key = RoomKey::new("r-retry-corrupt");
        s.create(key.clone(), "RetryCorrupt", None, now()).unwrap();
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, 'live', NULL, '[]')",
                params![key.as_str()],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO outbox (room_id, client_event_id, source_id, source_sequence,
                                     author_member_id, event_type, payload, mention_member_ids,
                                     state, position)
                 VALUES (?1, 'evt-corrupt', 'src-x', '1', 'm1', 'chat.message',
                         '{}', '[]', 'not_a_valid_state', 0)",
                params![key.as_str()],
            )
            .unwrap();
        let err = s.retry_failed_outbox(&key, "evt-corrupt").unwrap_err();
        assert!(
            matches!(&err, RetryOutboxError::Store(RoomStoreError::Encode(msg))
                     if msg.contains("bad outbox state")),
            "expected Store/Encode error for corrupt state, got: {err:?}"
        );
    }

    // ── P2-A federation durability ───────────────────────────────────────

    use ocean_core::{FederatedActorType, FederatedRoomRole, MemberPresence};

    fn fed_store_with_room(name: &str) -> (SqliteRoomStore, RoomKey) {
        let mut s = store();
        let key = RoomKey::new(name);
        s.create(key.clone(), name, None, now()).unwrap();
        (s, key)
    }

    fn seed_access_row(s: &SqliteRoomStore, key: &RoomKey, state: &str) {
        s.conn
            .execute(
                "INSERT INTO room_access (room_id, state, confirmed_sequence, member_projection)
                 VALUES (?1, ?2, NULL, '[]')",
                params![key.as_str(), state],
            )
            .unwrap();
    }

    fn fed_member(member_id: &str, name: &str) -> FederatedRoomMemberProjection {
        FederatedRoomMemberProjection {
            member_id: member_id.into(),
            owner_member_id: None,
            actor_type: FederatedActorType::User,
            role_in_room: FederatedRoomRole::Member,
            display_name: name.into(),
            public_agent_descriptor: None,
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(MemberPresence::Live),
            local_binding_available: None,
        }
    }

    fn confirmed_event(ledger: &str, gs: u64, sid: &str, sseq: u64, ceid: &str) -> ConfirmedEvent {
        ConfirmedEvent {
            ledger_event_id: ledger.into(),
            global_sequence: gs,
            source_id: sid.into(),
            source_sequence: sseq,
            client_event_id: ceid.into(),
            origin_principal_id: "principal-1".into(),
            origin_member_id: "m-author".into(),
            author_id: "m-author".into(),
            author_kind: RoomParticipantKind::Human,
            kind: RoomMessageKind::Message,
            body: format!("body-{ledger}"),
            trigger_targets: vec![],
        }
    }

    fn transcript_count(s: &SqliteRoomStore, key: &RoomKey) -> usize {
        s.get(key).unwrap().unwrap().transcript.len()
    }

    fn federated_events_count(s: &SqliteRoomStore, key: &RoomKey) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM federated_events WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_db_mode_enforced_on_open_and_reopen() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        {
            let _s = SqliteRoomStore::open(&path).unwrap();
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh DB must be owner-only");
        // Loosen deliberately: reopen must repair, not just assert.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _s = SqliteRoomStore::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "reopen must repair a loosened mode");
    }

    #[test]
    fn federation_instance_id_stable_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let id1 = {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            let a = s.federation_instance_id().unwrap();
            let b = s.federation_instance_id().unwrap();
            assert_eq!(a, b, "second read returns the same id");
            a
        };
        let mut s = SqliteRoomStore::open(&path).unwrap();
        let id2 = s.federation_instance_id().unwrap();
        assert_eq!(id1, id2, "instance id survives reopen");
        // v4 UUID shape: 8-4-4-4-12 hex, version nibble 4, variant nibble 8-b.
        let parts: Vec<&str> = id1.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('4'), "version nibble");
        assert!("89ab".contains(parts[3].chars().next().unwrap()), "variant");
    }

    #[test]
    fn credential_lifecycle_and_bearer_never_projected() {
        let (mut s, key) = fed_store_with_room("r-cred");
        let key2 = RoomKey::new("r-cred-2");
        s.create(key2.clone(), "r-cred-2", None, now()).unwrap();
        s.install_room_credential(&key, "super-secret-bearer", "m-human")
            .unwrap();
        s.install_room_credential(&key2, "other-bearer", "m-human-2")
            .unwrap();
        // Read back for daemon network use.
        let cred = s.room_credential(&key).unwrap().unwrap();
        assert_eq!(cred.bearer_token, "super-secret-bearer");
        assert_eq!(cred.local_human_member_id, "m-human");
        // Debug redacts the bearer.
        let dbg = format!("{cred:?}");
        assert!(dbg.contains("[redacted]"));
        assert!(!dbg.contains("super-secret-bearer"));
        // Startup recovery list carries both rooms, ordered by id.
        let listed = s.list_credentialed_rooms().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].room_id, key);
        assert_eq!(listed[1].room_id, key2);
        // Bearer never appears in projection/transcript serialization.
        seed_access_row(&s, &key, "live");
        s.update_room_access_safe(&key, None, Some(&[fed_member("m-human", "H")]), None)
            .unwrap();
        s.allocate_outbox_pending(
            &key,
            "m-human",
            "evt-c1",
            "message",
            serde_json::json!({"body":"x"}),
            vec![],
        )
        .unwrap();
        let proj = s.room_access(&key).unwrap();
        let proj_json = serde_json::to_string(&proj).unwrap();
        assert!(!proj_json.contains("super-secret-bearer"));
        let rec = s.get(&key).unwrap().unwrap();
        let transcript_json = serde_json::to_string(&rec.transcript).unwrap();
        assert!(!transcript_json.contains("super-secret-bearer"));
        // Unknown room rejects install; revoke returns existence.
        let missing = RoomKey::new("r-missing");
        assert!(matches!(
            s.install_room_credential(&missing, "t", "m"),
            Err(RoomStoreError::UnknownRoom(_))
        ));
        assert!(s.revoke_room_credential(&key).unwrap());
        assert!(!s.revoke_room_credential(&key).unwrap());
        assert!(s.room_credential(&key).unwrap().is_none());
    }

    #[test]
    fn update_room_access_safe_preserves_outbox_and_cursor_monotonic() {
        let (mut s, key) = fed_store_with_room("r-safe");
        // Bootstrap: no access row yet — upsert defaults to Connecting.
        let proj = s
            .update_room_access_safe(&key, None, Some(&[fed_member("m-a", "A")]), None)
            .unwrap();
        assert_eq!(proj.state, RoomAccessState::Connecting);
        assert_eq!(proj.members.len(), 1);
        assert_eq!(proj.last_confirmed_global_sequence, None);
        // One pending outbox row; a safe refresh must not touch it.
        let item = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-s1",
                "message",
                serde_json::json!({"body":"hi"}),
                vec![],
            )
            .unwrap();
        let proj = s
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[fed_member("m-a", "A2")]),
                Some(42),
            )
            .unwrap();
        assert_eq!(proj.state, RoomAccessState::Live);
        assert_eq!(proj.members[0].display_name, "A2");
        assert_eq!(proj.last_confirmed_global_sequence, Some(42));
        assert_eq!(
            proj.outbox,
            vec![item.clone()],
            "outbox untouched by safe refresh"
        );
        // Cursor never regresses through the safe path.
        let proj = s
            .update_room_access_safe(&key, None, None, Some(20))
            .unwrap();
        assert_eq!(proj.last_confirmed_global_sequence, Some(42));
        // State-only update keeps roster and cursor.
        let proj = s
            .update_room_access_safe(&key, Some(RoomAccessState::Recovering), None, None)
            .unwrap();
        assert_eq!(proj.state, RoomAccessState::Recovering);
        assert_eq!(proj.members.len(), 1);
        assert_eq!(proj.last_confirmed_global_sequence, Some(42));
        // u64::MAX cursor survives a reopen as canonical decimal text.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key2 = RoomKey::new("r-max-cursor");
        {
            let mut s2 = SqliteRoomStore::open(&path).unwrap();
            s2.create(key2.clone(), "Max", None, now()).unwrap();
            s2.update_room_access_safe(&key2, None, None, Some(u64::MAX))
                .unwrap();
        }
        let s2 = SqliteRoomStore::open(&path).unwrap();
        let proj = s2.room_access(&key2).unwrap();
        assert_eq!(proj.last_confirmed_global_sequence, Some(u64::MAX));
    }

    #[test]
    fn bindings_resolve_unbind_and_registration_key_stays_private() {
        let (mut s, key) = fed_store_with_room("r-bind");
        s.bind_room_agent(&key, "m-agent-1", "context-cartographer", "reg-key-AAA")
            .unwrap();
        assert_eq!(
            s.resolve_room_agent(&key, "m-agent-1").unwrap().as_deref(),
            Some("context-cartographer")
        );
        // Registration key is stored but only reachable via raw SQL (never a public read).
        let raw: String = s
            .conn
            .query_row(
                "SELECT registration_key FROM room_member_bindings WHERE room_id = ?1 AND member_id = ?2",
                params![key.as_str(), "m-agent-1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "reg-key-AAA");
        // Retried registration with the IDENTICAL tuple is an idempotent
        // no-op (v1.1 §1b — response-loss recovery).
        s.bind_room_agent(&key, "m-agent-1", "context-cartographer", "reg-key-AAA")
            .unwrap();
        assert_eq!(
            s.resolve_room_agent(&key, "m-agent-1").unwrap().as_deref(),
            Some("context-cartographer")
        );
        // Same member with a different agent name or key fails closed.
        let diff_agent = s.bind_room_agent(&key, "m-agent-1", "other-agent", "reg-key-AAA");
        assert!(matches!(
            diff_agent,
            Err(RoomStoreError::FederationCorruption(_))
        ));
        let diff_key = s.bind_room_agent(&key, "m-agent-1", "context-cartographer", "reg-key-BBB");
        assert!(matches!(
            diff_key,
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // The stored binding is unchanged by the failed attempts.
        assert_eq!(
            s.resolve_room_agent(&key, "m-agent-1").unwrap().as_deref(),
            Some("context-cartographer")
        );
        // A second member bound to the same agent name fails (unique per room).
        let dup = s.bind_room_agent(&key, "m-agent-2", "context-cartographer", "reg-key-CCC");
        assert!(dup.is_err(), "agent name is unique per room");
        // Unbind, then rebinding with new values is legitimate.
        assert!(s.unbind_room_agent(&key, "m-agent-1").unwrap());
        assert!(!s.unbind_room_agent(&key, "m-agent-1").unwrap());
        assert!(s.resolve_room_agent(&key, "m-agent-1").unwrap().is_none());
        s.bind_room_agent(&key, "m-agent-1", "other-agent", "reg-key-BBB")
            .unwrap();
        assert_eq!(
            s.resolve_room_agent(&key, "m-agent-1").unwrap().as_deref(),
            Some("other-agent")
        );
    }

    #[test]
    fn allocate_outbox_pending_atomic_counter_positions_no_transcript() {
        let (mut s, key) = fed_store_with_room("r-alloc");
        let instance = s.federation_instance_id().unwrap();
        // First allocation per producer member starts at 1.
        let a1 = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-a1",
                "message",
                serde_json::json!({"body":"a1"}),
                vec!["m-b".into()],
            )
            .unwrap();
        let b1 = s
            .allocate_outbox_pending(
                &key,
                "m-b",
                "evt-b1",
                "message",
                serde_json::json!({"body":"b1"}),
                vec![],
            )
            .unwrap();
        assert_eq!(a1.source_sequence, 1);
        assert_eq!(b1.source_sequence, 1, "distinct producers each start at 1");
        assert_eq!(
            a1.source_id,
            format!(
                "room:{}:member:{}:producer:{}",
                key.as_str(),
                "m-a",
                instance
            )
        );
        assert_ne!(a1.source_id, b1.source_id);
        assert_eq!(a1.state, OutboxItemState::Pending);
        // Same producer continues at 2; positions are stable allocation order.
        let a2 = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-a2",
                "message",
                serde_json::json!({"body":"a2"}),
                vec![],
            )
            .unwrap();
        assert_eq!(a2.source_sequence, 2);
        let pending = s.pending_outbox(&key).unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].client_event_id, "evt-a1");
        assert_eq!(pending[1].client_event_id, "evt-b1");
        assert_eq!(pending[2].client_event_id, "evt-a2");
        // Allocation never enters the transcript.
        assert_eq!(
            transcript_count(&s, &key),
            0,
            "outbox allocation never enters transcript"
        );
    }

    #[test]
    fn producer_counter_survives_reopen_u64_max_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("r-counter");
        {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            s.create(key.clone(), "C", None, now()).unwrap();
            let one = s
                .allocate_outbox_pending(
                    &key,
                    "m-a",
                    "evt-1",
                    "message",
                    serde_json::json!({}),
                    vec![],
                )
                .unwrap();
            assert_eq!(one.source_sequence, 1);
        }
        let mut s = SqliteRoomStore::open(&path).unwrap();
        let two = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-2",
                "message",
                serde_json::json!({}),
                vec![],
            )
            .unwrap();
        assert_eq!(two.source_sequence, 2, "counter survives reopen");
        // Drive the counter to u64::MAX - 1 via raw SQL.
        s.conn
            .execute(
                "UPDATE producer_counters SET next_sequence = ?2
                 WHERE room_id = ?1 AND author_member_id = 'm-a'",
                params![key.as_str(), write_u64_text(u64::MAX - 1)],
            )
            .unwrap();
        let max_minus_one = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-3",
                "message",
                serde_json::json!({}),
                vec![],
            )
            .unwrap();
        assert_eq!(max_minus_one.source_sequence, u64::MAX - 1);
        // Stored counter is now exactly u64::MAX as canonical decimal text.
        let stored: String = s
            .conn
            .query_row(
                "SELECT next_sequence FROM producer_counters WHERE room_id = ?1 AND author_member_id = 'm-a'",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "18446744073709551615");
        // Exhaustion fails closed — no value is ever reused.
        let exhausted = s.allocate_outbox_pending(
            &key,
            "m-a",
            "evt-4",
            "message",
            serde_json::json!({}),
            vec![],
        );
        assert!(matches!(
            exhausted,
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // u64::MAX itself survives a reopen.
        drop(s);
        let mut s = SqliteRoomStore::open(&path).unwrap();
        let still_max = s.allocate_outbox_pending(
            &key,
            "m-a",
            "evt-5",
            "message",
            serde_json::json!({}),
            vec![],
        );
        assert!(
            still_max.is_err(),
            "exhausted counter stays closed across reopen"
        );
        // Corrupt/noncanonical counter text fails closed on read.
        s.conn
            .execute(
                "UPDATE producer_counters SET next_sequence = '01'
                 WHERE room_id = ?1 AND author_member_id = 'm-a'",
                params![key.as_str()],
            )
            .unwrap();
        let corrupt = s.allocate_outbox_pending(
            &key,
            "m-a",
            "evt-6",
            "message",
            serde_json::json!({}),
            vec![],
        );
        assert!(matches!(corrupt, Err(RoomStoreError::Encode(_))));
    }

    #[test]
    fn producer_counter_two_connections_no_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("r-conc");
        let mut s1 = SqliteRoomStore::open(&path).unwrap();
        s1.create(key.clone(), "C", None, now()).unwrap();
        let mut s2 = SqliteRoomStore::open(&path).unwrap();
        let mut seqs = Vec::new();
        for i in 0..3 {
            for s in [&mut s1, &mut s2] {
                let n = seqs.len();
                let item = s
                    .allocate_outbox_pending(
                        &key,
                        "m-a",
                        &format!("evt-{i}-{n}"),
                        "message",
                        serde_json::json!({}),
                        vec![],
                    )
                    .unwrap();
                seqs.push(item.source_sequence);
            }
        }
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs,
            vec![1, 2, 3, 4, 5, 6],
            "no sequence reused across connections"
        );
    }

    #[test]
    fn fail_outbox_pending_preserves_producer_tuple() {
        let (mut s, key) = fed_store_with_room("r-fail");
        seed_access_row(&s, &key, "live");
        let item = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-f1",
                "message",
                serde_json::json!({"body":"x"}),
                vec!["m-b".into()],
            )
            .unwrap();
        assert!(s.fail_outbox_pending(&key, "evt-f1").unwrap());
        let proj = s.room_access(&key).unwrap();
        let after = &proj.outbox[0];
        assert_eq!(after.state, OutboxItemState::Failed);
        assert_eq!(after.source_id, item.source_id);
        assert_eq!(after.source_sequence, item.source_sequence);
        assert_eq!(after.client_event_id, item.client_event_id);
        assert_eq!(after.payload, item.payload);
        assert_eq!(after.mention_member_ids, item.mention_member_ids);
        // Pending list shrinks; re-fail is a no-op false; missing id is false.
        assert!(s.pending_outbox(&key).unwrap().is_empty());
        assert!(!s.fail_outbox_pending(&key, "evt-f1").unwrap());
        assert!(!s.fail_outbox_pending(&key, "evt-missing").unwrap());
    }

    #[test]
    fn ingest_commits_all_steps_atomically() {
        let (mut s, key) = fed_store_with_room("r-ingest");
        s.install_room_credential(&key, "bearer", "m-human")
            .unwrap();
        seed_access_row(&s, &key, "live");
        s.bind_room_agent(&key, "m-target", "context-cartographer", "reg-1")
            .unwrap();
        // A pending outbox row whose full tuple matches the confirmed event.
        let pending = s
            .allocate_outbox_pending(
                &key,
                "m-author",
                "evt-i1",
                "message",
                serde_json::json!({"body":"hello"}),
                vec![],
            )
            .unwrap();
        let mut ev = confirmed_event(
            "ledger-1",
            100,
            &pending.source_id,
            pending.source_sequence,
            "evt-i1",
        );
        ev.trigger_targets = vec!["m-target".into(), "m-unbound".into()];
        let outcome = s.ingest_confirmed_event(&key, &ev, now()).unwrap();
        let IngestOutcome::Ingested(commit) = outcome else {
            panic!("expected Ingested");
        };
        // One federated transcript row with exact metadata.
        assert_eq!(commit.message.seq, 0);
        assert_eq!(commit.message.body, "body-ledger-1");
        let meta = commit.message.federated.as_ref().unwrap();
        assert_eq!(meta.ledger_event_id, "ledger-1");
        assert_eq!(meta.global_sequence, 100);
        assert_eq!(transcript_count(&s, &key), 1);
        // Outbox row removed via full tuple match; cursor advanced.
        let proj = s.room_access(&key).unwrap();
        assert!(proj.outbox.is_empty(), "matching outbox row removed");
        assert_eq!(proj.last_confirmed_global_sequence, Some(100));
        // Dedup index recorded.
        assert_eq!(federated_events_count(&s, &key), 1);
        // Only the locally-bound target was claimed; unbound was not.
        assert_eq!(commit.claimed_trigger_targets, vec!["m-target".to_string()]);
        let claims: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM processed_room_triggers WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1);
    }

    #[test]
    fn ingest_gap_acceptance_and_monotonic_corruption() {
        let (mut s, key) = fed_store_with_room("r-order");
        seed_access_row(&s, &key, "live");
        // Gaps are accepted: 100 then 105.
        let e1 = confirmed_event("l-1", 100, "src-a", 1, "evt-o1");
        let e2 = confirmed_event("l-2", 105, "src-a", 2, "evt-o2");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &e1, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        assert!(matches!(
            s.ingest_confirmed_event(&key, &e2, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        // Lower sequence under a new ledger id ⇒ corruption.
        let low = confirmed_event("l-3", 50, "src-a", 3, "evt-o3");
        let err = s.ingest_confirmed_event(&key, &low, now()).unwrap_err();
        assert!(matches!(err, RoomStoreError::FederationCorruption(_)));
        // Equal sequence under a different ledger id ⇒ corruption.
        let same = confirmed_event("l-4", 105, "src-a", 4, "evt-o4");
        let err = s.ingest_confirmed_event(&key, &same, now()).unwrap_err();
        assert!(matches!(err, RoomStoreError::FederationCorruption(_)));
        // Everything rolled back: transcript/dedup/cursor unchanged.
        assert_eq!(transcript_count(&s, &key), 2);
        assert_eq!(federated_events_count(&s, &key), 2);
        assert_eq!(
            s.room_access(&key).unwrap().last_confirmed_global_sequence,
            Some(105)
        );
    }

    #[test]
    fn ingest_duplicate_is_idempotent_noop() {
        let (mut s, key) = fed_store_with_room("r-dup");
        seed_access_row(&s, &key, "live");
        s.bind_room_agent(&key, "m-target", "agent-a", "reg-1")
            .unwrap();
        let mut ev = confirmed_event("l-dup", 100, "src-a", 1, "evt-d1");
        ev.trigger_targets = vec!["m-target".into()];
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        // Identical replay ⇒ Duplicate: no new row, no cursor move, no claims.
        let outcome = s.ingest_confirmed_event(&key, &ev, now()).unwrap();
        assert!(matches!(outcome, IngestOutcome::Duplicate));
        assert_eq!(transcript_count(&s, &key), 1);
        let claims: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM processed_room_triggers WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1, "replay cannot claim twice");
        // Same ledger id, divergent metadata ⇒ corruption, not duplicate.
        let mut bad = ev.clone();
        bad.global_sequence = 101;
        let err = s.ingest_confirmed_event(&key, &bad, now()).unwrap_err();
        assert!(matches!(err, RoomStoreError::FederationCorruption(_)));
    }

    #[test]
    fn ingest_never_removes_outbox_on_client_event_id_alone() {
        let (mut s, key) = fed_store_with_room("r-collision");
        seed_access_row(&s, &key, "live");
        // Outbox row (evt-x, producer A tuple).
        let pending = s
            .allocate_outbox_pending(
                &key,
                "m-a",
                "evt-x",
                "message",
                serde_json::json!({}),
                vec![],
            )
            .unwrap();
        // Confirmed event with the same client_event_id but a DIFFERENT
        // producer tuple (another member's stream).
        let ev = confirmed_event("l-col", 100, "room:r:member:m-b:producer:zzz", 7, "evt-x");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        let proj = s.room_access(&key).unwrap();
        assert_eq!(
            proj.outbox,
            vec![pending.clone()],
            "nonmatching outbox row survives a client_event_id collision"
        );
    }

    #[test]
    fn ingest_requires_federated_room_and_rolls_back() {
        let (mut s, key) = fed_store_with_room("r-notfed");
        // No access row — ingest must fail closed and write nothing.
        let ev = confirmed_event("l-nf", 100, "src-a", 1, "evt-nf1");
        let err = s.ingest_confirmed_event(&key, &ev, now()).unwrap_err();
        assert!(matches!(err, RoomStoreError::RoomNotFederated(_)));
        assert_eq!(transcript_count(&s, &key), 0);
        assert_eq!(federated_events_count(&s, &key), 0);
        assert!(s
            .room_access(&key)
            .unwrap()
            .last_confirmed_global_sequence
            .is_none());
    }

    #[test]
    fn ingest_agent_authored_rows_claim_no_triggers() {
        let (mut s, key) = fed_store_with_room("r-agent");
        seed_access_row(&s, &key, "live");
        s.bind_room_agent(&key, "m-target", "agent-a", "reg-1")
            .unwrap();
        let mut ev = confirmed_event("l-ag", 100, "src-a", 1, "evt-ag1");
        ev.author_kind = RoomParticipantKind::Agent;
        ev.author_id = "m-agent-author".into();
        ev.trigger_targets = vec!["m-target".into()];
        let outcome = s.ingest_confirmed_event(&key, &ev, now()).unwrap();
        let IngestOutcome::Ingested(commit) = outcome else {
            panic!("expected Ingested");
        };
        assert!(
            commit.claimed_trigger_targets.is_empty(),
            "agent-authored rows claim nothing"
        );
        let claims: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM processed_room_triggers WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 0);
        // The message itself still committed.
        assert_eq!(transcript_count(&s, &key), 1);
    }

    // ── P2-A gate corrections 2/3/5 + v1.2 amendment pins ─────────────────

    #[test]
    fn ingest_dedup_compares_full_persisted_meta() {
        let (mut s, key) = fed_store_with_room("r-fullmeta");
        seed_access_row(&s, &key, "live");
        let ev = confirmed_event("l-fm", 100, "src-a", 1, "evt-fm1");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        // Same ledger id + same gs/source tuple/client id, but divergent
        // ORIGIN metadata ⇒ corruption, never Duplicate (correction 2).
        let mut diff_principal = ev.clone();
        diff_principal.origin_principal_id = "principal-EVIL".into();
        assert!(matches!(
            s.ingest_confirmed_event(&key, &diff_principal, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        let mut diff_member = ev.clone();
        diff_member.origin_member_id = "m-other".into();
        assert!(matches!(
            s.ingest_confirmed_event(&key, &diff_member, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // Nothing was written by the rejected replays.
        assert_eq!(transcript_count(&s, &key), 1);
        assert_eq!(federated_events_count(&s, &key), 1);
        // A byte-identical replay is still a Duplicate no-op.
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Duplicate
        ));
        // An indexed row whose transcript metadata is missing ⇒ corruption
        // (parsed persisted meta is the comparison authority, correction 2).
        s.conn
            .execute(
                "UPDATE messages SET federated = NULL WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
    }

    #[test]
    fn ingest_ordering_respects_persisted_cursor() {
        let (mut s, key) = fed_store_with_room("r-cursor-base");
        // Bootstrap/recovery sets the cursor AHEAD of the (empty) local
        // index — correction 3's upgrade case.
        s.update_room_access_safe(&key, Some(RoomAccessState::Live), None, Some(100))
            .unwrap();
        let stale = confirmed_event("l-cb1", 50, "src-a", 1, "evt-cb1");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &stale, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        let equal = confirmed_event("l-cb2", 100, "src-a", 2, "evt-cb2");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &equal, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // The cursor never regressed and nothing was written.
        let proj = s.room_access(&key).unwrap();
        assert_eq!(proj.last_confirmed_global_sequence, Some(100));
        assert_eq!(transcript_count(&s, &key), 0);
        assert_eq!(federated_events_count(&s, &key), 0);
        // A sequence above the cursor ingests and advances it.
        let fresh = confirmed_event("l-cb3", 150, "src-a", 3, "evt-cb3");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &fresh, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        assert_eq!(
            s.room_access(&key).unwrap().last_confirmed_global_sequence,
            Some(150)
        );
    }

    #[test]
    fn federated_index_u64_max_reopen_and_corrupt_text_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let key = RoomKey::new("r-idx-max");
        {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            s.create(key.clone(), "Max", None, now()).unwrap();
            seed_access_row(&s, &key, "live");
            let ev = confirmed_event("l-max", u64::MAX, "src-a", 1, "evt-max");
            assert!(matches!(
                s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
                IngestOutcome::Ingested { .. }
            ));
        }
        // u64::MAX survives reopen as canonical decimal text in BOTH the
        // index and the cursor; any further sequence is ≤ MAX ⇒ corruption.
        let mut s = SqliteRoomStore::open(&path).unwrap();
        assert_eq!(
            s.room_access(&key).unwrap().last_confirmed_global_sequence,
            Some(u64::MAX)
        );
        let stored: String = s
            .conn
            .query_row(
                "SELECT global_sequence FROM federated_events WHERE room_id = ?1",
                params![key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "18446744073709551615");
        let next = confirmed_event("l-after-max", u64::MAX, "src-a", 2, "evt-am");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &next, now()),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // Corrupt/noncanonical index text fails closed on the ordering read.
        s.conn
            .execute(
                "UPDATE federated_events SET global_sequence = '01' WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();
        // Also reset the cursor to NULL so the ordering read must hit the
        // corrupt index text rather than the cursor.
        s.conn
            .execute(
                "UPDATE room_access SET confirmed_sequence = NULL WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();
        let after_corrupt = confirmed_event("l-c", 7, "src-a", 3, "evt-c");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &after_corrupt, now()),
            Err(RoomStoreError::Encode(_))
        ));
        // Corrupt CURSOR text also fails closed.
        s.conn
            .execute(
                "UPDATE room_access SET confirmed_sequence = '+1' WHERE room_id = ?1",
                params![key.as_str()],
            )
            .unwrap();
        assert!(matches!(
            s.ingest_confirmed_event(&key, &after_corrupt, now()),
            Err(RoomStoreError::Encode(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_mode_repairs_sidecars_before_db_work() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        {
            let _s = SqliteRoomStore::open(&path).unwrap();
        }
        // Loosen the DB and plant a loosened sidecar; reopen must repair both
        // BEFORE any DB work (correction 4).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let wal = dir.path().join("rooms.db-wal");
        std::fs::write(&wal, b"").unwrap();
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o664)).unwrap();
        let _s = SqliteRoomStore::open(&path).unwrap();
        for p in [&path, &wal] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} must be owner-only", p.display());
        }
    }

    #[test]
    fn pending_redemption_get_or_insert_never_forks_a_code() {
        let mut s = store();
        let (fresh, was_fresh) = s
            .get_or_insert_pending_redemption("code-1", "red-1", "bearer-1", now())
            .unwrap();
        assert!(was_fresh);
        assert_eq!(fresh.redemption_id, "red-1");
        assert_eq!(fresh.bearer_token, "bearer-1");
        assert_eq!(fresh.invite_code, "code-1");
        // Same code with DIFFERENT caller-supplied values returns the STORED
        // triple, marked existing — one code never forks (v1.2 §1).
        let (stored, was_fresh) = s
            .get_or_insert_pending_redemption("code-1", "red-OTHER", "bearer-OTHER", now())
            .unwrap();
        assert!(!was_fresh);
        assert_eq!(stored.redemption_id, "red-1");
        assert_eq!(stored.bearer_token, "bearer-1");
        // Duplicate redemption_id under a DIFFERENT code still fails closed.
        let dup = s.get_or_insert_pending_redemption("code-2", "red-1", "bearer-2", now());
        assert!(dup.is_err(), "redemption_id is a primary key");
        // Debug redacts BOTH secrets.
        let dbg = format!("{fresh:?}");
        assert!(dbg.contains("[redacted]"));
        assert!(!dbg.contains("bearer-1"));
        assert!(!dbg.contains("code-1"));
        assert!(dbg.contains("red-1"), "redemption id itself is not secret");
    }

    #[test]
    fn pending_redemptions_survive_reopen_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rooms.db");
        let before = {
            let mut s = SqliteRoomStore::open(&path).unwrap();
            s.get_or_insert_pending_redemption("code-a", "red-a", "bearer-a", now())
                .unwrap();
            s.get_or_insert_pending_redemption("code-b", "red-b", "bearer-b", now())
                .unwrap();
            s.list_pending_redemptions().unwrap()
        };
        let s = SqliteRoomStore::open(&path).unwrap();
        let after = s.list_pending_redemptions().unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(before, after, "triples survive reopen byte-identical");
    }

    #[test]
    fn promote_pending_redemption_is_all_or_nothing() {
        let (mut s, key) = fed_store_with_room("r-promote");
        s.get_or_insert_pending_redemption("code-p", "red-p", "bearer-p", now())
            .unwrap();
        // Unknown room fails closed; the pending row is preserved.
        let missing = RoomKey::new("r-nope");
        assert!(matches!(
            s.promote_pending_redemption("red-p", &missing, "bearer-p", "m-h"),
            Err(RoomStoreError::UnknownRoom(_))
        ));
        assert_eq!(s.list_pending_redemptions().unwrap().len(), 1);
        // Bearer mismatch against the pending row fails closed, no partial
        // write: pending preserved, no credential installed.
        assert!(matches!(
            s.promote_pending_redemption("red-p", &key, "bearer-WRONG", "m-h"),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        assert_eq!(s.list_pending_redemptions().unwrap().len(), 1);
        assert!(s.room_credential(&key).unwrap().is_none());
        // Happy path: credential installed AND pending deleted, one tx.
        assert!(s
            .promote_pending_redemption("red-p", &key, "bearer-p", "m-h")
            .unwrap());
        let cred = s.room_credential(&key).unwrap().unwrap();
        assert_eq!(cred.bearer_token, "bearer-p");
        assert_eq!(cred.local_human_member_id, "m-h");
        assert!(s.list_pending_redemptions().unwrap().is_empty());
        // Exact replay (pending gone, same room+bearer+member) ⇒ idempotent
        // no-op success — the response-loss case (v1.2 §2).
        assert!(!s
            .promote_pending_redemption("red-p", &key, "bearer-p", "m-h")
            .unwrap());
        // Replay with a different bearer or member ⇒ corruption.
        assert!(matches!(
            s.promote_pending_redemption("red-p", &key, "bearer-X", "m-h"),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        assert!(matches!(
            s.promote_pending_redemption("red-p", &key, "bearer-p", "m-OTHER"),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        // Missing pending AND no credential ⇒ corruption.
        let (mut s2, key2) = fed_store_with_room("r-promote-2");
        assert!(matches!(
            s2.promote_pending_redemption("red-never", &key2, "bearer", "m-h"),
            Err(RoomStoreError::FederationCorruption(_))
        ));
        assert!(s2.room_credential(&key2).unwrap().is_none());
        // Corruption errors never leak the bearer.
        let err = s
            .promote_pending_redemption("red-p", &key, "bearer-X", "m-h")
            .unwrap_err();
        assert!(!err.to_string().contains("bearer-X"));
        assert!(!err.to_string().contains("bearer-p"));
    }

    #[test]
    fn ingest_dedup_detects_index_transcript_divergence() {
        let (mut s, key) = fed_store_with_room("r-idx-div");
        seed_access_row(&s, &key, "live");
        let ev = confirmed_event("l-div", 100, "src-a", 1, "evt-div1");
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Ingested { .. }
        ));
        // Corrupt each INDEX column in turn while the transcript metadata
        // stays intact and equal to the incoming replay. A replay must be
        // corruption — never Duplicate — because index ≠ transcript.
        for (col, val) in [
            ("global_sequence", "99"),
            ("source_id", "src-EVIL"),
            ("source_sequence", "9"),
            ("client_event_id", "evt-EVIL"),
        ] {
            s.conn
                .execute(
                    &format!("UPDATE federated_events SET {col} = ?2 WHERE room_id = ?1"),
                    params![key.as_str(), val],
                )
                .unwrap();
            let err = s.ingest_confirmed_event(&key, &ev, now()).unwrap_err();
            assert!(
                matches!(err, RoomStoreError::FederationCorruption(_)),
                "corrupt index column {col} must be corruption, got: {err:?}"
            );
            // Restore the column for the next round.
            let orig = match col {
                "global_sequence" => "100",
                "source_id" => "src-a",
                "source_sequence" => "1",
                _ => "evt-div1",
            };
            s.conn
                .execute(
                    &format!("UPDATE federated_events SET {col} = ?2 WHERE room_id = ?1"),
                    params![key.as_str(), orig],
                )
                .unwrap();
        }
        // Restored index: the byte-identical replay is a Duplicate again.
        assert!(matches!(
            s.ingest_confirmed_event(&key, &ev, now()).unwrap(),
            IngestOutcome::Duplicate
        ));
    }

    #[test]
    fn promote_rolls_back_on_injected_failure_between_writes() {
        let (mut s, key) = fed_store_with_room("r-promote-inject");
        s.get_or_insert_pending_redemption("code-i", "red-i", "bearer-i", now())
            .unwrap();
        // Inject a failure BETWEEN the credential insert and the pending-row
        // delete: the delete statement aborts after the insert succeeded
        // in-transaction, so commit is never reached (v1.1 all-or-nothing
        // pin under injected failure).
        s.conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_pending_delete
                 BEFORE DELETE ON pending_redemptions
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        let err = s.promote_pending_redemption("red-i", &key, "bearer-i", "m-h");
        assert!(err.is_err(), "injected delete failure must surface");
        // No partial write: no credential installed AND pending retained.
        assert!(
            s.room_credential(&key).unwrap().is_none(),
            "credential insert must roll back with the failed delete"
        );
        assert_eq!(s.list_pending_redemptions().unwrap().len(), 1);
        // Remove the injection: the same promote commits atomically.
        s.conn
            .execute_batch("DROP TRIGGER fail_pending_delete;")
            .unwrap();
        assert!(s
            .promote_pending_redemption("red-i", &key, "bearer-i", "m-h")
            .unwrap());
        assert_eq!(
            s.room_credential(&key).unwrap().unwrap().bearer_token,
            "bearer-i"
        );
        assert!(s.list_pending_redemptions().unwrap().is_empty());
    }

    #[test]
    fn remove_pending_redemption_never_touches_credentials() {
        let (mut s, key) = fed_store_with_room("r-remove");
        s.install_room_credential(&key, "bearer-keep", "m-h")
            .unwrap();
        s.get_or_insert_pending_redemption("code-r", "red-r", "bearer-r", now())
            .unwrap();
        assert!(s.remove_pending_redemption("red-r").unwrap());
        assert!(!s.remove_pending_redemption("red-r").unwrap());
        // The room credential is untouched.
        let cred = s.room_credential(&key).unwrap().unwrap();
        assert_eq!(cred.bearer_token, "bearer-keep");
    }
}
