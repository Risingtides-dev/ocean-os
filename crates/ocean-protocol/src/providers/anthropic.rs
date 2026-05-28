//! Anthropic Messages API provider — streaming via Server-Sent Events.
//!
//! Mirrors `packages/ai/src/providers/anthropic.ts` from the upstream TS.
//! Emits `text_delta`, `thinking_delta`, and `toolcall_delta` events as data
//! arrives. Tool-call input JSON is assembled across `input_json_delta`
//! events and parsed at `content_block_stop` time.

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
    StreamOptions, ThinkingLevel, Usage,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartPayload },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: BlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: BlockDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaPayload,
        #[serde(default)]
        usage: Option<UsageDelta>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: ErrorBody },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
struct MessageStartPayload {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<UsageDelta>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum BlockStart {
    #[serde(rename = "text")]
    Text {},
    #[serde(rename = "thinking")]
    Thinking {},
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum BlockDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Default)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct UsageDelta {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize, Debug)]
struct ErrorBody {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(4)
                .build()
                .expect("reqwest client"),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::User { content, .. } => {
                let blocks = content.iter().map(content_to_block).collect::<Vec<_>>();
                out.push(json!({"role": "user", "content": blocks}));
            }
            Message::Assistant(a) => {
                let blocks = a.content.iter().map(content_to_block).collect::<Vec<_>>();
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult(tr) => {
                let body: Vec<Value> = tr
                    .content
                    .iter()
                    .map(|c| match c {
                        Content::Text { text } => json!({"type": "text", "text": text}),
                        Content::Image { data, mime_type } => json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mime_type, "data": data}
                        }),
                        _ => json!({"type": "text", "text": ""}),
                    })
                    .collect();
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tr.tool_call_id,
                        "content": body,
                        "is_error": tr.is_error,
                    }]
                }));
            }
        }
    }
    out
}

fn content_to_block(c: &Content) -> Value {
    match c {
        Content::Text { text } => json!({"type": "text", "text": text}),
        Content::Thinking {
            thinking,
            thinking_signature,
        } => {
            let mut v = json!({"type": "thinking", "thinking": thinking});
            if let Some(sig) = thinking_signature {
                v["signature"] = json!(sig);
            }
            v
        }
        Content::Image { data, mime_type } => json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data}
        }),
        Content::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": arguments,
        }),
    }
}

fn thinking_budget(level: ThinkingLevel) -> Option<u32> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(1024),
        ThinkingLevel::Low => Some(2048),
        ThinkingLevel::Medium => Some(8192),
        ThinkingLevel::High => Some(16384),
        ThinkingLevel::Xhigh => Some(24576),
    }
}

fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "model": model.id,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "messages": convert_messages(&context.messages),
        "stream": true,
    });
    if let Some(sp) = &context.system_prompt {
        body["system"] = json!(sp);
    }
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(level) = options.reasoning {
        if let Some(budget) = thinking_budget(level) {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
    }
    if !context.tools.is_empty() {
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

#[derive(Default)]
struct BlockState {
    kind: BlockKind,
    text_buf: String,
    json_buf: String,
    tool_id: String,
    tool_name: String,
    signature: Option<String>,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum BlockKind {
    #[default]
    Unknown,
    Text,
    Thinking,
    ToolUse,
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| Error::MissingApiKey("anthropic".into()))?;
        let base_url = options
            .base_url
            .clone()
            .unwrap_or_else(|| model.base_url.clone());
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let body = build_body(model, context, options);
        let cancel = options.cancel.clone();
        let extra_headers: BTreeMap<String, String> = options.headers.clone();

        let resp = with_retry(&RetryConfig::default(), cancel.as_ref(), |_attempt| {
            let client = self.client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let body = body.clone();
            let extra_headers = extra_headers.clone();
            async move {
                let mut req = client
                    .post(&url)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
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

            let byte_stream = resp.bytes_stream();
            let mut sse = byte_stream.eventsource();

            let mut blocks: std::collections::HashMap<usize, BlockState> = std::collections::HashMap::new();
            let mut order: Vec<usize> = Vec::new();
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
                if ev.data.is_empty() {
                    continue;
                }
                let parsed: SseEvent = match serde_json::from_str(&ev.data) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                match parsed {
                    SseEvent::Ping | SseEvent::Other => {}
                    SseEvent::MessageStart { message } => {
                        if let Some(m) = message.model { response_model = Some(m); }
                        if let Some(u) = message.usage {
                            usage.input += u.input_tokens;
                            usage.cache_read += u.cache_read_input_tokens;
                            usage.cache_write += u.cache_creation_input_tokens;
                        }
                    }
                    SseEvent::ContentBlockStart { index, content_block } => {
                        let st = blocks.entry(index).or_default();
                        order.push(index);
                        match content_block {
                            BlockStart::Text {} => {
                                st.kind = BlockKind::Text;
                                yield Ok(AssistantMessageEvent::TextStart { content_index: index });
                            }
                            BlockStart::Thinking {} => {
                                st.kind = BlockKind::Thinking;
                                yield Ok(AssistantMessageEvent::ThinkingStart { content_index: index });
                            }
                            BlockStart::ToolUse { id, name } => {
                                st.kind = BlockKind::ToolUse;
                                st.tool_id = id.clone();
                                st.tool_name = name.clone();
                                yield Ok(AssistantMessageEvent::ToolCallStart {
                                    content_index: index,
                                    id,
                                    name,
                                });
                            }
                            BlockStart::Other => {}
                        }
                    }
                    SseEvent::ContentBlockDelta { index, delta } => {
                        let st = blocks.entry(index).or_default();
                        match delta {
                            BlockDelta::TextDelta { text } => {
                                st.text_buf.push_str(&text);
                                yield Ok(AssistantMessageEvent::TextDelta { content_index: index, delta: text });
                            }
                            BlockDelta::ThinkingDelta { thinking } => {
                                st.text_buf.push_str(&thinking);
                                yield Ok(AssistantMessageEvent::ThinkingDelta { content_index: index, delta: thinking });
                            }
                            BlockDelta::InputJsonDelta { partial_json } => {
                                st.json_buf.push_str(&partial_json);
                                yield Ok(AssistantMessageEvent::ToolCallDelta { content_index: index, delta: partial_json });
                            }
                            BlockDelta::SignatureDelta { signature } => {
                                st.signature = Some(signature);
                            }
                            BlockDelta::Other => {}
                        }
                    }
                    SseEvent::ContentBlockStop { index } => {
                        if let Some(st) = blocks.get(&index) {
                            match st.kind {
                                BlockKind::Text => {
                                    yield Ok(AssistantMessageEvent::TextEnd { content_index: index, content: st.text_buf.clone() });
                                }
                                BlockKind::Thinking => {
                                    yield Ok(AssistantMessageEvent::ThinkingEnd { content_index: index, content: st.text_buf.clone() });
                                }
                                BlockKind::ToolUse => {
                                    let args: Value = if st.json_buf.is_empty() {
                                        Value::Object(Default::default())
                                    } else {
                                        serde_json::from_str(&st.json_buf).unwrap_or(Value::Object(Default::default()))
                                    };
                                    yield Ok(AssistantMessageEvent::ToolCallEnd {
                                        content_index: index,
                                        id: st.tool_id.clone(),
                                        name: st.tool_name.clone(),
                                        arguments: args,
                                    });
                                }
                                BlockKind::Unknown => {}
                            }
                        }
                    }
                    SseEvent::MessageDelta { delta, usage: maybe_usage } => {
                        if let Some(u) = maybe_usage {
                            usage.output += u.output_tokens;
                        }
                        if let Some(reason) = delta.stop_reason {
                            stop = match reason.as_str() {
                                "tool_use" => StopReason::ToolUse,
                                "max_tokens" => StopReason::Length,
                                "end_turn" | "stop_sequence" => StopReason::Stop,
                                _ => StopReason::Stop,
                            };
                        }
                    }
                    SseEvent::MessageStop => {}
                    SseEvent::Error { error } => {
                        let err_msg = format!("{}: {}", error.kind, error.message);
                        let am = AssistantMessage {
                            content: vec![],
                            api: api.clone(),
                            provider: provider.clone(),
                            model: response_model.clone().unwrap_or_else(|| model_id.clone()),
                            usage: usage.clone(),
                            stop_reason: StopReason::Error,
                            error_message: Some(err_msg),
                            timestamp: now_ms(),
                        };
                        yield Ok(AssistantMessageEvent::Error { reason: StopReason::Error, error: am });
                        return;
                    }
                }
            }

            usage.total_tokens = usage.input + usage.output;
            let mut out_content = Vec::with_capacity(order.len());
            for idx in &order {
                if let Some(st) = blocks.get(idx) {
                    match st.kind {
                        BlockKind::Text => out_content.push(Content::Text { text: st.text_buf.clone() }),
                        BlockKind::Thinking => out_content.push(Content::Thinking {
                            thinking: st.text_buf.clone(),
                            thinking_signature: st.signature.clone(),
                        }),
                        BlockKind::ToolUse => {
                            let args: Value = if st.json_buf.is_empty() {
                                Value::Object(Default::default())
                            } else {
                                serde_json::from_str(&st.json_buf).unwrap_or(Value::Object(Default::default()))
                            };
                            out_content.push(Content::ToolCall {
                                id: st.tool_id.clone(),
                                name: st.tool_name.clone(),
                                arguments: args,
                            });
                        }
                        BlockKind::Unknown => {}
                    }
                }
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
