//! Shared fixture tests for the Ocean Observatory module.
//!
//! These tests verify that JSON fixture files deserialize correctly into the
//! expected event structure and that fixture state expectations are reproducible.
//!
//! # Integration (CalmIce — task-2 owner)
//!
//! This test file should be added to `tests/fixture_tests.rs` in the
//! ocean-observatory crate once Cargo.toml and lib.rs are established.
//! The fixture JSON files live in `tests/fixtures/` alongside this test.
//!
//! Requires:
//! - `serde_json` dependency
//! - Test fixtures at `tests/fixtures/*.json`
//! - `src/snapshot.rs` for snapshot type round-trip verification

#[cfg(test)]
mod fixtures {
    use serde::{Deserialize, Serialize};
    use std::fs;
    use std::path::Path;

    /// Top-level structure of a fixture file.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FixtureFile {
        description: String,
        schema_version: u32,
        events: Vec<serde_json::Value>,
        expected_state_at_cursor: serde_json::Map<String, serde_json::Value>,
    }

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn load_fixture(name: &str) -> FixtureFile {
        let path = Path::new(FIXTURE_DIR).join(name);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", name, e));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", name, e))
    }

    // -----------------------------------------------------------------------
    // All fixtures deserialize
    // -----------------------------------------------------------------------

    #[test]
    fn test_daemon_lifecycle_deserializes() {
        let fixture = load_fixture("daemon_lifecycle.json");
        assert_eq!(fixture.schema_version, 1);
        assert!(!fixture.events.is_empty(), "fixture must have events");
        assert!(
            !fixture.expected_state_at_cursor.is_empty(),
            "fixture must have expected_state_at_cursor"
        );
    }

    #[test]
    fn test_execution_lifecycle_deserializes() {
        let fixture = load_fixture("execution_lifecycle.json");
        assert_eq!(fixture.schema_version, 1);
        assert_eq!(fixture.events.len(), 8, "expected 8 events");
    }

    #[test]
    fn test_topology_tree_deserializes() {
        let fixture = load_fixture("topology_tree.json");
        assert_eq!(fixture.schema_version, 1);
        assert_eq!(fixture.events.len(), 6, "expected 6 events");
    }

    #[test]
    fn test_restart_interruption_deserializes() {
        let fixture = load_fixture("restart_interruption.json");
        assert_eq!(fixture.schema_version, 1);
        assert_eq!(fixture.events.len(), 8, "expected 8 events");
    }

    #[test]
    fn test_gap_and_resume_deserializes() {
        let fixture = load_fixture("gap_and_resume.json");
        assert_eq!(fixture.schema_version, 1);
        assert_eq!(fixture.events.len(), 8, "expected 8 events");
    }

    // -----------------------------------------------------------------------
    // All five fixtures load without error
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_fixtures_load() {
        let names = [
            "daemon_lifecycle.json",
            "execution_lifecycle.json",
            "topology_tree.json",
            "restart_interruption.json",
            "gap_and_resume.json",
        ];
        for name in &names {
            let fixture = load_fixture(name);
            assert!(!fixture.description.is_empty(), "fixture {} has no description", name);
            assert_eq!(fixture.schema_version, 1, "fixture {} has wrong schema_version", name);
            assert!(!fixture.events.is_empty(), "fixture {} has no events", name);
        }
    }

    // -----------------------------------------------------------------------
    // Event structure validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_events_have_required_fields() {
        let fixture = load_fixture("daemon_lifecycle.json");
        for (i, event) in fixture.events.iter().enumerate() {
            let obj = event.as_object()
                .unwrap_or_else(|| panic!("event {} is not an object", i));

            assert!(obj.contains_key("cursor"), "event {} missing cursor", i);
            assert!(obj.contains_key("event_id"), "event {} missing event_id", i);
            assert!(obj.contains_key("kind"), "event {} missing kind", i);
            assert!(obj.contains_key("producer"), "event {} missing producer", i);
            assert!(obj.contains_key("topology"), "event {} missing topology", i);
            assert!(obj.contains_key("payload"), "event {} missing payload", i);
            assert!(obj.contains_key("occurred_at"), "event {} missing occurred_at", i);
            assert!(obj.contains_key("recorded_at"), "event {} missing recorded_at", i);
        }
    }

    #[test]
    fn test_payload_has_kind_and_data() {
        let fixture = load_fixture("execution_lifecycle.json");
        for (i, event) in fixture.events.iter().enumerate() {
            let payload = &event["payload"];
            assert!(
                payload.get("kind").is_some(),
                "event {} payload missing kind",
                i
            );
        }
    }

    // -----------------------------------------------------------------------
    // Event cursor ordering
    // -----------------------------------------------------------------------

    #[test]
    fn test_events_are_cursor_ordered() {
        let names = [
            "daemon_lifecycle.json",
            "execution_lifecycle.json",
            "topology_tree.json",
            "restart_interruption.json",
            "gap_and_resume.json",
        ];
        for name in &names {
            let fixture = load_fixture(name);
            let mut prev_cursor: u64 = 0;
            for (i, event) in fixture.events.iter().enumerate() {
                let cursor_str = event["cursor"].as_str()
                    .unwrap_or_else(|| panic!("{} event {} has non-string cursor", name, i));
                let cursor: u64 = cursor_str.parse()
                    .unwrap_or_else(|_| panic!("{} event {} cursor not a u64", name, i));
                assert!(
                    cursor > prev_cursor,
                    "{} event {} cursor {} is not > previous {}",
                    name, i, cursor, prev_cursor
                );
                prev_cursor = cursor;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Fixture-specific event type validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_daemon_lifecycle_has_daemon_events() {
        let fixture = load_fixture("daemon_lifecycle.json");
        let kinds: Vec<&str> = fixture.events.iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();

        assert!(kinds.contains(&"daemon.started"), "missing daemon.started");
        assert!(kinds.contains(&"execution.admitted"), "missing execution.admitted");
        assert!(kinds.contains(&"execution.finished"), "missing execution.finished");
    }

    #[test]
    fn test_topology_tree_has_extension_attested() {
        let fixture = load_fixture("topology_tree.json");
        let attested: Vec<&str> = fixture.events.iter()
            .filter(|e| e["truth"].as_str() == Some("extension_attested"))
            .map(|e| e["event_id"].as_str().unwrap())
            .collect();

        assert_eq!(attested.len(), 4, "expected 4 extension-attested events (2 admissions + phase change + finish)");
    }

    #[test]
    fn test_restart_has_interrupted_semantics() {
        let fixture = load_fixture("restart_interruption.json");
        let kinds: Vec<&str> = fixture.events.iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();

        assert!(kinds.contains(&"daemon.stopping"), "missing daemon.stopping");
        assert!(kinds.contains(&"stream.reset"), "missing stream.reset");
        assert!(kinds.contains(&"daemon.started"), "missing second daemon.started");
    }

    #[test]
    fn test_gap_has_gap_event() {
        let fixture = load_fixture("gap_and_resume.json");
        let kinds: Vec<&str> = fixture.events.iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();

        assert!(kinds.contains(&"stream.gap"), "missing stream.gap");
        let gap_idx = kinds.iter().position(|&k| k == "stream.gap").unwrap();
        let gap_cursor = fixture.events[gap_idx]["cursor"].as_str().unwrap();
        assert_eq!(gap_cursor, "5", "gap should be at cursor 5");
    }

    // -----------------------------------------------------------------------
    // Property: events have no forbidden fields
    // -----------------------------------------------------------------------

    const FORBIDDEN_FIELDS: &[&str] = &[
        "prompt", "prompts", "system_prompt", "user_input", "instructions",
        "thinking", "thinking_text", "reasoning", "scratchpad",
        "tool_args", "tool_arguments", "args", "tool_output", "result", "stdout",
        "error_message", "stack_trace", "traceback",
        "env_vars", "PATH", "HOME", "OPENAI_API_KEY", "API_KEY", "api_key",
        "secret", "password", "token", "Bearer",
        "cwd", "file_path", "absolute_path",
    ];

    #[test]
    fn test_no_forbidden_fields_in_any_fixture() {
        let names = [
            "daemon_lifecycle.json",
            "execution_lifecycle.json",
            "topology_tree.json",
            "restart_interruption.json",
            "gap_and_resume.json",
        ];
        for name in &names {
            let fixture = load_fixture(name);
            let json_str = serde_json::to_string(&fixture.events).unwrap();
            for field in FORBIDDEN_FIELDS {
                assert!(
                    !json_str.contains(field),
                    "fixture {} contains forbidden field '{}'",
                    name,
                    field
                );
            }
        }
    }
}
