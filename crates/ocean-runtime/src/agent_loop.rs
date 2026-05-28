//! Agent loop — Rust port of `packages/agent/src/agent-loop.ts`.
//!
//! Streams assistant deltas, executes tool calls, and surfaces permission
//! decisions. Cancellation is honored via `StreamOptions::cancel`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use ocean_protocol::{
    stream_simple, AssistantMessageEvent, Content, Context, Message, StopReason, ToolResultMessage,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::error::{AgentError, Result};
use crate::types::{AgentConfig, AgentEvent, AgentTool, AgentToolResult, PermissionDecision};

pub struct AgentRun {
    pub messages: Vec<Message>,
    pub stopped_at_turn_limit: bool,
}

#[instrument(skip(config, initial_prompt, events), fields(model = %config.model.id))]
pub async fn run_agent(
    config: &AgentConfig,
    initial_prompt: Message,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> Result<AgentRun> {
    run_agent_with_history(config, vec![initial_prompt], events).await
}

/// Continue a run with an existing transcript. Use this for `pi --resume`.
pub async fn run_agent_with_history(
    config: &AgentConfig,
    mut messages: Vec<Message>,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> Result<AgentRun> {
    if let Some(last) = messages.last().cloned() {
        emit(&events, AgentEvent::UserMessage { message: last });
    }
    emit(&events, AgentEvent::AgentStart);

    let tool_index: HashMap<String, Arc<dyn AgentTool>> = config
        .tools
        .iter()
        .map(|t| (t.name().to_string(), t.clone()))
        .collect();
    let tool_defs: Vec<ocean_protocol::Tool> = config
        .tools
        .iter()
        .map(|t| crate::types::tool_def(t.as_ref()))
        .collect();

    let mut session_allowed: HashSet<String> = HashSet::new();
    let mut turn: u32 = 0;
    let mut stopped_at_turn_limit = false;

    'outer: while turn < config.max_turns {
        turn += 1;
        emit(&events, AgentEvent::TurnStart);

        let ctx = Context {
            system_prompt: Some(config.system_prompt.clone()),
            messages: messages.clone(),
            tools: tool_defs.clone(),
        };

        let mut options = config.stream_options.clone();
        if options.reasoning.is_none()
            && config.thinking_level != ocean_protocol::ThinkingLevel::Off
        {
            options.reasoning = Some(config.thinking_level);
        }

        let mut stream = stream_simple(&config.model, &ctx, &options).await?;

        let mut final_message: Option<ocean_protocol::AssistantMessage> = None;
        let mut stop = StopReason::Stop;

        while let Some(ev) = stream.next().await {
            let ev = ev?;
            match ev {
                AssistantMessageEvent::Done { reason, message } => {
                    stop = reason;
                    final_message = Some(message);
                    break;
                }
                AssistantMessageEvent::Error { reason: _, error } => {
                    let err_msg = error
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "provider error".into());
                    return Err(AgentError::Other(err_msg));
                }
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    emit(&events, AgentEvent::TextDelta { delta });
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    emit(&events, AgentEvent::ThinkingDelta { delta });
                }
                _ => {}
            }
        }

        let Some(msg) = final_message else {
            return Err(AgentError::Other(
                "provider stream produced no terminal event".into(),
            ));
        };

        let assistant_message = Message::Assistant(msg.clone());
        messages.push(assistant_message.clone());
        emit(
            &events,
            AgentEvent::AssistantMessage {
                message: assistant_message,
            },
        );

        let tool_calls: Vec<(String, String, Value)> = msg
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() || stop != StopReason::ToolUse {
            emit(&events, AgentEvent::TurnEnd);
            break 'outer;
        }

        let mut any_terminate = !tool_calls.is_empty();
        for (id, name, args) in tool_calls {
            // Permission gate (only for tools that require it, and only once
            // per name per run if the user said "allow session").
            let tool_obj = tool_index.get(&name);
            let needs_perm = tool_obj.map(|t| t.requires_permission()).unwrap_or(false)
                && !session_allowed.contains(&name);
            if needs_perm {
                match config.permission.check(&name, &args).await {
                    PermissionDecision::Allow => {}
                    PermissionDecision::AllowSession => {
                        session_allowed.insert(name.clone());
                    }
                    PermissionDecision::Deny { reason } => {
                        emit(
                            &events,
                            AgentEvent::PermissionDenied {
                                tool_name: name.clone(),
                                reason: reason.clone(),
                            },
                        );
                        let tr = ToolResultMessage {
                            tool_call_id: id,
                            tool_name: name,
                            content: vec![Content::text(format!("permission denied: {reason}"))],
                            is_error: true,
                            timestamp: ocean_protocol::now_ms(),
                        };
                        messages.push(Message::ToolResult(tr));
                        any_terminate = false;
                        continue;
                    }
                }
            }

            emit(
                &events,
                AgentEvent::ToolExecutionStart {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    args: args.clone(),
                },
            );
            let (content, is_error, terminate) = match tool_obj {
                Some(tool) => match tool.execute(&id, args).await {
                    Ok(AgentToolResult {
                        content,
                        details: _,
                        terminate,
                    }) => (content, false, terminate),
                    Err(e) => (vec![Content::text(format!("tool error: {e}"))], true, false),
                },
                None => (
                    vec![Content::text(format!("unknown tool: {name}"))],
                    true,
                    false,
                ),
            };
            if !terminate {
                any_terminate = false;
            }
            emit(
                &events,
                AgentEvent::ToolExecutionEnd {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    is_error,
                    content: content.clone(),
                },
            );
            let tr = ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content,
                is_error,
                timestamp: ocean_protocol::now_ms(),
            };
            messages.push(Message::ToolResult(tr));
        }
        emit(&events, AgentEvent::TurnEnd);
        if any_terminate {
            break;
        }
    }

    if turn >= config.max_turns {
        stopped_at_turn_limit = true;
    }

    emit(
        &events,
        AgentEvent::AgentEnd {
            messages: messages.clone(),
        },
    );
    Ok(AgentRun {
        messages,
        stopped_at_turn_limit,
    })
}

fn emit(sink: &Option<mpsc::UnboundedSender<AgentEvent>>, ev: AgentEvent) {
    if let Some(s) = sink {
        let _ = s.send(ev);
    }
}
