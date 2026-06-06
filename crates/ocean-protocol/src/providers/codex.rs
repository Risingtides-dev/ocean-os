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
    let mut body = json!({
        "model": model.id,
        "input": convert_input(&context.messages),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
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
    body
}

// --- SSE payload shapes (only the fields we consume) ---

#[derive(Deserialize)]
struct OutputItemDone {
    item: OutputItem,
}

#[derive(Deserialize)]
struct OutputItem {
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
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
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
    id: String,
    name: String,
    args: String,
    block_index: usize,
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
            // Tool calls keyed by the Responses `item_id` so streamed argument
            // deltas land in the right block; finalized on output_item.done.
            let mut tool_calls: std::collections::BTreeMap<String, PartialToolCall> = Default::default();
            let mut tool_order: Vec<String> = Vec::new();
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
                    "response.function_call_arguments.delta" => {
                        if let Ok(d) = serde_json::from_value::<ArgsDeltaEvent>(value) {
                            if d.item_id.is_empty() || d.delta.is_empty() {
                                continue;
                            }
                            let is_new = !tool_calls.contains_key(&d.item_id);
                            let entry = tool_calls.entry(d.item_id.clone()).or_default();
                            if is_new {
                                entry.block_index = next_block_index;
                                next_block_index += 1;
                                tool_order.push(d.item_id.clone());
                                yield Ok(AssistantMessageEvent::ToolCallStart {
                                    content_index: entry.block_index,
                                    id: entry.id.clone(),
                                    name: entry.name.clone(),
                                });
                            }
                            entry.args.push_str(&d.delta);
                            let block_index = entry.block_index;
                            yield Ok(AssistantMessageEvent::ToolCallDelta {
                                content_index: block_index,
                                delta: d.delta,
                            });
                        }
                    }
                    "response.output_item.done" => {
                        if let Ok(done) = serde_json::from_value::<OutputItemDone>(value) {
                            let item = done.item;
                            if item.r#type == "function_call" {
                                stop = StopReason::ToolUse;
                                // Match the partial we accumulated (by call_id, or
                                // create one if the args never streamed).
                                let key = tool_calls
                                    .iter()
                                    .find(|(_, v)| v.id == item.call_id || v.id.is_empty())
                                    .map(|(k, _)| k.clone());
                                match key {
                                    Some(k) => {
                                        let entry = tool_calls.get_mut(&k).unwrap();
                                        entry.id = item.call_id.clone();
                                        entry.name = item.name.clone();
                                        if entry.args.is_empty() {
                                            entry.args = item.arguments.clone();
                                        }
                                    }
                                    None => {
                                        let block_index = next_block_index;
                                        next_block_index += 1;
                                        let key = item.call_id.clone();
                                        tool_order.push(key.clone());
                                        yield Ok(AssistantMessageEvent::ToolCallStart {
                                            content_index: block_index,
                                            id: item.call_id.clone(),
                                            name: item.name.clone(),
                                        });
                                        tool_calls.insert(key, PartialToolCall {
                                            id: item.call_id,
                                            name: item.name,
                                            args: item.arguments,
                                            block_index,
                                        });
                                    }
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
            for key in &tool_order {
                let Some(tc) = tool_calls.get(key) else { continue };
                let args: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args).unwrap_or(Value::Object(Default::default()))
                };
                yield Ok(AssistantMessageEvent::ToolCallEnd {
                    content_index: tc.block_index,
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: args.clone(),
                });
                out_content.push(Content::ToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
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
}
