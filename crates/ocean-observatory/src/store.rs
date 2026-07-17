use crate::{Cursor, EventEnvelope, RetentionPolicy};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::{path::Path, sync::Arc};
use thiserror::Error;
#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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
const SCHEMA:&str="CREATE TABLE IF NOT EXISTS observatory_events (cursor INTEGER PRIMARY KEY,event_id TEXT NOT NULL UNIQUE,envelope_json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS execution_nodes(execution_id TEXT PRIMARY KEY,root_execution_id TEXT NOT NULL,parent_execution_id TEXT,phase TEXT NOT NULL,created_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS execution_edges(edge_id TEXT PRIMARY KEY,parent_execution_id TEXT NOT NULL,child_execution_id TEXT NOT NULL,created_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS watermarks(key TEXT PRIMARY KEY,cursor INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS retention_archive(pruned_at TEXT NOT NULL,from_cursor INTEGER NOT NULL,to_cursor INTEGER NOT NULL,count_events INTEGER NOT NULL);";
impl ObservatoryStore {
    pub fn open(path: &Path, retention_policy: RetentionPolicy) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        db.execute_batch(SCHEMA)?;
        let max = db.query_row(
            "SELECT COALESCE(MAX(cursor),0) FROM observatory_events",
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
        let cursor = {
            let mut c = self.current_cursor.lock();
            *c = c.next();
            *c
        };
        event.cursor = cursor;
        let json = serde_json::to_string(&event)?;
        tx.execute(
            "INSERT INTO observatory_events(cursor,event_id,envelope_json) VALUES(?1,?2,?3)",
            params![cursor.into_inner(), event.event_id, json],
        )?;
        let phase = phase(&event);
        tx.execute("INSERT INTO execution_nodes(execution_id,root_execution_id,parent_execution_id,phase,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(execution_id) DO UPDATE SET phase=excluded.phase",params![event.topology.execution_id,event.topology.root_execution_id,event.topology.parent_execution_id,phase,event.recorded_at])?;
        if let (Some(edge), Some(parent)) = (
            event.topology.edge_id.as_ref(),
            event.topology.parent_execution_id.as_ref(),
        ) {
            tx.execute("INSERT OR IGNORE INTO execution_edges(edge_id,parent_execution_id,child_execution_id,created_at) VALUES(?1,?2,?3,?4)",params![edge,parent,event.topology.execution_id,event.recorded_at])?;
        }
        tx.execute("INSERT INTO watermarks(key,cursor) VALUES('snapshot_watermark',?1) ON CONFLICT(key) DO UPDATE SET cursor=excluded.cursor",[cursor.into_inner()])?;
        tx.commit()?;
        Ok(cursor)
    }
    pub fn latest_cursor(&self) -> Cursor {
        *self.current_cursor.lock()
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
            complete: !more && end == self.latest_cursor(),
        })
    }
    /// Prune only when all projected executions are terminal; records a retention boundary.
    pub fn apply_retention(&self) -> Result<usize> {
        let mut db = self.db.lock();
        let active: u64 = db.query_row(
            "SELECT COUNT(*) FROM execution_nodes WHERE phase IN ('admitted','running')",
            [],
            |r| r.get(0),
        )?;
        if active != 0 {
            return Ok(0);
        }
        let mut stmt =
            db.prepare("SELECT cursor, envelope_json FROM observatory_events ORDER BY cursor")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let cutoff =
            chrono::Utc::now() - chrono::Duration::days(self.retention_policy.max_age_days as i64);
        let mut total: u64 = rows.iter().map(|(_, j)| j.len() as u64).sum();
        let mut boundary = 0;
        for (cursor, raw) in &rows {
            let event: EventEnvelope = serde_json::from_str(raw)?;
            let old = chrono::DateTime::parse_from_rfc3339(&event.recorded_at)
                .map(|t| t.with_timezone(&chrono::Utc).lt(&cutoff))
                .unwrap_or(false);
            if old || total > self.retention_policy.max_bytes {
                boundary = *cursor;
                total = total.saturating_sub(raw.len() as u64);
            } else {
                break;
            }
        }
        if boundary == 0 {
            return Ok(0);
        }
        let tx = db.transaction()?;
        let count = tx.execute(
            "DELETE FROM observatory_events WHERE cursor <= ?1",
            [boundary],
        )?;
        tx.execute("INSERT INTO retention_archive(pruned_at,from_cursor,to_cursor,count_events) VALUES(?1,?2,?3,?4)", params![chrono::Utc::now().to_rfc3339(), 1u64, boundary, count])?;
        tx.execute("INSERT INTO watermarks(key,cursor) VALUES('retention_boundary',?1) ON CONFLICT(key) DO UPDATE SET cursor=excluded.cursor", [boundary])?;
        tx.commit()?;
        Ok(count)
    }
    pub fn snapshot_at(&self, at: Option<Cursor>) -> Result<Snapshot> {
        let watermark = at.unwrap_or_else(|| self.latest_cursor());
        let db = self.db.lock();
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
