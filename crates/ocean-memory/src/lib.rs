//! `ocean-memory` — the typed memory primitive for Ocean agents.
//!
//! Memory is a queryable, provenance-bearing SQLite store at three scopes:
//!
//! - [`MemoryScope::Agent`]    — one per agent; recursive learning, private.
//! - [`MemoryScope::Operator`] — portable, one per coworker (the "dossier");
//!   travels into sessions and is mounted queryable when ported into a room.
//! - [`MemoryScope::Shared`]   — ocean-bedrock, git-backed; inferrable by all
//!   (not stored here — the `Shared` arm exists so the typed vocabulary is
//!   complete, but this store owns only `Agent` and `Operator` rows).
//!
//! A memory row *is* an attested claim: provenance, anchors, and the trust
//! lifecycle (`Verified`/`Stale`/`Dead`) are reused verbatim from
//! [`ocean_context`], so the handoff/claim `reverify` engine drives memory drift
//! detection unchanged — a memory is a claim with a queryable body.
//!
//! This crate mirrors `ocean-store`'s proven SQLite pattern: sync `rusqlite`
//! (bundled), `&mut self` held behind a `Mutex` by the caller (guard dropped
//! before any `.await`), [`TransactionBehavior::Immediate`] for monotonic-seq
//! allocation, and soft-delete for audit. It is **additive** — the daemon does
//! not depend on it yet (see `ocean-agents/docs/AGENT_FILESYSTEM_ARCHITECTURE.md`).
//!
//! Agent-memory versioning is MVCC inside SQLite (no external git): a per-owner
//! monotonic `seq` plus a self-versioning `history` of
//! [`ocean_context::ClaimEvent`]s; [`ocean_context::reverify`] still validates
//! drift against ground truth on load.

pub mod ingest;
use std::path::Path;

use ocean_context::{ClaimEvent, ClaimStatus, Provenance};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

// ===========================================================================
// Scopes, access, kinds
// ===========================================================================

/// Scope of a memory store. Determines lifetime, portability, and default
/// access — see [`MemoryAccess`], the security boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    /// One per agent — private recursive learning.
    Agent,
    /// Portable, one per operator — the dossier; mounts queryable in a room.
    Operator,
    /// Shared (ocean-bedrock). Not stored here; inferrable only.
    Shared,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Agent => "agent",
            MemoryScope::Operator => "operator",
            MemoryScope::Shared => "shared",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "agent" => MemoryScope::Agent,
            "operator" => MemoryScope::Operator,
            "shared" => MemoryScope::Shared,
            _ => return None,
        })
    }
}

/// What a turn may do with a memory store — **the security boundary**.
///
/// `Infer` is the default for [`MemoryScope::Shared`] and for any operator db
/// not granted to the caller: read summaries / semantic search, no raw `SELECT`.
/// `Query` is granted only by ownership or by a room convene — a ported
/// operator db mounted live for the session's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAccess {
    /// Infer about others from summaries / semantic search. No raw SELECT.
    Infer,
    /// A ported operator db mounted in this room/session: direct SELECT.
    Query,
}

/// The kind of fact a memory row holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Fact,
    Preference,
    Relationship,
    Event,
    Skill,
}

impl MemoryKind {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Relationship => "relationship",
            MemoryKind::Event => "event",
            MemoryKind::Skill => "skill",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "fact" => MemoryKind::Fact,
            "preference" => MemoryKind::Preference,
            "relationship" => MemoryKind::Relationship,
            "event" => MemoryKind::Event,
            "skill" => MemoryKind::Skill,
            _ => return None,
        })
    }
}

// ===========================================================================
// Identifiers
// ===========================================================================

/// Stable id of a memory row. Generated as a UUIDv4 string; opaque to the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(pub String);

impl MemoryId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The agent or operator that owns a memory row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(pub String);

impl PrincipalId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ===========================================================================
// Session partitions
// ===========================================================================

/// The durable partition used by ordinary operator memory rows.
///
/// This is also the additive SQLite default, so rows written before partitions
/// existed remain in the same logical namespace without a data rewrite.
const OPERATOR_PARTITION: &str = "operator:v1";

/// Stable owner inside every room partition. The partition carries the exact
/// room identity; this shared owner makes room memory room-wide instead of
/// accidentally creating one silo per admitted agent.
const ROOM_PARTITION_OWNER: &str = "room:v1";

/// Return the only owner principal accepted for a room-memory handle.
///
/// Admission code should use this rather than an agent member id. The value is
/// intentionally the same across rooms because the independently constrained
/// partition carries the room identity.
pub fn room_memory_owner() -> PrincipalId {
    PrincipalId::new(ROOM_PARTITION_OWNER)
}

/// An opaque, store-authorized room memory scope.
///
/// The fields and constructor are deliberately private: model-facing memory
/// tools may carry this value, but cannot manufacture a different room by
/// concatenating a caller-controlled key. Only [`SqliteMemoryStore::trusted_room_scope`]
/// can mint one at the trusted admission/store boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomMemoryScope {
    partition: String,
}

/// The memory authority attached to one agent session.
///
/// `Room` contains an opaque capability rather than a raw string. This keeps
/// room selection at the trusted admission boundary while ordinary tool calls
/// receive only a fixed partition. The type intentionally has no serde
/// implementation, so a request payload cannot deserialize itself into room
/// memory authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMemoryScope {
    /// Existing operator/global memory.
    Operator,
    /// Memory is unavailable for this session. Every scoped operation fails
    /// closed and leaves the database untouched.
    Disabled,
    /// Memory isolated to one exact persistent Room key.
    Room(RoomMemoryScope),
}

impl SessionMemoryScope {
    /// Stable partition principal used for persistence and audit. `None` means
    /// memory is disabled. This is observable but not accepted as authority by
    /// any constructor, so reading it cannot be used to forge another scope.
    pub fn partition_principal(&self) -> Option<&str> {
        match self {
            Self::Operator => Some(OPERATOR_PARTITION),
            Self::Disabled => None,
            Self::Room(room) => Some(&room.partition),
        }
    }
}

// ===========================================================================
// The memory row
// ===========================================================================

/// One memory row — an attested claim with a queryable body. Provenance and the
/// trust lifecycle are reused from [`ocean_context`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: MemoryId,
    pub scope: MemoryScope,
    pub owner: PrincipalId,
    pub kind: MemoryKind,
    /// The queryable payload.
    pub body: serde_json::Value,
    /// Who/what/when attested this — reused from the claim engine.
    pub provenance: Provenance,
    /// Trust state — `Verified`/`Stale`/`Dead`/…; `reverify()` mutates on load.
    pub trust: ClaimStatus,
    /// Per-owner monotonic counter, assigned by the store on insert.
    pub seq: u64,
    /// Unix seconds the memory was first written. Caller-supplied, never `now()`.
    pub written_at: i64,
    /// Unix seconds of the last mutation. Caller-supplied, never `now()`.
    pub updated_at: i64,
    /// Self-versioning event log (written/reverified/…); grows on each mutation.
    #[serde(default)]
    pub history: Vec<ClaimEvent>,
}

// ===========================================================================
// Errors + Result
// ===========================================================================

/// Errors from memory-store operations.
#[derive(Debug)]
pub enum MemoryError {
    /// SQLite I/O failure.
    Db(String),
    /// (De)serialization failure of a stored JSON column.
    Encode(String),
    /// Caller input was invalid (empty id, unknown enum, duplicate id, …).
    BadInput(String),
    /// This session was admitted without memory authority.
    Disabled,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::Db(s) => write!(f, "memory db error: {s}"),
            MemoryError::Encode(s) => write!(f, "memory encode error: {s}"),
            MemoryError::BadInput(s) => write!(f, "memory bad input: {s}"),
            MemoryError::Disabled => f.write_str("memory is disabled for this session"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<rusqlite::Error> for MemoryError {
    fn from(e: rusqlite::Error) -> Self {
        MemoryError::Db(e.to_string())
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Encode(e.to_string())
    }
}

/// Result alias with the error type defaulted, so common call sites stay short
/// while precise-error call sites keep an escape hatch.
pub type Result<T, E = MemoryError> = std::result::Result<T, E>;

// ===========================================================================
// Store trait
// ===========================================================================

/// Default page size for [`MemoryStore::list_page`] when the caller omits a
/// limit (mirrors `ocean-store`'s `DEFAULT_LIST_LIMIT`).
pub const DEFAULT_PAGE_LIMIT: usize = 100;

/// Hard ceiling on a single [`MemoryStore::list_page`] (mirrors
/// `ocean_store::MAX_LIST_LIMIT`).
pub const MAX_PAGE_LIMIT: usize = 1000;

/// Clamp a caller-supplied page limit: `None` ⇒ [`DEFAULT_PAGE_LIMIT`]; any
/// value capped at [`MAX_PAGE_LIMIT`] and floored at 1.
pub fn clamp_page_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

/// One bounded page of an owner's memories.
///
/// `memories` holds at most the effective limit of rows in descending `seq`
/// order. `next_seq` is the cursor to replay as the next `after_seq` to fetch
/// the following page; `None` at the end. `has_more` is the same signal as a bool.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryPage {
    pub memories: Vec<Memory>,
    pub next_seq: Option<u64>,
    pub has_more: bool,
}

/// The common memory-store operations. Sync + `&mut self`, held behind a
/// `Mutex` by the caller with the guard dropped before any `.await` — the same
/// discipline as `ocean_store::RoomStore`.
pub trait MemoryStore {
    /// Insert a new memory. The store assigns `seq`; `id` must be fresh.
    /// Returns the stored row (with `seq` filled).
    fn put(&mut self, mem: Memory) -> Result<Memory>;

    /// One live memory by id (`None` if absent or soft-deleted).
    fn get(&self, id: &MemoryId) -> Result<Option<Memory>>;

    /// Bounded page of an owner's live memories, highest `seq` first, starting
    /// after `after_seq` (or from the top when `None`). `limit` is clamped by
    /// [`clamp_page_limit`].
    fn list_page(
        &self,
        owner: &PrincipalId,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<MemoryPage>;

    /// Soft-delete (set `deleted_at`); the row survives for audit. Idempotent.
    fn delete(&mut self, id: &MemoryId, now: i64) -> Result<()>;

    /// Count of an owner's live (non-deleted) memories.
    fn count(&self, owner: &PrincipalId) -> Result<u64>;
}

// ===========================================================================
// SQLite-backed store
// ===========================================================================

/// SQLite-backed memory store. One [`Connection`] held directly; the caller
/// wraps it in a `Mutex` (same as `ocean_store::SqliteRoomStore`).
pub struct SqliteMemoryStore {
    conn: Connection,
}

impl SqliteMemoryStore {
    /// Open (or create) a store at `path`. Runs [`Self::migrate`] idempotently.
    /// `open(":memory:")` yields a fresh in-memory db for tests.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Convenience: a fresh in-memory store.
    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// Mint an opaque room-memory scope from an already-authorized persistent
    /// Room key.
    ///
    /// This method is the trusted constructor: callers must invoke it only
    /// after room-agent admission has resolved the authoritative [`RoomKey`]
    /// equivalent. The exact string is preserved and length-prefixed by UTF-8
    /// bytes, preventing ambiguous principals such as `a:bc` and `a:b:c`.
    /// Model-facing tools receive the returned value, never a raw key-to-scope
    /// constructor.
    pub fn trusted_room_scope(&self, room_key: &str) -> Result<SessionMemoryScope> {
        if room_key.trim().is_empty() {
            return Err(MemoryError::BadInput(
                "room memory key cannot be empty".into(),
            ));
        }
        let partition = format!("room:v1:{}:{room_key}", room_key.len());
        Ok(SessionMemoryScope::Room(RoomMemoryScope { partition }))
    }

    /// Borrow this database through one fixed session scope and owner.
    ///
    /// The returned handle is the only API room tools should receive. Its
    /// fields are private, and every SQL operation injects both the fixed
    /// partition and fixed owner instead of accepting either from tool input.
    pub fn scoped<'a>(
        &'a mut self,
        scope: &SessionMemoryScope,
        owner: &PrincipalId,
    ) -> Result<ScopedMemoryStore<'a>> {
        if matches!(scope, SessionMemoryScope::Room(_)) && owner.0 != ROOM_PARTITION_OWNER {
            return Err(MemoryError::BadInput(
                "room memory must use the stable room-wide owner".into(),
            ));
        }
        Ok(ScopedMemoryStore {
            store: self,
            scope: scope.clone(),
            owner: owner.clone(),
        })
    }

    /// Idempotent schema bootstrap.
    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                scope       TEXT NOT NULL,
                owner       TEXT NOT NULL,
                kind        TEXT NOT NULL,
                body        TEXT NOT NULL,
                provenance  TEXT NOT NULL,
                trust       TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                written_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                deleted_at  INTEGER,
                history     TEXT NOT NULL DEFAULT '[]',
                partition   TEXT NOT NULL DEFAULT 'operator:v1'
            );
            CREATE INDEX IF NOT EXISTS idx_memories_owner_seq
                ON memories(owner, seq DESC);
            CREATE INDEX IF NOT EXISTS idx_memories_owner_live
                ON memories(owner) WHERE deleted_at IS NULL;",
        )?;

        // SQLite's additive-column migration changes only the table metadata;
        // existing rows are not rewritten and read the stable operator default.
        // Keep this separate from CREATE TABLE because IF NOT EXISTS does not
        // add columns to databases created by older Ocean versions.
        let has_partition = {
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names
                .collect::<std::result::Result<Vec<_>, _>>()?
                .iter()
                .any(|name| name == "partition")
        };
        if !has_partition {
            conn.execute(
                "ALTER TABLE memories ADD COLUMN partition TEXT NOT NULL DEFAULT 'operator:v1'",
                [],
            )?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_partition_owner_seq
                ON memories(partition, owner, seq DESC);
             CREATE INDEX IF NOT EXISTS idx_memories_partition_owner_live
                ON memories(partition, owner) WHERE deleted_at IS NULL;",
        )?;
        Ok(())
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn put(&mut self, mut mem: Memory) -> Result<Memory> {
        if mem.id.0.trim().is_empty() {
            return Err(MemoryError::BadInput("memory id cannot be empty".into()));
        }
        if mem.owner.0.trim().is_empty() {
            return Err(MemoryError::BadInput("memory owner cannot be empty".into()));
        }
        // Live, soft-deleted, or missing — a present id is always a collision.
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM memories WHERE id = ?1",
                params![mem.id.0],
                |_| Ok(true),
            )
            .optional()?
            .is_some();
        if exists {
            return Err(MemoryError::BadInput(format!(
                "memory id {} already exists",
                mem.id
            )));
        }

        let body = serde_json::to_string(&mem.body)?;
        let provenance = serde_json::to_string(&mem.provenance)?;
        let history = serde_json::to_string(&mem.history)?;
        let trust = trust_to_str(mem.trust);

        // Allocate the per-owner monotonic seq inside an IMMEDIATE transaction,
        // exactly like ocean-store's transcript seq: the write lock is taken at
        // BEGIN so a second connection cannot interleave a commit between the
        // MAX(seq)+1 read and the INSERT (the race that used to tear seq order).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_seq: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM memories WHERE owner = ?1",
                params![mem.owner.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v.max(0) as u64)?;
        mem.seq = next_seq;
        tx.execute(
            "INSERT INTO memories
                (id, scope, owner, kind, body, provenance, trust, seq,
                 written_at, updated_at, history)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                mem.id.0,
                mem.scope.as_str(),
                mem.owner.0,
                mem.kind.as_str(),
                body,
                provenance,
                trust,
                next_seq as i64,
                mem.written_at,
                mem.updated_at,
                history,
            ],
        )?;
        tx.commit()?;
        Ok(mem)
    }

    fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let mem = self
            .conn
            .query_row(
                "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                        written_at, updated_at, history
                 FROM memories
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![id.0],
                decode_row,
            )
            .optional()?;
        Ok(mem)
    }

    fn list_page(
        &self,
        owner: &PrincipalId,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<MemoryPage> {
        let limit = clamp_page_limit(limit);
        // Fetch limit+1 to detect `has_more` without a second query.
        let rows: Vec<Memory> = match after_seq {
            Some(s) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, history
                     FROM memories
                     WHERE owner = ?1 AND deleted_at IS NULL AND seq < ?2
                     ORDER BY seq DESC LIMIT ?3",
                )?;
                let mapped =
                    stmt.query_map(params![owner.0, s as i64, (limit + 1) as i64], decode_row)?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, history
                     FROM memories
                     WHERE owner = ?1 AND deleted_at IS NULL
                     ORDER BY seq DESC LIMIT ?2",
                )?;
                let mapped = stmt.query_map(params![owner.0, (limit + 1) as i64], decode_row)?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            }
        };

        let has_more = rows.len() > limit;
        let mut rows = rows;
        if has_more {
            rows.truncate(limit);
        }
        let next_seq = rows.last().map(|m| m.seq);
        Ok(MemoryPage {
            memories: rows,
            next_seq,
            has_more,
        })
    }

    fn delete(&mut self, id: &MemoryId, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id.0],
        )?;
        Ok(())
    }

    fn count(&self, owner: &PrincipalId) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE owner = ?1 AND deleted_at IS NULL",
            params![owner.0],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }
}

// ===========================================================================
// Capability-safe scoped store
// ===========================================================================

/// A borrow of [`SqliteMemoryStore`] fixed to one session partition and owner.
///
/// This is the store boundary for room-facing memory tools. Neither the room
/// key/partition nor the owner can be supplied per operation, and all queries
/// constrain both values in SQL. The legacy [`MemoryStore`] implementation on
/// [`SqliteMemoryStore`] remains unchanged for existing ordinary callers.
pub struct ScopedMemoryStore<'a> {
    store: &'a mut SqliteMemoryStore,
    scope: SessionMemoryScope,
    owner: PrincipalId,
}

impl ScopedMemoryStore<'_> {
    /// The fixed owner for this capability handle.
    pub fn owner(&self) -> &PrincipalId {
        &self.owner
    }

    /// The fixed session memory scope for this capability handle.
    pub fn scope(&self) -> &SessionMemoryScope {
        &self.scope
    }

    fn partition(&self) -> Result<&str> {
        self.scope
            .partition_principal()
            .ok_or(MemoryError::Disabled)
    }

    /// Insert into this handle's fixed partition and owner.
    ///
    /// `mem.owner` must agree with the capability owner; the SQL insert uses
    /// the captured owner regardless. Sequence numbers are monotonic within
    /// `(partition, owner)`, so activity in another room cannot influence the
    /// visible ordering of this one.
    pub fn put(&mut self, mut mem: Memory) -> Result<Memory> {
        let partition = self.partition()?.to_owned();
        if mem.id.0.trim().is_empty() {
            return Err(MemoryError::BadInput("memory id cannot be empty".into()));
        }
        if mem.owner.0.trim().is_empty() {
            return Err(MemoryError::BadInput("memory owner cannot be empty".into()));
        }
        if mem.owner != self.owner {
            return Err(MemoryError::BadInput(
                "memory owner does not match scoped authority".into(),
            ));
        }

        let body = serde_json::to_string(&mem.body)?;
        let provenance = serde_json::to_string(&mem.provenance)?;
        let history = serde_json::to_string(&mem.history)?;
        let trust = trust_to_str(mem.trust);
        let tx = self
            .store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_seq: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1
                 FROM memories
                 WHERE partition = ?1 AND owner = ?2",
                params![partition, self.owner.0],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value.max(0) as u64)?;
        mem.seq = next_seq;
        let inserted = tx.execute(
            "INSERT INTO memories
                (id, partition, scope, owner, kind, body, provenance, trust, seq,
                 written_at, updated_at, history)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                mem.id.0,
                partition,
                mem.scope.as_str(),
                self.owner.0,
                mem.kind.as_str(),
                body,
                provenance,
                trust,
                next_seq as i64,
                mem.written_at,
                mem.updated_at,
                history,
            ],
        );
        if let Err(error) = inserted {
            if matches!(
                error,
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _
                )
            ) {
                return Err(MemoryError::BadInput("memory id is unavailable".into()));
            }
            return Err(error.into());
        }
        tx.commit()?;
        Ok(mem)
    }

    /// Read one live row only when both partition and owner match this handle.
    pub fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let partition = self.partition()?;
        let memory = self
            .store
            .conn
            .query_row(
                "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                        written_at, updated_at, history
                 FROM memories
                 WHERE id = ?1 AND partition = ?2 AND owner = ?3
                       AND deleted_at IS NULL",
                params![id.0, partition, self.owner.0],
                decode_row,
            )
            .optional()?;
        Ok(memory)
    }

    /// List a bounded newest-first page inside the fixed partition and owner.
    pub fn list_page(&self, after_seq: Option<u64>, limit: Option<usize>) -> Result<MemoryPage> {
        let partition = self.partition()?;
        let limit = clamp_page_limit(limit);
        let rows: Vec<Memory> = match after_seq {
            Some(sequence) => {
                let mut stmt = self.store.conn.prepare(
                    "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, history
                     FROM memories
                     WHERE partition = ?1 AND owner = ?2
                           AND deleted_at IS NULL AND seq < ?3
                     ORDER BY seq DESC LIMIT ?4",
                )?;
                let mapped = stmt.query_map(
                    params![partition, self.owner.0, sequence as i64, (limit + 1) as i64],
                    decode_row,
                )?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.store.conn.prepare(
                    "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, history
                     FROM memories
                     WHERE partition = ?1 AND owner = ?2 AND deleted_at IS NULL
                     ORDER BY seq DESC LIMIT ?3",
                )?;
                let mapped = stmt.query_map(
                    params![partition, self.owner.0, (limit + 1) as i64],
                    decode_row,
                )?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            }
        };

        let has_more = rows.len() > limit;
        let mut memories = rows;
        if has_more {
            memories.truncate(limit);
        }
        let next_seq = memories.last().map(|memory| memory.seq);
        Ok(MemoryPage {
            memories,
            next_seq,
            has_more,
        })
    }

    /// Soft-delete only a row in the fixed partition and owner. The result is
    /// intentionally idempotent and does not disclose whether a guessed id
    /// exists in another partition.
    pub fn delete(&mut self, id: &MemoryId, now: i64) -> Result<()> {
        let partition = self.partition()?.to_owned();
        self.store.conn.execute(
            "UPDATE memories
             SET deleted_at = ?1
             WHERE id = ?2 AND partition = ?3 AND owner = ?4
                   AND deleted_at IS NULL",
            params![now, id.0, partition, self.owner.0],
        )?;
        Ok(())
    }

    /// Count live rows only inside the fixed partition and owner.
    pub fn count(&self) -> Result<u64> {
        let partition = self.partition()?;
        let count: i64 = self.store.conn.query_row(
            "SELECT COUNT(*)
             FROM memories
             WHERE partition = ?1 AND owner = ?2 AND deleted_at IS NULL",
            params![partition, self.owner.0],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Deterministic newest-first substring search inside the fixed partition
    /// and owner. An empty query returns the newest rows.
    ///
    /// Search is performed by SQLite so the partition and owner predicates are
    /// inseparable from the content match; callers never scan an unscoped page.
    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<Vec<Memory>> {
        let partition = self.partition()?;
        let limit = clamp_page_limit(limit);
        let mut stmt = self.store.conn.prepare(
            "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                    written_at, updated_at, history
             FROM memories
             WHERE partition = ?1 AND owner = ?2 AND deleted_at IS NULL
                   AND (?3 = '' OR instr(lower(body), lower(?3)) > 0)
             ORDER BY seq DESC LIMIT ?4",
        )?;
        let mapped = stmt.query_map(
            params![partition, self.owner.0, query, limit as i64],
            decode_row,
        )?;
        Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

// ===========================================================================
// Row (de)serialization helpers
// ===========================================================================

fn trust_to_str(t: ClaimStatus) -> &'static str {
    match t {
        ClaimStatus::Verified => "verified",
        ClaimStatus::Reverify => "reverify",
        ClaimStatus::Stale => "stale",
        ClaimStatus::Dead => "dead",
        ClaimStatus::Asserted => "asserted",
    }
}

fn trust_parse(s: &str) -> Option<ClaimStatus> {
    Some(match s {
        "verified" => ClaimStatus::Verified,
        "reverify" => ClaimStatus::Reverify,
        "stale" => ClaimStatus::Stale,
        "dead" => ClaimStatus::Dead,
        "asserted" => ClaimStatus::Asserted,
        _ => return None,
    })
}

/// Decode one row into a [`Memory`]. Column order MUST match every SELECT above.
fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let id: String = row.get(0)?;
    let scope: String = row.get(1)?;
    let owner: String = row.get(2)?;
    let kind: String = row.get(3)?;
    let body: String = row.get(4)?;
    let provenance: String = row.get(5)?;
    let trust: String = row.get(6)?;
    let seq: i64 = row.get(7)?;
    let written_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let history: String = row.get(10)?;

    let scope = MemoryScope::parse(&scope).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown scope {scope}").into(),
        )
    })?;
    let kind = MemoryKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown kind {kind}").into(),
        )
    })?;
    let trust = trust_parse(&trust).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            format!("unknown trust {trust}").into(),
        )
    })?;

    let body: serde_json::Value = serde_json::from_str(&body).map_err(boxed)?;
    let provenance: Provenance = serde_json::from_str(&provenance).map_err(boxed)?;
    let history: Vec<ClaimEvent> = serde_json::from_str(&history).map_err(boxed)?;

    Ok(Memory {
        id: MemoryId(id),
        scope,
        owner: PrincipalId(owner),
        kind,
        body,
        provenance,
        trust,
        seq: seq.max(0) as u64,
        written_at,
        updated_at,
        history,
    })
}

/// serde errors are not `std::error::Error + Send + Sync + 'static`-boxed in a
/// way rusqlite accepts directly; box them via a string round-trip.
fn boxed<E: std::fmt::Display>(e: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.to_string().into())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_context::Anchor;

    fn sample_memory(owner: &str, body: &str) -> Memory {
        Memory {
            id: MemoryId::new(),
            scope: MemoryScope::Operator,
            owner: PrincipalId::new(owner),
            kind: MemoryKind::Fact,
            body: serde_json::json!({ "note": body }),
            provenance: Provenance {
                anchors: vec![Anchor {
                    file: Some("docs/brief.md".into()),
                    symbol: None,
                    lines: vec![12],
                    sig_hash: None,
                }],
                tickets: vec!["OCEAN-1".into()],
                commit_sha: "abc1234".into(),
            },
            trust: ClaimStatus::Asserted,
            seq: 0,
            written_at: 1_780_000_000,
            updated_at: 1_780_000_000,
            history: vec![ClaimEvent {
                at: 1_780_000_000,
                event: "written".into(),
                by_session: "sess-1".into(),
            }],
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let m = sample_memory("coworker-a", "prefers dark mode");
        let stored = store.put(m.clone()).unwrap();
        assert!(stored.seq >= 1, "store assigns a seq");

        let got = store.get(&stored.id).unwrap().expect("present");
        // seq is store-assigned; compare everything else via the serde shape.
        assert_eq!(got.scope, m.scope);
        assert_eq!(got.owner, m.owner);
        assert_eq!(got.kind, m.kind);
        assert_eq!(got.body, m.body);
        assert_eq!(got.provenance, m.provenance);
        assert_eq!(got.trust, m.trust);
        assert_eq!(got.history, m.history);
        assert_eq!(got.seq, stored.seq);
    }

    #[test]
    fn seq_is_monotonic_per_owner() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let a = store.put(sample_memory("a", "1")).unwrap();
        let b = store.put(sample_memory("a", "2")).unwrap();
        let c = store.put(sample_memory("b", "3")).unwrap();
        assert!(b.seq > a.seq, "same owner seq increases");
        assert_eq!(c.seq, 1, "different owner restarts at 1");
    }

    #[test]
    fn list_page_is_bounded_and_descending() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        for i in 0..5 {
            store.put(sample_memory("a", &format!("n{i}"))).unwrap();
        }
        let page = store
            .list_page(&PrincipalId::new("a"), None, Some(2))
            .unwrap();
        assert_eq!(page.memories.len(), 2);
        assert!(page.has_more);
        assert!(page.memories[0].seq > page.memories[1].seq);

        let next = store
            .list_page(&PrincipalId::new("a"), page.next_seq, Some(2))
            .unwrap();
        assert!(next.memories[0].seq < page.memories[1].seq);
    }

    #[test]
    fn delete_is_soft_and_counted() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let m = store.put(sample_memory("a", "x")).unwrap();
        assert_eq!(store.count(&PrincipalId::new("a")).unwrap(), 1);
        store.delete(&m.id, 1_780_000_999).unwrap();
        assert_eq!(
            store.count(&PrincipalId::new("a")).unwrap(),
            0,
            "soft-deleted is not counted"
        );
        assert!(
            store.get(&m.id).unwrap().is_none(),
            "soft-deleted is not returned"
        );
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let m = sample_memory("a", "x");
        store.put(m.clone()).unwrap();
        let err = store.put(m).unwrap_err();
        assert!(
            matches!(err, MemoryError::BadInput(_)),
            "duplicate id is BadInput"
        );
    }

    #[test]
    fn open_is_idempotent_across_migrate() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let mut store = SqliteMemoryStore::open(path).unwrap();
            store.put(sample_memory("a", "persisted")).unwrap();
        }
        // Re-open: migrate must not error on the existing schema, data survives.
        let store = SqliteMemoryStore::open(path).unwrap();
        assert_eq!(store.count(&PrincipalId::new("a")).unwrap(), 1);
    }

    #[test]
    fn operator_scope_preserves_existing_global_rows() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let original = sample_memory("operator", "global preference");
        let stored = MemoryStore::put(&mut store, original.clone()).unwrap();

        // The legacy API and row serde shape remain unchanged.
        let legacy = MemoryStore::get(&store, &stored.id)
            .unwrap()
            .expect("legacy global row");
        assert_eq!(legacy.body, original.body);
        assert_eq!(legacy.scope, MemoryScope::Operator);

        // The capability-safe operator handle maps old rows into the additive
        // operator partition without rewriting them.
        let owner = PrincipalId::new("operator");
        let mut scoped = store.scoped(&SessionMemoryScope::Operator, &owner).unwrap();
        assert_eq!(scoped.get(&stored.id).unwrap(), Some(legacy));
        assert_eq!(scoped.count().unwrap(), 1);
        assert_eq!(scoped.search("GLOBAL", Some(8)).unwrap().len(), 1);

        let second = scoped
            .put(sample_memory("operator", "another global fact"))
            .unwrap();
        assert_eq!(second.seq, stored.seq + 1);
    }

    #[test]
    fn room_partition_uses_exact_utf8_byte_length() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        let key = "røom/潮";
        let scope = store.trusted_room_scope(key).unwrap();
        assert_eq!(
            scope.partition_principal(),
            Some(format!("room:v1:{}:{key}", key.len()).as_str())
        );
        assert!(store.trusted_room_scope("   ").is_err());
    }

    #[test]
    fn room_partitions_cannot_cross_read_write_delete_list_count_or_search() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = room_memory_owner();
        let room_a = store.trusted_room_scope("room:a").unwrap();
        let room_b = store.trusted_room_scope("room:a:extra").unwrap();

        let global = MemoryStore::put(
            &mut store,
            sample_memory(ROOM_PARTITION_OWNER, "global-only-secret"),
        )
        .unwrap();
        let a = {
            let mut scoped = store.scoped(&room_a, &owner).unwrap();
            scoped
                .put(sample_memory(ROOM_PARTITION_OWNER, "alpha-room-only"))
                .unwrap()
        };
        let b = {
            let mut scoped = store.scoped(&room_b, &owner).unwrap();
            scoped
                .put(sample_memory(ROOM_PARTITION_OWNER, "beta-room-secret"))
                .unwrap()
        };

        {
            let mut scoped_a = store.scoped(&room_a, &owner).unwrap();
            assert_eq!(scoped_a.get(&global.id).unwrap(), None);
            assert_eq!(scoped_a.get(&b.id).unwrap(), None);
            assert_eq!(scoped_a.count().unwrap(), 1);
            assert_eq!(
                scoped_a
                    .list_page(None, Some(100))
                    .unwrap()
                    .memories
                    .iter()
                    .map(|memory| &memory.id)
                    .collect::<Vec<_>>(),
                vec![&a.id]
            );
            assert!(scoped_a
                .search("beta-room-secret", Some(100))
                .unwrap()
                .is_empty());
            assert!(scoped_a
                .search("global-only-secret", Some(100))
                .unwrap()
                .is_empty());

            // Guessed ids in other partitions are neither deleted nor usable
            // as a write target. The duplicate error is deliberately generic.
            scoped_a.delete(&b.id, 1_780_000_100).unwrap();
            scoped_a.delete(&global.id, 1_780_000_100).unwrap();
            let mut collision = sample_memory(ROOM_PARTITION_OWNER, "overwrite");
            collision.id = b.id.clone();
            assert!(matches!(
                scoped_a.put(collision),
                Err(MemoryError::BadInput(message)) if message == "memory id is unavailable"
            ));
        }

        assert!(store
            .scoped(&room_b, &owner)
            .unwrap()
            .get(&b.id)
            .unwrap()
            .is_some());
        assert!(MemoryStore::get(&store, &global.id).unwrap().is_some());
    }

    #[test]
    fn room_scope_enforces_one_room_wide_owner() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let room = store.trusted_room_scope("shared-room").unwrap();
        let error = match store.scoped(&room, &PrincipalId::new("agent-member-7")) {
            Ok(_) => panic!("per-agent room owner must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, MemoryError::BadInput(_)));

        let owner = room_memory_owner();
        let scoped = store.scoped(&room, &owner).unwrap();
        assert_eq!(scoped.owner(), &owner);
    }

    #[test]
    fn owners_cannot_cross_within_the_same_partition() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let scope = SessionMemoryScope::Operator;
        let alice = PrincipalId::new("alice");
        let bob = PrincipalId::new("bob");
        let alice_memory = {
            let mut scoped = store.scoped(&scope, &alice).unwrap();
            scoped.put(sample_memory("alice", "alice secret")).unwrap()
        };

        {
            let mut scoped = store.scoped(&scope, &bob).unwrap();
            assert_eq!(scoped.get(&alice_memory.id).unwrap(), None);
            assert_eq!(scoped.count().unwrap(), 0);
            assert!(scoped.list_page(None, None).unwrap().memories.is_empty());
            assert!(scoped.search("alice secret", None).unwrap().is_empty());
            scoped.delete(&alice_memory.id, 1_780_000_111).unwrap();
            assert!(matches!(
                scoped.put(sample_memory("alice", "wrong owner")),
                Err(MemoryError::BadInput(_))
            ));
        }

        assert!(store
            .scoped(&scope, &alice)
            .unwrap()
            .get(&alice_memory.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn disabled_scope_refuses_every_operation_and_is_inert() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("operator");
        let baseline = MemoryStore::put(&mut store, sample_memory("operator", "baseline")).unwrap();
        {
            let mut disabled = store.scoped(&SessionMemoryScope::Disabled, &owner).unwrap();
            assert!(matches!(
                disabled.get(&baseline.id),
                Err(MemoryError::Disabled)
            ));
            assert!(matches!(disabled.count(), Err(MemoryError::Disabled)));
            assert!(matches!(
                disabled.list_page(None, None),
                Err(MemoryError::Disabled)
            ));
            assert!(matches!(
                disabled.search("baseline", None),
                Err(MemoryError::Disabled)
            ));
            assert!(matches!(
                disabled.delete(&baseline.id, 1_780_000_222),
                Err(MemoryError::Disabled)
            ));
            assert!(matches!(
                disabled.put(sample_memory("operator", "must not persist")),
                Err(MemoryError::Disabled)
            ));
        }
        assert_eq!(MemoryStore::count(&store, &owner).unwrap(), 1);
        assert!(MemoryStore::get(&store, &baseline.id).unwrap().is_some());
    }

    #[test]
    fn legacy_database_migrates_additively_and_reopens() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        let legacy = sample_memory("operator", "pre-partition row");
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                    id          TEXT PRIMARY KEY,
                    scope       TEXT NOT NULL,
                    owner       TEXT NOT NULL,
                    kind        TEXT NOT NULL,
                    body        TEXT NOT NULL,
                    provenance  TEXT NOT NULL,
                    trust       TEXT NOT NULL,
                    seq         INTEGER NOT NULL,
                    written_at  INTEGER NOT NULL,
                    updated_at  INTEGER NOT NULL,
                    deleted_at  INTEGER,
                    history     TEXT NOT NULL DEFAULT '[]'
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memories
                    (id, scope, owner, kind, body, provenance, trust, seq,
                     written_at, updated_at, history)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    legacy.id.0,
                    legacy.scope.as_str(),
                    legacy.owner.0,
                    legacy.kind.as_str(),
                    serde_json::to_string(&legacy.body).unwrap(),
                    serde_json::to_string(&legacy.provenance).unwrap(),
                    trust_to_str(legacy.trust),
                    1_i64,
                    legacy.written_at,
                    legacy.updated_at,
                    serde_json::to_string(&legacy.history).unwrap(),
                ],
            )
            .unwrap();
        }

        let room_row = {
            let mut store = SqliteMemoryStore::open(path).unwrap();
            let migrated = MemoryStore::get(&store, &legacy.id)
                .unwrap()
                .expect("legacy row survives migration");
            assert_eq!(migrated.body, legacy.body);
            assert_eq!(migrated.owner, legacy.owner);
            let partition: String = store
                .conn
                .query_row(
                    "SELECT partition FROM memories WHERE id = ?1",
                    params![legacy.id.0],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(partition, OPERATOR_PARTITION);
            for index in [
                "idx_memories_partition_owner_seq",
                "idx_memories_partition_owner_live",
            ] {
                let present: i64 = store
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master
                         WHERE type = 'index' AND name = ?1",
                        params![index],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(present, 1, "missing additive index {index}");
            }

            let room = store.trusted_room_scope("persisted-room").unwrap();
            let owner = room_memory_owner();
            store
                .scoped(&room, &owner)
                .unwrap()
                .put(sample_memory(ROOM_PARTITION_OWNER, "room survives reopen"))
                .unwrap()
        };

        let mut reopened = SqliteMemoryStore::open(path).unwrap();
        assert!(MemoryStore::get(&reopened, &legacy.id).unwrap().is_some());
        let room = reopened.trusted_room_scope("persisted-room").unwrap();
        let owner = room_memory_owner();
        assert_eq!(
            reopened
                .scoped(&room, &owner)
                .unwrap()
                .get(&room_row.id)
                .unwrap(),
            Some(room_row)
        );
    }

    #[test]
    fn memory_serde_round_trips() {
        let m = sample_memory("a", "x");
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
