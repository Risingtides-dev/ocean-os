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
