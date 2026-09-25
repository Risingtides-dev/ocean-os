use crate::{Cursor, EventEnvelope, RetentionPolicy};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::{path::Path, sync::Arc, time::Duration};
use thiserror::Error;
#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// G1: the projection is destructive (one current row per execution), so
    /// Gate 1 cannot answer a snapshot at an earlier cursor. Refused rather
    /// than answered with current state under an old label.
    #[error("snapshot at cursor {requested} is historical; the current watermark is {latest}")]
    HistoricalSnapshot { requested: u64, latest: u64 },
    /// F2: the database was written by a newer build (its `user_version` is
    /// past [`crate::STORE_SCHEMA_VERSION`]); refused rather than guessed at.
    #[error(
        "observatory.db schema version {found} is newer than this build supports ({supported})"
    )]
    UnsupportedSchema { found: u32, supported: u32 },
    /// F2: a table rebuild left rows that break a foreign key; the migration
    /// rolled back and the database is unchanged.
    #[error("schema migration found {count} foreign-key violations; rolled back")]
    ForeignKeyViolation { count: i64 },
}
pub type Result<T> = std::result::Result<T, StoreError>;
pub struct ObservatoryStore {
    db: Arc<Mutex<Connection>>,
    current_cursor: Arc<Mutex<Cursor>>,
    pub retention_policy: RetentionPolicy,
}
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub watermark_cursor: Cursor,
    pub earliest_available_cursor: Cursor,
    pub nodes: Vec<ExecutionNode>,
    pub edges: Vec<ExecutionEdge>,
}
#[derive(Debug, Clone)]
pub struct ExecutionNode {
    pub execution_id: String,
    pub root_execution_id: String,
    pub parent_execution_id: Option<String>,
    pub phase: String,
    pub created_at: String,
}
#[derive(Debug, Clone)]
pub struct ExecutionEdge {
    pub edge_id: String,
    pub parent_execution_id: String,
    pub child_execution_id: String,
    pub created_at: String,
}
#[derive(Debug, Clone)]
pub struct EventsPage {
    pub events: Vec<EventEnvelope>,
    pub next_after: Option<Cursor>,
    pub has_more: bool,
    pub complete: bool,
}
impl ObservatoryStore {
    pub fn open(path: &Path, retention_policy: RetentionPolicy) -> Result<Self> {
        let mut db = Connection::open(path)?;
        // F10: wait for a competing writer (a checkpoint, an operator's
        // sqlite3 shell) instead of failing the append with SQLITE_BUSY at
        // once. Callers run store methods on blocking threads (F1), so the
        // wait never stalls an async worker.
        db.busy_timeout(BUSY_TIMEOUT)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        // F2: versioned, idempotent, step-by-step to the §4.1 shape.
        crate::migration::migrate(&mut db)?;
        // G4: seed from EVERY durable record of how far the log reached, not
        // only surviving events. A retention pass that prunes every row
        // followed by a restart would otherwise reissue cursor 1 — a reused
        // cursor, which the startup rule (§2.3) exists to forbid.
        let max = db.query_row(
            "SELECT MAX(COALESCE((SELECT MAX(cursor) FROM observatory_events),0),
                        COALESCE((SELECT MAX(cursor) FROM watermarks),0))",
            [],
            |r| r.get::<_, u64>(0),
        )?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            current_cursor: Arc::new(Mutex::new(Cursor::new(max))),
            retention_policy,
        })
    }
    pub fn append_event(&self, mut event: EventEnvelope) -> Result<Cursor> {
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        // The next cursor, published only after the commit below: a failed
        // insert (a duplicate event_id, a full disk) must not leave the
        // in-memory watermark ahead of anything durable, where a header or a
        // snapshot label could name a cursor a restart would reissue. The db
        // guard held for this whole function keeps the read-then-publish
        // race-free.
        let cursor = self.current_cursor.lock().next();
        event.cursor = cursor;
        let json = serde_json::to_string(&event)?;
        // F2: every §4.1 column, from the same envelope `envelope_json` holds.
        tx.execute(
            "INSERT INTO observatory_events(cursor,event_id,schema_version,created_at,kind,producer_id,visibility,envelope_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                cursor.into_inner(),
                event.event_id,
                event.schema_version,
                event.recorded_at,
                wire_str(&event.kind)?,
                event.producer.id,
                wire_str(&event.visibility)?,
                json
            ],
        )?;
        let phase = phase(&event);
        // `finished_at` is set exactly while the phase is terminal, so a late
        // event that reopens an execution clears it.
        let finished_at = (!matches!(phase.as_str(), "admitted" | "running"))
            .then_some(event.recorded_at.as_str());
        let t = &event.topology;
        tx.execute(
            "INSERT INTO execution_nodes(execution_id,root_execution_id,parent_execution_id,session_id,turn_id,request_id,producer_id,phase,first_cursor,last_cursor,created_at,finished_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,?10,?11)
             ON CONFLICT(execution_id) DO UPDATE SET
               phase=excluded.phase,
               last_cursor=excluded.last_cursor,
               finished_at=excluded.finished_at,
               session_id=CASE WHEN session_id='' THEN excluded.session_id ELSE session_id END,
               turn_id=CASE WHEN turn_id='' THEN excluded.turn_id ELSE turn_id END,
               request_id=CASE WHEN request_id='' THEN excluded.request_id ELSE request_id END,
               producer_id=CASE WHEN producer_id='' THEN excluded.producer_id ELSE producer_id END",
            params![
                t.execution_id,
                t.root_execution_id,
                t.parent_execution_id,
                t.session_id,
                t.turn_id,
                t.request_id,
                event.producer.id,
                phase,
                cursor.into_inner(),
                event.recorded_at,
                finished_at
            ],
        )?;
        if let (Some(edge), Some(parent)) = (t.edge_id.as_ref(), t.parent_execution_id.as_ref()) {
            tx.execute(
                "INSERT OR IGNORE INTO execution_edges(edge_id,parent_execution_id,child_execution_id,root_execution_id,created_at)
                 VALUES(?1,?2,?3,?4,?5)",
                params![edge, parent, t.execution_id, t.root_execution_id, event.recorded_at],
            )?;
        }
        tx.execute("INSERT INTO watermarks(key,cursor,created_at) VALUES('snapshot_watermark',?1,?2) ON CONFLICT(key) DO UPDATE SET cursor=excluded.cursor",params![cursor.into_inner(), event.recorded_at])?;
        tx.commit()?;
        *self.current_cursor.lock() = cursor;
        Ok(cursor)
    }
    pub fn latest_cursor(&self) -> Cursor {
        *self.current_cursor.lock()
    }
    /// Earliest cursor still present in the durable log (0 when empty).
    ///
    /// Cursors at or below a pruned retention boundary are gone; readers use
    /// this to answer 410/reset instead of silently skipping missing history.
    pub fn earliest_available_cursor(&self) -> Result<Cursor> {
        let db = self.db.lock();
        let earliest = db.query_row(
            "SELECT COALESCE(MIN(cursor),0) FROM observatory_events",
            [],
            |r| r.get::<_, u64>(0),
        )?;
        Ok(Cursor::new(earliest))
    }
    /// Nonterminal executions (phase `admitted`/`running`) with their current
    /// phase string. Used by the daemon restart sweep to close out executions
    /// a previous boot left hanging.
    pub fn nonterminal_executions(&self) -> Result<Vec<(String, String)>> {
        let db = self.db.lock();
        let mut stmt = db.prepare(
            "SELECT execution_id, phase FROM execution_nodes WHERE phase IN ('admitted','running')",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Last pruned retention boundary, when any pruning has occurred.
    ///
    /// Cursors at or below this watermark are gone for good. Unlike
    /// `earliest_available_cursor` (which is 1 on a fresh, unpruned log),
    /// this distinguishes "history was destroyed" from "history starts at 1".
    pub fn retention_boundary(&self) -> Result<Option<Cursor>> {
        let db = self.db.lock();
        let mut stmt =
            db.prepare("SELECT cursor FROM watermarks WHERE key = 'retention_boundary'")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(Cursor::new(row.get::<_, u64>(0)?))),
            None => Ok(None),
        }
    }
    pub fn events_after(&self, after: Cursor, limit: Option<usize>) -> Result<Vec<EventEnvelope>> {
        self.events_page(after, None, limit.unwrap_or(1000))
            .map(|p| p.events)
    }
    pub fn replay_page(
        &self,
        after: Cursor,
        through: Option<Cursor>,
        limit: usize,
    ) -> Result<EventsPage> {
        self.events_page(after, through, limit)
    }
    pub fn events_page(
        &self,
        after: Cursor,
        through: Option<Cursor>,
        limit: usize,
    ) -> Result<EventsPage> {
        let end = through.unwrap_or_else(|| self.latest_cursor());
        let db = self.db.lock();
        let mut s=db.prepare("SELECT envelope_json FROM observatory_events WHERE cursor>?1 AND cursor<=?2 ORDER BY cursor LIMIT ?3")?;
        let events = s
            .query_map(
                params![after.into_inner(), end.into_inner(), limit.min(10000)],
                |r| r.get::<_, String>(0),
            )?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect::<Result<Vec<EventEnvelope>>>()?;
        let more = events.len() == limit.min(10000);
        let next = events.last().map(|e| e.cursor);
        Ok(EventsPage {
            events,
            next_after: next,
            has_more: more,
            // §7.3: complete when this page reaches `through` (or, unbounded,
            // the watermark read above). The old `end == latest` test made a
            // `through`-bounded page below the watermark never complete (F4).
            complete: !more,
        })
    }
    /// Apply the retention policy (G3; manifest §4.2) and record the boundary.
    ///
    /// Events older than `max_age_days` are pruned, oldest first, and so are
    /// the oldest events while the database's live pages exceed `max_bytes` —
    /// measured from SQLite's page accounting (freelist excluded), not an
    /// estimate from envelope lengths. Neither rule may cross the first cursor of any non-terminal
    /// execution: an execution still admitted or running keeps its whole
    /// history. A non-terminal row written before `first_cursor` existed has
    /// no recorded start, so it blocks pruning until the restart sweep closes
    /// it rather than risking its history.
    pub fn apply_retention(&self) -> Result<usize> {
        let mut db = self.db.lock();
        let (nonterminal, unknown_start, min_first): (u64, u64, Option<u64>) = db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(first_cursor IS NULL),0), MIN(first_cursor)
             FROM execution_nodes WHERE phase IN ('admitted','running')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if unknown_start > 0 {
            return Ok(0);
        }
        let prunable_through = match (nonterminal, min_first) {
            // SQLite integers are signed; this is "no ceiling".
            (0, _) | (_, None) => i64::MAX as u64,
            (_, Some(first)) => first.saturating_sub(1),
        };
        let cutoff =
            chrono::Utc::now() - chrono::Duration::days(self.retention_policy.max_age_days as i64);

        // Age: the contiguous run of oldest prunable events recorded before
        // the cutoff.
        let mut stmt = db.prepare(
            "SELECT cursor, envelope_json FROM observatory_events WHERE cursor <= ?1 ORDER BY cursor",
        )?;
        let rows = stmt
            .query_map([prunable_through], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut boundary = 0;
        for (cursor, raw) in &rows {
            let event: EventEnvelope = serde_json::from_str(raw)?;
            let old = chrono::DateTime::parse_from_rfc3339(&event.recorded_at)
                .map(|t| t.with_timezone(&chrono::Utc).lt(&cutoff))
                .unwrap_or(false);
            if !old {
                break;
            }
            boundary = *cursor;
        }
        let mut pruned = prune_through(&mut db, boundary, "age")?;

        // Size: while the LIVE pages exceed the bound, prune the next oldest
        // batch and measure again. Measuring after each delete — rather than
        // subtracting JSON lengths from a page-measured excess, which ignores
        // indexes and the never-pruned projection tables — is what keeps one
        // pass from overshooting the bound. Freed pages go to the freelist
        // on commit, so `live_bytes` falls as soon as a batch lands.
        // Floor: the projection (nodes, edges, watermarks, archive) is never
        // pruned. When it alone is over the bound, no amount of event pruning
        // gets under it, and the loop would empty the log on every pass.
        // Size-prune only while the event log is what is over.
        let event_bytes: u64 = db.query_row(
            "SELECT COALESCE(SUM(length(envelope_json)),0) FROM observatory_events",
            [],
            |r| r.get(0),
        )?;
        let floor = live_bytes(&db)?.saturating_sub(event_bytes);
        let size_bound_reachable = floor < self.retention_policy.max_bytes;
        while size_bound_reachable && live_bytes(&db)? > self.retention_policy.max_bytes {
            let batch_end: Option<u64> = db.query_row(
                "SELECT MAX(cursor) FROM (SELECT cursor FROM observatory_events
                 WHERE cursor > ?1 AND cursor <= ?2 ORDER BY cursor LIMIT ?3)",
                params![boundary, prunable_through, RETENTION_BATCH],
                |r| r.get(0),
            )?;
            let Some(end) = batch_end else {
                break;
            };
            pruned += prune_through(&mut db, end, "size")?;
            boundary = end;
        }
        Ok(pruned)
    }
    /// The current projection, labelled with the watermark it actually
    /// reflects (G1). The watermark is read INSIDE the database lock that
    /// `append_event` holds for its whole transaction, so no append can land
    /// between the label and the rows. `at` is accepted only when it IS that
    /// watermark: an earlier cursor is `HistoricalSnapshot`, because the
    /// destructive projection cannot reconstruct past state.
    pub fn snapshot_at(&self, at: Option<Cursor>) -> Result<Snapshot> {
        let db = self.db.lock();
        let watermark = self.latest_cursor();
        if let Some(requested) = at {
            if requested != watermark {
                return Err(StoreError::HistoricalSnapshot {
                    requested: requested.into_inner(),
                    latest: watermark.into_inner(),
                });
            }
        }
        let mut s=db.prepare("SELECT execution_id,root_execution_id,parent_execution_id,phase,created_at FROM execution_nodes")?;
        let nodes = s
            .query_map([], |r| {
                Ok(ExecutionNode {
                    execution_id: r.get(0)?,
                    root_execution_id: r.get(1)?,
                    parent_execution_id: r.get(2)?,
                    phase: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut e = db.prepare(
            "SELECT edge_id,parent_execution_id,child_execution_id,created_at FROM execution_edges",
        )?;
        let edges = e
            .query_map([], |r| {
                Ok(ExecutionEdge {
                    edge_id: r.get(0)?,
                    parent_execution_id: r.get(1)?,
                    child_execution_id: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let earliest = db.query_row(
            "SELECT COALESCE(MIN(cursor),0) FROM observatory_events",
            [],
            |r| r.get::<_, u64>(0),
        )?;
        Ok(Snapshot {
            watermark_cursor: watermark,
            earliest_available_cursor: Cursor::new(earliest),
            nodes,
            edges,
        })
    }
}
/// How long an append or read waits on a competing SQLite lock (F10).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many events one size-bound step removes before measuring again.
const RETENTION_BATCH: u64 = 64;

/// Bytes in LIVE database pages. A DELETE returns pages to SQLite's freelist
/// but not to the filesystem, so `page_count` alone never shrinks after a
/// prune and a database that once crossed the bound would read as over it
/// forever.
fn live_bytes(db: &Connection) -> Result<u64> {
    Ok(db.query_row(
        "SELECT (page_count - freelist_count) * page_size
         FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        [],
        |r| r.get(0),
    )?)
}

/// Delete every event at or below `boundary` in one transaction and record
/// the new retention boundary, archived under `reason` (`age` or `size`).
/// `0` is a no-op.
fn prune_through(db: &mut Connection, boundary: u64, reason: &str) -> Result<usize> {
    if boundary == 0 {
        return Ok(0);
    }
    let tx = db.transaction()?;
    // F10: this pass pruned the span just past the previous boundary, not
    // everything from cursor 1.
    let previous: u64 = tx.query_row(
        "SELECT COALESCE((SELECT cursor FROM watermarks WHERE key='retention_boundary'),0)",
        [],
        |r| r.get(0),
    )?;
    let count = tx.execute(
        "DELETE FROM observatory_events WHERE cursor <= ?1",
        [boundary],
    )?;
    if count > 0 {
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute("INSERT INTO retention_archive(pruned_at,from_cursor,to_cursor,reason,count_events) VALUES(?1,?2,?3,?4,?5)", params![now, previous + 1, boundary, reason, count])?;
        tx.execute("INSERT INTO watermarks(key,cursor,created_at) VALUES('retention_boundary',?1,?2) ON CONFLICT(key) DO UPDATE SET cursor=excluded.cursor", params![boundary, now])?;
    }
    tx.commit()?;
    Ok(count)
}

/// The serde wire string of a unit enum (`kind`, `visibility`), so the column
/// holds exactly what `envelope_json` holds.
fn wire_str<T: serde::Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

fn phase(e: &EventEnvelope) -> String {
    match &e.payload {
        crate::EventPayload::ExecutionAdmitted { phase, .. }
        | crate::EventPayload::ExecutionFinished { phase, .. } => {
            format!("{phase:?}").to_lowercase()
        }
        crate::EventPayload::ExecutionPhaseChanged { to_phase, .. } => {
            format!("{to_phase:?}").to_lowercase()
        }
        _ => "running".into(),
    }
}
