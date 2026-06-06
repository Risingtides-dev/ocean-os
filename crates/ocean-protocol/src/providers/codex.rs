//! Codex provider — OpenAI Responses API over a ChatGPT/Codex OAuth token.
//!
//! Drives `gpt-5.x` through the operator's ChatGPT subscription (no API key) by
//! speaking the Responses API to `https://chatgpt.com/backend-api/codex/responses`.
//! The bearer credential is the Codex OAuth `access` token; the request also
//! carries a `chatgpt-account-id` header (passed via `StreamOptions::headers`).
//!
//! Stateless: `store: false`, full history sent on every call. The wire shape
//! (flat tools, `input` item types, SSE event names) follows the open-source
//! `openai/codex` Rust client and the `codex-openai-proxy` reference.

use async_stream::stream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::providers::Provider;
use crate::retry::{classify_status, parse_retry_after, with_retry, Attempt, RetryConfig};
use crate::stream::AssistantMessageEventStream;
use crate::types::{
    now_ms, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, Usage,
};

const ORIGINATOR: &str = "codex_cli_rs";
const OPENAI_BETA: &str = "responses=experimental";

pub struct CodexProvider {
    client: reqwest::Client,
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the Responses API `input` array from Ocean messages.
///
/// - user/assistant text → `message` items with typed content parts
/// - assistant tool calls → `function_call` items (arguments as a JSON string)
/// - tool results → `function_call_output` items
fn convert_input(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m {
            Message::User { content, .. } => {
                // Responses API content parts: input_text + input_image (data-URL).
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => {
                            Some(json!({"type": "input_text", "text": text}))
                        }
                        Content::Image { data, mime_type } => Some(json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", mime_type, data),
                        })),
                        _ => None,
                    })
                    .collect();
                out.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
            Message::Assistant(a) => {
                let mut text = String::new();
                for c in &a.content {
                    match c {
                        Content::Text { text: t } => text.push_str(t),
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            // Flush any accumulated text as its own message item
                            // ahead of the call, preserving order.
                            if !text.is_empty() {
                                out.push(json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{"type": "output_text", "text": text}],
                                }));
                                text = String::new();
                            }
                            out.push(json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": arguments.to_string(),
                                "call_id": id,
                            }));
                        }
                        // OCEAN-140: the Responses API replays chain-of-thought
                        // only via the original `reasoning` items, carried across
                        // turns as opaque `reasoning.encrypted_content` blobs tied
                        // to the item id the API emitted. A `reasoning` input item
                        // cannot be synthesized from free-form text — the API
                        // rejects reconstructed reasoning. Ocean's
                        // Content::Thinking carries an Anthropic-style thinking
                        // string + signature, not an encrypted Responses reasoning
                        // item, so there is nothing valid to re-encode here. Drop
                        // it EXPLICITLY rather than via a silent `_ => {}` (kills
                        // the OCEAN-101 silent-drop class).
                        Content::Thinking { .. } => {}
                        // Images never appear in assistant content on this API.
                        Content::Image { .. } => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tr.tool_call_id,
                    "output": text,
                }));

                // OCEAN-133: tool-result images (browser/computer-use screenshots)
                // can't ride inside `function_call_output.output`, which is a plain
                // string on the Responses API. To keep vision parity with Anthropic,
                // follow the function output with a user-role `message` that carries
                // the screenshot(s) as `input_image` parts (same data-URL shape used
                // for user-message images in convert_input above). Without this the
                // image is silently dropped and the model "can't see the screenshot".
                let images: Vec<Value> = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Image { data, mime_type } => Some(json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", mime_type, data),
                        })),
                        _ => None,
                    })
                    .collect();
                if !images.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": images,
                    }));
                }
            }
        }
    }
    out
}

fn reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh => Some("high"),
    }
}

fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    // OCEAN-165: do NOT set `parallel_tool_calls`. The Codex backend speaks the
    // OpenAI Responses API, which accepts this param and defaults it to `true`
    // (parallel tool calls allowed) when omitted. Hardcoding `false` forced every
    // multi-tool turn to serialize: a 3-tool turn that is one round-trip on
    // Anthropic/OpenAI/Gemini became 3 sequential round-trips here. The sibling
    // providers (openai.rs, anthropic, google.rs) never set this field and so ride
    // their API default; omitting it here restores parity — Codex now allows
    // parallel tool calls like the rest.
    let mut body = json!({
        "model": model.id,
        "input": convert_input(&context.messages),
        "tool_choice": "auto",
        "store": false,
        "stream": true,
    });
    if let Some(sp) = &context.system_prompt {
        body["instructions"] = json!(sp);
    }
    if !context.tools.is_empty() {
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    if let Some(level) = options.reasoning {
        if let Some(effort) = reasoning_effort(level) {
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
    // OCEAN-176: Codex speaks the OpenAI Responses API, whose output cap is the
    // top-level `max_output_tokens` (NOT Chat-Completions `max_tokens` /
    // `max_completion_tokens`). Without this the operator's output-length cap was
    // silently dropped while every sibling provider honored it.
    if let Some(m) = options.max_tokens {
        body["max_output_tokens"] = json!(m);
    }
    body
}

// --- SSE payload shapes (only the fields we consume) ---

#[derive(Deserialize)]
struct OutputItemEnvelope {
    item: OutputItem,
}

#[derive(Deserialize)]
struct OutputItem {
    // OCEAN-165: the item's own id (e.g. `fc_*`). The Responses API uses THIS as
    // the `item_id` on `response.function_call_arguments.delta` frames, so it is
    // the only stable key that ties streamed argument deltas to the right call
    // when several function calls are in flight (parallel tool calls). `call_id`
    // (the `call_*` token Ocean replays in function_call_output) is a SEPARATE id
    // and must NOT be used to match streaming partials.
    #[serde(default)]
    id: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    call_id: String,
}

#[derive(Deserialize)]
struct TextDeltaEvent {
    #[serde(default)]
    delta: String,
}

#[derive(Deserialize)]
struct ArgsDeltaEvent {
    #[serde(default)]
    delta: String,
    #[serde(default)]
    item_id: String,
}

#[derive(Deserialize)]
struct CompletedEvent {
    #[serde(default)]
    response: CompletedResponse,
}

#[derive(Deserialize, Default)]
struct CompletedResponse {
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    // OCEAN-158: the Responses API reports cached-prompt tokens under
    // `input_tokens_details.cached_tokens`. Decode them into usage.cache_read.
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    // OCEAN-164: the Responses API reports reasoning tokens under
    // `output_tokens_details.reasoning_tokens`. They are already part of
    // `output_tokens`, so we decode them only to surface the reasoning subset.
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Deserialize)]
struct FailedEvent {
    #[serde(default)]
    response: FailedResponse,
}

#[derive(Deserialize, Default)]
struct FailedResponse {
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Deserialize, Default)]
struct ResponseError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Default)]
struct PartialToolCall {
    /// The `call_id` (`call_*`) replayed back to the API in function_call_output.
    /// Distinct from the map key, which is the item id (`fc_*`).
    id: String,
    name: String,
    args: String,
    block_index: usize,
    /// Whether ToolCallStart has already been emitted for this block, so the
    /// added/delta/done arms never double-start the same call.
    started: bool,
}

/// OCEAN-165: item-id-keyed accumulator for streamed function calls.
///
/// Every function-call lifecycle frame on the Responses API carries the item id
/// (`fc_*`): `output_item.added` introduces it, `function_call_arguments.delta`
/// repeats it as `item_id`, and `output_item.done` reports it as `item.id`.
/// Keying on item id (NOT `call_id`) is what makes PARALLEL tool calls correct —
/// two interleaved calls never collide and each `done` finalizes its own block.
///
/// The struct owns block-index assignment and the started-once flag so the stream
/// loop and the unit tests share the exact same disambiguation logic.
#[derive(Default)]
struct ToolCalls {
    by_item: std::collections::BTreeMap<String, PartialToolCall>,
    order: Vec<String>,
}

/// What a single frame did to the accumulator, so the stream loop knows which
/// `AssistantMessageEvent`s to yield.
struct ToolCallStep {
    /// Set when this frame first started the block (emit ToolCallStart).
    started: Option<ToolCallStarted>,
    /// Block index + delta to forward (emit ToolCallDelta).
    delta: Option<(usize, String)>,
}

struct ToolCallStarted {
    block_index: usize,
    call_id: String,
    name: String,
}

/// A finalized call, in stream order.
#[derive(Debug, PartialEq, Eq)]
struct FinalToolCall {
    block_index: usize,
    call_id: String,
    name: String,
    args: String,
}

impl ToolCalls {
    /// Ensure a block exists for `item_id`, assigning the next block index and
    /// recording stream order the first time. Returns whether it was just started.
    fn ensure(&mut self, item_id: &str, next_block_index: &mut usize) -> bool {
        let exists_started = self.by_item.get(item_id).map(|e| e.started).unwrap_or(false);
        if exists_started {
            return false;
        }
        let entry = self.by_item.entry(item_id.to_string()).or_default();
        entry.block_index = *next_block_index;
        *next_block_index += 1;
        entry.started = true;
        self.order.push(item_id.to_string());
        true
    }

    /// Handle `response.output_item.added` for a function_call item.
    fn on_added(&mut self, item: &OutputItem, next_block_index: &mut usize) -> ToolCallStep {
        let started = self.ensure(&item.id, next_block_index);
        let entry = self.by_item.get_mut(&item.id).unwrap();
        entry.id = item.call_id.clone();
        entry.name = item.name.clone();
        ToolCallStep {
            started: started.then(|| ToolCallStarted {
                block_index: entry.block_index,
                call_id: item.call_id.clone(),
                name: item.name.clone(),
            }),
            delta: None,
        }
    }

    /// Handle `response.function_call_arguments.delta`.
    fn on_args_delta(
        &mut self,
        item_id: &str,
        delta: &str,
        next_block_index: &mut usize,
    ) -> ToolCallStep {
        let started_now = self.ensure(item_id, next_block_index);
        let entry = self.by_item.get_mut(item_id).unwrap();
        let started = started_now.then(|| ToolCallStarted {
            block_index: entry.block_index,
            call_id: entry.id.clone(),
            name: entry.name.clone(),
        });
        entry.args.push_str(delta);
        let block_index = entry.block_index;
        ToolCallStep {
            started,
            delta: Some((block_index, delta.to_string())),
        }
    }

    /// Handle `response.output_item.done` for a function_call item.
    fn on_done(&mut self, item: &OutputItem, next_block_index: &mut usize) -> ToolCallStep {
        // Match by item id when present (the stable streaming key); otherwise
        // synthesize a block under the call_id so the call is never lost.
        let key = if !item.id.is_empty() {
            item.id.clone()
        } else {
            item.call_id.clone()
        };
        let started_now = self.ensure(&key, next_block_index);
        let entry = self.by_item.get_mut(&key).unwrap();
        let started = started_now.then(|| ToolCallStarted {
            block_index: entry.block_index,
            call_id: item.call_id.clone(),
            name: item.name.clone(),
        });
        entry.id = item.call_id.clone();
        entry.name = item.name.clone();
        if entry.args.is_empty() {
            entry.args = item.arguments.clone();
        }
        ToolCallStep {
            started,
            delta: None,
        }
    }

    /// Finalized calls in stream order, args parsed to JSON (empty → `{}`).
    fn finalize(&self) -> Vec<FinalToolCall> {
        self.order
            .iter()
            .filter_map(|k| self.by_item.get(k))
            .map(|tc| FinalToolCall {
                block_index: tc.block_index,
                call_id: tc.id.clone(),
                name: tc.name.clone(),
                args: tc.args.clone(),
            })
            .collect()
    }
}

#[async_trait]
impl Provider for CodexProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        let access = options
            .api_key
            .clone()
            .ok_or_else(|| Error::MissingApiKey("openai-codex".into()))?;
        let account_id = options.headers.get("chatgpt-account-id").cloned();
        let base_url = options
            .base_url
            .clone()
            .unwrap_or_else(|| model.base_url.clone());
        let url = format!("{}/responses", base_url.trim_end_matches('/'));
        let body = build_body(model, context, options);
        let cancel = options.cancel.clone();

        let resp = with_retry(&RetryConfig::default(), cancel.as_ref(), |_| {
            let client = self.client.clone();
            let url = url.clone();
            let access = access.clone();
            let account_id = account_id.clone();
            let body = body.clone();
            async move {
                let session_id = uuid::Uuid::new_v4().to_string();
                let mut req = client
                    .post(&url)
                    .bearer_auth(&access)
                    .header("accept", "text/event-stream")
                    .header("content-type", "application/json")
                    .header("originator", ORIGINATOR)
                    .header("openai-beta", OPENAI_BETA)
                    .header("session_id", session_id);
                if let Some(acct) = &account_id {
                    req = req.header("chatgpt-account-id", acct);
                }
                let r = match req.json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return if e.is_timeout() || e.is_connect() {
                            Attempt::Retry {
                                error: Error::Http(e),
                                retry_after: None,
                            }
                        } else {
                            Attempt::Fatal(Error::Http(e))
                        }
                    }
                };
                let status = r.status();
                if status.is_success() {
                    return Attempt::Ok(r);
                }
                let retry_after = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                let body = r.text().await.unwrap_or_default();
                let err = Error::ProviderError {
                    status: status.as_u16(),
                    body,
                };
                match classify_status(status.as_u16()) {
                    Some(_) => Attempt::Retry {
                        error: err,
                        retry_after,
                    },
                    None => Attempt::Fatal(err),
                }
            }
        })
        .await?;

        let api = model.api.clone();
        let provider = model.provider.clone();
        let model_id = model.id.clone();
        let cancel_for_stream = cancel.clone();

        let s = stream! {
            yield Ok(AssistantMessageEvent::Start);

            let mut sse = resp.bytes_stream().eventsource();

            let mut next_block_index: usize = 0;
            let mut text_buf = String::new();
            let mut text_index: Option<usize> = None;
            // OCEAN-165: item-id-keyed accumulator (see ToolCalls). Makes parallel
            // tool calls correct — each call's added/delta/done frames are tied
            // together by the Responses item id, so two in-flight calls never
            // collide and each `done` finalizes the exact block it belongs to.
            let mut tool_calls = ToolCalls::default();
            let mut stop = StopReason::Stop;
            let mut usage = Usage::default();

            while let Some(ev) = sse.next().await {
                if let Some(c) = &cancel_for_stream {
                    if c.is_cancelled() {
                        yield Err(Error::Cancelled);
                        return;
                    }
                }
                let ev = match ev {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(Error::InvalidResponse(format!("sse: {e}")));
                        return;
                    }
                };
                if ev.data.is_empty() || ev.data == "[DONE]" {
                    continue;
                }
                // The Responses API names events via the SSE `event:` field, but
                // also embeds `type` in the JSON payload. Prefer the payload type.
                let value: Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(error = %e, "skipping unparseable Codex SSE frame");
                        continue;
                    }
                };
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or(ev.event.as_str())
                    .to_string();

                match kind.as_str() {
                    "response.output_text.delta" => {
                        if let Ok(d) = serde_json::from_value::<TextDeltaEvent>(value) {
                            if !d.delta.is_empty() {
                                let idx = match text_index {
                                    Some(i) => i,
                                    None => {
                                        let i = next_block_index;
                                        next_block_index += 1;
                                        text_index = Some(i);
                                        yield Ok(AssistantMessageEvent::TextStart { content_index: i });
                                        i
                                    }
                                };
                                text_buf.push_str(&d.delta);
                                yield Ok(AssistantMessageEvent::TextDelta {
                                    content_index: idx,
                                    delta: d.delta,
                                });
                            }
                        }
                    }
                    // OCEAN-165: the Responses API introduces each function call
                    // with an `output_item.added` frame carrying the item `id`
                    // (`fc_*`), `call_id`, and `name` BEFORE any argument deltas.
                    // Register the call (keyed by item id) up front.
                    "response.output_item.added" => {
                        if let Ok(added) = serde_json::from_value::<OutputItemEnvelope>(value) {
                            let item = added.item;
                            if item.r#type == "function_call" && !item.id.is_empty() {
                                let step = tool_calls.on_added(&item, &mut next_block_index);
                                if let Some(s) = step.started {
                                    yield Ok(AssistantMessageEvent::ToolCallStart {
                                        content_index: s.block_index,
                                        id: s.call_id,
                                        name: s.name,
                                    });
                                }
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Ok(d) = serde_json::from_value::<ArgsDeltaEvent>(value) {
                            if d.item_id.is_empty() || d.delta.is_empty() {
                                continue;
                            }
                            let step = tool_calls.on_args_delta(&d.item_id, &d.delta, &mut next_block_index);
                            if let Some(s) = step.started {
                                yield Ok(AssistantMessageEvent::ToolCallStart {
                                    content_index: s.block_index,
                                    id: s.call_id,
                                    name: s.name,
                                });
                            }
                            if let Some((block_index, delta)) = step.delta {
                                yield Ok(AssistantMessageEvent::ToolCallDelta {
                                    content_index: block_index,
                                    delta,
                                });
                            }
                        }
                    }
                    "response.output_item.done" => {
                        if let Ok(done) = serde_json::from_value::<OutputItemEnvelope>(value) {
                            let item = done.item;
                            if item.r#type == "function_call" {
                                stop = StopReason::ToolUse;
                                let step = tool_calls.on_done(&item, &mut next_block_index);
                                if let Some(s) = step.started {
                                    yield Ok(AssistantMessageEvent::ToolCallStart {
                                        content_index: s.block_index,
                                        id: s.call_id,
                                        name: s.name,
                                    });
                                }
                            }
                        }
                    }
                    "response.completed" => {
                        if let Ok(c) = serde_json::from_value::<CompletedEvent>(value) {
                            if let Some(u) = c.response.usage {
                                usage.input = u.input_tokens;
                                usage.output = u.output_tokens;
                                usage.total_tokens = if u.total_tokens > 0 {
                                    u.total_tokens
                                } else {
                                    u.input_tokens + u.output_tokens
                                };
                                if let Some(d) = u.input_tokens_details {
                                    usage.cache_read = d.cached_tokens;
                                }
                                if let Some(d) = u.output_tokens_details {
                                    usage.reasoning = d.reasoning_tokens;
                                }
                            }
                        }
                        break;
                    }
                    // Safety refusal. The Responses API streams a decline through
                    // its own refusal channel; without handling it the turn would
                    // end with empty text and no explanation (OCEAN-101). Surface
                    // it as visible assistant text.
                    "response.refusal.delta" => {
                        if let Ok(d) = serde_json::from_value::<TextDeltaEvent>(value) {
                            if !d.delta.is_empty() {
                                let idx = match text_index {
                                    Some(i) => i,
                                    None => {
                                        let i = next_block_index;
                                        next_block_index += 1;
                                        text_index = Some(i);
                                        yield Ok(AssistantMessageEvent::TextStart { content_index: i });
                                        i
                                    }
                                };
                                text_buf.push_str(&d.delta);
                                yield Ok(AssistantMessageEvent::TextDelta {
                                    content_index: idx,
                                    delta: d.delta,
                                });
                            }
                        }
                    }
                    "response.failed" | "response.incomplete" => {
                        let parsed = serde_json::from_value::<FailedEvent>(value).ok();
                        let (code, message) = parsed
                            .and_then(|f| f.response.error)
                            .map(|e| (e.code, e.message))
                            .unwrap_or_default();
                        yield Err(Error::InvalidResponse(format!(
                            "codex response {kind}: {code} {message}"
                        )));
                        return;
                    }
                    // Top-level transport/stream error event (distinct from the
                    // `response.failed` envelope). The Responses API emits a bare
                    // `error` event for stream-level failures; surface it instead
                    // of silently ending the turn.
                    "error" | "response.error" => {
                        let message = value
                            .get("message")
                            .and_then(Value::as_str)
                            .or_else(|| value.get("error").and_then(|e| e.get("message")).and_then(Value::as_str))
                            .unwrap_or("unknown stream error")
                            .to_string();
                        yield Err(Error::InvalidResponse(format!(
                            "codex stream error: {message}"
                        )));
                        return;
                    }
                    _ => {}
                }
            }

            if let Some(i) = text_index {
                yield Ok(AssistantMessageEvent::TextEnd {
                    content_index: i,
                    content: text_buf.clone(),
                });
            }

            let mut out_content: Vec<Content> = Vec::new();
            if !text_buf.is_empty() {
                out_content.push(Content::Text { text: text_buf.clone() });
            }
            for tc in tool_calls.finalize() {
                let args: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args).unwrap_or(Value::Object(Default::default()))
                };
                yield Ok(AssistantMessageEvent::ToolCallEnd {
                    content_index: tc.block_index,
                    id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: args.clone(),
                });
                out_content.push(Content::ToolCall {
                    id: tc.call_id,
                    name: tc.name,
                    arguments: args,
                });
            }

            let message = AssistantMessage {
                content: out_content,
                api,
                provider,
                model: model_id,
                usage,
                stop_reason: stop,
                error_message: None,
                timestamp: now_ms(),
            };
            yield Ok(AssistantMessageEvent::Done { reason: stop, message });
        };

        Ok(s.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, ToolResultMessage};

    // OCEAN-99: vision parity for the OpenAI Responses API. A user image must
    // serialize as an input_image content part (data-URL), not be dropped.
    #[test]
    fn user_image_is_encoded_as_input_image_part() {
        let messages = vec![Message::User {
            content: vec![
                Content::text("describe this"),
                Content::Image {
                    data: "AAECAwQ=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            timestamp: now_ms(),
        }];

        let out = convert_input(&messages);
        assert_eq!(out.len(), 1);
        let parts = out[0]["content"]
            .as_array()
            .expect("content array missing");

        let has_text = parts
            .iter()
            .any(|p| p["type"] == "input_text" && p["text"] == "describe this");
        assert!(has_text, "input_text part missing: {:?}", parts);

        let image = parts
            .iter()
            .find(|p| p["type"] == "input_image")
            .expect("input_image part missing — image was dropped");
        assert_eq!(image["image_url"], "data:image/png;base64,AAECAwQ=");
    }

    // OCEAN-133: tool-result vision parity. A screenshot returned by a tool
    // (browser/computer-use → Content::Image) must reach the model. The
    // function_call_output string can't carry an image, so the encoder appends a
    // user-role message with an input_image part. Assert the image is NOT dropped.
    #[test]
    fn tool_result_image_is_appended_as_input_image() {
        let messages = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_42".into(),
            tool_name: "browser_screenshot".into(),
            content: vec![
                Content::text("here is the screenshot"),
                Content::Image {
                    data: "AAECAwQ=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            is_error: false,
            timestamp: now_ms(),
        })];

        let out = convert_input(&messages);

        // The text output still rides on the function_call_output, unchanged.
        let fco = out
            .iter()
            .find(|v| v["type"] == "function_call_output")
            .expect("function_call_output missing");
        assert_eq!(fco["call_id"], "call_42");
        assert_eq!(fco["output"], "here is the screenshot");

        // The image is appended as a user-role message with an input_image part.
        let msg = out
            .iter()
            .find(|v| v["type"] == "message" && v["role"] == "user")
            .expect("input_image follow-up message missing — image was dropped");
        let parts = msg["content"].as_array().expect("content array missing");
        let image = parts
            .iter()
            .find(|p| p["type"] == "input_image")
            .expect("input_image part missing — tool-result image was dropped");
        assert_eq!(image["image_url"], "data:image/png;base64,AAECAwQ=");
    }

    // OCEAN-133: text-only tool results must stay exactly as before — a single
    // function_call_output with no trailing image message.
    #[test]
    fn text_only_tool_result_stays_text_only() {
        let messages = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_7".into(),
            tool_name: "read_file".into(),
            content: vec![Content::text("file contents")],
            is_error: false,
            timestamp: now_ms(),
        })];

        let out = convert_input(&messages);

        assert_eq!(out.len(), 1, "expected only the function_call_output: {out:?}");
        assert_eq!(out[0]["type"], "function_call_output");
        assert_eq!(out[0]["call_id"], "call_7");
        assert_eq!(out[0]["output"], "file contents");
        assert!(
            !out.iter().any(|v| v["type"] == "input_image"
                || v["content"]
                    .as_array()
                    .map(|p| p.iter().any(|x| x["type"] == "input_image"))
                    .unwrap_or(false)),
            "no input_image should be emitted for a text-only tool result"
        );
    }

    // OCEAN-140: a replayed assistant turn carrying a Content::Thinking block must
    // hit an EXPLICIT match arm, not the old silent `_ => {}`. The Responses API
    // replays reasoning only via opaque encrypted_content items (which Ocean does
    // not carry), so the documented behavior is an intentional drop: the thinking
    // text must NOT leak into any emitted item, while the text message and
    // function_call items survive in order.
    #[test]
    fn assistant_thinking_is_explicitly_dropped() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "secret chain of thought".into(),
                    thinking_signature: Some("sig-abc".into()),
                },
                Content::text("visible answer"),
                Content::ToolCall {
                    id: "call_1".into(),
                    name: "calc".into(),
                    arguments: serde_json::json!({"x": 1}),
                },
            ],
            api: "responses".into(),
            provider: "codex".into(),
            model: "gpt-5".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: now_ms(),
        })];

        let out = convert_input(&messages);

        // No reasoning text or signature anywhere in the encoded input.
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            !serialized.contains("secret chain of thought"),
            "thinking text must not appear in encoded input: {serialized}"
        );
        assert!(
            !serialized.contains("sig-abc"),
            "thinking signature must not appear in encoded input: {serialized}"
        );

        // Exactly the assistant text message + the function_call survive.
        assert_eq!(out.len(), 2, "thinking must not add an item: {out:?}");
        assert_eq!(out[0]["type"], "message");
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["content"][0]["text"], "visible answer");
        assert_eq!(out[1]["type"], "function_call");
        assert_eq!(out[1]["name"], "calc");
    }

    fn codex_model() -> Model {
        Model {
            id: "gpt-5-codex".into(),
            name: "GPT-5 Codex".into(),
            api: "responses".into(),
            provider: "codex".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            reasoning: true,
            context_window: 272_000,
            max_tokens: 16_384,
        }
    }

    // OCEAN-165: the Codex backend speaks the OpenAI Responses API, which allows
    // parallel tool calls by default (parallel_tool_calls defaults to true when
    // omitted). The provider previously hardcoded `parallel_tool_calls: false`,
    // serializing multi-tool turns vs Anthropic/OpenAI/Gemini. build_body must NOT
    // emit the field at all, so Codex rides the API default like the siblings.
    #[test]
    fn build_body_does_not_force_parallel_tool_calls_off() {
        let model = codex_model();
        let context = Context::default();
        let options = StreamOptions::default();

        let body = build_body(&model, &context, &options);

        assert!(
            body.get("parallel_tool_calls").is_none(),
            "Codex must not set parallel_tool_calls — it should ride the Responses \
             API default (parallel allowed), matching the other providers. Got: {body}"
        );
        // Sanity: the rest of the request shape is intact.
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    // OCEAN-176: Codex (OpenAI Responses API) must emit the operator's output cap
    // as top-level `max_output_tokens` — the Responses-API field, NOT the
    // Chat-Completions `max_tokens` / `max_completion_tokens`. It was silently
    // dropped before, so gpt-5.x turns could run uncapped.
    #[test]
    fn build_body_emits_max_output_tokens_when_set() {
        let model = codex_model();
        let context = Context::default();
        let options = StreamOptions {
            max_tokens: Some(8192),
            ..StreamOptions::default()
        };

        let body = build_body(&model, &context, &options);

        assert_eq!(
            body["max_output_tokens"], 8192,
            "Codex must emit the Responses-API max_output_tokens cap. Got: {body}"
        );
        // It must be the Responses-API field name, not the Chat-Completions ones.
        assert!(
            body.get("max_tokens").is_none(),
            "must not emit Chat-Completions max_tokens: {body}"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "must not emit Chat-Completions max_completion_tokens: {body}"
        );
    }

    // OCEAN-176: when no cap is set, no output-length field is emitted at all
    // (rides the Responses API default), matching the sibling providers.
    #[test]
    fn build_body_omits_max_output_tokens_when_none() {
        let model = codex_model();
        let context = Context::default();
        let options = StreamOptions {
            max_tokens: None,
            ..StreamOptions::default()
        };

        let body = build_body(&model, &context, &options);

        assert!(
            body.get("max_output_tokens").is_none(),
            "Codex must omit max_output_tokens when no cap is set. Got: {body}"
        );
    }

    fn fc_item(id: &str, call_id: &str, name: &str, arguments: &str) -> OutputItem {
        OutputItem {
            id: id.into(),
            r#type: "function_call".into(),
            name: name.into(),
            arguments: arguments.into(),
            call_id: call_id.into(),
        }
    }

    // OCEAN-165: the core parallel-tool-calls correctness test. Two function calls
    // stream with INTERLEAVED frames (added A, added B, args A, args B, args A,
    // done B, done A) — the exact pattern that broke the old call_id /
    // first-empty-partial matching. Both calls must finalize with the right
    // id/name/args attached to the right block. This is what makes it safe to omit
    // parallel_tool_calls (let Codex return multiple calls in one response).
    #[test]
    fn interleaved_parallel_tool_calls_finalize_to_correct_blocks() {
        let mut tc = ToolCalls::default();
        let mut next: usize = 0;

        // The model opens two calls before completing either.
        tc.on_added(&fc_item("fc_A", "call_A", "get_weather", ""), &mut next);
        tc.on_added(&fc_item("fc_B", "call_B", "get_time", ""), &mut next);

        // Argument deltas arrive interleaved, keyed by item id.
        tc.on_args_delta("fc_A", "{\"city\":", &mut next);
        tc.on_args_delta("fc_B", "{\"tz\":", &mut next);
        tc.on_args_delta("fc_A", "\"SF\"}", &mut next);
        tc.on_args_delta("fc_B", "\"UTC\"}", &mut next);

        // Done events arrive OUT OF ORDER relative to start (B before A).
        tc.on_done(
            &fc_item("fc_B", "call_B", "get_time", "{\"tz\":\"UTC\"}"),
            &mut next,
        );
        tc.on_done(
            &fc_item("fc_A", "call_A", "get_weather", "{\"city\":\"SF\"}"),
            &mut next,
        );

        let finals = tc.finalize();
        assert_eq!(finals.len(), 2, "both parallel calls must survive: {finals:?}");

        // Stream order preserved: A was added first, so it stays block 0.
        let a = &finals[0];
        assert_eq!(a.block_index, 0);
        assert_eq!(a.call_id, "call_A");
        assert_eq!(a.name, "get_weather");
        assert_eq!(a.args, "{\"city\":\"SF\"}");

        let b = &finals[1];
        assert_eq!(b.block_index, 1);
        assert_eq!(b.call_id, "call_B");
        assert_eq!(b.name, "get_time");
        assert_eq!(b.args, "{\"tz\":\"UTC\"}");
    }

    // OCEAN-165: a call whose args never stream and that has no `added` frame —
    // only an `output_item.done` — must still finalize from the done payload alone.
    #[test]
    fn done_only_tool_call_finalizes_from_done_payload() {
        let mut tc = ToolCalls::default();
        let mut next: usize = 0;

        tc.on_done(
            &fc_item("fc_X", "call_X", "noop", "{\"k\":1}"),
            &mut next,
        );

        let finals = tc.finalize();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].call_id, "call_X");
        assert_eq!(finals[0].name, "noop");
        assert_eq!(finals[0].args, "{\"k\":1}");
    }

    // OCEAN-165: ToolCallStart must fire exactly once per call. `added` then
    // `delta` then `done` for the same item id must only report `started` on the
    // first frame, never re-start the block.
    #[test]
    fn tool_call_starts_exactly_once() {
        let mut tc = ToolCalls::default();
        let mut next: usize = 0;

        let s1 = tc.on_added(&fc_item("fc_1", "call_1", "f", ""), &mut next);
        assert!(s1.started.is_some(), "added must start the call");

        let s2 = tc.on_args_delta("fc_1", "{}", &mut next);
        assert!(s2.started.is_none(), "delta must NOT re-start an open call");

        let s3 = tc.on_done(&fc_item("fc_1", "call_1", "f", "{}"), &mut next);
        assert!(s3.started.is_none(), "done must NOT re-start an open call");

        assert_eq!(next, 1, "exactly one block index consumed");
    }

    // OCEAN-158: the Responses API reports cached-prompt tokens under
    // `input_tokens_details.cached_tokens`. The completed-event usage must decode
    // that field so it can populate usage.cache_read; otherwise Codex users see a
    // false 0 in the cache-read HUD.
    #[test]
    fn responses_usage_decodes_cached_tokens() {
        let raw = r#"{
            "input_tokens": 2000,
            "output_tokens": 80,
            "total_tokens": 2080,
            "input_tokens_details": {"cached_tokens": 1792}
        }"#;
        let u: ResponsesUsage = serde_json::from_str(raw).expect("responses usage parses");
        assert_eq!(
            u.input_tokens_details.expect("details present").cached_tokens,
            1792,
            "cached_tokens must decode from input_tokens_details"
        );
    }

    // OCEAN-164: the Responses API reports reasoning tokens under
    // `output_tokens_details.reasoning_tokens`. The completed-event usage must
    // decode that field so it can populate usage.reasoning. Reasoning is already
    // inside output_tokens, so it is surfaced only to show the reasoning subset.
    #[test]
    fn responses_usage_decodes_reasoning_tokens() {
        let raw = r#"{
            "input_tokens": 2000,
            "output_tokens": 320,
            "total_tokens": 2320,
            "output_tokens_details": {"reasoning_tokens": 256}
        }"#;
        let u: ResponsesUsage = serde_json::from_str(raw).expect("responses usage parses");
        assert_eq!(
            u.output_tokens_details
                .expect("details present")
                .reasoning_tokens,
            256,
            "reasoning_tokens must decode from output_tokens_details"
        );
    }
}
