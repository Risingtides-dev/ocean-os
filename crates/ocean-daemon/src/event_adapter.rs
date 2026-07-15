use ocean_agent_sdk::{AgentTurnEvent, AgentTurnStatus};
use ocean_core::OceanEvent;

pub(super) fn agent_to_ocean_event(event: AgentTurnEvent) -> Option<OceanEvent> {
    match event {
        AgentTurnEvent::TurnStarted { .. } => None,
        AgentTurnEvent::AssistantTextDelta {
            turn_id: _, delta, ..
        } => Some(OceanEvent::AssistantDelta { text: delta }),
        AgentTurnEvent::ModelRerouted { .. } => None,
        AgentTurnEvent::ThinkingDelta { .. } => None,
        AgentTurnEvent::ToolCallStarted {
            turn_id: _, call, ..
        } => Some(OceanEvent::ToolStarted {
            tool: call.name,
            args: call.args_json,
        }),
        AgentTurnEvent::TurnFinished {
            status, wall_ms, ..
        } => Some(OceanEvent::TurnFinished {
            ok: matches!(status, AgentTurnStatus::Completed),
            wall_ms: wall_ms.unwrap_or(0),
        }),
        AgentTurnEvent::ToolCallChunk {
            turn_id: _,
            call_id: _,
            chunk,
            ..
        } => Some(OceanEvent::ToolOutput {
            tool: "tool".into(),
            text: chunk,
            is_error: false,
        }),
        AgentTurnEvent::ToolCallFinished {
            turn_id: _,
            call_id: _,
            result,
            ..
        } => Some(OceanEvent::ToolEnded {
            tool: "tool".into(),
            is_error: !result.ok,
        }),
        AgentTurnEvent::SessionCreated {
            session_id: _,
            title: _,
            cwd: _,
        } => Some(OceanEvent::SessionCreated),
        AgentTurnEvent::Extension { .. } => None,
        AgentTurnEvent::ComponentRender { .. } => None,
        AgentTurnEvent::ComponentUnmount { .. } => None,
        AgentTurnEvent::BrowserActivity { .. } => None,
        AgentTurnEvent::SurfacePatch { .. } => None,
        AgentTurnEvent::SlackCanvas { .. } => None,
    }
}

pub(super) fn agent_event_type_name(event: &AgentTurnEvent) -> &'static str {
    match event {
        AgentTurnEvent::TurnStarted { .. } => "turn_started",
        AgentTurnEvent::ModelRerouted { .. } => "model_rerouted",
        AgentTurnEvent::AssistantTextDelta { .. } => "assistant_text_delta",
        AgentTurnEvent::ThinkingDelta { .. } => "thinking_delta",
        AgentTurnEvent::ToolCallStarted { .. } => "tool_call_started",
        AgentTurnEvent::ToolCallChunk { .. } => "tool_call_chunk",
        AgentTurnEvent::ToolCallFinished { .. } => "tool_call_finished",
        AgentTurnEvent::TurnFinished { .. } => "turn_finished",
        AgentTurnEvent::SessionCreated { .. } => "session_created",
        AgentTurnEvent::Extension { .. } => "extension",
        AgentTurnEvent::ComponentRender { .. } => "component_render",
        AgentTurnEvent::ComponentUnmount { .. } => "component_unmount",
        AgentTurnEvent::BrowserActivity { .. } => "browser_activity",
        AgentTurnEvent::SurfacePatch { .. } => "surface_patch",
        AgentTurnEvent::SlackCanvas { .. } => "slack_canvas",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{
        slack_canvas::{SlackCanvasOp, SlackCanvasResult, SlackChannelId},
        surface::CanvasId,
        AgentSessionId, AgentTurnId, ToolCall, ToolCallId, ToolResult,
    };
    use serde_json::json;

    fn event_name_fixtures() -> Vec<(AgentTurnEvent, &'static str)> {
        let session_id = AgentSessionId::new_v4();
        let turn_id = AgentTurnId::new_v4();
        vec![
            (
                AgentTurnEvent::TurnStarted {
                    turn_id,
                    session_id,
                    model: Some("model-a".into()),
                },
                "turn_started",
            ),
            (
                AgentTurnEvent::ModelRerouted {
                    session_id,
                    turn_id,
                    requested: "model-a".into(),
                    effective: "model-b".into(),
                    reason: "degraded".into(),
                },
                "model_rerouted",
            ),
            (
                AgentTurnEvent::AssistantTextDelta {
                    session_id,
                    turn_id,
                    delta: "hello".into(),
                },
                "assistant_text_delta",
            ),
            (
                AgentTurnEvent::ThinkingDelta {
                    session_id,
                    turn_id,
                    delta: "hmm".into(),
                },
                "thinking_delta",
            ),
            (
                AgentTurnEvent::ToolCallStarted {
                    session_id,
                    turn_id,
                    call: ToolCall {
                        id: ToolCallId::new_v4(),
                        name: "read".into(),
                        args_json: json!({"path": "README.md"}),
                    },
                },
                "tool_call_started",
            ),
            (
                AgentTurnEvent::ToolCallChunk {
                    session_id,
                    turn_id,
                    call_id: ToolCallId::new_v4(),
                    chunk: "partial".into(),
                },
                "tool_call_chunk",
            ),
            (
                AgentTurnEvent::ToolCallFinished {
                    session_id,
                    turn_id,
                    call_id: ToolCallId::new_v4(),
                    result: ToolResult {
                        ok: true,
                        output: "done".into(),
                        metadata_json: None,
                    },
                },
                "tool_call_finished",
            ),
            (
                AgentTurnEvent::TurnFinished {
                    session_id,
                    turn_id,
                    status: AgentTurnStatus::Completed,
                    error: None,
                    wall_ms: Some(42),
                    output_tokens: Some(3),
                    input_tokens: Some(2),
                    cache_read_tokens: Some(1),
                    tokens_per_second: Some(1.5),
                    context_usage: None,
                },
                "turn_finished",
            ),
            (
                AgentTurnEvent::SessionCreated {
                    session_id,
                    title: "Session".into(),
                    cwd: "/tmp".into(),
                },
                "session_created",
            ),
            (
                AgentTurnEvent::Extension {
                    extension: "test".into(),
                    payload: json!({"ok": true}),
                    scope: Some(session_id),
                },
                "extension",
            ),
            (
                AgentTurnEvent::ComponentRender {
                    session_id,
                    component_id: "component-1".into(),
                    kind: "markdown".into(),
                    props: json!({"text": "hello"}),
                    replace: false,
                },
                "component_render",
            ),
            (
                AgentTurnEvent::ComponentUnmount {
                    session_id,
                    component_id: "component-1".into(),
                },
                "component_unmount",
            ),
            (
                AgentTurnEvent::BrowserActivity {
                    session_id,
                    active: true,
                },
                "browser_activity",
            ),
            (
                AgentTurnEvent::SurfacePatch {
                    session_id,
                    turn_id,
                    canvas_id: CanvasId::new("canvas-1"),
                    patches: vec![],
                },
                "surface_patch",
            ),
            (
                AgentTurnEvent::SlackCanvas {
                    session_id,
                    turn_id,
                    op: SlackCanvasOp::List {
                        channel_id: SlackChannelId::new("channel-1"),
                    },
                    result: SlackCanvasResult::pending_list(),
                },
                "slack_canvas",
            ),
        ]
    }

    #[test]
    fn legacy_mirror_preserves_supported_payloads() {
        let session_id = AgentSessionId::new_v4();
        let turn_id = AgentTurnId::new_v4();

        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::AssistantTextDelta {
                session_id,
                turn_id,
                delta: "hello".into(),
            }),
            Some(OceanEvent::AssistantDelta {
                text: "hello".into(),
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::ToolCallStarted {
                session_id,
                turn_id,
                call: ToolCall {
                    id: ToolCallId::new_v4(),
                    name: "read".into(),
                    args_json: json!({"path": "README.md"}),
                },
            }),
            Some(OceanEvent::ToolStarted {
                tool: "read".into(),
                args: json!({"path": "README.md"}),
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::ToolCallChunk {
                session_id,
                turn_id,
                call_id: ToolCallId::new_v4(),
                chunk: "partial".into(),
            }),
            Some(OceanEvent::ToolOutput {
                tool: "tool".into(),
                text: "partial".into(),
                is_error: false,
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::ToolCallFinished {
                session_id,
                turn_id,
                call_id: ToolCallId::new_v4(),
                result: ToolResult {
                    ok: false,
                    output: "failed".into(),
                    metadata_json: Some(json!({"exit_code": 1})),
                },
            }),
            Some(OceanEvent::ToolEnded {
                tool: "tool".into(),
                is_error: true,
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: Some(42),
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                context_usage: None,
            }),
            Some(OceanEvent::TurnFinished {
                ok: true,
                wall_ms: 42,
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status: AgentTurnStatus::Failed,
                error: Some("failed".into()),
                wall_ms: None,
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                context_usage: None,
            }),
            Some(OceanEvent::TurnFinished {
                ok: false,
                wall_ms: 0,
            })
        );
        assert_eq!(
            agent_to_ocean_event(AgentTurnEvent::SessionCreated {
                session_id,
                title: "Session".into(),
                cwd: "/tmp".into(),
            }),
            Some(OceanEvent::SessionCreated)
        );
    }

    #[test]
    fn legacy_mirror_filters_agent_only_events() {
        let mirrored = [
            "assistant_text_delta",
            "tool_call_started",
            "tool_call_chunk",
            "tool_call_finished",
            "turn_finished",
            "session_created",
        ];

        for (event, name) in event_name_fixtures() {
            if !mirrored.contains(&name) {
                assert_eq!(
                    agent_to_ocean_event(event),
                    None,
                    "{name} must remain agent-rail-only"
                );
            }
        }
    }

    #[test]
    fn agent_event_type_names_match_sdk_wire_tags() {
        for (event, expected) in event_name_fixtures() {
            let value = serde_json::to_value(&event).expect("event must serialize");
            let wire_tag = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .expect("event must carry its SDK wire tag");
            assert_eq!(wire_tag, expected);
            assert_eq!(agent_event_type_name(&event), expected);
        }
    }
}
