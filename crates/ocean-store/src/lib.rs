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
    Room, RoomKey, RoomMessage, RoomMessageKind, RoomParticipant, RoomParticipantKind,
    RoomTriggerPolicy,
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

    /// One room record (room + transcript) by key.
    fn get(&self, key: &RoomKey) -> Result<Option<RoomRecord>>;

    /// All open rooms, most-recently-updated first, ties broken by key.
    fn list(&self) -> Result<Vec<Room>>;

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

    /// Remove a participant by id and append a `ParticipantLeft` marker. Fails
    /// if the participant isn't present. Bumps `updated_at`.
    fn remove_participant(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord>;

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
                PRIMARY KEY (room_id, seq)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_room_seq ON messages(room_id, seq);
            CREATE INDEX IF NOT EXISTS idx_participants_room ON participants(room_id, position);
            "#,
        )?;
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

    /// Load a full record (room + roster + transcript). `include_closed` decides
    /// whether soft-closed rooms are visible.
    fn load_record(&self, key: &RoomKey, include_closed: bool) -> Result<Option<RoomRecord>> {
        let sql = if include_closed {
            "SELECT id, name, trigger_policy, created_at, updated_at FROM rooms WHERE id = ?1"
        } else {
            "SELECT id, name, trigger_policy, created_at, updated_at FROM rooms WHERE id = ?1 AND closed_at IS NULL"
        };
        let room = self
            .conn
            .query_row(sql, params![key.as_str()], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let policy_json: Option<String> = row.get(2)?;
                let created_at: String = row.get(3)?;
                let updated_at: String = row.get(4)?;
                Ok((id, name, policy_json, created_at, updated_at))
            })
            .optional()?;
        let Some((id, name, policy_json, created_at, updated_at)) = room else {
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
            "SELECT seq, author_id, author_kind, kind, body, created_at
             FROM messages WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![key.as_str(), after, fetch], |row| {
            let seq: i64 = row.get(0)?;
            let author_id: String = row.get(1)?;
            let author_kind: String = row.get(2)?;
            let kind: String = row.get(3)?;
            let body: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            Ok((seq, author_id, author_kind, kind, body, created_at))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (seq, author_id, author_kind, kind, body, created_at) = r?;
            out.push(RoomMessage {
                seq: seq as u64,
                author_id,
                author_kind: decode_participant_kind(&author_kind)?,
                kind: decode_message_kind(&kind)?,
                body,
                created_at: parse_ts(&created_at)?,
            });
        }
        // If we got the sentinel row back, there is at least one more page. Drop
        // it so the page holds exactly `effective_limit` rows, then expose the
        // last *kept* row's seq as the next cursor.
        let has_more = out.len() > effective_limit;
        if has_more {
            out.truncate(effective_limit);
        }
        let next_seq = if has_more { out.last().map(|m| m.seq) } else { None };
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
            "INSERT INTO messages (room_id, seq, author_id, author_kind, kind, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
            "INSERT INTO rooms (id, name, trigger_policy, created_at, updated_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL)",
            params![
                key.as_str(),
                name,
                encode_policy(trigger_policy.as_ref())?,
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
        // updated_at DESC, ties broken by id ASC — same ordering as the registry.
        let mut stmt = self.conn.prepare(
            "SELECT id FROM rooms WHERE closed_at IS NULL ORDER BY updated_at DESC, id ASC",
        )?;
        let keys: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let key = RoomKey::new(k);
            if let Some(rec) = self.load_record(&key, false)? {
                out.push(rec.room);
            }
        }
        Ok(out)
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
        Self::insert_message_on(
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
        Ok(self.load_record(key, false)?.expect("room exists"))
    }

    fn remove_participant(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord> {
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
        Self::insert_message_on(
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
        Ok(self.load_record(key, false)?.expect("room exists"))
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
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
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
        other => return Err(RoomStoreError::Encode(format!("unknown participant kind: {other}"))),
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
        other => return Err(RoomStoreError::Encode(format!("unknown message kind: {other}"))),
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

    #[test]
    fn participant_join_leave_writes_transcript_markers() {
        let mut s = store();
        let key = RoomKey::new("r1");
        s.create(key.clone(), "R1", None, now()).unwrap();

        let rec = s.add_participant(&key, human("john", "John"), now()).unwrap();
        assert_eq!(rec.room.participants.len(), 1);
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].seq, 0);
        assert_eq!(rec.transcript[0].kind, RoomMessageKind::ParticipantJoined);
        assert_eq!(rec.transcript[0].body, "John joined");

        // Re-adding same id does not duplicate the roster entry.
        s.add_participant(&key, human("john", "John"), now()).unwrap();
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
        assert_eq!(collected, expected, "every row retrieved once, in seq order");
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
        let page = s
            .transcript_page(&key, None, Some(usize::MAX))
            .unwrap();
        assert_eq!(page.messages.len(), 3);
        assert!(!page.has_more);
        assert_eq!(clamp_transcript_limit(Some(usize::MAX)), MAX_TRANSCRIPT_LIMIT);
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
        s.add_participant(&key, human("john", "John"), now()).unwrap(); // seq 0
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
        let seqs: Vec<u64> = s.transcript(&key, None).unwrap().iter().map(|m| m.seq).collect();
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
        assert!(matches!(
            s.close(&key),
            Err(RoomStoreError::UnknownRoom(_))
        ));
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
        assert_eq!(msgs_before, count(&s, "messages", &key), "transcript intact");

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
        s.add_participant(&key, human("john", "John"), now()).unwrap(); // seq 0

        let msgs_before = count(&s, "messages", &key);
        let parts_before = count(&s, "participants", &key);

        let err = s.remove_participant(&key, "ghost", now());
        assert!(matches!(err, Err(RoomStoreError::UnknownParticipant { .. })));

        assert_eq!(msgs_before, count(&s, "messages", &key), "no leaked marker");
        assert_eq!(parts_before, count(&s, "participants", &key), "roster intact");
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
        assert_eq!(fk_on, 1, "foreign_keys pragma must be ON or FK clauses are inert");
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
        assert_eq!(count(&s, "participants", &key), 0, "participants must cascade");
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
        s.add_participant(&key, human("john", "John"), now()).unwrap();
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
        assert_eq!(count(&s, "participants", &key), parts_before, "roster retained");
        assert_eq!(count(&s, "messages", &key), msgs_before, "transcript retained");
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
        assert_eq!(rec.transcript[0].seq, 0, "rolled-back op must not consume a seq");
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
}
