//! OpenAI Chat Completions provider — streaming via Server-Sent Events.
//!
//! Handles arbitrary OpenAI-compatible endpoints by honoring
//! `StreamOptions::base_url`. Works with OpenAI, OpenRouter, Together, Groq,
//! Cerebras, DeepSeek, Fireworks, etc., whose APIs implement the same wire
//! format.

use std::collections::BTreeMap;

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
    StreamOptions, Usage,
};

#[derive(Deserialize, Debug)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
    #[serde(default)]
    model: Option<String>,
    /// Several OpenAI-compatible gateways (OpenRouter, Together, etc.) deliver a
    /// mid-stream failure as a data frame carrying an `error` object instead of
    /// an HTTP status — `{"error": {"message": "...", ...}}`. Without capturing
    /// it the chunk parses with empty `choices` and the turn ends as a clean but
    /// empty success, hiding the real failure (OCEAN-101).
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize, Debug, Default)]
struct StreamError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: Option<Value>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

impl StreamError {
    fn describe(&self) -> String {
        let mut s = String::new();
        if let Some(k) = &self.kind {
            s.push_str(k);
            s.push_str(": ");
        }
        if self.message.is_empty() {
            s.push_str("provider returned an in-stream error");
        } else {
            s.push_str(&self.message);
        }
        if let Some(c) = &self.code {
            s.push_str(&format!(" (code: {c})"));
        }
        s
    }
}

#[derive(Deserialize, Debug)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek extended-reasoning models (deepseek-reasoner, deepseek-v4-pro)
    /// stream their chain-of-thought through `reasoning_content` alongside the
    /// final `content`. OpenAI-compatible "reasoning" models (o1, o3-style)
    /// alias the same channel as `reasoning`. We accept both spellings.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    /// OpenAI emits a structured refusal (safety decline) through its own
    /// `refusal` channel with `content` left null. Previously this was ignored,
    /// so a refused turn produced an empty assistant message with no explanation
    /// (OCEAN-101). We surface it as visible text so the user sees the decline.
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize, Debug)]
struct ToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Debug, Default)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

pub struct OpenAiProvider {
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn convert_messages(system_prompt: Option<&str>, messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(sp) = system_prompt {
        out.push(json!({"role": "system", "content": sp}));
    }
    for m in messages {
        match m {
            Message::User { content, .. } => {
                // If any image is present, emit the content-array form so vision
                // turns survive. Otherwise keep the simple string form.
                let has_image = content
                    .iter()
                    .any(|c| matches!(c, Content::Image { .. }));
                if has_image {
                    let parts: Vec<Value> = content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => {
                                Some(json!({"type": "text", "text": text}))
                            }
                            Content::Image { data, mime_type } => Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", mime_type, data)
                                }
                            })),
                            _ => None,
                        })
                        .collect();
                    out.push(json!({"role": "user", "content": parts}));
                } else {
                    let text = content
                        .iter()
                        .filter_map(|c| c.as_text().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                        .join("");
                    out.push(json!({"role": "user", "content": text}));
                }
            }
            Message::Assistant(a) => {
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for c in &a.content {
                    match c {
                        Content::Text { text: t } => text.push_str(t),
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": arguments.to_string(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                let mut msg = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "content": text,
                }));
            }
        }
    }
    out
}

fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "model": model.id,
        "messages": convert_messages(context.system_prompt.as_deref(), &context.messages),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = options.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if !context.tools.is_empty() {
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| Error::MissingApiKey("openai".into()))?;
        let base_url = options
            .base_url
            .clone()
            .unwrap_or_else(|| model.base_url.clone());
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let body = build_body(model, context, options);
        let cancel = options.cancel.clone();
        let extra_headers: BTreeMap<String, String> = options.headers.clone();

        let resp = with_retry(&RetryConfig::default(), cancel.as_ref(), |_| {
            let client = self.client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let body = body.clone();
            let extra_headers = extra_headers.clone();
            async move {
                let mut req = client
                    .post(&url)
                    .bearer_auth(&api_key)
                    .header("accept", "text/event-stream")
                    .header("content-type", "application/json");
                for (k, v) in extra_headers {
                    req = req.header(k, v);
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

            // Block layout: thinking → text → tool calls (in upstream tc.index order).
            // We assign content_indexes in arrival order via `next_block_index` and
            // remember the index for each open block so deltas can re-use it.
            let mut next_block_index: usize = 0;
            let mut thinking_buf = String::new();
            let mut thinking_index: Option<usize> = None;
            let mut text_buf = String::new();
            let mut text_index: Option<usize> = None;
            let mut tool_calls: std::collections::BTreeMap<usize, PartialToolCall> = Default::default();
            let mut tool_block_indexes: std::collections::BTreeMap<usize, usize> = Default::default();
            let mut stop = StopReason::Stop;
            let mut usage = Usage::default();
            let mut response_model: Option<String> = None;

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
                if ev.data == "[DONE]" {
                    break;
                }
                if ev.data.is_empty() {
                    continue;
                }
                let chunk: Chunk = match serde_json::from_str(&ev.data) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!(error = %e, "skipping unparseable OpenAI SSE frame");
                        continue;
                    }
                };
                // Mid-stream error frame (OpenRouter/Together/etc.). Surface it
                // rather than ending the turn as a clean empty success.
                if let Some(err) = chunk.error {
                    let msg = err.describe();
                    tracing::warn!(error = %msg, "OpenAI-compatible stream returned an in-stream error");
                    let am = AssistantMessage {
                        content: vec![],
                        api: api.clone(),
                        provider: provider.clone(),
                        model: response_model.clone().unwrap_or_else(|| model_id.clone()),
                        usage: usage.clone(),
                        stop_reason: StopReason::Error,
                        error_message: Some(msg),
                        timestamp: now_ms(),
                    };
                    yield Ok(AssistantMessageEvent::Error { reason: StopReason::Error, error: am });
                    return;
                }
                if let Some(m) = chunk.model { response_model = Some(m); }
                if let Some(u) = chunk.usage {
                    usage.input = u.prompt_tokens;
                    usage.output = u.completion_tokens;
                    usage.total_tokens = u.total_tokens;
                }
                for choice in chunk.choices {
                    if let Some(reason) = choice.finish_reason {
                        stop = match reason.as_str() {
                            "tool_calls" => StopReason::ToolUse,
                            "length" => StopReason::Length,
                            _ => StopReason::Stop,
                        };
                    }
                    if let Some(delta) = choice.delta {
                        // Reasoning / chain-of-thought (DeepSeek reasoner & v4-pro,
                        // OpenAI o-series). Streams as its own block ahead of text.
                        if let Some(r) = delta.reasoning_content {
                            if !r.is_empty() {
                                let idx = match thinking_index {
                                    Some(i) => i,
                                    None => {
                                        let i = next_block_index;
                                        next_block_index += 1;
                                        thinking_index = Some(i);
                                        yield Ok(AssistantMessageEvent::ThinkingStart { content_index: i });
                                        i
                                    }
                                };
                                thinking_buf.push_str(&r);
                                yield Ok(AssistantMessageEvent::ThinkingDelta {
                                    content_index: idx,
                                    delta: r,
                                });
                            }
                        }
                        // Structured refusal (safety decline). `content` is null
                        // in this case, so without surfacing `refusal` the turn
                        // would end empty with no explanation. Treat it as text so
                        // the user actually sees why the model declined.
                        if let Some(r) = delta.refusal {
                            if !r.is_empty() {
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
                                text_buf.push_str(&r);
                                yield Ok(AssistantMessageEvent::TextDelta {
                                    content_index: idx,
                                    delta: r,
                                });
                            }
                        }
                        if let Some(c) = delta.content {
                            if !c.is_empty() {
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
                                text_buf.push_str(&c);
                                yield Ok(AssistantMessageEvent::TextDelta {
                                    content_index: idx,
                                    delta: c,
                                });
                            }
                        }
                        for tc in delta.tool_calls {
                            let entry = tool_calls.entry(tc.index).or_default();
                            if let Some(id) = tc.id { entry.id = id; }
                            if let Some(f) = tc.function {
                                if let Some(n) = f.name { entry.name = n; }
                                if let Some(a) = f.arguments {
                                    entry.args.push_str(&a);
                                    let block_index = match tool_block_indexes.get(&tc.index) {
                                        Some(i) => *i,
                                        None => {
                                            let i = next_block_index;
                                            next_block_index += 1;
                                            tool_block_indexes.insert(tc.index, i);
                                            yield Ok(AssistantMessageEvent::ToolCallStart {
                                                content_index: i,
                                                id: entry.id.clone(),
                                                name: entry.name.clone(),
                                            });
                                            i
                                        }
                                    };
                                    yield Ok(AssistantMessageEvent::ToolCallDelta {
                                        content_index: block_index,
                                        delta: a,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            if let Some(i) = thinking_index {
                yield Ok(AssistantMessageEvent::ThinkingEnd {
                    content_index: i,
                    content: thinking_buf.clone(),
                });
            }
            if let Some(i) = text_index {
                yield Ok(AssistantMessageEvent::TextEnd {
                    content_index: i,
                    content: text_buf.clone(),
                });
            }

            // Fallback: some reasoning-capable OAI-compat models (notably
            // DeepSeek v4-pro) stream their entire conversational reply
            // through `reasoning_content` and never populate `content`.
            // If we got reasoning but no text AND no tool calls, surface
            // the reasoning as the assistant's text so the user actually
            // sees an answer.
            //
            // We deliberately skip the promotion when tool calls are
            // present: in that case the reasoning is the model's plan for
            // the tool call, not a user-facing reply, and the real text
            // answer will come on the next agent-loop turn after the tool
            // results are appended. Promoting prematurely would dump the
            // private plan into the user's transcript and then duplicate
            // it again when the real answer arrives.
            let has_tool_calls = !tool_calls.is_empty();
            if text_buf.is_empty() && !thinking_buf.is_empty() && !has_tool_calls {
                let promoted_index = next_block_index;
                next_block_index += 1;
                yield Ok(AssistantMessageEvent::TextStart { content_index: promoted_index });
                yield Ok(AssistantMessageEvent::TextDelta {
                    content_index: promoted_index,
                    delta: thinking_buf.clone(),
                });
                yield Ok(AssistantMessageEvent::TextEnd {
                    content_index: promoted_index,
                    content: thinking_buf.clone(),
                });
                text_buf = thinking_buf.clone();
            }

            let mut out_content: Vec<Content> = Vec::new();
            if !thinking_buf.is_empty() {
                out_content.push(Content::Thinking {
                    thinking: thinking_buf.clone(),
                    thinking_signature: None,
                });
            }
            if !text_buf.is_empty() {
                out_content.push(Content::Text { text: text_buf.clone() });
            }
            for (i, tc) in tool_calls {
                let args: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args).unwrap_or(Value::Object(Default::default()))
                };
                let block_index = tool_block_indexes
                    .get(&i)
                    .copied()
                    .unwrap_or(next_block_index);
                yield Ok(AssistantMessageEvent::ToolCallEnd {
                    content_index: block_index,
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: args.clone(),
                });
                out_content.push(Content::ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: args,
                });
            }

            let message = AssistantMessage {
                content: out_content,
                api,
                provider,
                model: response_model.unwrap_or(model_id),
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
    use crate::types::now_ms;

    // OCEAN-99: vision parity. A user message carrying a Content::Image must
    // serialize as the OpenAI image_url content part (data-URL), not be dropped
    // into a text-only string.
    #[test]
    fn user_image_is_encoded_as_image_url_part() {
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

        let out = convert_messages(None, &messages);
        assert_eq!(out.len(), 1);
        let content = &out[0]["content"];
        // Must be the array form, not a bare string.
        assert!(content.is_array(), "expected content array, got {content}");
        let parts = content.as_array().unwrap();

        let has_text = parts
            .iter()
            .any(|p| p["type"] == "text" && p["text"] == "describe this");
        assert!(has_text, "text part missing: {content}");

        let image = parts
            .iter()
            .find(|p| p["type"] == "image_url")
            .expect("image_url part missing — image was dropped");
        assert_eq!(
            image["image_url"]["url"],
            "data:image/png;base64,AAECAwQ=",
            "image_url data-URL malformed"
        );
    }

    // Text-only user messages keep the simple string form (no regression).
    #[test]
    fn text_only_user_stays_string() {
        let messages = vec![Message::user_text("hello")];
        let out = convert_messages(None, &messages);
        assert_eq!(out[0]["content"], "hello");
    }

    // OCEAN-101: a mid-stream error frame must decode into `Chunk.error` so the
    // loop can surface it. Previously this frame parsed with empty `choices` and
    // the turn ended as a clean empty success, hiding the failure.
    #[test]
    fn in_stream_error_frame_is_captured() {
        let raw = r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":429}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("chunk parses");
        let err = chunk.error.expect("error object must be captured, not dropped");
        let desc = err.describe();
        assert!(desc.contains("rate limited"), "message lost: {desc}");
        assert!(desc.contains("rate_limit_error"), "type lost: {desc}");
        assert!(desc.contains("429"), "code lost: {desc}");
    }

    // OCEAN-101: a structured refusal must decode into `ChunkDelta.refusal` so it
    // can be surfaced as visible text. Previously `refusal` was ignored and the
    // refused turn produced an empty message.
    #[test]
    fn refusal_delta_is_captured() {
        let raw = r#"{"choices":[{"delta":{"refusal":"I can't help with that.","content":null}}]}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("chunk parses");
        let delta = chunk.choices[0].delta.as_ref().expect("delta present");
        assert_eq!(
            delta.refusal.as_deref(),
            Some("I can't help with that."),
            "refusal text was dropped"
        );
    }
}
