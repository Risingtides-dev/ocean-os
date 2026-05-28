//! Smoke tests for ocean-protocol serialization. No network access.

use ocean_protocol::{Content, Message, StopReason};
use serde_json::json;

#[test]
fn round_trip_user_message() {
    let m = Message::user_text("hello");
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hello");
}

#[test]
fn round_trip_tool_call_content() {
    let c = Content::ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        arguments: json!({"path": "/tmp/x"}),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["type"], "toolCall");
    assert_eq!(v["id"], "call_1");
    assert_eq!(v["arguments"]["path"], "/tmp/x");
}

#[test]
fn stop_reason_serializes_camel_case() {
    assert_eq!(
        serde_json::to_value(StopReason::ToolUse).unwrap(),
        "toolUse"
    );
    assert_eq!(serde_json::to_value(StopReason::Stop).unwrap(), "stop");
}
