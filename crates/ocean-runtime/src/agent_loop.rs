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
use crate::types::{
    AgentConfig, AgentEvent, AgentTool, AgentToolResult, PermissionDecision, ToolSideEffect,
};

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
    // Session this run belongs to (if any). Stamped onto every event we emit so
    // the daemon/SDK can route by session natively instead of re-attaching it.
    let sid = config.session_id.clone();

    if let Some(last) = messages.last().cloned() {
        emit(
            &events,
            AgentEvent::UserMessage {
                session_id: sid.clone(),
                message: last,
            },
        );
    }
    emit(
        &events,
        AgentEvent::AgentStart {
            session_id: sid.clone(),
        },
    );

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
        // Honor a cancellation request before starting another round. The daemon
        // signals `StreamOptions::cancel` from POST /v1/requests/{id}/cancel; the
        // provider stream also observes it mid-flight, but checking here stops the
        // loop cleanly between rounds (e.g. cancel arriving during tool execution)
        // and unwinds with `AgentError::Cancelled` rather than running one more turn.
        if is_cancelled(config) {
            return Err(AgentError::Cancelled);
        }

        turn += 1;
        emit(
            &events,
            AgentEvent::TurnStart {
                session_id: sid.clone(),
            },
        );

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

        let mut final_message: Option<ocean_protocol::AssistantMessage> = None;
        let mut stop = StopReason::Stop;

        // Bound the entire turn — provider request + full stream consumption —
        // by a single wall-clock deadline. A hung or slow provider that never
        // finishes streaming would otherwise block this turn forever; on elapse
        // we abort with `AgentError::Timeout` and unwind the run. This is a
        // *total* per-turn deadline (not an idle/inter-chunk timeout): the whole
        // round must complete within the window, matching the intent of bounding
        // a turn that a provider has hung on.
        let timeout_secs = config.turn_timeout_secs();
        let stream_work = async {
            // Stream from the injected provider when one is set (the e2e-test
            // seam), otherwise dispatch to the real provider via `stream_simple`
            // (`model.api` → Anthropic/OpenAI/…). Production never sets the
            // override, so this is an unconditional `stream_simple` in practice.
            let mut stream = match &config.provider {
                Some(provider) => provider.stream(&config.model, &ctx, &options).await?,
                None => stream_simple(&config.model, &ctx, &options).await?,
            };
            while let Some(ev) = stream.next().await {
                // A cancel can land mid-stream (client hit halt while the
                // assistant was still typing). The provider also yields
                // `Error::Cancelled` on its own, but checking here breaks out
                // immediately and surfaces a clean `AgentError::Cancelled`
                // rather than waiting on the next chunk.
                if is_cancelled(config) {
                    return Err(AgentError::Cancelled);
                }
                let ev = match ev {
                    Ok(ev) => ev,
                    // The provider maps a cancel-token trip to `Error::Cancelled`;
                    // normalize it to the agent-level `Cancelled` so callers see a
                    // single cancellation type regardless of where it tripped.
                    Err(ocean_protocol::Error::Cancelled) => return Err(AgentError::Cancelled),
                    Err(e) => return Err(AgentError::from(e)),
                };
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
                        emit(
                            &events,
                            AgentEvent::TextDelta {
                                session_id: sid.clone(),
                                delta,
                            },
                        );
                    }
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        emit(
                            &events,
                            AgentEvent::ThinkingDelta {
                                session_id: sid.clone(),
                                delta,
                            },
                        );
                    }
                    _ => {}
                }
            }
            Ok::<(), AgentError>(())
        };

        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs as u64),
            stream_work,
        )
        .await
        {
            Ok(result) => result?,
            Err(_elapsed) => {
                return Err(AgentError::Timeout { secs: timeout_secs });
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
                session_id: sid.clone(),
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
            // The turn is ending without executing tools. There is one hazard
            // here: the model may have *emitted* a complete tool_use block and
            // still stopped for a non-ToolUse reason — most commonly
            // `StopReason::Length`, where output was truncated at the token
            // limit after a tool_call was already assembled (the provider
            // reports `max_tokens`, not `tool_use`, as the stop reason). If we
            // simply break, that assistant tool_use is pushed (above) and
            // persisted with NO matching tool_result. On the *next* turn of the
            // session the transcript replays `Assistant(tool_use) … User(prompt)`
            // with an unanswered call — which Anthropic/OpenAI both 400 on
            // ("tool_use ids found without tool_result"), silently breaking the
            // whole session for every client. The trim path only drops orphan
            // tool_*results*, never an orphan tool_*call*, so nothing downstream
            // rescues this. We do NOT execute the call (its args may be truncated
            // and the model did not intend ToolUse); instead we pair each orphan
            // tool_use with a synthetic error tool_result so the transcript stays
            // provider-valid and the truncation is surfaced honestly to the model
            // on resume. See OCEAN-103.
            for (id, name, _args) in &tool_calls {
                messages.push(truncated_tool_use_result(id, name));
            }
            emit(
                &events,
                AgentEvent::TurnEnd {
                    session_id: sid.clone(),
                },
            );
            break 'outer;
        }

        let mut any_terminate = !tool_calls.is_empty();
        for (id, name, args) in tool_calls {
            // Permission gate (only for tools that require it, and only once
            // per name per run if the user said "allow session").
            //
            // ToolExecutionStart MUST be emitted *after* this gate (OCEAN-60):
            // a denied tool takes the `Deny` arm's `continue` below and never
            // reaches the Start emit, so it never produces a Start-without-End
            // orphan on the event stream. Only tools that are actually about to
            // run emit Start, and every Start is paired with a ToolExecutionEnd.
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
                                session_id: sid.clone(),
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
                    session_id: sid.clone(),
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    args: args.clone(),
                },
            );
            let (content, is_error, terminate, side_effects) = match tool_obj {
                // Race tool execution against cancellation (OCEAN-116). A
                // long-running tool (slow bash, a network call that hangs) would
                // otherwise block the loop until it completed even if the user
                // cancelled the turn — the mid-stream check (OCEAN-57) only fires
                // between provider chunks, never while a tool is in flight. We
                // select between the tool future and the same `StreamOptions::cancel`
                // token the rest of the loop reads: if cancellation wins we drop
                // the tool future (which stops polling it — no task leak) and
                // unwind the run with `AgentError::Cancelled`.
                Some(tool) => {
                    let exec = tool.execute(&id, args);
                    tokio::select! {
                        biased;
                        () = cancelled(config) => {
                            // Cancellation fired while the tool was running. The
                            // ToolExecutionStart emitted above has no paired End,
                            // but the whole turn is being torn down: the caller
                            // sees a clean `Cancelled` and discards the run.
                            return Err(AgentError::Cancelled);
                        }
                        result = exec => match result {
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
                    }
                }
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
                    session_id: sid.clone(),
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
                                session_id: sid.clone(),
                                id: id.clone(),
                                kind: kind.clone(),
                                props: props.clone(),
                                replace: *replace,
                            },
                        );
                    }
                    ToolSideEffect::Unmount { id } => {
                        emit(
                            &events,
                            AgentEvent::Unmount {
                                session_id: sid.clone(),
                                id: id.clone(),
                            },
                        );
                    }
                    ToolSideEffect::BrowserActivity { active } => {
                        emit(
                            &events,
                            AgentEvent::BrowserActivity {
                                session_id: sid.clone(),
                                active: *active,
                            },
                        );
                    }
                    ToolSideEffect::SurfacePatch { canvas_id, patches } => {
                        // Slice 3: forward the validated patches onto the event
                        // bus stamped with this run's session id. The daemon's
                        // SSE bridge wraps each patch in a `SurfacePatchEnvelope`
                        // and relays it as `AgentTurnEvent::SurfacePatch` over
                        // `/v1/agent/events`, scoped to `session_id`.
                        emit(
                            &events,
                            AgentEvent::SurfacePatch {
                                session_id: sid.clone(),
                                canvas_id: canvas_id.clone(),
                                patches: patches.clone(),
                            },
                        );
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
        emit(
            &events,
            AgentEvent::TurnEnd {
                session_id: sid.clone(),
            },
        );
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
            session_id: sid.clone(),
            messages: messages.clone(),
        },
    );
    Ok(AgentRun {
        messages,
        stopped_at_turn_limit,
        usage: total_usage,
    })
}

/// Build the synthetic error `ToolResult` that answers a tool_use the model
/// emitted but the loop did not execute, because the turn stopped for a
/// non-`ToolUse` reason (e.g. `StopReason::Length` truncating output after a
/// complete tool_call was assembled). Pairing the orphan tool_use with this
/// result keeps the persisted transcript provider-valid on the next turn —
/// without it, the unanswered tool_use 400s the whole session. See OCEAN-103.
fn truncated_tool_use_result(id: &str, name: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![Content::text(
            "tool call was not executed: the model's response was cut short \
             (stop reason was not tool_use, e.g. the output token limit was \
             reached) before this tool_use could be run",
        )],
        is_error: true,
        timestamp: ocean_protocol::now_ms(),
    })
}

/// Max bytes of a single tool-result text block we retain in the transcript.
/// ~32 KB ≈ 8k tokens — enough for the model to act on a tool result, bounded
/// so a giant `read`/`bash`/`ls` dump can't be resent indefinitely. The full
/// output still reaches the UI via the `ToolExecutionEnd` event.
const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

/// Max bytes of a single tool-result image block (the base64 `data` payload) we
/// retain in the transcript. Images legitimately need far more room than a text
/// dump, but they are *not* unbounded: a tool that returns a multi-megabyte
/// screenshot or a binary blob mislabelled as an image would otherwise be
/// re-sent verbatim on every subsequent round, ballooning input tokens with no
/// ceiling — and base64 strings hold poorly in memory at scale. 256 KB of
/// base64 (~192 KB of decoded image) is generous for a normal screenshot while
/// still bounding the worst case. Oversized images are dropped from the
/// transcript and replaced with a text marker; the full image still reaches the
/// UI via the `ToolExecutionEnd` event.
const MAX_TOOL_RESULT_IMAGE_BYTES: usize = 256 * 1024;

/// Cap each block of a tool result so a single tool can't blow the context /
/// memory budget:
/// - Oversized **text** is truncated on a char boundary with a marker noting how
///   much was elided.
/// - Oversized **image** data (the base64 `data` payload) is dropped and
///   replaced with a `[image omitted: …]` text marker. A normally-sized image
///   passes through unchanged.
///
/// In both cases the full, untruncated output still reaches the UI via the
/// `ToolExecutionEnd` event — only the copy retained in the model transcript is
/// bounded.
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
            Content::Image { data, mime_type } if data.len() > MAX_TOOL_RESULT_IMAGE_BYTES => {
                // Drop the oversized payload from the transcript, leaving a clear
                // marker. We don't try to re-encode/resize the image here — the
                // goal is purely to bound the bytes the model re-sends each round.
                let bytes = data.len();
                Content::Text {
                    text: format!(
                        "[image omitted: {bytes} bytes ({mime_type}) exceeds the \
                         {MAX_TOOL_RESULT_IMAGE_BYTES}-byte tool-result image cap; \
                         full image shown in UI]"
                    ),
                }
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
///
/// Exposed `pub` so the no-orphan-tool-result invariant can be exercised by the
/// `trim_invariants` property test in `crates/ocean-runtime/tests/`.
pub fn trim_to_context_window(
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
    // Never return empty: if dropping leading orphans ate everything, the last
    // message must itself be a tool result whose call fell outside the budget
    // window. We cannot keep that result alone — a `ToolResult` with no matching
    // `ToolCall` is rejected by providers (Anthropic/OpenAI both 400). So expand
    // the window back to the assistant turn that issued the call, keeping the
    // pair intact. The budget is a soft target; one over-budget pair is always
    // preferable to an invalid request. If no matching call exists anywhere
    // (never happens for histories the loop builds), the interior pairing pass
    // below will drop the orphan and we fall back to the last message.
    if keep_from >= messages.len() {
        keep_from = anchor_including_call_for_last(messages);
    }

    let suffix = &messages[keep_from..];

    // Final pairing pass. The contiguous suffix above can still contain an
    // *interior* orphan tool result — e.g. when the cut splits a batch of
    // parallel tool calls so some results in the kept window answer a call that
    // was trimmed. A `ToolResult` with no matching `ToolCall` earlier in the
    // request is rejected by providers, so we drop any such orphan, anywhere in
    // the kept slice. This keeps the returned history provider-valid for *any*
    // input shape, not just the well-ordered call-then-result histories the
    // loop normally produces.
    let mut seen_calls: HashSet<&str> = HashSet::new();
    let mut out: Vec<Message> = Vec::with_capacity(suffix.len());
    for msg in suffix {
        if let Message::Assistant(a) = msg {
            for c in &a.content {
                if let Content::ToolCall { id, .. } = c {
                    seen_calls.insert(id.as_str());
                }
            }
        }
        if let Message::ToolResult(r) = msg {
            if !seen_calls.contains(r.tool_call_id.as_str()) {
                continue; // orphan — its ToolCall is not in the kept window
            }
        }
        out.push(msg.clone());
    }
    // Last resort: if every kept message was an orphan tool result (a history
    // whose final tool result has no matching call anywhere), keep the most
    // recent non-tool-result message so we never hand back an empty request.
    if out.is_empty() && !messages.is_empty() {
        if let Some(msg) = messages
            .iter()
            .rev()
            .find(|m| !matches!(m, Message::ToolResult(_)))
        {
            out.push(msg.clone());
        }
    }

    let kept = out.len();
    if kept < messages.len() {
        tracing::debug!(
            total = messages.len(),
            kept,
            dropped = messages.len() - kept,
            budget,
            "trimmed message history to fit context window"
        );
    }
    out
}

/// Anchor index for the fallback case where budget-trimming + orphan-dropping
/// emptied the window because the **last** message is a tool result. Returns
/// the index of the assistant message that issued the matching `ToolCall`, so
/// the kept slice `[idx..]` carries the call/result pair (no orphan). If no
/// matching call exists anywhere — which the agent loop never produces, since
/// it always emits the call before the result — return the last index and let
/// the interior pairing pass drop the unpaired result.
fn anchor_including_call_for_last(messages: &[Message]) -> usize {
    let last = messages.len() - 1;
    let Message::ToolResult(result) = &messages[last] else {
        return last;
    };
    for idx in (0..last).rev() {
        if let Message::Assistant(a) = &messages[idx] {
            let issues_call = a
                .content
                .iter()
                .any(|c| matches!(c, Content::ToolCall { id, .. } if *id == result.tool_call_id));
            if issues_call {
                return idx;
            }
        }
    }
    last
}

fn emit(sink: &Option<mpsc::UnboundedSender<AgentEvent>>, ev: AgentEvent) {
    if let Some(s) = sink {
        let _ = s.send(ev);
    }
}

/// Whether this run has been asked to cancel. Reads the cancellation token the
/// daemon wires into `StreamOptions::cancel` (set by POST
/// /v1/requests/{id}/cancel). Returns `false` when no token is present — ad-hoc
/// runs (tests, embedded use) are never cancelled this way.
fn is_cancelled(config: &AgentConfig) -> bool {
    config
        .stream_options
        .cancel
        .as_ref()
        .map(|c| c.is_cancelled())
        .unwrap_or(false)
}

/// A future that resolves once this run's cancellation token trips, used to race
/// in-flight tool execution against a cancel (OCEAN-116). When no token is wired
/// (ad-hoc/embedded runs), it never resolves — so a `tokio::select!` against it
/// reduces to just awaiting the tool, preserving the non-cancelled path exactly.
async fn cancelled(config: &AgentConfig) {
    match config.stream_options.cancel.as_ref() {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentConfig;
    use ocean_protocol::{Model, ToolResultMessage};
    use tokio_util::sync::CancellationToken;

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

    /// OCEAN-103: a model can emit a complete tool_use and still stop for a
    /// non-`ToolUse` reason (e.g. `StopReason::Length` — output truncated at the
    /// token limit after the tool_call was assembled). The loop must NOT leave
    /// that tool_use unanswered in the transcript: an assistant tool_use with no
    /// matching tool_result is rejected (400) by Anthropic/OpenAI on the next
    /// turn, silently breaking the whole session for every client. This exercises
    /// the assembly the loop performs in that arm: push the assistant tool_use,
    /// then pair each orphan with `truncated_tool_use_result`. We assert the
    /// resulting transcript is provider-valid (every tool_use has a matching
    /// tool_result) and that it stays valid through `trim_to_context_window`.
    #[test]
    fn truncated_tool_use_is_paired_with_a_result_no_orphan_call() {
        use ocean_protocol::AssistantMessage;

        // Assistant message carrying a complete tool_use, but stopped on Length.
        let assistant = Message::Assistant(AssistantMessage {
            content: vec![
                Content::text("partial answer"),
                Content::ToolCall {
                    id: "call-trunc".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "cmd": "echo hi" }),
                },
            ],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Default::default(),
            stop_reason: StopReason::Length,
            error_message: None,
            timestamp: 0,
        });

        // Replicate the loop's non-ToolUse-stop assembly: push the assistant
        // message, then pair every emitted tool_use with a synthetic result.
        let mut messages = vec![user("do a thing"), assistant];
        messages.push(truncated_tool_use_result("call-trunc", "bash"));

        // Provider-validity invariant: every tool_use id has a matching
        // tool_result later in the transcript.
        assert_no_orphan_tool_use(&messages);

        // The paired result must be a flagged error so the model sees the call
        // didn't run, not a fake success.
        let Some(Message::ToolResult(tr)) = messages.last() else {
            panic!("last message must be the paired tool result");
        };
        assert!(tr.is_error, "truncated tool_use result must be is_error");
        assert_eq!(tr.tool_call_id, "call-trunc");

        // And the pairing survives a trim down to a tiny window: trimming must
        // not reintroduce an orphan (of either kind).
        let kept = trim_to_context_window(&messages, "sys", 200_000, 8_000);
        assert_no_orphan_tool_use(&kept);
    }

    /// Assert no assistant tool_use is left without a matching tool_result later
    /// in the transcript (the invariant providers enforce on the *next* turn).
    fn assert_no_orphan_tool_use(messages: &[Message]) {
        use std::collections::HashSet;
        let mut answered: HashSet<&str> = HashSet::new();
        for m in messages {
            if let Message::ToolResult(r) = m {
                answered.insert(r.tool_call_id.as_str());
            }
        }
        for m in messages {
            if let Message::Assistant(a) = m {
                for c in &a.content {
                    if let Content::ToolCall { id, .. } = c {
                        assert!(
                            answered.contains(id.as_str()),
                            "orphan tool_use {id:?} has no matching tool_result: {messages:#?}",
                        );
                    }
                }
            }
        }
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

    #[test]
    fn cap_tool_content_bounds_oversized_image() {
        // A tool that returns a huge image (e.g. a multi-MB screenshot or a
        // binary blob) must not pass through uncapped — that's the OOM / token
        // explosion path. The oversized image is dropped and replaced with a
        // text marker; the retained block must be far smaller than the input.
        let huge = "A".repeat(MAX_TOOL_RESULT_IMAGE_BYTES * 2);
        let original_len = huge.len();
        let capped = cap_tool_content(vec![Content::Image {
            data: huge,
            mime_type: "image/png".into(),
        }]);
        assert_eq!(capped.len(), 1);
        let text = capped[0]
            .as_text()
            .expect("oversized image should be replaced by a text marker");
        assert!(text.contains("image omitted"), "marker text: {text}");
        assert!(text.contains("image/png"), "marker keeps mime type: {text}");
        assert!(
            text.len() < 1024,
            "marker must be tiny relative to the {original_len}-byte input, got {}",
            text.len()
        );
    }

    #[test]
    fn cap_tool_content_leaves_normal_image_untouched() {
        // A normally-sized image passes through byte-for-byte: the cap only
        // trips on the pathological case.
        let data = "B".repeat(1024); // ~1 KB, well under the cap
        let img = Content::Image {
            data: data.clone(),
            mime_type: "image/jpeg".into(),
        };
        let capped = cap_tool_content(vec![img.clone()]);
        assert_eq!(capped.len(), 1);
        match &capped[0] {
            Content::Image {
                data: d,
                mime_type: m,
            } => {
                assert_eq!(d, &data, "image data must be untouched");
                assert_eq!(m, "image/jpeg", "mime type must be untouched");
            }
            other => panic!("expected the image to pass through unchanged, got {other:?}"),
        }
    }

    /// A config carrying an already-cancelled token must abort the run with
    /// `AgentError::Cancelled` *before* touching the provider (OCEAN-57). The
    /// cancellation check sits at the top of the turn loop, ahead of any
    /// `stream_simple` call, so no network/credentials are needed here.
    #[tokio::test]
    async fn cancelled_token_aborts_before_provider_call() {
        let token = CancellationToken::new();
        token.cancel(); // pre-cancelled: the very first round must bail out

        let mut cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
            .with_session_id("sess-cancel");
        cfg.stream_options.cancel = Some(token);

        let err = run_agent(&cfg, user("hello"), None)
            .await
            .err()
            .expect("a pre-cancelled run must return Err, not Ok");
        assert!(
            matches!(err, AgentError::Cancelled),
            "expected AgentError::Cancelled, got {err:?}"
        );
    }

    /// Events the loop emits before the first provider round (UserMessage,
    /// AgentStart) must carry the config's session id (OCEAN-54). We pair this
    /// with a pre-cancelled token so the run returns deterministically without a
    /// live provider, while still exercising the emit path.
    #[tokio::test]
    async fn emitted_events_carry_config_session_id() {
        let token = CancellationToken::new();
        token.cancel();

        let mut cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
            .with_session_id("sess-42");
        cfg.stream_options.cancel = Some(token);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = run_agent(&cfg, user("hi"), Some(tx)).await;

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.is_empty(), "loop should emit at least UserMessage + AgentStart");
        for ev in &events {
            assert_eq!(
                ev.session_id(),
                Some("sess-42"),
                "every emitted event must carry the config session id: {ev:?}"
            );
        }
        // Sanity: the pre-start events are present.
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::UserMessage { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentStart { .. })));
    }

    /// With no session bound, events carry `None` (ad-hoc/test runs).
    #[tokio::test]
    async fn emitted_events_have_no_session_id_when_unset() {
        let token = CancellationToken::new();
        token.cancel();

        let mut cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test");
        cfg.stream_options.cancel = Some(token);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = run_agent(&cfg, user("hi"), Some(tx)).await;

        while let Ok(ev) = rx.try_recv() {
            assert_eq!(ev.session_id(), None, "unset session must stay None: {ev:?}");
        }
    }
}
