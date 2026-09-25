use ocean_observatory::*;
use tempfile::tempdir;
fn event() -> EventEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        cursor: Cursor::new(0),
        event_id: "event".into(),
        observatory_id: "obs".into(),
        daemon_instance_id: "daemon".into(),
        occurred_at: "2026-07-17T00:00:00.000Z".into(),
        recorded_at: "2026-07-17T00:00:00.000Z".into(),
        kind: EventKind::ExecutionAdmitted,
        truth: TruthProvenance::HostObserved,
        producer: Producer {
            kind: ProducerKind::Daemon,
            id: "ocean-daemon".into(),
        },
        topology: Topology {
            execution_id: "execution".into(),
            root_execution_id: "execution".into(),
            parent_execution_id: None,
            edge_id: None,
            session_id: "session".into(),
            turn_id: "turn".into(),
            request_id: "request".into(),
        },
        correlation: Correlation {
            tool_call_id: None,
            permission_id: None,
        },
        visibility: Visibility::Metadata,
        payload: EventPayload::ExecutionAdmitted {
            phase: ExecutionPhase::Admitted,
            labels: vec!["safe".into()],
        },
    }
}
#[test]
fn cursor_is_wire_string() {
    assert_eq!(serde_json::to_string(&Cursor::new(42)).unwrap(), "\"42\"");
}
#[test]
fn append_is_durable_and_monotonic() {
    let d = tempdir().unwrap();
    let s = ObservatoryStore::open(&d.path().join("obs.db"), RetentionPolicy::default()).unwrap();
    assert_eq!(s.append_event(event()).unwrap(), Cursor::new(1));
    let mut next = event();
    next.event_id = "event-2".into();
    assert_eq!(s.append_event(next).unwrap(), Cursor::new(2));
    assert_eq!(s.events_after(Cursor::new(0), None).unwrap().len(), 2);
}

/// An event for `execution`, recorded `days_ago` days in the past, finished
/// (terminal) or admitted (live).
fn event_for(id: &str, execution: &str, days_ago: i64, finished: bool) -> EventEnvelope {
    let mut e = event();
    e.event_id = id.into();
    e.topology.execution_id = execution.into();
    e.topology.root_execution_id = execution.into();
    e.recorded_at = (chrono::Utc::now() - chrono::Duration::days(days_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if finished {
        e.kind = EventKind::ExecutionFinished;
        e.payload = EventPayload::ExecutionFinished {
            phase: ExecutionPhase::Finished,
            duration_millis: 1,
            error_classification: None,
        };
    }
    e
}

/// G3: retention actually prunes, and never crosses a live execution's first
/// event — one stuck `running` row no longer blocks pruning of everything
/// older than it, and never loses its own history.
#[test]
fn retention_prunes_old_events_but_keeps_a_live_executions_history() {
    let d = tempdir().unwrap();
    let s = ObservatoryStore::open(&d.path().join("obs.db"), RetentionPolicy::default()).unwrap();
    s.append_event(event_for("old-1", "done", 30, false))
        .unwrap(); // 1
    s.append_event(event_for("old-2", "done", 30, true))
        .unwrap(); // 2 (done is terminal)
    s.append_event(event_for("live-1", "live", 30, false))
        .unwrap(); // 3 (live starts)
    s.append_event(event_for("old-3", "other", 30, true))
        .unwrap(); // 4
    s.append_event(event_for("fresh", "other2", 0, true))
        .unwrap(); // 5
    let pruned = s.apply_retention().unwrap();
    assert_eq!(
        pruned, 2,
        "only events before the live execution's first cursor"
    );
    assert_eq!(s.retention_boundary().unwrap(), Some(Cursor::new(2)));
    let left: Vec<String> = s
        .events_after(Cursor::new(0), None)
        .unwrap()
        .into_iter()
        .map(|e| e.event_id)
        .collect();
    assert_eq!(left, ["live-1", "old-3", "fresh"]);
}

/// G4: a full prune followed by a restart never reissues a pruned cursor.
#[test]
fn reopen_after_full_prune_continues_the_cursor() {
    let d = tempdir().unwrap();
    let path = d.path().join("obs.db");
    {
        let s = ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap();
        for i in 0..3 {
            s.append_event(event_for(&format!("e{i}"), "done", 30, true))
                .unwrap();
        }
        assert_eq!(s.apply_retention().unwrap(), 3);
        assert!(s.events_after(Cursor::new(0), None).unwrap().is_empty());
    }
    let s = ObservatoryStore::open(&path, RetentionPolicy::default()).unwrap();
    assert_eq!(s.latest_cursor(), Cursor::new(3));
    assert_eq!(
        s.append_event(event_for("after", "next", 0, true)).unwrap(),
        Cursor::new(4)
    );
}

/// G3: the size bound measures the real database, so a tiny ceiling prunes
/// old-enough-to-keep terminal history too, oldest first.
#[test]
fn retention_enforces_the_size_bound_from_real_db_size() {
    let d = tempdir().unwrap();
    let s = ObservatoryStore::open(
        &d.path().join("obs.db"),
        RetentionPolicy {
            max_age_days: 365,
            max_bytes: 1,
        },
    )
    .unwrap();
    for i in 0..5 {
        s.append_event(event_for(&format!("e{i}"), &format!("x{i}"), 0, true))
            .unwrap();
    }
    assert!(s.apply_retention().unwrap() > 0, "an over-size db prunes");
}

/// G1: a snapshot is labelled with the watermark its rows reflect, an earlier
/// `at` is refused rather than mislabelled, and snapshot + tail from its
/// watermark replays exactly the events after it — never one it already saw.
#[test]
fn snapshot_is_point_in_time_and_tail_from_its_watermark_is_disjoint() {
    let d = tempdir().unwrap();
    let s = ObservatoryStore::open(&d.path().join("obs.db"), RetentionPolicy::default()).unwrap();
    s.append_event(event_for("a1", "a", 0, false)).unwrap();
    s.append_event(event_for("a2", "a", 0, true)).unwrap();
    let snap = s.snapshot_at(None).unwrap();
    assert_eq!(snap.watermark_cursor, Cursor::new(2));
    assert_eq!(snap.nodes.len(), 1);
    assert_eq!(snap.nodes[0].phase, "finished");

    s.append_event(event_for("b1", "b", 0, false)).unwrap();
    let tail: Vec<String> = s
        .events_after(snap.watermark_cursor, None)
        .unwrap()
        .into_iter()
        .map(|e| e.event_id)
        .collect();
    assert_eq!(
        tail,
        ["b1"],
        "the tail holds only what the snapshot did not"
    );

    assert!(matches!(
        s.snapshot_at(Some(Cursor::new(2))),
        Err(StoreError::HistoricalSnapshot {
            requested: 2,
            latest: 3
        })
    ));
    assert_eq!(
        s.snapshot_at(Some(Cursor::new(3)))
            .unwrap()
            .watermark_cursor,
        Cursor::new(3)
    );
}
