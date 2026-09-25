//! Versioned, idempotent schema migrations for `observatory.db` (F2;
//! manifest §4.1).
//!
//! The version lives in `PRAGMA user_version`. Every database that predates
//! this module reads 0, whatever shape it has (fresh, the Task 3 tables, or
//! those tables plus the G3 `first_cursor` column), and [`migrate`] walks it
//! forward one step at a time. Each step runs in its own `BEGIN IMMEDIATE`
//! transaction, re-reads the version inside it (so two processes opening the
//! same file cannot both run a step), and bumps the version in that same
//! transaction, so a crash leaves the database at the last completed step.
//!
//! - **v1** — the pre-F2 baseline: the Task 3 tables, created if absent, plus
//!   the G3 additive `execution_nodes.first_cursor` column.
//! - **v2** — the §4.1 shape. SQLite cannot add a constraint or a `NOT NULL`
//!   column to an existing table, so every table is rebuilt with SQLite's
//!   documented procedure: `foreign_keys` off, create `*_new`, copy, drop,
//!   rename, `PRAGMA foreign_key_check`, commit, `foreign_keys` back on. New
//!   columns are backfilled from `envelope_json`. Documented deviations from
//!   §4.1 are listed on [`SCHEMA_V2`] and in the Task 9 review doc.

use crate::store::{Result, StoreError};
use rusqlite::{Connection, TransactionBehavior};

/// The schema version this build writes and reads.
pub const STORE_SCHEMA_VERSION: u32 = 2;

/// The pre-F2 tables (Task 3), exactly as they shipped.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS observatory_events (cursor INTEGER PRIMARY KEY,event_id TEXT NOT NULL UNIQUE,envelope_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS execution_nodes(execution_id TEXT PRIMARY KEY,root_execution_id TEXT NOT NULL,parent_execution_id TEXT,phase TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS execution_edges(edge_id TEXT PRIMARY KEY,parent_execution_id TEXT NOT NULL,child_execution_id TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS watermarks(key TEXT PRIMARY KEY,cursor INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS retention_archive(pruned_at TEXT NOT NULL,from_cursor INTEGER NOT NULL,to_cursor INTEGER NOT NULL,count_events INTEGER NOT NULL);
";

/// The §4.1 tables, created under `*_new` names and renamed into place.
///
/// Deviations from §4.1, each forced by a Gate 1 invariant:
///
/// - `observatory_events` keeps `cursor INTEGER PRIMARY KEY` (the rowid)
///   instead of a surrogate `id` plus `UNIQUE(cursor)`: the cursor already is
///   the unique, monotonic row identity, and replay range scans stay on the
///   table b-tree. It keeps `envelope_json` (the whole envelope) instead of
///   `payload_json`, because replay and the SSE tail return the full §7.3
///   envelope — truth, topology, correlation — which no §4.1 column holds.
/// - `execution_nodes.first_cursor`/`last_cursor` carry no foreign key to
///   `observatory_events(cursor)`. Retention prunes terminal executions'
///   events while their nodes stay in the snapshot, so `RESTRICT` would block
///   every prune, `CASCADE` would delete projection rows, and `SET NULL`
///   would turn a terminal row's start into the "unknown start" NULL that G3
///   reads as "block retention" if the execution ever runs again. Both stay
///   nullable: rows written before G3 have no recorded start, and a node
///   whose events were all pruned before this migration has no last cursor.
/// - `execution_edges` has a foreign key only on `child_execution_id`. The
///   child node is upserted in the same transaction as its edge, so that one
///   always holds. The parent and root are sessions the daemon may never
///   have observed being created (a session restored from disk, a lagged
///   pump), so enforcing them would refuse — and lose — every event of such a
///   turn, and would fail `foreign_key_check` on existing databases.
/// - `watermarks.cursor` carries no foreign key: `retention_boundary` names a
///   pruned cursor by definition, and G4 reseeds the cursor from watermarks
///   after a full prune.
const SCHEMA_V2: &str = "
CREATE TABLE observatory_events_new (
    cursor INTEGER PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    schema_version INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'unknown',
    producer_id TEXT NOT NULL DEFAULT '',
    visibility TEXT NOT NULL DEFAULT 'metadata',
    envelope_json TEXT NOT NULL
);
CREATE TABLE execution_nodes_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id TEXT NOT NULL UNIQUE,
    root_execution_id TEXT NOT NULL,
    parent_execution_id TEXT,
    session_id TEXT NOT NULL DEFAULT '',
    turn_id TEXT NOT NULL DEFAULT '',
    request_id TEXT NOT NULL DEFAULT '',
    producer_id TEXT NOT NULL DEFAULT '',
    phase TEXT NOT NULL,
    first_cursor INTEGER,
    last_cursor INTEGER,
    created_at TEXT NOT NULL,
    finished_at TEXT
);
CREATE TABLE execution_edges_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id TEXT NOT NULL UNIQUE,
    parent_execution_id TEXT NOT NULL,
    child_execution_id TEXT NOT NULL,
    root_execution_id TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    FOREIGN KEY (child_execution_id) REFERENCES execution_nodes(execution_id)
);
CREATE TABLE watermarks_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL UNIQUE,
    cursor INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT ''
);
CREATE TABLE retention_archive_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pruned_at TEXT NOT NULL,
    from_cursor INTEGER NOT NULL,
    to_cursor INTEGER NOT NULL,
    reason TEXT,
    count_events INTEGER NOT NULL
);
";

/// Copy every pre-F2 row into the §4.1 tables, backfilling from
/// `envelope_json`. `watermarks` is copied separately by [`to_v2`], because
/// its `created_at` is the migration time: no older record holds one.
///
/// A row whose `envelope_json` is not valid JSON is still copied, with
/// sentinel values, rather than failing the migration or dropping the event.
const COPY_V1_TO_V2: &str = "
INSERT INTO observatory_events_new(cursor,event_id,schema_version,created_at,kind,producer_id,visibility,envelope_json)
SELECT cursor, event_id,
       COALESCE(CASE WHEN json_valid(envelope_json) THEN json_extract(envelope_json,'$.schema_version') END, 0),
       COALESCE(CASE WHEN json_valid(envelope_json) THEN json_extract(envelope_json,'$.recorded_at') END, ''),
       COALESCE(CASE WHEN json_valid(envelope_json) THEN json_extract(envelope_json,'$.kind') END, 'unknown'),
       COALESCE(CASE WHEN json_valid(envelope_json) THEN json_extract(envelope_json,'$.producer.id') END, ''),
       COALESCE(CASE WHEN json_valid(envelope_json) THEN json_extract(envelope_json,'$.visibility') END, 'metadata'),
       envelope_json
FROM observatory_events ORDER BY cursor;

CREATE TEMP TABLE f2_last_event AS
SELECT json_extract(envelope_json,'$.topology.execution_id') AS execution_id, MAX(cursor) AS last_cursor
FROM observatory_events WHERE json_valid(envelope_json) GROUP BY 1;

INSERT INTO execution_nodes_new(execution_id,root_execution_id,parent_execution_id,session_id,turn_id,request_id,producer_id,phase,first_cursor,last_cursor,created_at,finished_at)
SELECT n.execution_id, n.root_execution_id, n.parent_execution_id,
       COALESCE(json_extract(e.envelope_json,'$.topology.session_id'), ''),
       COALESCE(json_extract(e.envelope_json,'$.topology.turn_id'), ''),
       COALESCE(json_extract(e.envelope_json,'$.topology.request_id'), ''),
       COALESCE(json_extract(e.envelope_json,'$.producer.id'), ''),
       n.phase, n.first_cursor, l.last_cursor, n.created_at,
       CASE WHEN n.phase NOT IN ('admitted','running') THEN json_extract(e.envelope_json,'$.recorded_at') END
FROM execution_nodes n
LEFT JOIN f2_last_event l ON l.execution_id = n.execution_id
LEFT JOIN observatory_events e ON e.cursor = l.last_cursor
ORDER BY n.rowid;

INSERT INTO execution_edges_new(edge_id,parent_execution_id,child_execution_id,root_execution_id,created_at)
SELECT g.edge_id, g.parent_execution_id, g.child_execution_id,
       COALESCE(n.root_execution_id, g.parent_execution_id), g.created_at
FROM execution_edges g JOIN execution_nodes n ON n.execution_id = g.child_execution_id
ORDER BY g.rowid;

INSERT INTO retention_archive_new(pruned_at,from_cursor,to_cursor,reason,count_events)
SELECT pruned_at, from_cursor, to_cursor, NULL, count_events FROM retention_archive ORDER BY rowid;

DROP TABLE f2_last_event;
DROP TABLE observatory_events;
DROP TABLE execution_nodes;
DROP TABLE execution_edges;
DROP TABLE watermarks;
DROP TABLE retention_archive;
ALTER TABLE observatory_events_new RENAME TO observatory_events;
ALTER TABLE execution_nodes_new RENAME TO execution_nodes;
ALTER TABLE execution_edges_new RENAME TO execution_edges;
ALTER TABLE watermarks_new RENAME TO watermarks;
ALTER TABLE retention_archive_new RENAME TO retention_archive;
";

/// The §4.1 indexes, by their manifest names — minus four that would
/// duplicate an index SQLite already keeps: `idx_observatory_events_cursor`
/// (the cursor IS the rowid), and `idx_observatory_events_event_id`,
/// `idx_execution_nodes_execution_id`, `idx_execution_edges_edge_id` (each
/// column is UNIQUE, so it already has its autoindex). A duplicate costs every
/// append and several MB at production size for no read benefit.
const INDEXES_V2: &str = "
CREATE INDEX IF NOT EXISTS idx_observatory_events_kind ON observatory_events(kind);
CREATE INDEX IF NOT EXISTS idx_execution_nodes_root_id ON execution_nodes(root_execution_id);
CREATE INDEX IF NOT EXISTS idx_execution_nodes_parent_id ON execution_nodes(parent_execution_id);
CREATE INDEX IF NOT EXISTS idx_execution_nodes_phase ON execution_nodes(phase);
CREATE INDEX IF NOT EXISTS idx_execution_nodes_session_id ON execution_nodes(session_id);
CREATE INDEX IF NOT EXISTS idx_execution_edges_parent_id ON execution_edges(parent_execution_id);
CREATE INDEX IF NOT EXISTS idx_execution_edges_child_id ON execution_edges(child_execution_id);
CREATE INDEX IF NOT EXISTS idx_retention_archive_pruned_at ON retention_archive(pruned_at);
";

/// The §4.1 index names [`migrate`] guarantees exist.
pub const STORE_INDEXES: [&str; 8] = [
    "idx_observatory_events_kind",
    "idx_execution_nodes_root_id",
    "idx_execution_nodes_parent_id",
    "idx_execution_nodes_phase",
    "idx_execution_nodes_session_id",
    "idx_execution_edges_parent_id",
    "idx_execution_edges_child_id",
    "idx_retention_archive_pruned_at",
];

fn user_version(db: &Connection) -> Result<u32> {
    Ok(db.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Bring `db` to [`STORE_SCHEMA_VERSION`]. A no-op on a current database;
/// refuses a database from a newer build rather than guessing at its shape.
pub fn migrate(db: &mut Connection) -> Result<()> {
    loop {
        let found = user_version(db)?;
        if found > STORE_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found,
                supported: STORE_SCHEMA_VERSION,
            });
        }
        if found == STORE_SCHEMA_VERSION {
            return Ok(());
        }
        match found {
            0 => step(db, 0, to_v1)?,
            1 => {
                // Foreign-key enforcement cannot change inside a transaction,
                // and must be off while tables are dropped and renamed.
                db.execute_batch("PRAGMA foreign_keys=OFF")?;
                let result = step(db, 1, to_v2);
                db.execute_batch("PRAGMA foreign_keys=ON")?;
                result?;
            }
            _ => unreachable!("every version below the current one has a step"),
        }
    }
}

/// Run one step in its own IMMEDIATE transaction, only if the database is
/// still at `from` once the write lock is held.
fn step(db: &mut Connection, from: u32, body: fn(&Connection) -> Result<()>) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if user_version(&tx)? != from {
        return Ok(()); // another opener ran it first; the loop re-reads
    }
    body(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version={}", from + 1))?;
    tx.commit()?;
    Ok(())
}

fn to_v1(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA_V1)?;
    let has_first_cursor = db
        .prepare("SELECT 1 FROM pragma_table_info('execution_nodes') WHERE name='first_cursor'")?
        .exists([])?;
    if !has_first_cursor {
        db.execute_batch("ALTER TABLE execution_nodes ADD COLUMN first_cursor INTEGER")?;
    }
    Ok(())
}

fn to_v2(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA_V2)?;
    db.execute(
        "INSERT INTO watermarks_new(key,cursor,created_at)
         SELECT key, cursor, ?1 FROM watermarks ORDER BY rowid",
        [chrono::Utc::now().to_rfc3339()],
    )?;
    db.execute_batch(COPY_V1_TO_V2)?;
    db.execute_batch(INDEXES_V2)?;
    let violations: i64 =
        db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if violations > 0 {
        return Err(StoreError::ForeignKeyViolation { count: violations });
    }
    Ok(())
}
