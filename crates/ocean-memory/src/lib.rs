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
/// This is also the SQLite default and rebuild-migration target, so rows written
/// before partitions existed remain in the same logical namespace.
const OPERATOR_PARTITION: &str = "operator:v1";

/// Stable owner inside every room partition. The partition carries the exact
/// room identity; this shared owner makes room memory room-wide instead of
/// accidentally creating one silo per admitted agent.
const ROOM_PARTITION_OWNER: &str = "room:v1";

/// Archive table used by the explicit legacy rollback preparation.
///
/// Older Ocean binaries ignore this table and continue to read the restored
/// single-id `memories` table. A later upgrade rehydrates these room rows and
/// removes the archive in the same migration transaction.
const ROOM_ROLLBACK_ARCHIVE: &str = "memories_room_partition_archive_v1";

/// Return the only owner principal accepted for a room-memory handle.
///
/// Admission code should use this rather than an agent member id. The value is
/// intentionally the same across rooms because the independently constrained
/// partition carries the room identity.
pub fn room_memory_owner() -> PrincipalId {
    PrincipalId::new(ROOM_PARTITION_OWNER)
}

/// Evidence supplied by the room-agent admission authority when it asks the
/// memory store to issue a Room scope.
///
/// The memory crate deliberately does not implement this trait for `String`,
/// `&str`, a request DTO, or the public Room key type. The authority layer must
/// define a private, non-deserializable admitted-binding type and implement this
/// trait for that type only. That makes scope issuance an explicit, reviewable
/// call boundary instead of a raw model/tool string conversion.
///
/// This trait cannot prove that another crate implemented its admission check
/// correctly. Its implementation and the call to
/// [`SqliteMemoryStore::trusted_room_scope`] remain security-critical authority
/// code and must occur only after the binding generation and room membership
/// have been validated.
pub trait RoomMemoryAdmission {
    /// The exact authoritative persistent Room key, without normalization.
    fn admitted_room_key(&self) -> &str;
}

/// An opaque, store-authorized room memory scope.
///
/// The fields and constructor are deliberately private: model-facing memory
/// tools may carry this value, but cannot manufacture a different room by
/// concatenating a caller-controlled key. Only
/// [`SqliteMemoryStore::trusted_room_scope`] can mint one from an explicit
/// [`RoomMemoryAdmission`] value at the trusted authority/store boundary.
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

/// Counts produced by [`SqliteMemoryStore::prepare_legacy_rollback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyRollbackReport {
    /// Operator rows retained in the legacy `memories` table.
    pub operator_rows: u64,
    /// Room rows moved to the private rollback archive.
    pub archived_room_rows: u64,
}

impl SqliteMemoryStore {
    /// Open (or create) a store at `path`. Runs [`Self::migrate`] idempotently.
    /// `open(":memory:")` yields a fresh in-memory db for tests.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        Self::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Convenience: a fresh in-memory store.
    pub fn open_in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// Mint an opaque room-memory scope from explicit evidence produced by the
    /// already-authorized room-agent admission path.
    ///
    /// This method is the trusted issuer. The admission layer must use a
    /// private, non-deserializable type implementing [`RoomMemoryAdmission`];
    /// never implement that trait for a model/tool argument or request DTO.
    /// The exact admitted key is preserved and length-prefixed by UTF-8 bytes,
    /// preventing ambiguous principals such as `a:bc` and `a:b:c`.
    pub fn trusted_room_scope(
        &self,
        admission: &impl RoomMemoryAdmission,
    ) -> Result<SessionMemoryScope> {
        let room_key = admission.admitted_room_key();
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

    /// Prepare this database for opening with a pre-partition Ocean binary.
    ///
    /// This is an explicit operator rollback operation, not an automatic
    /// startup migration. It transactionally archives every non-operator row,
    /// rebuilds `memories` with the historical globally-unique `id` primary
    /// key and no `partition` column, and preserves all operator rows. The
    /// caller must close this store immediately after success; the current
    /// process must not continue using partition-aware memory operations.
    ///
    /// A subsequent open by this version migrates the legacy table forward,
    /// restores the archived room rows, and removes the archive. This makes a
    /// rollback rehearsal reversible without exposing room rows to the legacy
    /// API or discarding them.
    pub fn prepare_legacy_rollback(&mut self) -> Result<LegacyRollbackReport> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {ROOM_ROLLBACK_ARCHIVE} (
                id          TEXT NOT NULL,
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
                partition   TEXT NOT NULL,
                PRIMARY KEY (partition, owner, id)
             );
             INSERT OR IGNORE INTO {ROOM_ROLLBACK_ARCHIVE}
                (id, scope, owner, kind, body, provenance, trust, seq,
                 written_at, updated_at, deleted_at, history, partition)
             SELECT id, scope, owner, kind, body, provenance, trust, seq,
                    written_at, updated_at, deleted_at, history, partition
             FROM memories
             WHERE partition <> 'operator:v1';"
        ))?;

        let operator_rows: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE partition = ?1",
            params![OPERATOR_PARTITION],
            |row| row.get(0),
        )?;
        let archived_room_rows: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM {ROOM_ROLLBACK_ARCHIVE}"),
            [],
            |row| row.get(0),
        )?;

        tx.execute_batch(
            "DROP TABLE IF EXISTS memories_legacy_rollback;
             CREATE TABLE memories_legacy_rollback (
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
             );
             INSERT INTO memories_legacy_rollback
                (id, scope, owner, kind, body, provenance, trust, seq,
                 written_at, updated_at, deleted_at, history)
             SELECT id, scope, owner, kind, body, provenance, trust, seq,
                    written_at, updated_at, deleted_at, history
             FROM memories
             WHERE partition = 'operator:v1';
             DROP TABLE memories;
             ALTER TABLE memories_legacy_rollback RENAME TO memories;",
        )?;
        tx.commit()?;

        Ok(LegacyRollbackReport {
            operator_rows: operator_rows.max(0) as u64,
            archived_room_rows: archived_room_rows.max(0) as u64,
        })
    }

    /// Idempotent schema bootstrap and namespace migration.
    fn migrate(conn: &mut Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id          TEXT NOT NULL,
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
                partition   TEXT NOT NULL DEFAULT 'operator:v1',
                PRIMARY KEY (partition, owner, id)
            );",
        )?;

        let (has_partition, primary_key) = {
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let columns = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })?;
            let columns = columns.collect::<std::result::Result<Vec<_>, _>>()?;
            let has_partition = columns.iter().any(|(name, _)| name == "partition");
            let mut primary_key = columns
                .into_iter()
                .filter(|(_, ordinal)| *ordinal > 0)
                .collect::<Vec<_>>();
            primary_key.sort_by_key(|(_, ordinal)| *ordinal);
            (
                has_partition,
                primary_key
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
        };

        // Pre-partition databases used a globally unique `id` primary key. A
        // global key lets one room reserve or probe an id in another room, so
        // rebuild transactionally to the store-bound composite identity. The
        // API-visible `MemoryId` remains unchanged; only the SQLite key changes.
        // Legacy rows are copied verbatim into the operator partition.
        if primary_key != ["partition", "owner", "id"] {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS memories_partitioned_v2;
                 CREATE TABLE memories_partitioned_v2 (
                    id          TEXT NOT NULL,
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
                    partition   TEXT NOT NULL DEFAULT 'operator:v1',
                    PRIMARY KEY (partition, owner, id)
                 );",
            )?;
            if has_partition {
                tx.execute_batch(
                    "INSERT INTO memories_partitioned_v2
                        (id, scope, owner, kind, body, provenance, trust, seq,
                         written_at, updated_at, deleted_at, history, partition)
                     SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, deleted_at, history, partition
                     FROM memories;",
                )?;
            } else {
                tx.execute_batch(
                    "INSERT INTO memories_partitioned_v2
                        (id, scope, owner, kind, body, provenance, trust, seq,
                         written_at, updated_at, deleted_at, history, partition)
                     SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, deleted_at, history, 'operator:v1'
                     FROM memories;",
                )?;
            }
            tx.execute_batch(
                "DROP TABLE memories;
                 ALTER TABLE memories_partitioned_v2 RENAME TO memories;",
            )?;
            tx.commit()?;
        }

        let rollback_archive_exists: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = ?1
             )",
            params![ROOM_ROLLBACK_ARCHIVE],
            |row| row.get(0),
        )?;
        if rollback_archive_exists {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(&format!(
                "INSERT INTO memories
                    (id, scope, owner, kind, body, provenance, trust, seq,
                     written_at, updated_at, deleted_at, history, partition)
                 SELECT id, scope, owner, kind, body, provenance, trust, seq,
                        written_at, updated_at, deleted_at, history, partition
                 FROM {ROOM_ROLLBACK_ARCHIVE};
                 DROP TABLE {ROOM_ROLLBACK_ARCHIVE};"
            ))?;
            tx.commit()?;
        }

        // The legacy API addresses operator rows by id without an owner
        // parameter. Preserve its historical global-id invariant even though
        // Room partitions deliberately allow the same logical id. Detect a
        // database written by an invalid intermediate build before installing
        // the partial unique index, and fail without changing its rows.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate_operator_id: Option<String> = tx
            .query_row(
                "SELECT id FROM memories
                 WHERE partition = ?1
                 GROUP BY id HAVING COUNT(*) > 1
                 LIMIT 1",
                params![OPERATOR_PARTITION],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = duplicate_operator_id {
            return Err(MemoryError::BadInput(format!(
                "operator memory id {id} exists for multiple owners"
            )));
        }
        // Recreate a versioned index under the same write transaction as the
        // duplicate check. `IF NOT EXISTS` alone is unsafe here: an invalid
        // intermediate build could have created the expected name as a
        // non-unique or differently-filtered index, and SQLite would silently
        // keep it. Dropping both historical names makes the installed
        // invariant self-verifying on every open.
        tx.execute_batch(
            "DROP INDEX IF EXISTS idx_memories_operator_id;
             DROP INDEX IF EXISTS idx_memories_operator_id_v2;
             CREATE UNIQUE INDEX idx_memories_operator_id_v2
                ON memories(id) WHERE partition = 'operator:v1';",
        )?;
        tx.commit()?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_owner_seq
                ON memories(owner, seq DESC);
             CREATE INDEX IF NOT EXISTS idx_memories_owner_live
                ON memories(owner) WHERE deleted_at IS NULL;
             CREATE INDEX IF NOT EXISTS idx_memories_partition_owner_seq
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
        // The legacy operator API addresses rows by id without an owner
        // parameter, so ids remain globally unique inside that partition. A
        // matching id in a Room partition is unrelated and must not block or
        // reveal this insert.
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM memories
                 WHERE partition = ?1 AND id = ?2",
                params![OPERATOR_PARTITION, mem.id.0],
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

        // Allocate the per-owner monotonic seq inside the operator partition
        // and an IMMEDIATE transaction,
        // exactly like ocean-store's transcript seq: the write lock is taken at
        // BEGIN so a second connection cannot interleave a commit between the
        // MAX(seq)+1 read and the INSERT (the race that used to tear seq order).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_seq: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM memories
                 WHERE partition = ?1 AND owner = ?2",
                params![OPERATOR_PARTITION, mem.owner.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v.max(0) as u64)?;
        mem.seq = next_seq;
        tx.execute(
            "INSERT INTO memories
                (id, partition, scope, owner, kind, body, provenance, trust, seq,
                 written_at, updated_at, history)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                mem.id.0,
                OPERATOR_PARTITION,
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
                 WHERE partition = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![OPERATOR_PARTITION, id.0],
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
                     WHERE partition = ?1 AND owner = ?2
                           AND deleted_at IS NULL AND seq < ?3
                     ORDER BY seq DESC LIMIT ?4",
                )?;
                let mapped = stmt.query_map(
                    params![OPERATOR_PARTITION, owner.0, s as i64, (limit + 1) as i64],
                    decode_row,
                )?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, scope, owner, kind, body, provenance, trust, seq,
                            written_at, updated_at, history
                     FROM memories
                     WHERE partition = ?1 AND owner = ?2 AND deleted_at IS NULL
                     ORDER BY seq DESC LIMIT ?3",
                )?;
                let mapped = stmt.query_map(
                    params![OPERATOR_PARTITION, owner.0, (limit + 1) as i64],
                    decode_row,
                )?;
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
            "UPDATE memories SET deleted_at = ?1
             WHERE partition = ?2 AND id = ?3 AND deleted_at IS NULL",
            params![now, OPERATOR_PARTITION, id.0],
        )?;
        Ok(())
    }

    fn count(&self, owner: &PrincipalId) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM memories
             WHERE partition = ?1 AND owner = ?2 AND deleted_at IS NULL",
            params![OPERATOR_PARTITION, owner.0],
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
/// constrain both values in SQL. The legacy [`MemoryStore`] API remains
/// available for ordinary callers and is itself fixed to the operator partition.
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

    /// Test-only stand-in for the daemon's private, non-deserializable admitted
    /// binding type. Production must implement the trait only in its authority
    /// layer after admission succeeds.
    struct TestRoomAdmission(String);

    impl RoomMemoryAdmission for TestRoomAdmission {
        fn admitted_room_key(&self) -> &str {
            &self.0
        }
    }

    fn room_scope(store: &SqliteMemoryStore, key: &str) -> SessionMemoryScope {
        store
            .trusted_room_scope(&TestRoomAdmission(key.to_owned()))
            .unwrap()
    }

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

        // The capability-safe operator handle maps migrated old rows into the
        // operator partition without changing their API-visible content.
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
        let scope = room_scope(&store, key);
        assert_eq!(
            scope.partition_principal(),
            Some(format!("room:v1:{}:{key}", key.len()).as_str())
        );
        assert!(store
            .trusted_room_scope(&TestRoomAdmission("   ".into()))
            .is_err());
    }

    #[test]
    fn room_partitions_cannot_cross_read_write_delete_list_count_or_search() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = room_memory_owner();
        let room_a = room_scope(&store, "room:a");
        let room_b = room_scope(&store, "room:a:extra");

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

            // Guessed ids in other partitions are not deleted. Reusing the
            // same logical id creates an independent row in Room A; it neither
            // blocks on nor reveals Room B's row.
            scoped_a.delete(&b.id, 1_780_000_100).unwrap();
            scoped_a.delete(&global.id, 1_780_000_100).unwrap();
            let mut collision = sample_memory(ROOM_PARTITION_OWNER, "overwrite");
            collision.id = b.id.clone();
            let independent = scoped_a.put(collision).unwrap();
            assert_eq!(independent.id, b.id);
            assert_eq!(
                scoped_a.get(&b.id).unwrap().unwrap().body,
                serde_json::json!({ "note": "overwrite" })
            );
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
    fn logical_ids_are_namespaced_across_rooms_legacy_api_and_reopen() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        let logical_id = MemoryId("shared-logical-id".into());
        let room_owner = room_memory_owner();

        {
            let mut store = SqliteMemoryStore::open(path).unwrap();
            let room_a = room_scope(&store, "namespace-a");
            let room_b = room_scope(&store, "namespace-b");
            let mut memory_a = sample_memory(ROOM_PARTITION_OWNER, "room a value");
            memory_a.id = logical_id.clone();
            let mut memory_b = sample_memory(ROOM_PARTITION_OWNER, "room b value");
            memory_b.id = logical_id.clone();

            store
                .scoped(&room_a, &room_owner)
                .unwrap()
                .put(memory_a)
                .unwrap();
            store
                .scoped(&room_b, &room_owner)
                .unwrap()
                .put(memory_b)
                .unwrap();

            assert_eq!(
                store
                    .scoped(&room_a, &room_owner)
                    .unwrap()
                    .get(&logical_id)
                    .unwrap()
                    .unwrap()
                    .body,
                serde_json::json!({ "note": "room a value" })
            );
            assert_eq!(
                store
                    .scoped(&room_b, &room_owner)
                    .unwrap()
                    .get(&logical_id)
                    .unwrap()
                    .unwrap()
                    .body,
                serde_json::json!({ "note": "room b value" })
            );

            // Even using the room-wide owner and a guessed logical id, every
            // legacy API remains confined to the operator partition.
            assert_eq!(MemoryStore::get(&store, &logical_id).unwrap(), None);
            assert_eq!(MemoryStore::count(&store, &room_owner).unwrap(), 0);
            assert!(MemoryStore::list_page(&store, &room_owner, None, None)
                .unwrap()
                .memories
                .is_empty());
            MemoryStore::delete(&mut store, &logical_id, 1_780_000_300).unwrap();
            assert!(store
                .scoped(&room_a, &room_owner)
                .unwrap()
                .get(&logical_id)
                .unwrap()
                .is_some());
            assert!(store
                .scoped(&room_b, &room_owner)
                .unwrap()
                .get(&logical_id)
                .unwrap()
                .is_some());

            // The same logical id may also exist independently in the operator
            // partition, proving room rows do not create a collision oracle.
            let mut operator_row = sample_memory(ROOM_PARTITION_OWNER, "operator value");
            operator_row.id = logical_id.clone();
            let operator_row = MemoryStore::put(&mut store, operator_row).unwrap();
            assert_eq!(
                operator_row.seq, 1,
                "room sequences must not influence legacy allocation"
            );
            assert_eq!(MemoryStore::count(&store, &room_owner).unwrap(), 1);
            assert_eq!(
                MemoryStore::get(&store, &logical_id).unwrap().unwrap().body,
                serde_json::json!({ "note": "operator value" })
            );
            MemoryStore::delete(&mut store, &logical_id, 1_780_000_301).unwrap();
            assert_eq!(MemoryStore::get(&store, &logical_id).unwrap(), None);
        }

        let mut reopened = SqliteMemoryStore::open(path).unwrap();
        for (key, expected) in [
            ("namespace-a", "room a value"),
            ("namespace-b", "room b value"),
        ] {
            let scope = room_scope(&reopened, key);
            let memory = reopened
                .scoped(&scope, &room_owner)
                .unwrap()
                .get(&logical_id)
                .unwrap()
                .expect("room row survives reopen");
            assert_eq!(memory.body, serde_json::json!({ "note": expected }));
        }
    }

    #[test]
    fn room_scope_enforces_one_room_wide_owner() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let room = room_scope(&store, "shared-room");
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
    fn operator_ids_remain_globally_unique_across_owners() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let logical_id = MemoryId("operator-global-id".into());
        let scope = SessionMemoryScope::Operator;
        let alice = PrincipalId::new("alice");
        let bob = PrincipalId::new("bob");

        let mut alice_memory = sample_memory("alice", "alice value");
        alice_memory.id = logical_id.clone();
        store
            .scoped(&scope, &alice)
            .unwrap()
            .put(alice_memory)
            .unwrap();

        let mut bob_memory = sample_memory("bob", "bob value");
        bob_memory.id = logical_id.clone();
        assert!(matches!(
            store.scoped(&scope, &bob).unwrap().put(bob_memory.clone()),
            Err(MemoryError::BadInput(_))
        ));
        assert!(matches!(
            MemoryStore::put(&mut store, bob_memory),
            Err(MemoryError::BadInput(_))
        ));

        let legacy = MemoryStore::get(&store, &logical_id)
            .unwrap()
            .expect("one unambiguous operator row");
        assert_eq!(legacy.owner, alice);
        MemoryStore::delete(&mut store, &logical_id, 1_780_000_444).unwrap();
        assert_eq!(MemoryStore::get(&store, &logical_id).unwrap(), None);
        assert_eq!(
            store
                .scoped(&scope, &bob)
                .unwrap()
                .get(&logical_id)
                .unwrap(),
            None
        );

        // Room keys use a different store-bound partition and may still reuse
        // the same logical id without observing the operator collision.
        let room = room_scope(&store, "operator-id-room");
        let room_owner = room_memory_owner();
        let mut room_memory = sample_memory(ROOM_PARTITION_OWNER, "room value");
        room_memory.id = logical_id.clone();
        store
            .scoped(&room, &room_owner)
            .unwrap()
            .put(room_memory)
            .unwrap();
        assert!(store
            .scoped(&room, &room_owner)
            .unwrap()
            .get(&logical_id)
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
    fn legacy_database_rebuild_migrates_and_reopens() {
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
            let primary_key = {
                let mut stmt = store.conn.prepare("PRAGMA table_info(memories)").unwrap();
                let columns = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
                    })
                    .unwrap();
                let mut columns = columns
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap()
                    .into_iter()
                    .filter(|(_, ordinal)| *ordinal > 0)
                    .collect::<Vec<_>>();
                columns.sort_by_key(|(_, ordinal)| *ordinal);
                columns
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>()
            };
            assert_eq!(primary_key, ["partition", "owner", "id"]);
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
                assert_eq!(present, 1, "missing partition index {index}");
            }

            let room = room_scope(&store, "persisted-room");
            let owner = room_memory_owner();
            let mut same_logical_id = sample_memory(ROOM_PARTITION_OWNER, "room survives reopen");
            same_logical_id.id = legacy.id.clone();
            store
                .scoped(&room, &owner)
                .unwrap()
                .put(same_logical_id)
                .unwrap()
        };

        let mut reopened = SqliteMemoryStore::open(path).unwrap();
        assert!(MemoryStore::get(&reopened, &legacy.id).unwrap().is_some());
        let room = room_scope(&reopened, "persisted-room");
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
    fn single_id_partition_schema_rebuild_preserves_room_rows() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        let key = "pre-composite-room";
        let partition = format!("room:v1:{}:{key}", key.len());
        let mut original = sample_memory(ROOM_PARTITION_OWNER, "pre-composite value");
        original.id = MemoryId("pre-composite-logical-id".into());
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
                    history     TEXT NOT NULL DEFAULT '[]',
                    partition   TEXT NOT NULL DEFAULT 'operator:v1'
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memories
                    (id, scope, owner, kind, body, provenance, trust, seq,
                     written_at, updated_at, history, partition)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    original.id.0,
                    original.scope.as_str(),
                    original.owner.0,
                    original.kind.as_str(),
                    serde_json::to_string(&original.body).unwrap(),
                    serde_json::to_string(&original.provenance).unwrap(),
                    trust_to_str(original.trust),
                    1_i64,
                    original.written_at,
                    original.updated_at,
                    serde_json::to_string(&original.history).unwrap(),
                    partition,
                ],
            )
            .unwrap();
        }

        let mut migrated = SqliteMemoryStore::open(path).unwrap();
        let owner = room_memory_owner();
        let room = room_scope(&migrated, key);
        assert_eq!(
            migrated
                .scoped(&room, &owner)
                .unwrap()
                .get(&original.id)
                .unwrap()
                .unwrap()
                .body,
            original.body
        );

        // The rebuilt composite key now admits the same logical id in another
        // room without changing or revealing the migrated row.
        let other = room_scope(&migrated, "post-composite-room");
        let mut other_memory = sample_memory(ROOM_PARTITION_OWNER, "other room value");
        other_memory.id = original.id.clone();
        migrated
            .scoped(&other, &owner)
            .unwrap()
            .put(other_memory)
            .unwrap();
        assert_eq!(
            migrated
                .scoped(&room, &owner)
                .unwrap()
                .get(&original.id)
                .unwrap()
                .unwrap()
                .body,
            original.body
        );
    }

    #[test]
    fn legacy_rollback_archives_rooms_and_round_trips_through_old_schema() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        let logical_id = MemoryId("rollback-shared-id".into());
        let room_owner = room_memory_owner();

        {
            let mut store = SqliteMemoryStore::open(path).unwrap();
            let mut operator = sample_memory("operator", "operator survives rollback");
            operator.id = logical_id.clone();
            MemoryStore::put(&mut store, operator).unwrap();
            for (key, note) in [("rollback-a", "room a"), ("rollback-b", "room b")] {
                let room = room_scope(&store, key);
                let mut memory = sample_memory(ROOM_PARTITION_OWNER, note);
                memory.id = logical_id.clone();
                store
                    .scoped(&room, &room_owner)
                    .unwrap()
                    .put(memory)
                    .unwrap();
            }

            let report = store.prepare_legacy_rollback().unwrap();
            assert_eq!(
                report,
                LegacyRollbackReport {
                    operator_rows: 1,
                    archived_room_rows: 2,
                }
            );
        }

        // Emulate a pre-partition binary: it sees the historical schema and
        // exactly the operator row; the private archive is unreachable through
        // its table and query contract.
        {
            let conn = Connection::open(path).unwrap();
            let columns = {
                let mut stmt = conn.prepare("PRAGMA table_info(memories)").unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap()
            };
            assert!(!columns.iter().any(|column| column == "partition"));
            let operator_note: String = conn
                .query_row(
                    "SELECT body FROM memories WHERE id = ?1",
                    params![logical_id.0],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(operator_note.contains("operator survives rollback"));
            let visible_rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                .unwrap();
            assert_eq!(visible_rows, 1);
            let archived_rows: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {ROOM_ROLLBACK_ARCHIVE}"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(archived_rows, 2);
        }

        // Upgrading again restores the archived Room rows and removes the
        // rollback-only table without changing operator memory.
        let mut reopened = SqliteMemoryStore::open(path).unwrap();
        assert!(MemoryStore::get(&reopened, &logical_id).unwrap().is_some());
        for (key, expected) in [("rollback-a", "room a"), ("rollback-b", "room b")] {
            let room = room_scope(&reopened, key);
            let memory = reopened
                .scoped(&room, &room_owner)
                .unwrap()
                .get(&logical_id)
                .unwrap()
                .expect("room row restored after re-upgrade");
            assert_eq!(memory.body, serde_json::json!({ "note": expected }));
        }
        let archive_exists: bool = reopened
            .conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name = ?1
                 )",
                params![ROOM_ROLLBACK_ARCHIVE],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!archive_exists);
    }

    #[test]
    fn migration_refuses_ambiguous_operator_ids_without_changing_rows() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                    id TEXT NOT NULL, scope TEXT NOT NULL, owner TEXT NOT NULL,
                    kind TEXT NOT NULL, body TEXT NOT NULL, provenance TEXT NOT NULL,
                    trust TEXT NOT NULL, seq INTEGER NOT NULL, written_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL, deleted_at INTEGER,
                    history TEXT NOT NULL DEFAULT '[]',
                    partition TEXT NOT NULL DEFAULT 'operator:v1',
                    PRIMARY KEY (partition, owner, id)
                 );",
            )
            .unwrap();
            for owner in ["alice", "bob"] {
                let memory = sample_memory(owner, owner);
                conn.execute(
                    "INSERT INTO memories
                        (id, scope, owner, kind, body, provenance, trust, seq,
                         written_at, updated_at, history, partition)
                     VALUES ('ambiguous', ?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)",
                    params![
                        memory.scope.as_str(),
                        memory.owner.0,
                        memory.kind.as_str(),
                        serde_json::to_string(&memory.body).unwrap(),
                        serde_json::to_string(&memory.provenance).unwrap(),
                        trust_to_str(memory.trust),
                        memory.written_at,
                        memory.updated_at,
                        serde_json::to_string(&memory.history).unwrap(),
                        OPERATOR_PARTITION,
                    ],
                )
                .unwrap();
            }
        }

        assert!(matches!(
            SqliteMemoryStore::open(path),
            Err(MemoryError::BadInput(message))
                if message.contains("multiple owners")
        ));
        let conn = Connection::open(path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "failed migration must preserve ambiguous rows");
    }

    #[test]
    fn migration_replaces_wrong_named_operator_index() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                    id TEXT NOT NULL, scope TEXT NOT NULL, owner TEXT NOT NULL,
                    kind TEXT NOT NULL, body TEXT NOT NULL, provenance TEXT NOT NULL,
                    trust TEXT NOT NULL, seq INTEGER NOT NULL, written_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL, deleted_at INTEGER,
                    history TEXT NOT NULL DEFAULT '[]',
                    partition TEXT NOT NULL DEFAULT 'operator:v1',
                    PRIMARY KEY (partition, owner, id)
                 );
                 CREATE INDEX idx_memories_operator_id_v2
                    ON memories(owner);",
            )
            .unwrap();
        }

        let store = SqliteMemoryStore::open(path).unwrap();
        let index_sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_memories_operator_id_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index_sql.contains("CREATE UNIQUE INDEX"));
        assert!(index_sql.contains("partition = 'operator:v1'"));

        let first = sample_memory("alice", "one");
        let second = sample_memory("bob", "two");
        for (owner, memory) in [("alice", first), ("bob", second)] {
            let result = store.conn.execute(
                "INSERT INTO memories
                    (id, scope, owner, kind, body, provenance, trust, seq,
                     written_at, updated_at, history, partition)
                 VALUES ('same-id', ?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)",
                params![
                    memory.scope.as_str(),
                    owner,
                    memory.kind.as_str(),
                    serde_json::to_string(&memory.body).unwrap(),
                    serde_json::to_string(&memory.provenance).unwrap(),
                    trust_to_str(memory.trust),
                    memory.written_at,
                    memory.updated_at,
                    serde_json::to_string(&memory.history).unwrap(),
                    OPERATOR_PARTITION,
                ],
            );
            if owner == "alice" {
                result.unwrap();
            } else {
                assert!(matches!(
                    result,
                    Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error {
                            code: rusqlite::ErrorCode::ConstraintViolation,
                            ..
                        },
                        _
                    ))
                ));
            }
        }
    }

    #[test]
    fn memory_serde_round_trips() {
        let m = sample_memory("a", "x");
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
