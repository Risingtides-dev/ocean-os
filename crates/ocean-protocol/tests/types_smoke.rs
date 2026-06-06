//! Smoke tests for ocean-protocol serialization. No network access.

use ocean_protocol::{Content, Message, StopReason, ToolResultMessage};
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

/// Every `Content` variant must survive a serialize → deserialize round-trip
/// with its tag and payload intact. These blocks are what `AgentTurnEvent`s
/// carry over the SSE wire (tool results, assistant messages), so a regression
/// in the `Content` tagging would silently corrupt every downstream event.
/// `Image` is included because tool-result image capping (OCEAN-113) depends on
/// the `data`/`mime_type` shape staying stable.
#[test]
fn every_content_variant_round_trips() {
    let variants = vec![
        Content::text("plain text"),
        Content::Thinking {
            thinking: "reasoning…".into(),
            thinking_signature: Some("sig".into()),
        },
        Content::Image {
            data: "QUJD".into(), // base64 "ABC"
            mime_type: "image/png".into(),
        },
        Content::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({"path": "/tmp/x"}),
        },
    ];

    for c in variants {
        let json = serde_json::to_string(&c).expect("serialize");
        let back: Content = serde_json::from_str(&json).expect("deserialize round-trips");
        assert_eq!(back, c, "content round-trip mismatch for {json}");
    }
}

/// A `ToolResult` message carrying an image block round-trips intact — the
/// transcript shape the runtime's tool-result cap operates on.
#[test]
fn tool_result_with_image_round_trips() {
    let msg = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_1".into(),
        tool_name: "browser_screenshot".into(),
        content: vec![Content::Image {
            data: "QUJD".into(),
            mime_type: "image/jpeg".into(),
        }],
        is_error: false,
        timestamp: 1_700_000_000_000,
    });
    let json = serde_json::to_string(&msg).expect("serialize");
    let back: Message = serde_json::from_str(&json).expect("deserialize");
    match back {
        Message::ToolResult(tr) => {
            assert_eq!(tr.tool_call_id, "call_1");
            assert_eq!(tr.tool_name, "browser_screenshot");
            assert_eq!(tr.content.len(), 1);
            match &tr.content[0] {
                Content::Image { data, mime_type } => {
                    assert_eq!(data, "QUJD");
                    assert_eq!(mime_type, "image/jpeg");
                }
                other => panic!("expected image content, got {other:?}"),
            }
        }
        other => panic!("expected tool-result message, got {other:?}"),
    }
}
