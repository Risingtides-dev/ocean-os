//! F2: the versioned §4.1 migration on databases written by the old schema.
use ocean_observatory::*;
use rusqlite::{params, Connection};
use std::path::Path;
use tempfile::tempdir;

/// The Task 3 schema exactly as it shipped, before G3's `first_cursor`.
const TASK3_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS observatory_events (cursor INTEGER PRIMARY KEY,event_id TEXT NOT NULL UNIQUE,envelope_json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS execution_nodes(execution_id TEXT PRIMARY KEY,root_execution_id TEXT NOT NULL,parent_execution_id TEXT,phase TEXT NOT NULL,created_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS execution_edges(edge_id TEXT PRIMARY KEY,parent_execution_id TEXT NOT NULL,child_execution_id TEXT NOT NULL,created_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS watermarks(key TEXT PRIMARY KEY,cursor INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS retention_archive(pruned_at TEXT NOT NULL,from_cursor INTEGER NOT NULL,to_cursor INTEGER NOT NULL,count_events INTEGER NOT NULL);";

#[allow(clippy::too_many_arguments)]
fn envelope(
    cursor: u64,
    id: &str,
    execution: &str,
    root: &str,
    parent: Option<&str>,
    kind: EventKind,
    payload: EventPayload,
    days_ago: i64,
) -> EventEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        cursor: Cursor::new(cursor),
        event_id: id.into(),
        observatory_id: "obs".into(),
        daemon_instance_id: "daemon".into(),
        occurred_at: "2026-07-17T00:00:00.000Z".into(),
        recorded_at: (chrono::Utc::now() - chrono::Duration::days(days_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        kind,
        truth: TruthProvenance::HostObserved,
        producer: Producer {
            kind: ProducerKind::Daemon,
            id: format!("producer-{execution}"),
        },
        topology: Topology {
            execution_id: execution.into(),
            root_execution_id: root.into(),
            parent_execution_id: parent.map(Into::into),
            edge_id: parent.map(|p| format!("edge:{p}:{execution}")),
            session_id: format!("session-{root}"),
            turn_id: format!("turn-{execution}"),
            request_id: format!("request-{execution}"),
        },
        correlation: Correlation {
            tool_call_id: None,
            permission_id: None,
        },
        visibility: Visibility::Metadata,
        payload,
    }
}

fn admitted(phase: ExecutionPhase) -> (EventKind, EventPayload) {
    (
        EventKind::ExecutionAdmitted,
        EventPayload::ExecutionAdmitted {
            phase,
            labels: vec![],
        },
    )
}

fn finished() -> (EventKind, EventPayload) {
    (
        EventKind::ExecutionFinished,
        EventPayload::ExecutionFinished {
            phase: ExecutionPhase::Finished,
            duration_millis: 5,
            error_classification: None,
        },
    )
}

/// (execution, root, parent, phase, created_at, first_cursor)
type LegacyNode = (
    &'static str,
    &'static str,
    Option<&'static str>,
    &'static str,
    &'static str,
    Option<u64>,
);

/// Write a v0 database by hand with the OLD schema: `with_first_cursor`
/// chooses the G3 shape (the additive column, one row NULL) or plain Task 3.
///
/// Contents: a session `s` (admitted, running forever), a finished turn `t`
/// parented to it, a turn `orphan` whose parent session was never observed,
/// and a finished execution `gone` whose events were all pruned already.
fn build_v0(path: &Path, with_first_cursor: bool) -> Vec<EventEnvelope> {
    let db = Connection::open(path).unwrap();
    db.execute_batch(TASK3_SCHEMA).unwrap();
    if with_first_cursor {
        db.execute_batch("ALTER TABLE execution_nodes ADD COLUMN first_cursor INTEGER")
            .unwrap();
    }
    let (ka, pa) = admitted(ExecutionPhase::Admitted);
    let (kr, pr) = admitted(ExecutionPhase::Running);
    let (kf, pf) = finished();
    let events = vec![
        envelope(3, "e3", "s", "s", None, ka, pa, 30),
        envelope(4, "e4", "t", "s", Some("s"), kr, pr.clone(), 30),
        envelope(5, "e5", "t", "s", Some("s"), kf, pf, 30),
        envelope(6, "e6", "orphan", "lost", Some("lost"), kr, pr, 0),
    ];
    for e in &events {
        db.execute(
            "INSERT INTO observatory_events(cursor,event_id,envelope_json) VALUES(?1,?2,?3)",
            params![
                e.cursor.into_inner(),
                e.event_id,
                serde_json::to_string(e).unwrap()
            ],
        )
        .unwrap();
    }
    let nodes: [LegacyNode; 4] = [
        (
            "gone",
            "gone",
            None,
            "finished",
            "2026-07-01T00:00:00Z",
            Some(1),
        ),
        ("s", "s", None, "admitted", "2026-07-02T00:00:00Z", None),
        (
            "t",
            "s",
            Some("s"),
            "finished",
            "2026-07-03T00:00:00Z",
            Some(4),
        ),
        (
            "orphan",
            "lost",
            Some("lost"),
            "running",
            "2026-07-04T00:00:00Z",
            Some(6),
        ),
    ];
    for (id, root, parent, phase, at, first) in nodes {
        if with_first_cursor {
            db.execute(
                "INSERT INTO execution_nodes(execution_id,root_execution_id,parent_execution_id,phase,created_at,first_cursor) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, root, parent, phase, at, first],
            )
            .unwrap();
        } else {
            db.execute(
                "INSERT INTO execution_nodes(execution_id,root_execution_id,parent_execution_id,phase,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![id, root, parent, phase, at],
            )
            .unwrap();
        }
    }
    db.execute_batch(
        "INSERT INTO execution_edges VALUES('edge:s:t','s','t','2026-07-03T00:00:00Z');
         INSERT INTO execution_edges VALUES('edge:lost:orphan','lost','orphan','2026-07-04T00:00:00Z');
         INSERT INTO watermarks VALUES('snapshot_watermark',6);
         INSERT INTO watermarks VALUES('retention_boundary',2);
         INSERT INTO retention_archive VALUES('2026-07-10T00:00:00Z',1,2,2);",
    )
    .unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
    events
}

fn user_version(db: &Connection) -> u32 {
    db.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// Every table's schema and every row, as text, in a stable order.
fn dump(path: &Path) -> Vec<String> {
    let db = Connection::open(path).unwrap();
    let mut out = vec![format!("user_version={}", user_version(&db))];
    let mut s = db
        .prepare("SELECT type, name, COALESCE(sql,'') FROM sqlite_master ORDER BY type, name")
        .unwrap();
    out.extend(
        s.query_map([], |r| {
            Ok(format!(
                "{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ))
        })
        .unwrap()
        .map(std::result::Result::unwrap),
    );
    for table in [
        "observatory_events",
        "execution_nodes",
        "execution_edges",
        "watermarks",
        "retention_archive",
    ] {
        let mut s = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let n = s.column_count();
        out.extend(
            s.query_map([], |r| {
                let cols: Vec<String> = (0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect();
                Ok(format!("{table}:{}", cols.join("|")))
            })
            .unwrap()
            .map(std::result::Result::unwrap),
        );
    }
    out
}

type NodeRow = (
    String,
    String,
    String,
    String,
    String,
    Option<u64>,
    Option<u64>,
    Option<String>,
);

fn node(db: &Connection, id: &str) -> NodeRow {
    db.query_row(
        "SELECT session_id,turn_id,request_id,producer_id,phase,first_cursor,last_cursor,finished_at
         FROM execution_nodes WHERE execution_id=?1",
        [id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        },
    )
    .unwrap()
}

fn check_migrated(path: &Path, events: &[EventEnvelope], with_first_cursor: bool) {
    let store = ObservatoryStore::open(path, RetentionPolicy::default()).unwrap();
    // G4 still seeds from the watermarks.
    assert_eq!(store.latest_cursor(), Cursor::new(6));
    assert_eq!(store.retention_boundary().unwrap(), Some(Cursor::new(2)));

    // Every event survives, byte-for-byte in envelope_json.
    let replayed = store.events_after(Cursor::new(0), None).unwrap();
    assert_eq!(
        replayed
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["e3", "e4", "e5", "e6"]
    );
    let db = Connection::open(path).unwrap();
    assert_eq!(user_version(&db), STORE_SCHEMA_VERSION);
    for e in events {
        let (json, sv, created, kind, producer, vis): (String, u32, String, String, String, String) = db
            .query_row(
                "SELECT envelope_json,schema_version,created_at,kind,producer_id,visibility FROM observatory_events WHERE cursor=?1",
                [e.cursor.into_inner()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(json, serde_json::to_string(e).unwrap());
        assert_eq!(sv, e.schema_version);
        assert_eq!(created, e.recorded_at);
        assert_eq!(
            kind,
            serde_json::to_value(e.kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        );
        assert_eq!(producer, e.producer.id);
        assert_eq!(vis, "metadata");
    }

    // Every node survives; last_cursor is backfilled from its newest event,
    // NULL where every event was already pruned; first_cursor is carried as
    // it was, NULL included.
    let count: u32 = db
        .query_row("SELECT COUNT(*) FROM execution_nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
    let first = |c: u64| with_first_cursor.then_some(c);
    let gone = node(&db, "gone");
    assert_eq!(
        gone,
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "finished".into(),
            first(1),
            None,
            None
        )
    );
    let s = node(&db, "s");
    assert_eq!(
        s,
        (
            "session-s".into(),
            "turn-s".into(),
            "request-s".into(),
            "producer-s".into(),
            "admitted".into(),
            None,
            Some(3),
            None
        )
    );
    let t = node(&db, "t");
    assert_eq!(
        t,
        (
            "session-s".into(),
            "turn-t".into(),
            "request-t".into(),
            "producer-t".into(),
            "finished".into(),
            first(4),
            Some(5),
            Some(events[2].recorded_at.clone())
        )
    );
    assert_eq!(node(&db, "orphan").6, Some(6));

    // Both edges survive — including the one whose parent was never
    // observed — with the root backfilled from the child node.
    let mut s = db
        .prepare("SELECT edge_id,parent_execution_id,child_execution_id,root_execution_id FROM execution_edges ORDER BY id")
        .unwrap();
    let edges: Vec<(String, String, String, String)> = s
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();
    assert_eq!(
        edges,
        [
            ("edge:s:t".into(), "s".into(), "t".into(), "s".into()),
            (
                "edge:lost:orphan".into(),
                "lost".into(),
                "orphan".into(),
                "lost".into()
            ),
        ]
    );

    // Watermarks and the archive carry over.
    let marks: u32 = db
        .query_row(
            "SELECT COUNT(*) FROM watermarks WHERE created_at <> ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marks, 2);
    let archive: (u64, u64, u64, Option<String>) = db
        .query_row(
            "SELECT from_cursor,to_cursor,count_events,reason FROM retention_archive",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(archive, (1, 2, 2, None));

    // The snapshot is unchanged: same nodes in the same order, same edges.
    let snap = store.snapshot_at(None).unwrap();
    assert_eq!(
        snap.nodes
            .iter()
            .map(|n| n.execution_id.as_str())
            .collect::<Vec<_>>(),
        ["gone", "s", "t", "orphan"]
    );
    assert_eq!(snap.edges.len(), 2);
    let violations: u32 = db
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn migrating_a_g3_v0_database_preserves_every_row_and_backfills() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    let events = build_v0(&path, true);
    check_migrated(&path, &events, true);
}

#[test]
fn migrating_a_task3_v0_database_without_first_cursor_preserves_every_row() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    let events = build_v0(&path, false);
    check_migrated(&path, &events, false);
}

#[test]
fn running_the_migration_twice_is_a_no_op() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    build_v0(&path, true);
    drop(ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap());
    let once = dump(&path);
    drop(ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap());
    assert_eq!(dump(&path), once, "a second open changes nothing");
    let mut db = Connection::open(&path).unwrap();
    migrate(&mut db).unwrap();
    drop(db);
    assert_eq!(dump(&path), once, "a direct second migrate changes nothing");
}

#[test]
fn a_database_from_a_newer_build_is_refused_untouched() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    build_v0(&path, true);
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=99")
        .unwrap();
    let before = dump(&path);
    let err = ObservatoryStore::open(&path, RetentionPolicy::default())
        .err()
        .expect("refused");
    assert!(matches!(
        err,
        StoreError::UnsupportedSchema {
            found: 99,
            supported: STORE_SCHEMA_VERSION
        }
    ));
    assert_eq!(dump(&path), before);
}

#[test]
fn an_unparseable_envelope_is_kept_with_sentinel_columns() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(TASK3_SCHEMA).unwrap();
        db.execute_batch("INSERT INTO observatory_events VALUES(1,'bad','not json')")
            .unwrap();
    }
    drop(ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap());
    let db = Connection::open(&path).unwrap();
    let row: (String, u32, String, String, String, String) = db
        .query_row(
            "SELECT envelope_json,schema_version,created_at,kind,producer_id,visibility FROM observatory_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "not json".into(),
            0,
            String::new(),
            "unknown".into(),
            String::new(),
            "metadata".into()
        )
    );
}

#[test]
fn new_appends_populate_every_column() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    let store = ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap();
    let (ka, pa) = admitted(ExecutionPhase::Admitted);
    let (kr, pr) = admitted(ExecutionPhase::Running);
    let (kf, pf) = finished();
    store
        .append_event(envelope(0, "a", "s", "s", None, ka, pa, 0))
        .unwrap();
    let mut running = envelope(0, "b", "t", "s", Some("s"), kr, pr, 0);
    running.visibility = Visibility::ExtensionProducer;
    store.append_event(running).unwrap();
    let done = envelope(0, "c", "t", "s", Some("s"), kf, pf, 0);
    let done_at = done.recorded_at.clone();
    store.append_event(done).unwrap();

    let db = Connection::open(&path).unwrap();
    let nulls: u32 = db
        .query_row(
            "SELECT COUNT(*) FROM observatory_events WHERE schema_version IS NULL OR created_at='' OR kind='' OR producer_id='' OR visibility=''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(nulls, 0);
    let (kind, vis, producer): (String, String, String) = db
        .query_row(
            "SELECT kind,visibility,producer_id FROM observatory_events WHERE cursor=2",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), vis.as_str(), producer.as_str()),
        ("execution_admitted", "extension_producer", "producer-t")
    );

    // last_cursor follows every event; finished_at only while terminal.
    assert_eq!(
        node(&db, "s"),
        (
            "session-s".into(),
            "turn-s".into(),
            "request-s".into(),
            "producer-s".into(),
            "admitted".into(),
            Some(1),
            Some(1),
            None
        )
    );
    assert_eq!(
        node(&db, "t"),
        (
            "session-s".into(),
            "turn-t".into(),
            "request-t".into(),
            "producer-t".into(),
            "finished".into(),
            Some(2),
            Some(3),
            Some(done_at)
        )
    );
    let (kr, pr) = admitted(ExecutionPhase::Running);
    store
        .append_event(envelope(0, "d", "t", "s", Some("s"), kr, pr, 0))
        .unwrap();
    let t = node(&db, "t");
    assert_eq!((t.4.as_str(), t.6, t.7), ("running", Some(4), None));

    let edge: (String, String, String, String) = db
        .query_row(
            "SELECT parent_execution_id,child_execution_id,root_execution_id,created_at FROM execution_edges",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (edge.0.as_str(), edge.1.as_str(), edge.2.as_str()),
        ("s", "t", "s")
    );
    assert!(!edge.3.is_empty());
    let marks: u32 = db
        .query_row(
            "SELECT COUNT(*) FROM watermarks WHERE created_at <> ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marks, 1);
}

/// Retention prunes across the new constraints: terminal executions' events
/// go, their nodes and edges stay (no FK from nodes to events), the live
/// session's history stays, and the archive records why.
#[test]
fn retention_still_prunes_a_migrated_database() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    build_v0(&path, true);
    // Give the NULL-start session a start so it no longer blocks retention
    // (the restart sweep would otherwise close it first), then close it.
    Connection::open(&path)
        .unwrap()
        .execute_batch("UPDATE execution_nodes SET first_cursor=3 WHERE execution_id='s'")
        .unwrap();
    let store = ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap();
    // `orphan` (cursor 6, fresh, running) keeps itself; everything older
    // than 7 days before it is terminal once `s` finishes.
    let (kf, pf) = finished();
    store
        .append_event(envelope(0, "s-done", "s", "s", None, kf, pf, 30))
        .unwrap();
    let pruned = store.apply_retention().unwrap();
    assert_eq!(pruned, 3, "cursors 3..=5, stopping at the fresh event");
    assert_eq!(store.retention_boundary().unwrap(), Some(Cursor::new(5)));
    assert_eq!(
        store
            .events_after(Cursor::new(0), None)
            .unwrap()
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["e6", "s-done"]
    );
    let db = Connection::open(&path).unwrap();
    let nodes: u32 = db
        .query_row("SELECT COUNT(*) FROM execution_nodes", [], |r| r.get(0))
        .unwrap();
    let edges: u32 = db
        .query_row("SELECT COUNT(*) FROM execution_edges", [], |r| r.get(0))
        .unwrap();
    assert_eq!((nodes, edges), (4, 2), "the projection outlives the log");
    // `t`'s cursors now name pruned events; they are kept, not nulled.
    assert_eq!(node(&db, "t").6, Some(5));
    let reason: String = db
        .query_row(
            "SELECT reason FROM retention_archive ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "age");
    let violations: u32 = db
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn every_manifest_index_and_the_edge_foreign_key_exist() {
    let d = tempdir().unwrap();
    for (name, legacy) in [("fresh.db", false), ("migrated.db", true)] {
        let path = d.path().join(name);
        if legacy {
            build_v0(&path, true);
        }
        drop(ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap());
        let db = Connection::open(&path).unwrap();
        for index in STORE_INDEXES {
            let found = db
                .prepare("SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1")
                .unwrap()
                .exists([index])
                .unwrap();
            assert!(found, "{name}: missing {index}");
        }
        let fk: (String, String, String) = db
            .query_row(
                "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list('execution_edges')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            fk,
            (
                "execution_nodes".into(),
                "child_execution_id".into(),
                "execution_id".into()
            )
        );
        // Enforced, not inert: with enforcement on (as the store runs), an
        // edge to a node that does not exist fails.
        db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let orphan_child = db.execute(
            "INSERT INTO execution_edges(edge_id,parent_execution_id,child_execution_id,root_execution_id,created_at) VALUES('x','p','nobody','r','now')",
            [],
        );
        assert!(orphan_child.is_err(), "{name}: FK not enforced");
    }
}
