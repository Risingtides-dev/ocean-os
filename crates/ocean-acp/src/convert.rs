//! Translation from Ocean daemon [`AgentTurnEvent`]s to ACP [`SessionUpdate`]s.
//!
//! This is a pure, side-effect-free mapping. The caller is responsible for
//! filtering events to the active session and for turn lifecycle (turn_started
//! / turn_finished drive the prompt response, not a SessionUpdate).
//!
//! Design notes:
//! - Ocean's interactive **components** (kanban/table/chart/…) have no ACP
//!   equivalent. Rather than drop them, we render a compact Markdown summary
//!   into the agent message stream so nothing is lost in the editor.
//! - Tool calls map onto ACP's tool-call lifecycle: started → `ToolCall`
//!   (Pending), chunk/finished → `ToolCallUpdate` (InProgress/Completed/Failed).

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use ocean_agent_sdk::{AgentTurnEvent, AgentTurnStatus, ToolCall as OceanToolCall, ToolResult};

/// Wrap a plain string into an ACP text `ContentBlock`.
pub fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(text_block(text))
}

fn tool_content(text: impl Into<String>) -> ToolCallContent {
    // `ToolCallContent: From<T> where T: Into<ContentBlock>`.
    text_block(text).into()
}

/// Best-effort mapping from an Ocean tool name to an ACP [`ToolKind`] so the
/// editor can pick a sensible icon. Unknown tools fall back to `Other`.
fn tool_kind(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    if n.contains("read") || n.contains("cat") || n.contains("view") || n.contains("ls") {
        ToolKind::Read
    } else if n.contains("write") || n.contains("edit") || n.contains("apply") || n.contains("patch") {
        ToolKind::Edit
    } else if n.contains("delete") || n.contains("rm") || n.contains("remove") {
        ToolKind::Delete
    } else if n.contains("move") || n.contains("rename") || n.contains("mv") {
        ToolKind::Move
    } else if n.contains("search") || n.contains("grep") || n.contains("find") || n.contains("glob") {
        ToolKind::Search
    } else if n.contains("fetch") || n.contains("http") || n.contains("web") || n.contains("curl") {
        ToolKind::Fetch
    } else if n.contains("bash") || n.contains("exec") || n.contains("run") || n.contains("shell") || n.contains("cmd") {
        ToolKind::Execute
    } else {
        ToolKind::Other
    }
}

/// Convert one daemon event into zero or one ACP session update.
///
/// Returns `None` for events that don't correspond to a streamed update
/// (turn lifecycle, session bookkeeping, component unmount, extensions).
pub fn event_to_update(event: &AgentTurnEvent) -> Option<SessionUpdate> {
    match event {
        AgentTurnEvent::AssistantTextDelta { delta, .. } => {
            Some(SessionUpdate::AgentMessageChunk(text_chunk(delta.clone())))
        }

        AgentTurnEvent::ThinkingDelta { delta, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(text_chunk(delta.clone())))
        }

        AgentTurnEvent::ToolCallStarted { call, .. } => {
            Some(SessionUpdate::ToolCall(ocean_tool_to_acp(call)))
        }

        AgentTurnEvent::ToolCallChunk { call_id, chunk, .. } => {
            let mut fields = ToolCallUpdateFields::default();
            fields.status = Some(ToolCallStatus::InProgress);
            fields.content = Some(vec![tool_content(chunk.clone())]);
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                acp_tool_id(call_id),
                fields,
            )))
        }

        AgentTurnEvent::ToolCallFinished { call_id, result, .. } => {
            Some(SessionUpdate::ToolCallUpdate(tool_result_update(call_id, result)))
        }

        // Components: summarize as Markdown so the content survives in editors
        // that can't render the live component.
        AgentTurnEvent::ComponentRender { kind, props, .. } => Some(
            SessionUpdate::AgentMessageChunk(text_chunk(render_component_markdown(kind, props))),
        ),

        // No streamed update for these.
        AgentTurnEvent::TurnStarted { .. }
        | AgentTurnEvent::TurnFinished { .. }
        | AgentTurnEvent::SessionCreated { .. }
        | AgentTurnEvent::ComponentUnmount { .. }
        | AgentTurnEvent::BrowserActivity { .. }
        | AgentTurnEvent::Extension { .. } => None,
    }
}

fn ocean_tool_to_acp(call: &OceanToolCall) -> ToolCall {
    let mut tc = ToolCall::new(acp_tool_id_from_uuid(&call.id.0), call.name.clone());
    tc.kind = tool_kind(&call.name);
    tc.status = ToolCallStatus::Pending;
    tc.raw_input = Some(call.args_json.clone());
    tc
}

fn tool_result_update(call_id: &ocean_agent_sdk::ToolCallId, result: &ToolResult) -> ToolCallUpdate {
    let status = if result.ok {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    let content = if result.output.is_empty() {
        None
    } else {
        Some(vec![tool_content(result.output.clone())])
    };
    let mut fields = ToolCallUpdateFields::default();
    fields.status = Some(status);
    fields.content = content;
    fields.raw_output = result.metadata_json.clone();
    ToolCallUpdate::new(acp_tool_id(call_id), fields)
}

fn acp_tool_id(id: &ocean_agent_sdk::ToolCallId) -> ToolCallId {
    acp_tool_id_from_uuid(&id.0)
}

fn acp_tool_id_from_uuid(id: &uuid::Uuid) -> ToolCallId {
    ToolCallId::new(id.to_string())
}

/// Render an Ocean component as a compact Markdown block. We special-case the
/// common, easily-linearized kinds and fall back to a fenced JSON dump so the
/// information is always present even for kinds we don't format explicitly.
fn render_component_markdown(kind: &str, props: &serde_json::Value) -> String {
    match kind {
        "markdown" => props
            .get("content")
            .or_else(|| props.get("markdown"))
            .and_then(|v| v.as_str())
            .map(|s| format!("\n{s}\n"))
            .unwrap_or_else(|| fenced_json(kind, props)),

        "table" => render_table(props).unwrap_or_else(|| fenced_json(kind, props)),

        "callout" => {
            let variant = props.get("variant").and_then(|v| v.as_str()).unwrap_or("info");
            let title = props.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let body = props
                .get("body")
                .or_else(|| props.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let head = if title.is_empty() {
                format!("**[{}]**", variant.to_uppercase())
            } else {
                format!("**[{}] {}**", variant.to_uppercase(), title)
            };
            format!("\n> {head}\n>\n> {}\n", body.replace('\n', "\n> "))
        }

        "code" => {
            let lang = props.get("language").and_then(|v| v.as_str()).unwrap_or("");
            let code = props
                .get("code")
                .or_else(|| props.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("\n```{lang}\n{code}\n```\n")
        }

        // Generic fallback: a labeled fenced JSON block. Nothing is lost.
        _ => fenced_json(kind, props),
    }
}

fn render_table(props: &serde_json::Value) -> Option<String> {
    let columns = props.get("columns")?.as_array()?;
    let rows = props.get("rows")?.as_array()?;

    let headers: Vec<String> = columns
        .iter()
        .map(|c| {
            c.get("label")
                .or_else(|| c.get("key"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| c.as_str().unwrap_or("").to_string())
        })
        .collect();

    let mut out = String::from("\n| ");
    out.push_str(&headers.join(" | "));
    out.push_str(" |\n| ");
    out.push_str(&headers.iter().map(|_| "---").collect::<Vec<_>>().join(" | "));
    out.push_str(" |\n");

    for row in rows {
        let cells: Vec<String> = match row {
            serde_json::Value::Array(arr) => arr.iter().map(cell_to_string).collect(),
            serde_json::Value::Object(_) => headers
                .iter()
                .map(|h| row.get(h).map(cell_to_string).unwrap_or_default())
                .collect(),
            other => vec![cell_to_string(other)],
        };
        out.push_str("| ");
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    Some(out)
}

fn cell_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn fenced_json(kind: &str, props: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(props).unwrap_or_else(|_| props.to_string());
    format!("\n**[component: {kind}]**\n```json\n{pretty}\n```\n")
}

/// Map a finished turn's Ocean status to the ACP stop reason for the prompt
/// response. `EndTurn` is the success case; failures and cancellations map to
/// their ACP equivalents.
pub fn stop_reason_for(status: &AgentTurnStatus) -> agent_client_protocol::schema::StopReason {
    use agent_client_protocol::schema::StopReason;
    match status {
        AgentTurnStatus::Completed => StopReason::EndTurn,
        AgentTurnStatus::Cancelled => StopReason::Cancelled,
        AgentTurnStatus::Failed => StopReason::Refusal,
        // Queued/Running shouldn't appear on a TurnFinished, but if they do,
        // treat as a normal end so the editor isn't left hanging.
        AgentTurnStatus::Queued | AgentTurnStatus::Running => StopReason::EndTurn,
    }
}
