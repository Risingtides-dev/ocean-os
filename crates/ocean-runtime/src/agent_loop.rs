//! Agent loop.
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
use crate::types::{AgentConfig, AgentEvent, AgentTool, AgentToolResult, PermissionDecision, ToolSideEffect};

pub struct AgentRun {
    pub messages: Vec<Message>,
    pub stopped_at_turn_limit: bool,
    /// Real provider token usage, summed across every round of the turn.
    /// `Usage::default()` (all zero) when the provider reported none.
    pub usage: ocean_protocol::Usage,
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
    let mut total_usage = ocean_protocol::Usage::default();

    'outer: while turn < config.max_turns {
        turn += 1;
        emit(&events, AgentEvent::TurnStart);

        let ctx = Context {
            system_prompt: Some(config.system_prompt.clone()),
            messages: trim_to_context_window(
                &messages,
                &config.system_prompt,
                config.model.context_window,
                config.model.max_tokens,
            ),
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
                    // Accumulate this round's real provider usage.
                    total_usage.input += message.usage.input;
                    total_usage.output += message.usage.output;
                    total_usage.cache_read += message.usage.cache_read;
                    total_usage.cache_write += message.usage.cache_write;
                    total_usage.total_tokens += message.usage.total_tokens;
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
            let (content, is_error, terminate, side_effects) = match tool_obj {
                Some(tool) => match tool.execute(&id, args).await {
                    Ok(AgentToolResult {
                        content,
                        details: _,
                        terminate,
                        side_effects,
                    }) => (content, false, terminate, side_effects),
                    Err(e) => (
                        vec![Content::text(format!("tool error: {e}"))],
                        true,
                        false,
                        Vec::new(),
                    ),
                },
                None => (
                    vec![Content::text(format!("unknown tool: {name}"))],
                    true,
                    false,
                    Vec::new(),
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
            // Emit any side-effect events the tool requested (render, unmount, etc.)
            for effect in &side_effects {
                match effect {
                    ToolSideEffect::Render {
                        id,
                        kind,
                        props,
                        replace,
                    } => {
                        emit(
                            &events,
                            AgentEvent::Render {
                                id: id.clone(),
                                kind: kind.clone(),
                                props: props.clone(),
                                replace: *replace,
                            },
                        );
                    }
                    ToolSideEffect::Unmount { id } => {
                        emit(&events, AgentEvent::Unmount { id: id.clone() });
                    }
                }
            }
            // The live SSE display (ToolExecutionEnd above) got the FULL output.
            // What we push into the transcript is capped, because this result
            // is resent on every subsequent round of this turn AND reloaded on
            // every future turn of the session — an uncapped `read`/`bash`/`ls`
            // dump would otherwise be paid for in input tokens indefinitely.
            let tr = ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content: cap_tool_content(content),
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
        usage: total_usage,
    })
}

/// Max bytes of a single tool-result text block we retain in the transcript.
/// ~32 KB ≈ 8k tokens — enough for the model to act on a tool result, bounded
/// so a giant `read`/`bash`/`ls` dump can't be resent indefinitely. The full
/// output still reaches the UI via the `ToolExecutionEnd` event.
const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

/// Cap each text block of a tool result. Oversized text is truncated on a char
/// boundary with a marker noting how much was elided; non-text content (images,
/// etc.) passes through untouched.
fn cap_tool_content(content: Vec<Content>) -> Vec<Content> {
    content
        .into_iter()
        .map(|c| match c {
            Content::Text { text } if text.len() > MAX_TOOL_RESULT_BYTES => {
                // Truncate at the last char boundary at or before the cap.
                let mut end = MAX_TOOL_RESULT_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                let elided = text.len() - end;
                let mut truncated = text[..end].to_string();
                truncated.push_str(&format!(
                    "\n\n[… {elided} bytes truncated to fit context; full output shown in UI …]"
                ));
                Content::Text { text: truncated }
            }
            other => other,
        })
        .collect()
}

/// Rough token estimate for a payload: serialized JSON length / 4. This is the
/// same shape that goes on the wire, so it captures content, tool args, and
/// tool results without matching every `Content` variant. Deliberately
/// conservative-ish; we keep a generous safety margin below the real window.
fn estimate_tokens_json<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0) / 4
}

/// Trim the message history to fit the model's context window before sending.
///
/// Without this, every agent round resends the entire (growing) transcript and
/// every new turn reloads the full prior session — so input tokens balloon
/// quadratically with no ceiling, regardless of the model's actual window. We
/// keep the newest contiguous suffix of messages that fits the input budget
/// (`context_window − max_tokens − system-prompt − margin`), always preserving
/// at least the final message (the current user prompt).
///
/// Tool-call/tool-result pairing: a `ToolResult` is only valid to a provider if
/// the assistant `ToolCall` it answers is also present. Since we keep a suffix,
/// the only risk is the *oldest kept* message being an orphan `ToolResult` whose
/// originating assistant turn was trimmed away — so we drop any such leading
/// orphan(s). (Pairing within the kept suffix is intact by construction.)
fn trim_to_context_window(
    messages: &[Message],
    system_prompt: &str,
    context_window: u32,
    max_tokens: u32,
) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Reserve room for the system prompt and the model's max output, plus a
    // 5% margin to cover tool-schema overhead and estimator slop.
    let window = context_window as usize;
    let reserve_output = max_tokens as usize;
    let system = estimate_tokens_json(&system_prompt);
    let margin = window / 20;
    let budget = window
        .saturating_sub(reserve_output)
        .saturating_sub(system)
        .saturating_sub(margin);

    // Walk newest → oldest, accumulating until we'd exceed the budget. Always
    // keep the last message even if it alone exceeds the budget (the provider
    // can reject an over-long single message, but dropping the live prompt is
    // never correct).
    let mut used = 0usize;
    let mut keep_from = messages.len();
    for (idx, msg) in messages.iter().enumerate().rev() {
        let cost = estimate_tokens_json(msg);
        let is_last = idx == messages.len() - 1;
        if !is_last && used + cost > budget {
            break;
        }
        used += cost;
        keep_from = idx;
    }

    // Drop leading orphan tool-results whose originating ToolCall was trimmed.
    while keep_from < messages.len() && matches!(messages[keep_from], Message::ToolResult(_)) {
        keep_from += 1;
    }
    // Never return empty: if trimming orphans ate everything, fall back to the
    // last message alone.
    if keep_from >= messages.len() {
        keep_from = messages.len() - 1;
    }

    let kept = messages.len() - keep_from;
    if kept < messages.len() {
        tracing::debug!(
            total = messages.len(),
            kept,
            dropped = messages.len() - kept,
            budget,
            "trimmed message history to fit context window"
        );
    }
    messages[keep_from..].to_vec()
}

fn emit(sink: &Option<mpsc::UnboundedSender<AgentEvent>>, ev: AgentEvent) {
    if let Some(s) = sink {
        let _ = s.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_protocol::ToolResultMessage;

    fn user(s: &str) -> Message {
        Message::user_text(s)
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "bash".into(),
            content: vec![Content::text(text)],
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn trim_keeps_everything_when_it_fits() {
        let msgs = vec![user("a"), user("b"), user("c")];
        // Huge window: nothing trimmed.
        let kept = trim_to_context_window(&msgs, "sys", 1_000_000, 8_192);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn trim_keeps_newest_suffix_and_always_the_last_message() {
        // Many big messages, tiny window → only the newest few survive, and the
        // final message is always retained even though the window is small.
        let big = "x".repeat(4_000); // ~1k tokens each
        let msgs: Vec<Message> = (0..50).map(|i| user(&format!("{i}:{big}"))).collect();
        let last_text = format!("49:{big}");
        let kept = trim_to_context_window(&msgs, "sys", 8_000, 2_000);
        assert!(kept.len() < msgs.len(), "should have trimmed");
        assert!(!kept.is_empty(), "must never be empty");
        // The most recent message is preserved.
        assert!(matches!(kept.last(), Some(Message::User { content, .. })
            if content.iter().any(|c| c.as_text() == Some(last_text.as_str()))));
    }

    #[test]
    fn trim_drops_leading_orphan_tool_result() {
        // If the suffix would start with a tool-result, it's an orphan (its
        // ToolCall got trimmed) and must be dropped.
        let big = "y".repeat(8_000);
        let msgs = vec![
            user(&format!("old:{big}")),
            tool_result("call-1", &format!("orphan:{big}")),
            user("live prompt"),
        ];
        // Budget admits roughly the last two; the orphan tool-result at the
        // front of the kept window must be removed.
        let kept = trim_to_context_window(&msgs, "sys", 6_000, 1_000);
        assert!(
            !matches!(kept.first(), Some(Message::ToolResult(_))),
            "kept history must not begin with an orphan tool result"
        );
        assert!(matches!(kept.last(), Some(Message::User { .. })));
    }

    #[test]
    fn cap_tool_content_truncates_oversized_text() {
        let huge = "z".repeat(MAX_TOOL_RESULT_BYTES * 2);
        let capped = cap_tool_content(vec![Content::text(huge)]);
        let Content::Text { text } = &capped[0] else {
            panic!("expected text");
        };
        assert!(text.len() < MAX_TOOL_RESULT_BYTES + 256);
        assert!(text.contains("truncated to fit context"));
    }

    #[test]
    fn cap_tool_content_leaves_small_text_untouched() {
        let small = "ok".to_string();
        let capped = cap_tool_content(vec![Content::text(small.clone())]);
        assert_eq!(capped[0].as_text(), Some(small.as_str()));
    }
}
