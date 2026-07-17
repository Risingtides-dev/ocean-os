//! Typed API response types for Observatory snapshot and replay routes.
//!
//! These types are consumed by:
//! - The ocean-surface-ui reducer (client-side, via wasm/JSON deserialization)
//! - The ocean-daemon API handler (server-side, via serde serialization)
//!
//! # Integration (CalmIce — task-2 owner)
//!
//! This module depends on the following types from the `ocean-observatory` crate.
//! When the crate structure is finalized, verify imports still resolve:
//!
//! - `crate::Cursor` → from `cursor.rs`
//! - `crate::Producer`, `crate::ProducerKind` → from `event.rs`
//! - `crate::ExecutionPhase` → from `event.rs`
//! - `crate::TruthProvenance` → from `event.rs`
//!
//! These types MUST be re-exported from `crate` (i.e., `pub use cursor::Cursor;` etc.)
//! for this module to compile. Coordinate re-exports and any type migrations through
//! CalmIce (task-2 owner).

use crate::{Cursor, ExecutionPhase, Producer, TruthProvenance};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ObservatorySnapshot — full state snapshot (GET /v1/observatory/snapshot)
// ---------------------------------------------------------------------------

/// A consistent, transactionally-valid snapshot of the Observatory at a cursor.
///
/// The snapshot is the authoritative baseline for the client-side reducer.
/// It represents a point-in-time view of all known execution nodes, topology
/// edges, and attention-requiring items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ObservatorySnapshot {
    /// Cursor at which this snapshot was taken. This is the watermark cursor:
    /// all events up to and including this cursor are reflected in the snapshot.
    pub watermark_cursor: Cursor,

    /// Earliest cursor still available in the durable event log.
    /// Snapshots at cursors below this value return a 410 Gone.
    pub earliest_available_cursor: Cursor,

    /// Stable daemon instance identity. Changes only on permanent daemon reset.
    pub observatory_id: String,

    /// Ephemeral daemon boot identity. Changes on every daemon restart.
    pub daemon_instance_id: String,

    /// All execution nodes active or known at the snapshot cursor.
    #[serde(default)]
    pub nodes: Vec<SnapshotNode>,

    /// All parent/child topology edges known at the snapshot cursor.
    #[serde(default)]
    pub edges: Vec<SnapshotEdge>,

    /// Attention-shelf items: conditions requiring operator notice.
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
}

// ---------------------------------------------------------------------------
// SnapshotNode — a single execution in the snapshot
// ---------------------------------------------------------------------------

/// A single execution node in the Observatory topology.
///
/// Nodes represent either root executions (no parent) or child executions
/// (admitted via the extension binding seam). Each node is identified by its
/// `execution_id` (UUID v4, immutable) and carries its current phase and
/// topology relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotNode {
    /// Immutable execution identity (UUID v4).
    pub execution_id: String,

    /// Root of this execution tree. Equal to `execution_id` for root nodes.
    pub root_execution_id: String,

    /// Immediate parent execution. `None` for root nodes.
    pub parent_execution_id: Option<String>,

    /// Session identifier (from daemon session authority).
    pub session_id: String,

    /// Turn identifier (from daemon transcript).
    pub turn_id: String,

    /// Request identifier (from daemon request handling).
    pub request_id: String,

    /// Current lifecycle phase.
    pub phase: ExecutionPhase,

    /// Who produced this execution (daemon or extension).
    pub producer: Producer,

    /// Provenance: host-observed or extension-attested.
    pub truth: TruthProvenance,

    /// UTC timestamp when this execution was admitted (RFC 3339).
    pub started_at: String,

    /// UTC timestamp of the most recent event for this execution (RFC 3339).
    pub last_activity_at: String,

    /// Safe human-readable labels. Never contains PII or paths.
    #[serde(default)]
    pub labels: Vec<String>,

    /// Execution duration in milliseconds.
    /// Only present for terminal executions (Finished, Error, Canceled, TimedOut).
    pub duration_millis: Option<u64>,
}

// ---------------------------------------------------------------------------
// SnapshotEdge — a parent/child topology relationship
// ---------------------------------------------------------------------------

/// A parent/child relationship between two executions.
///
/// Edges are created through the extension admission/binding seam. The
/// relationship is immutable once recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotEdge {
    /// Immutable edge identity (UUID v4).
    pub edge_id: String,

    /// Parent execution ID.
    pub parent_execution_id: String,

    /// Child execution ID.
    pub child_execution_id: String,

    /// Root execution ID shared by both parent and child.
    pub root_execution_id: String,

    /// UTC timestamp when this edge was created (RFC 3339).
    pub created_at: String,

    /// Provenance: host-observed or extension-attested.
    pub truth: TruthProvenance,
}

// ---------------------------------------------------------------------------
// AttentionItem — a single attention-shelf entry
// ---------------------------------------------------------------------------

/// An item requiring operator notice on the attention shelf.
///
/// Attention items are derived from events during snapshot construction.
/// They represent conditions the operator should be aware of, ordered by
/// priority and recency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttentionItem {
    /// The execution this attention item relates to.
    pub execution_id: String,

    /// Priority level (lower index = higher priority):
    /// `critical` (0), `high` (1), `medium` (2), `low` (3), `info` (4).
    pub priority: AttentionPriority,

    /// Human-readable reason code from a fixed set of enum values.
    /// Examples: "permission_waiting", "execution_error", "execution_timeout",
    /// "model_reroute", "topology_rejection", "gap_interruption", "stream_reset".
    pub reason: String,

    /// UTC timestamp when this condition was first observed (RFC 3339).
    pub occurred_at: String,

    /// Whether this item has been dismissed by the operator.
    #[serde(default)]
    pub dismissed: bool,

    /// Whether this item was interrupted by a gap or stream reset before resolution.
    #[serde(default)]
    pub interrupted: bool,
}

/// Priority level for attention items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionPriority {
    /// Red — Permission blocked, error, timeout.
    Critical,
    /// Orange — Permission waiting, extended runtime, gap.
    High,
    /// Yellow — Model reroute, topology rejection.
    Medium,
    /// Blue — Execution finished, tool completed.
    Low,
    /// Neutral — Execution admitted, daemon started.
    Info,
}

impl AttentionPriority {
    /// Returns the numeric rank (0 = highest priority, 4 = lowest).
    pub fn rank(self) -> u8 {
        match self {
            AttentionPriority::Critical => 0,
            AttentionPriority::High => 1,
            AttentionPriority::Medium => 2,
            AttentionPriority::Low => 3,
            AttentionPriority::Info => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// ReplayPage — paginated replay results (GET /v1/observatory/replay)
// ---------------------------------------------------------------------------

/// A single page of paginated replay results.
///
/// Clients follow the `continuation_url` for the next page, or use the
/// `next_after` cursor in a subsequent `after` parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReplayPage {
    /// Events in this page, ordered by cursor ascending.
    #[serde(default)]
    pub events: Vec<ReplayEvent>,

    /// Cursor to use as `after` in the next request.
    /// `None` if this is the last page (no events remaining).
    pub next_after: Option<Cursor>,

    /// Whether more events exist beyond `next_after`.
    pub has_more: bool,

    /// Whether this page reaches the `through` cursor or the latest watermark.
    pub complete: bool,

    /// URL for the next page.
    /// Present when `has_more` is true.
    pub continuation_url: Option<String>,

    /// Pagination metadata.
    pub meta: ReplayMeta,
}

/// A single event in a replay page.
///
/// This is a subset of the full `EventEnvelope` — sufficient for replay
/// fidelity without repeating the full envelope's metadata for every event.
/// The client-side reducer reconstructs the equivalent state from these fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReplayEvent {
    /// Monotonic daemon-allocated cursor.
    pub cursor: Cursor,

    /// Immutable event identity (UUID v4).
    pub event_id: String,

    /// Schema version for this event.
    pub schema_version: u32,

    /// UTC timestamp when the fact occurred (RFC 3339).
    pub occurred_at: String,

    /// Kind of event.
    pub kind: String,

    /// JSON payload — varies by kind, never contains forbidden fields.
    pub payload: serde_json::Value,
}

/// Pagination metadata for a replay page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReplayMeta {
    /// The daemon instance that produced these events.
    pub daemon_instance_id: String,

    /// The observatory instance (stable daemon identity).
    pub observatory_id: String,

    /// Cursor range requested (start, exclusive).
    pub after: Cursor,

    /// Cursor range requested (end, inclusive). `None` means latest.
    pub through: Option<Cursor>,

    /// Timestamp when this page was generated (RFC 3339).
    pub generated_at: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_round_trip() {
        let snapshot = ObservatorySnapshot {
            watermark_cursor: Cursor::new(42),
            earliest_available_cursor: Cursor::new(1),
            observatory_id: "obs-123".to_string(),
            daemon_instance_id: "daemon-456".to_string(),
            nodes: vec![SnapshotNode {
                execution_id: "exec-1".to_string(),
                root_execution_id: "exec-1".to_string(),
                parent_execution_id: None,
                session_id: "sess-1".to_string(),
                turn_id: "turn-1".to_string(),
                request_id: "req-1".to_string(),
                phase: ExecutionPhase::Running,
                producer: Producer {
                    kind: crate::ProducerKind::Daemon,
                    id: "ocean-daemon".to_string(),
                },
                truth: TruthProvenance::HostObserved,
                started_at: "2026-07-17T18:02:31.123Z".to_string(),
                last_activity_at: "2026-07-17T18:02:35.456Z".to_string(),
                labels: vec!["root_agent".to_string()],
                duration_millis: None,
            }],
            edges: vec![],
            attention: vec![],
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: ObservatorySnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.watermark_cursor, Cursor::new(42));
        assert_eq!(deserialized.nodes.len(), 1);
        assert_eq!(deserialized.nodes[0].phase, ExecutionPhase::Running);
        assert_eq!(
            deserialized.nodes[0].producer.kind,
            crate::ProducerKind::Daemon
        );
    }

    #[test]
    fn test_snapshot_empty_edges_and_attention() {
        let snapshot = ObservatorySnapshot {
            watermark_cursor: Cursor::new(10),
            earliest_available_cursor: Cursor::new(1),
            observatory_id: "obs-1".to_string(),
            daemon_instance_id: "daemon-1".to_string(),
            nodes: vec![],
            edges: vec![],
            attention: vec![],
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        // Verify edges and attention are serialized as empty arrays (not null)
        assert!(json.contains(r#""edges":[]"#));
        assert!(json.contains(r#""attention":[]"#));
    }

    #[test]
    fn test_attention_item_priority_rank() {
        assert_eq!(AttentionPriority::Critical.rank(), 0);
        assert_eq!(AttentionPriority::High.rank(), 1);
        assert_eq!(AttentionPriority::Medium.rank(), 2);
        assert_eq!(AttentionPriority::Low.rank(), 3);
        assert_eq!(AttentionPriority::Info.rank(), 4);
    }

    #[test]
    fn test_attention_item_round_trip() {
        let item = AttentionItem {
            execution_id: "exec-2".to_string(),
            priority: AttentionPriority::High,
            reason: "permission_waiting".to_string(),
            occurred_at: "2026-07-17T18:05:00.000Z".to_string(),
            dismissed: false,
            interrupted: false,
        };

        let json = serde_json::to_string(&item).unwrap();
        let deserialized: AttentionItem = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.execution_id, "exec-2");
        assert_eq!(deserialized.priority, AttentionPriority::High);
        assert!(!deserialized.dismissed);
    }

    #[test]
    fn test_replay_page_round_trip() {
        let page = ReplayPage {
            events: vec![ReplayEvent {
                cursor: Cursor::new(100),
                event_id: "evt-100".to_string(),
                schema_version: 1,
                occurred_at: "2026-07-17T18:10:00.000Z".to_string(),
                kind: "execution.admitted".to_string(),
                payload: serde_json::json!({
                    "phase": "running",
                    "labels": ["test"]
                }),
            }],
            next_after: Some(Cursor::new(101)),
            has_more: true,
            complete: false,
            continuation_url: Some(
                "/v1/observatory/replay?after=101&through=200&limit=1000".to_string(),
            ),
            meta: ReplayMeta {
                daemon_instance_id: "daemon-456".to_string(),
                observatory_id: "obs-123".to_string(),
                after: Cursor::new(99),
                through: Some(Cursor::new(200)),
                generated_at: "2026-07-17T18:10:00.500Z".to_string(),
            },
        };

        let json = serde_json::to_string(&page).unwrap();
        let deserialized: ReplayPage = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.events.len(), 1);
        assert_eq!(deserialized.events[0].kind, "execution.admitted");
        assert!(deserialized.has_more);
        assert_eq!(deserialized.next_after, Some(Cursor::new(101)));
    }

    #[test]
    fn test_replay_page_empty() {
        let page = ReplayPage {
            events: vec![],
            next_after: None,
            has_more: false,
            complete: true,
            continuation_url: None,
            meta: ReplayMeta {
                daemon_instance_id: "daemon-1".to_string(),
                observatory_id: "obs-1".to_string(),
                after: Cursor::new(200),
                through: Some(Cursor::new(200)),
                generated_at: "2026-07-17T18:20:00.000Z".to_string(),
            },
        };

        let json = serde_json::to_string(&page).unwrap();
        let deserialized: ReplayPage = serde_json::from_str(&json).unwrap();

        assert!(deserialized.events.is_empty());
        assert!(deserialized.complete);
        assert!(deserialized.continuation_url.is_none());
    }

    #[test]
    fn test_snapshot_node_with_duration() {
        let node = SnapshotNode {
            execution_id: "exec-3".to_string(),
            root_execution_id: "exec-3".to_string(),
            parent_execution_id: None,
            session_id: "sess-3".to_string(),
            turn_id: "turn-3".to_string(),
            request_id: "req-3".to_string(),
            phase: ExecutionPhase::Finished,
            producer: Producer {
                kind: crate::ProducerKind::Daemon,
                id: "ocean-daemon".to_string(),
            },
            truth: TruthProvenance::HostObserved,
            started_at: "2026-07-17T18:00:00.000Z".to_string(),
            last_activity_at: "2026-07-17T18:01:30.000Z".to_string(),
            labels: vec![],
            duration_millis: Some(90_000),
        };

        let json = serde_json::to_string(&node).unwrap();
        let deserialized: SnapshotNode = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.duration_millis, Some(90_000));
        assert_eq!(deserialized.phase, ExecutionPhase::Finished);
    }

    #[test]
    fn test_edge_round_trip() {
        let edge = SnapshotEdge {
            edge_id: "edge-1".to_string(),
            parent_execution_id: "parent-1".to_string(),
            child_execution_id: "child-1".to_string(),
            root_execution_id: "root-1".to_string(),
            created_at: "2026-07-17T18:02:32.789Z".to_string(),
            truth: TruthProvenance::HostObserved,
        };

        let json = serde_json::to_string(&edge).unwrap();
        let deserialized: SnapshotEdge = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.parent_execution_id, "parent-1");
        assert_eq!(deserialized.child_execution_id, "child-1");
    }
}
