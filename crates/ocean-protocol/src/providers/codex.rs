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
use crate::retry::{
    classify_status, parse_retry_after, retry_config, with_retry_observed, Attempt,
};
use crate::stream::AssistantMessageEventStream;
use crate::types::{
    now_ms, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, Usage,
};

/// Prefix marking a `thinking_signature` that carries a verbatim Responses API
/// `reasoning` output item (JSON with `id` + `encrypted_content`). Stateless
/// replay (`store: false`) must send these items back between tool rounds —
/// dropping them degrades gpt-5.x into malformed tool calls (harmony-format
/// leakage like `to=functions.edit` and token salad inside argument strings).
/// Other providers must treat a signature with this prefix as not-theirs.
pub(crate) const REASONING_ITEM_MARKER: &str = "codex-item:";

const ORIGINATOR: &str = "codex_cli_rs";
const OPENAI_BETA: &str = "responses=experimental";
// ChatGPT's Codex backend version-gates newly released models. Keep this aligned
// with the current open-source Codex CLI wire version.
const CODEX_VERSION: &str = "0.144.1";

fn apply_request_headers(
    request: reqwest::RequestBuilder,
    access: &str,
    session_id: &str,
    account_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = request
        .bearer_auth(access)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .header("originator", ORIGINATOR)
        .header("openai-beta", OPENAI_BETA)
        .header("version", CODEX_VERSION)
        .header("session_id", session_id);

    match account_id {
        Some(account_id) => request.header("chatgpt-account-id", account_id),
        None => request,
    }
}

pub struct CodexProvider {
    client: reqwest::Client,
}

impl CodexProvider {
    pub fn new() -> Self {
        // OCEAN-221: streaming SSE client with connect + idle (read) timeouts
        // and NO total request timeout, so long completions are never cut off.
        Self {
            client: crate::http::streaming_client(),
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
                        Content::Text { text } => Some(json!({"type": "input_text", "text": text})),
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
                for (i, c) in a.content.iter().enumerate() {
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
                        // Replay the original Responses `reasoning` item verbatim.
                        // The stream loop stores it (with `encrypted_content`)
                        // behind REASONING_ITEM_MARKER in thinking_signature;
                        // stateless replay needs it back or gpt-5.x degenerates
                        // into malformed tool calls. The API pairs a reasoning
                        // item with a FOLLOWING item from the same response, so a
                        // trailing reasoning item (aborted turn) must be dropped
                        // rather than 400 the whole request.
                        Content::Thinking {
                            thinking_signature: Some(sig),
                            ..
                        } if sig.starts_with(REASONING_ITEM_MARKER) => {
                            let has_follower = a.content[i + 1..].iter().any(|c| {
                                matches!(c, Content::ToolCall { .. })
                                    || matches!(c, Content::Text { text } if !text.is_empty())
                            });
                            if !has_follower {
                                continue;
                            }
                            if !text.is_empty() {
                                out.push(json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{"type": "output_text", "text": text}],
                                }));
                                text = String::new();
                            }
                            match serde_json::from_str::<Value>(&sig[REASONING_ITEM_MARKER.len()..])
                            {
                                Ok(item) => out.push(item),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "dropping unparseable stored codex reasoning item"
                                ),
                            }
                        }
                        // OCEAN-140: any OTHER thinking block (Anthropic-signed,
                        // unsigned cross-provider reasoning) cannot be re-encoded
                        // as a Responses `reasoning` item — the API rejects
                        // reconstructed reasoning. Drop it EXPLICITLY rather than
                        // via a silent `_ => {}` (kills the OCEAN-101 silent-drop
                        // class).
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

/// Pull `response.incomplete_details.reason` out of a `response.incomplete` SSE
/// frame. The Responses API nests it as
/// `{"response": {"incomplete_details": {"reason": "..."}}}`.
fn incomplete_reason(value: &Value) -> Option<String> {
    value
        .get("response")
        .and_then(|r| r.get("incomplete_details"))
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// OCEAN-176: is a `response.incomplete` reason a benign output-length cap?
///
/// When we emit `max_output_tokens`, the Responses API legitimately ends a
/// capped turn with `response.incomplete` + reason `"max_output_tokens"`. That
/// is a successful-but-truncated turn ([`StopReason::Length`]), NOT an error.
/// Every other incomplete reason (e.g. `content_filter`) stays on the error
/// path — we only whitelist the length cap.
fn incomplete_is_length_cap(reason: Option<&str>) -> bool {
    reason == Some("max_output_tokens")
}

fn stable_session_id(options: &StreamOptions) -> Option<&str> {
    options.session_id.as_deref().filter(|id| !id.is_empty())
}

fn request_session_id(options: &StreamOptions) -> String {
    stable_session_id(options)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
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
    if let Some(session_id) = stable_session_id(options) {
        body["prompt_cache_key"] = json!(session_id);
    }
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
    // Always request the encrypted reasoning payloads, not just when an explicit
    // effort is set: gpt-5.x models reason at a server-side default effort even
    // when Ocean sends no `reasoning` param, and stateless replay (store: false)
    // needs the encrypted item back on the NEXT turn to keep tool calling
    // coherent (see REASONING_ITEM_MARKER).
    body["include"] = json!(["reasoning.encrypted_content"]);
    if let Some(level) = options.reasoning {
        if let Some(effort) = reasoning_effort(level) {
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
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
        let exists_started = self
            .by_item
            .get(item_id)
            .map(|e| e.started)
            .unwrap_or(false);
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

#[derive(Default)]
struct PartialReasoning {
    block_index: usize,
    /// Human-readable summary streamed via `reasoning_summary_text.delta`
    /// frames (or pulled from the done item when nothing streamed).
    summary: String,
    /// The verbatim `reasoning` output item from `output_item.done`, minus its
    /// transient `status` field. Replayed on the next turn (see
    /// REASONING_ITEM_MARKER).
    raw_item: Option<Value>,
    started: bool,
    /// Whether ThinkingStart was already emitted for this block.
    visible: bool,
}

/// Item-id-keyed accumulator for streamed `reasoning` output items, sibling to
/// [`ToolCalls`]. The Responses API emits each reasoning item (`rs_*`) before
/// the function call(s) it drives; capturing them here (and replaying them in
/// convert_input) is what keeps gpt-5.x coherent across tool rounds — dropping
/// them is the documented trigger for malformed tool calls (harmony leakage,
/// token salad in argument strings).
#[derive(Default)]
struct ReasoningItems {
    by_item: std::collections::BTreeMap<String, PartialReasoning>,
    order: Vec<String>,
}

/// What a frame did to the accumulator: which Thinking events to yield.
struct ReasoningStep {
    started_index: Option<usize>,
    delta: Option<(usize, String)>,
    ended: Option<(usize, String)>,
}

/// A finalized reasoning item, in stream order.
struct FinalReasoning {
    block_index: usize,
    summary: String,
    raw_item: Option<Value>,
}

impl ReasoningItems {
    fn ensure(&mut self, item_id: &str, next_block_index: &mut usize) -> bool {
        let exists = self
            .by_item
            .get(item_id)
            .map(|e| e.started)
            .unwrap_or(false);
        if exists {
            return false;
        }
        let entry = self.by_item.entry(item_id.to_string()).or_default();
        entry.block_index = *next_block_index;
        *next_block_index += 1;
        entry.started = true;
        self.order.push(item_id.to_string());
        true
    }

    /// Handle `response.reasoning_summary_text.delta`.
    fn on_summary_delta(
        &mut self,
        item_id: &str,
        delta: &str,
        next_block_index: &mut usize,
    ) -> ReasoningStep {
        self.ensure(item_id, next_block_index);
        let entry = self.by_item.get_mut(item_id).unwrap();
        let started_index = (!entry.visible).then_some(entry.block_index);
        entry.visible = true;
        entry.summary.push_str(delta);
        ReasoningStep {
            started_index,
            delta: Some((entry.block_index, delta.to_string())),
            ended: None,
        }
    }

    /// Handle `response.reasoning_summary_part.added` for a SECOND (or later)
    /// summary part: separate parts so their text doesn't jam together.
    fn on_summary_part_added(
        &mut self,
        item_id: &str,
        next_block_index: &mut usize,
    ) -> ReasoningStep {
        let needs_separator = self
            .by_item
            .get(item_id)
            .map(|e| !e.summary.is_empty())
            .unwrap_or(false);
        if needs_separator {
            self.on_summary_delta(item_id, "\n\n", next_block_index)
        } else {
            ReasoningStep {
                started_index: None,
                delta: None,
                ended: None,
            }
        }
    }

    /// Handle `response.output_item.done` for a reasoning item: capture the
    /// verbatim item for replay and close the visible thinking block.
    fn on_done(&mut self, item: &Value, next_block_index: &mut usize) -> ReasoningStep {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            // Without its id the item can't be replayed; nothing to record.
            return ReasoningStep {
                started_index: None,
                delta: None,
                ended: None,
            };
        }
        self.ensure(id, next_block_index);
        let entry = self.by_item.get_mut(id).unwrap();
        let mut raw = item.clone();
        if let Some(obj) = raw.as_object_mut() {
            obj.remove("status");
        }
        entry.raw_item = Some(raw);
        // Nothing streamed → fall back to the summary text on the done item.
        if entry.summary.is_empty() {
            if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                let mut texts = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .filter(|t| !t.is_empty());
                if let Some(first) = texts.next() {
                    entry.summary.push_str(first);
                    for t in texts {
                        entry.summary.push_str("\n\n");
                        entry.summary.push_str(t);
                    }
                }
            }
        }
        // Close the visible block: if summary deltas already opened it, just
        // end it; if there's late summary text, open+end; a summary-less item
        // stays invisible (captured for replay only, no empty thinking card).
        if entry.visible {
            ReasoningStep {
                started_index: None,
                delta: None,
                ended: Some((entry.block_index, entry.summary.clone())),
            }
        } else if !entry.summary.is_empty() {
            entry.visible = true;
            ReasoningStep {
                started_index: Some(entry.block_index),
                delta: Some((entry.block_index, entry.summary.clone())),
                ended: Some((entry.block_index, entry.summary.clone())),
            }
        } else {
            ReasoningStep {
                started_index: None,
                delta: None,
                ended: None,
            }
        }
    }

    /// Finalized reasoning items in stream order.
    fn finalize(&self) -> Vec<FinalReasoning> {
        self.order
            .iter()
            .filter_map(|k| self.by_item.get(k))
            .map(|r| FinalReasoning {
                block_index: r.block_index,
                summary: r.summary.clone(),
                raw_item: r.raw_item.clone(),
            })
            .collect()
    }
}

/// Encode a captured reasoning item as the `thinking_signature` payload, but
/// only when it carries a non-empty `encrypted_content` — without the blob the
/// API rejects the replayed item under `store: false`.
fn reasoning_signature(raw_item: &Option<Value>) -> Option<String> {
    let item = raw_item.as_ref()?;
    let encrypted = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if encrypted.is_empty() {
        return None;
    }
    Some(format!("{REASONING_ITEM_MARKER}{item}"))
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
        crate::prompt_capture::capture_request_body(&model.api, &model.provider, &model.id, &body);
        let cancel = options.cancel.clone();
        // Keep the same request identity across retries. Agent sessions provide
        // a stable id across every model round; ad-hoc calls get one random id.
        let session_id = request_session_id(options);

        let resp = with_retry_observed(
            retry_config(),
            cancel.as_ref(),
            options.retry_observer.as_ref(),
            |_| {
                let client = self.client.clone();
                let url = url.clone();
                let access = access.clone();
                let account_id = account_id.clone();
                let body = body.clone();
                let session_id = session_id.clone();
                async move {
                    let req = apply_request_headers(
                        client.post(&url),
                        &access,
                        &session_id,
                        account_id.as_deref(),
                    );
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
            },
        )
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
            // Sibling accumulator for `reasoning` output items — captured for
            // verbatim replay on the next turn (see REASONING_ITEM_MARKER).
            let mut reasoning = ReasoningItems::default();
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
                    // The model narrates its reasoning through summary frames;
                    // surface them as Thinking events so the operator sees what
                    // the model is doing between tool calls.
                    "response.reasoning_summary_text.delta" => {
                        if let Ok(d) = serde_json::from_value::<ArgsDeltaEvent>(value) {
                            if d.item_id.is_empty() || d.delta.is_empty() {
                                continue;
                            }
                            let step = reasoning.on_summary_delta(&d.item_id, &d.delta, &mut next_block_index);
                            if let Some(idx) = step.started_index {
                                yield Ok(AssistantMessageEvent::ThinkingStart { content_index: idx });
                            }
                            if let Some((idx, delta)) = step.delta {
                                yield Ok(AssistantMessageEvent::ThinkingDelta { content_index: idx, delta });
                            }
                        }
                    }
                    "response.reasoning_summary_part.added" => {
                        if let Ok(d) = serde_json::from_value::<ArgsDeltaEvent>(value) {
                            if d.item_id.is_empty() {
                                continue;
                            }
                            let step = reasoning.on_summary_part_added(&d.item_id, &mut next_block_index);
                            if let Some((idx, delta)) = step.delta {
                                yield Ok(AssistantMessageEvent::ThinkingDelta { content_index: idx, delta });
                            }
                        }
                    }
                    "response.output_item.done" => {
                        let item_type = value
                            .get("item")
                            .and_then(|i| i.get("type"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if item_type == "reasoning" {
                            // Capture the verbatim reasoning item (with its
                            // encrypted_content) for next-turn replay. Dropping
                            // it is what used to degenerate gpt-5.x into
                            // malformed tool calls on later rounds.
                            if let Some(item) = value.get("item") {
                                let step = reasoning.on_done(item, &mut next_block_index);
                                if let Some(idx) = step.started_index {
                                    yield Ok(AssistantMessageEvent::ThinkingStart { content_index: idx });
                                }
                                if let Some((idx, delta)) = step.delta {
                                    yield Ok(AssistantMessageEvent::ThinkingDelta { content_index: idx, delta });
                                }
                                if let Some((idx, content)) = step.ended {
                                    yield Ok(AssistantMessageEvent::ThinkingEnd { content_index: idx, content });
                                }
                            }
                        } else if let Ok(done) = serde_json::from_value::<OutputItemEnvelope>(value) {
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
                    // OCEAN-176: `response.incomplete` is NOT inherently an error.
                    // Now that we emit `max_output_tokens`, the Responses API ends a
                    // turn that hit the output cap with `response.incomplete` +
                    // `incomplete_details.reason == "max_output_tokens"`. That is a
                    // successful-but-capped turn: treat it as `StopReason::Length` and
                    // `break` to the normal TextEnd/Done path (preserving partial text),
                    // mirroring `response.completed`. Any OTHER incomplete reason we
                    // can't cleanly map stays on the error path below.
                    "response.incomplete" => {
                        let reason = incomplete_reason(&value);
                        if incomplete_is_length_cap(reason.as_deref()) {
                            stop = StopReason::Length;
                            break;
                        }
                        let parsed = serde_json::from_value::<FailedEvent>(value).ok();
                        let (code, message) = parsed
                            .and_then(|f| f.response.error)
                            .map(|e| (e.code, e.message))
                            .unwrap_or_default();
                        let reason = reason.as_deref().unwrap_or("unknown");
                        yield Err(Error::InvalidResponse(format!(
                            "codex response incomplete ({reason}): {code} {message}"
                        )));
                        return;
                    }
                    "response.failed" => {
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

            // Assemble message content in STREAM order (reasoning items come
            // before the function calls they drive; convert_input replays the
            // content vec in order, and the API requires each reasoning item to
            // precede its paired item).
            let mut pieces: Vec<(usize, Content)> = Vec::new();
            for r in reasoning.finalize() {
                let thinking_signature = reasoning_signature(&r.raw_item);
                pieces.push((r.block_index, Content::Thinking {
                    thinking: r.summary,
                    thinking_signature,
                }));
            }
            // DSML salvage: gpt-5.6-family models (Sol) sometimes emit DeepSeek
            // DSML tool-call markup as literal text instead of a structured
            // function call — INTERLEAVED with prose, and even in MIXED turns
            // that ALSO made structured calls (TASK-53 live proof 2026-07-19: a
            // turn made 7 structured calls, then leaked one more as a trailing
            // DSML block). So salvage runs for the known leaker model whenever
            // there is text, regardless of structured-call count; the shared
            // helper dedupes recovered calls against the structured ones (the
            // model occasionally emits a call both ways) and forces ToolUse only
            // when a salvaged call actually survives.
            let finalized_tool_calls: Vec<_> = tool_calls.finalize();
            let mut salvaged_calls: Vec<(String, Value)> = Vec::new();
            let structured_pairs: Vec<(String, Value)> = finalized_tool_calls
                .iter()
                .map(|tc| {
                    let args = if tc.args.is_empty() {
                        Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&tc.args)
                            .unwrap_or_else(|_| Value::Object(Default::default()))
                    };
                    (tc.name.clone(), args)
                })
                .collect();
            if let Some(merge) = super::openai::merge_dsml_salvage(
                model_id.starts_with("gpt-5.6"),
                &text_buf,
                &structured_pairs,
            ) {
                if merge.forces_tool_use {
                    tracing::warn!(
                        count = merge.surviving.len(),
                        model = %model_id,
                        "salvaged DSML tool calls leaked as text content"
                    );
                    stop = StopReason::ToolUse;
                }
                text_buf = merge.cleaned_text;
                salvaged_calls = merge.surviving;
            }

            if let Some(i) = text_index {
                if !text_buf.is_empty() {
                    pieces.push((i, Content::Text { text: text_buf.clone() }));
                }
            }
            for tc in finalized_tool_calls {
                let args: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args).unwrap_or_else(|e| {
                        // Fail-open to {} so the turn survives, but LOUDLY: a
                        // silently emptied call looks like a model bug ("why did
                        // it call edit with no args?") when it's a parse fault.
                        tracing::warn!(
                            tool = %tc.name,
                            error = %e,
                            raw = %tc.args,
                            "codex tool call arguments failed to parse; substituting empty object"
                        );
                        Value::Object(Default::default())
                    })
                };
                yield Ok(AssistantMessageEvent::ToolCallEnd {
                    content_index: tc.block_index,
                    id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: args.clone(),
                });
                pieces.push((tc.block_index, Content::ToolCall {
                    id: tc.call_id,
                    name: tc.name,
                    arguments: args,
                }));
            }
            let salvage_base = pieces.iter().map(|(i, _)| *i + 1).max().unwrap_or(0);
            for (i, (name, arguments)) in salvaged_calls.into_iter().enumerate() {
                let id = format!("dsml-salvage-{i}");
                let block_index = salvage_base + i;
                yield Ok(AssistantMessageEvent::ToolCallEnd {
                    content_index: block_index,
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                pieces.push((block_index, Content::ToolCall { id, name, arguments }));
            }
            pieces.sort_by_key(|(i, _)| *i);
            let out_content: Vec<Content> = pieces.into_iter().map(|(_, c)| c).collect();

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
    #[test]
    fn codex_request_includes_client_version_header() {
        let built = apply_request_headers(
            reqwest::Client::new().post("https://example.test/codex/responses"),
            "oauth-token",
            "session-123",
            Some("account-456"),
        )
        .build()
        .expect("request builds");

        assert_eq!(
            built.headers().get("version").and_then(|v| v.to_str().ok()),
            Some(CODEX_VERSION),
            "new Codex models are version-gated by this header"
        );
    }

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
        let parts = out[0]["content"].as_array().expect("content array missing");

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

        assert_eq!(
            out.len(),
            1,
            "expected only the function_call_output: {out:?}"
        );
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
            supports_images: true,
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
    #[test]
    fn build_body_uses_stable_session_as_prompt_cache_key() {
        let options = StreamOptions {
            session_id: Some("session-stable-123".into()),
            ..StreamOptions::default()
        };

        let body = build_body(&codex_model(), &Context::default(), &options);

        assert_eq!(body["prompt_cache_key"], "session-stable-123");
        assert_eq!(
            request_session_id(&options),
            "session-stable-123",
            "the HTTP session header must use the same stable identity"
        );
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

    // OCEAN-176 (Codex P2 on #111): `response.incomplete` with reason
    // `max_output_tokens` is a benign output-length cap. The handler must read
    // the nested reason from the real Responses-API frame shape and classify it
    // as a length cap, so it `break`s to the normal TextEnd/Done path (preserving
    // accumulated text, finishing as StopReason::Length) instead of yielding Err.
    #[test]
    fn incomplete_max_output_tokens_is_a_length_cap() {
        // Exact shape the Responses API emits when the output cap is reached.
        let frame = json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": { "reason": "max_output_tokens" }
            }
        });

        let reason = incomplete_reason(&frame);
        assert_eq!(reason.as_deref(), Some("max_output_tokens"));
        assert!(
            incomplete_is_length_cap(reason.as_deref()),
            "max_output_tokens must classify as a length cap → StopReason::Length, \
             not an error: {frame}"
        );
    }

    // OCEAN-176: a `response.incomplete` for any OTHER reason (e.g. the safety
    // filter) is NOT a length cap and must stay on the error path — we only
    // whitelist the output-length cap.
    #[test]
    fn incomplete_other_reason_is_not_a_length_cap() {
        let frame = json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": { "reason": "content_filter" }
            }
        });
        assert_eq!(incomplete_reason(&frame).as_deref(), Some("content_filter"));
        assert!(
            !incomplete_is_length_cap(Some("content_filter")),
            "non-length incomplete reasons must keep erroring"
        );
        // A missing reason is also not a length cap.
        assert!(!incomplete_is_length_cap(None));
        assert!(incomplete_reason(&json!({"type": "response.failed"})).is_none());
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
        assert_eq!(
            finals.len(),
            2,
            "both parallel calls must survive: {finals:?}"
        );

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

        tc.on_done(&fc_item("fc_X", "call_X", "noop", "{\"k\":1}"), &mut next);

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
            u.input_tokens_details
                .expect("details present")
                .cached_tokens,
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

    // OCEAN-198: the reasoning-effort mapper covers every ThinkingLevel — Off →
    // None (omit the param), and Xhigh folds into "high" since the Responses API
    // has no distinct level above it.
    #[test]
    fn reasoning_effort_maps_every_level() {
        assert_eq!(reasoning_effort(ThinkingLevel::Off), None);
        assert_eq!(reasoning_effort(ThinkingLevel::Minimal), Some("minimal"));
        assert_eq!(reasoning_effort(ThinkingLevel::Low), Some("low"));
        assert_eq!(reasoning_effort(ThinkingLevel::Medium), Some("medium"));
        assert_eq!(reasoning_effort(ThinkingLevel::High), Some("high"));
        assert_eq!(
            reasoning_effort(ThinkingLevel::Xhigh),
            Some("high"),
            "Xhigh has no distinct Responses level; it folds into high"
        );
    }

    // OCEAN-198: incomplete_reason must return None — never panic — when the
    // nested {response.incomplete_details.reason} path is missing at any level
    // (no response, no incomplete_details, no reason, or reason not a string).
    // This guards the optional-chaining the handler relies on.
    #[test]
    fn incomplete_reason_handles_missing_nesting_gracefully() {
        // No `response` key at all.
        assert!(incomplete_reason(&json!({"type": "response.incomplete"})).is_none());
        // `response` present but no `incomplete_details`.
        assert!(incomplete_reason(&json!({"response": {}})).is_none());
        // `incomplete_details` present but no `reason`.
        assert!(incomplete_reason(&json!({"response": {"incomplete_details": {}}})).is_none());
        // `reason` present but not a string → None (as_str fails), no panic.
        assert!(
            incomplete_reason(&json!({"response": {"incomplete_details": {"reason": 42}}}))
                .is_none()
        );
        // A wholly empty value is fine too.
        assert!(incomplete_reason(&json!({})).is_none());
    }

    // OCEAN-198: a function call whose arguments stream across SEVERAL delta
    // frames (the single-call case, distinct from the interleaved-parallel test)
    // must concatenate in order into one parseable JSON object, started exactly
    // once by the first delta when no `added` frame preceded it.
    #[test]
    fn single_tool_call_args_assemble_across_sequential_deltas() {
        let mut tc = ToolCalls::default();
        let mut next: usize = 0;

        let s1 = tc.on_args_delta("fc_1", "{\"path\":", &mut next);
        assert!(
            s1.started.is_some(),
            "first delta with no prior `added` must start the call"
        );
        let s2 = tc.on_args_delta("fc_1", "\"/etc/hosts\",", &mut next);
        assert!(s2.started.is_none(), "subsequent deltas must not re-start");
        tc.on_args_delta("fc_1", "\"mode\":", &mut next);
        tc.on_args_delta("fc_1", "\"r\"}", &mut next);

        let finals = tc.finalize();
        assert_eq!(finals.len(), 1);
        let parsed: Value = serde_json::from_str(&finals[0].args).expect("assembled args parse");
        assert_eq!(parsed["path"], "/etc/hosts");
        assert_eq!(parsed["mode"], "r");
        assert_eq!(next, 1, "exactly one block index consumed");
    }

    // OCEAN-198: a tool call whose args are empty (never streamed and no done
    // payload args) finalizes with an empty arg string; the loop's downstream
    // `if args.is_empty() { {} }` then yields an empty object — no panic. And a
    // malformed buffer degrades to `{}` via the loop's unwrap_or.
    #[test]
    fn tool_call_empty_and_malformed_args_default_to_empty_object() {
        // Empty args path: an `added` with no args, no delta, a done with empty args.
        let mut tc = ToolCalls::default();
        let mut next: usize = 0;
        tc.on_added(&fc_item("fc_e", "call_e", "noop", ""), &mut next);
        tc.on_done(&fc_item("fc_e", "call_e", "noop", ""), &mut next);
        let finals = tc.finalize();
        assert_eq!(finals.len(), 1);
        assert!(
            finals[0].args.is_empty(),
            "no streamed args → empty arg buffer"
        );
        let args: Value = if finals[0].args.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&finals[0].args).unwrap_or(Value::Object(Default::default()))
        };
        assert_eq!(args, json!({}), "empty args finalize to an empty object");

        // Malformed buffer → {} via unwrap_or, no panic.
        let bad = "{\"path\": \"/tmp".to_string();
        let args2: Value = serde_json::from_str(&bad).unwrap_or(Value::Object(Default::default()));
        assert_eq!(
            args2,
            json!({}),
            "malformed args fall back to an empty object"
        );
    }

    // OCEAN-198: the completed-event usage decode must tolerate a usage object
    // with NONE of the detail sub-objects — input/output/total decode, and the
    // optional cached/reasoning details are None (→ stay zero), never an unwrap.
    #[test]
    fn responses_usage_without_details_leaves_cache_and_reasoning_zero() {
        let raw = r#"{"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}"#;
        let u: ResponsesUsage = serde_json::from_str(raw).expect("bare usage decodes");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.total_tokens, 120);
        assert!(u.input_tokens_details.is_none(), "no cached detail → None");
        assert!(
            u.output_tokens_details.is_none(),
            "no reasoning detail → None"
        );
    }

    // OCEAN-198: the completed event reports total_tokens=0 on some backends; the
    // loop falls back to input+output in that case. Prove the decode preserves the
    // zero so that fallback fires (rather than reporting a bogus 0 total).
    #[test]
    fn responses_usage_zero_total_is_preserved_for_fallback() {
        let raw = r#"{"input_tokens": 7, "output_tokens": 3, "total_tokens": 0}"#;
        let u: ResponsesUsage = serde_json::from_str(raw).expect("decodes");
        assert_eq!(
            u.total_tokens, 0,
            "zero total must survive so the loop can fall back to input+output"
        );
        let effective = if u.total_tokens > 0 {
            u.total_tokens
        } else {
            u.input_tokens + u.output_tokens
        };
        assert_eq!(
            effective, 10,
            "loop fallback must yield input+output when total is 0"
        );
    }

    // OCEAN-198: the failed/incomplete error envelope must decode even when the
    // nested `response.error` is absent — the handler does
    // `.and_then(|f| f.response.error)` and must get None (→ empty code/message),
    // not a deserialize failure, for a `response.failed` frame with no error body.
    #[test]
    fn failed_event_without_error_body_decodes_to_none() {
        let frame = json!({"type": "response.failed", "response": {}});
        let parsed: Option<FailedEvent> = serde_json::from_value(frame).ok();
        let err = parsed.and_then(|f| f.response.error);
        assert!(
            err.is_none(),
            "a failed frame with no error body must yield None, not panic"
        );
    }

    // OCEAN-198: an `output_item.done` for a NON-function item (e.g. a message or
    // reasoning item) must be ignored by the tool-call accumulator — only
    // function_call items are gated into ToolCalls in the loop. Decoding such an
    // envelope must still succeed; the loop's `r#type == "function_call"` guard
    // is what filters it. Assert the type field decodes so the guard can run.
    #[test]
    fn non_function_output_item_decodes_with_its_type() {
        let frame = json!({
            "type": "response.output_item.done",
            "item": {"id": "msg_1", "type": "message", "role": "assistant"}
        });
        let env: OutputItemEnvelope =
            serde_json::from_value(frame).expect("non-function item envelope decodes");
        assert_eq!(
            env.item.r#type, "message",
            "the loop's function_call guard reads this type"
        );
        assert_ne!(env.item.r#type, "function_call");
    }

    // Stateless replay (store:false) needs the encrypted reasoning payloads on
    // EVERY request — gpt-5.x reasons at a server default even when Ocean sends
    // no explicit effort, and dropping the items across tool rounds is the
    // documented trigger for malformed tool calls (harmony leakage, token salad
    // in argument strings — observed live from gpt-5.6).
    #[test]
    fn build_body_always_requests_encrypted_reasoning() {
        let body = build_body(
            &codex_model(),
            &Context::default(),
            &StreamOptions::default(),
        );
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "include must be requested even with no reasoning option set: {body}"
        );
        assert!(
            body.get("reasoning").is_none(),
            "no explicit effort → no reasoning param (server default): {body}"
        );
    }

    // A thinking block whose signature carries REASONING_ITEM_MARKER holds the
    // verbatim Responses `reasoning` output item. convert_input must replay it
    // as-is, positioned BEFORE the function call it drives.
    #[test]
    fn codex_reasoning_item_replays_verbatim_before_function_call() {
        let raw_item = json!({
            "type": "reasoning",
            "id": "rs_123",
            "summary": [{"type": "summary_text", "text": "planning the edit"}],
            "encrypted_content": "opaque-blob",
        });
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "planning the edit".into(),
                    thinking_signature: Some(format!("{REASONING_ITEM_MARKER}{raw_item}")),
                },
                Content::ToolCall {
                    id: "call_1".into(),
                    name: "edit".into(),
                    arguments: json!({"path": "/tmp/x"}),
                },
            ],
            api: "responses".into(),
            provider: "codex".into(),
            model: "gpt-5.6".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: now_ms(),
        })];

        let out = convert_input(&messages);
        assert_eq!(out.len(), 2, "reasoning item + function_call: {out:?}");
        assert_eq!(out[0], raw_item, "reasoning item must replay verbatim");
        assert_eq!(out[1]["type"], "function_call");
        assert_eq!(out[1]["call_id"], "call_1");
    }

    // The API pairs a reasoning item with a FOLLOWING item from the same
    // response; a trailing reasoning item (e.g. an aborted turn persisted with
    // only the thinking block) must be dropped, not 400 the whole request.
    #[test]
    fn trailing_reasoning_item_is_not_replayed() {
        let raw_item = json!({
            "type": "reasoning",
            "id": "rs_9",
            "encrypted_content": "blob",
        });
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![Content::Thinking {
                thinking: String::new(),
                thinking_signature: Some(format!("{REASONING_ITEM_MARKER}{raw_item}")),
            }],
            api: "responses".into(),
            provider: "codex".into(),
            model: "gpt-5.6".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        })];

        let out = convert_input(&messages);
        assert!(
            out.is_empty(),
            "a reasoning item with no follower must be dropped: {out:?}"
        );
    }

    // The reasoning accumulator: summary deltas stream into a visible thinking
    // block; done captures the verbatim item (minus transient status) and the
    // signature encoder marks it for replay only when encrypted_content rode in.
    #[test]
    fn reasoning_summary_streams_and_done_captures_item_for_replay() {
        let mut r = ReasoningItems::default();
        let mut next: usize = 0;

        let s1 = r.on_summary_delta("rs_1", "planning ", &mut next);
        assert_eq!(s1.started_index, Some(0), "first delta opens the block");
        let s2 = r.on_summary_delta("rs_1", "the edit", &mut next);
        assert!(s2.started_index.is_none(), "no re-start on later deltas");

        let done_item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": "planning the edit"}],
            "encrypted_content": "opaque-blob",
        });
        let s3 = r.on_done(&done_item, &mut next);
        assert_eq!(
            s3.ended,
            Some((0, "planning the edit".to_string())),
            "done closes the visible block with the assembled summary"
        );

        let finals = r.finalize();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].block_index, 0);
        let sig =
            reasoning_signature(&finals[0].raw_item).expect("encrypted item gets a signature");
        assert!(sig.starts_with(REASONING_ITEM_MARKER));
        let replayed: Value =
            serde_json::from_str(&sig[REASONING_ITEM_MARKER.len()..]).expect("payload parses");
        assert_eq!(replayed["id"], "rs_1");
        assert_eq!(replayed["encrypted_content"], "opaque-blob");
        assert!(
            replayed.get("status").is_none(),
            "transient status must not be replayed"
        );
        assert_eq!(next, 1, "one block index consumed");
    }

    // Without encrypted_content there is nothing the API will accept back under
    // store:false — the item must NOT be marked for replay (signature None), and
    // a summary-less item must stay invisible (no empty thinking card).
    #[test]
    fn reasoning_item_without_encrypted_content_gets_no_signature() {
        let mut r = ReasoningItems::default();
        let mut next: usize = 0;

        let step = r.on_done(
            &json!({"type": "reasoning", "id": "rs_2", "summary": []}),
            &mut next,
        );
        assert!(
            step.started_index.is_none(),
            "no summary → no visible block"
        );
        assert!(step.ended.is_none());

        let finals = r.finalize();
        assert_eq!(finals.len(), 1, "still finalized for ordering");
        assert!(
            reasoning_signature(&finals[0].raw_item).is_none(),
            "no encrypted_content → no replay signature"
        );
        // And an id-less item is unreplayable: nothing recorded at all.
        let mut r2 = ReasoningItems::default();
        r2.on_done(&json!({"type": "reasoning"}), &mut next);
        assert!(r2.finalize().is_empty(), "id-less items can't be replayed");
    }
}
