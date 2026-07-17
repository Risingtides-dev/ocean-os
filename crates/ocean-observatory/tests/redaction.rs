//! Regression fixtures for the V1 metadata-only allow-list.
//! Runtime AgentEvent values never enter these types; adapters must construct EventPayload
//! field-by-field and may only use fixed codes/safe aliases.
use ocean_observatory::*;
fn json(payload: EventPayload) -> String {
    serde_json::to_string(&payload).unwrap()
}
fn assert_absent(payload: EventPayload, forbidden: &[&str]) {
    let encoded = json(payload);
    for value in forbidden {
        assert!(
            !encoded.contains(value),
            "serialized payload leaked {value}: {encoded}"
        );
    }
}
#[test]
fn prompts_and_reasoning_are_unserializable() {
    assert_absent(
        EventPayload::ExecutionAdmitted {
            phase: ExecutionPhase::Running,
            labels: vec!["batch".into()],
        },
        &[
            "How can I help",
            "system instructions",
            "thinking_text",
            "scratchpad",
            "prompt",
            "user_input",
        ],
    );
}
#[test]
fn tool_args_and_output_are_unserializable() {
    assert_absent(
        EventPayload::ToolStarted {
            tool_name: "search".into(),
            model_alias: "safe-model".into(),
        },
        &[
            "secret API key",
            "Bearer token",
            "tool_arguments",
            "args",
            "parameters",
            "tool_output",
            "stdout",
            "result",
        ],
    );
    assert_absent(
        EventPayload::ToolFinished {
            tool_name: "search".into(),
            duration_millis: 1,
            outcome: ToolOutcome::Success,
            byte_count: 42,
        },
        &["Found results", "tool_output", "stdout", "response"],
    );
}
#[test]
fn errors_paths_and_environment_are_unserializable() {
    assert_absent(
        EventPayload::ExecutionFinished {
            phase: ExecutionPhase::Error,
            duration_millis: 1,
            error_classification: Some("execution_failed".into()),
        },
        &[
            "stack trace",
            "raw exception",
            "/home/",
            "/var/",
            "C:\\",
            "PATH",
            "HOME",
            "OPENAI_API_KEY",
            "cwd",
            "file_path",
        ],
    );
}
#[test]
fn credentials_and_permission_rationale_are_unserializable() {
    assert_absent(
        EventPayload::PermissionResolved {
            reason_code: "user_denied".into(),
            outcome: PermissionOutcome::Denied,
            duration_millis: 2,
        },
        &[
            "password",
            "api_key",
            "secret",
            "token",
            ".ssh",
            "sensitive file",
            "approval rationale",
        ],
    );
}
#[test]
fn stream_gap_keeps_cursors_as_decimal_strings() {
    let encoded = json(EventPayload::StreamGap {
        from_cursor: Cursor::new(4),
        to_cursor: Cursor::new(9),
        reason: "retention_boundary".into(),
    });
    assert!(encoded.contains("\"4\""));
    assert!(encoded.contains("\"9\""));
}
