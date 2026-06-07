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

/// Total token footprint for an Anthropic turn (OCEAN-188).
///
/// Anthropic reports cache tokens SEPARATELY from `input_tokens`:
/// `input_tokens` is the non-cached input only, while
/// `cache_creation_input_tokens` (cache_write) and `cache_read_input_tokens`
/// (cache_read) are reported alongside it and are NOT included in it. The full
/// footprint that hit the model is therefore input + output + cache_write +
/// cache_read. This keeps Anthropic consistent with the other providers, whose
/// reported totals already fold cache-read tokens into the prompt/input figure.
fn total_tokens(u: &crate::types::Usage) -> u64 {
    u.input + u.output + u.cache_write + u.cache_read
}

/// True for the Anthropic Messages family. Prompt caching via `cache_control`
/// breakpoints is an Anthropic-only feature, so we gate emission on the model's
/// API string. Any non-Anthropic backend routed through this provider (none
/// today, but the gate keeps the contract explicit and test-checkable) gets a
/// body with no `cache_control` markers.
fn is_anthropic_family(model: &Model) -> bool {
    model.api == "anthropic-messages" || model.provider == "anthropic"
}

const EPHEMERAL: &str = "ephemeral";

fn cache_control() -> Value {
    json!({"type": EPHEMERAL})
}

fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let cache = is_anthropic_family(model);

    let mut body = json!({
        "model": model.id,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "messages": convert_messages(&context.messages),
        "stream": true,
    });

    // OCEAN-159: emit Anthropic prompt-caching breakpoints on the stable prefix.
    //
    // Anthropic reads the cache prefix in a fixed order — tools, then system,
    // then messages — and a `cache_control` marker caches everything up to and
    // including the block it sits on. The API allows at most 4 breakpoints. We
    // place at most 3, on the *last* block of each stable region so the whole
    // region caches:
    //   1. last tool definition  -> caches every tool schema
    //   2. last system block     -> caches the system prompt
    //   3. a rolling breakpoint near the end of history -> caches the stable
    //      conversation prefix while leaving the newest turn(s) fresh
    // This turns each agent-loop turn from "re-bill the full prompt" into
    // "re-bill only the new tail", and is what makes usage.cache_read non-zero
    // on Anthropic at all.

    if let Some(sp) = &context.system_prompt {
        if cache {
            // Convert the system prompt to a single text block carrying the
            // cache breakpoint (a bare string cannot hold cache_control).
            body["system"] = json!([{
                "type": "text",
                "text": sp,
                "cache_control": cache_control(),
            }]);
        } else {
            body["system"] = json!(sp);
        }
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
        let last = context.tools.len() - 1;
        let tools: Vec<Value> = context
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                });
                // Breakpoint on the last tool caches the whole tool-definition
                // prefix (tools are read before system/messages).
                if cache && i == last {
                    v["cache_control"] = cache_control();
                }
                v
            })
            .collect();
        body["tools"] = json!(tools);
    }

    // Rolling message breakpoint: mark the last content block of the last
    // *stable* message so the conversation prefix caches and only the freshest
    // turn is re-billed. We treat everything except the final message as the
    // stable prefix; with >=2 messages that means a breakpoint on the
    // second-to-last message. With a single message there is no stable history
    // yet, so we skip it (the system + tools breakpoints still cache the prefix).
    if cache {
        let n = context.messages.len();
        if n >= 2 {
            if let Some(arr) = body["messages"].as_array_mut() {
                let target = n - 2;
                if let Some(content) = arr[target].get_mut("content").and_then(Value::as_array_mut) {
                    if let Some(last_block) = content.last_mut() {
                        if let Some(obj) = last_block.as_object_mut() {
                            obj.insert("cache_control".into(), cache_control());
                        }
                    }
                }
            }
        }
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
                    Err(e) => {
                        // An SSE data frame we couldn't parse. Skip to stay
                        // resilient, but log it (OCEAN-101) so a malformed frame
                        // isn't an invisible black hole.
                        tracing::debug!(error = %e, "skipping unparseable Anthropic SSE frame");
                        continue;
                    }
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
                            BlockStart::Other => {
                                // Unmapped block kind (redacted_thinking,
                                // server_tool_use, web_search_result, image
                                // output, …). We have no Content variant for it,
                                // but log so the drop is visible instead of
                                // silent (OCEAN-101).
                                tracing::warn!(
                                    content_index = index,
                                    "Anthropic content_block_start of an unmapped kind; its content will be dropped"
                                );
                            }
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
                            BlockDelta::Other => {
                                tracing::warn!(
                                    content_index = index,
                                    "Anthropic content_block_delta of an unmapped kind; dropping its delta"
                                );
                            }
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

            // OCEAN-188: Anthropic reports `cache_creation_input_tokens` (->
            // cache_write) and `cache_read_input_tokens` (-> cache_read) as
            // fields SEPARATE from `input_tokens` — `input_tokens` counts only
            // the non-cached input. So input + output alone under-counts the
            // total whenever prompt caching is active (the cache footprint goes
            // invisible in the HUD). The true model footprint includes both
            // cache buckets. This also matches every other provider's
            // convention, where cache-read tokens are already folded into the
            // provider-reported total (OpenAI/Gemini: cached_tokens ⊆
            // prompt/total; Codex: cached ⊆ input ⊆ total).
            usage.total_tokens = total_tokens(&usage);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Context, Message, Tool, Usage};

    fn anthropic_model() -> Model {
        Model::anthropic_claude_sonnet_4_6()
    }

    // A non-Anthropic model routed through this provider's build_body — used to
    // prove cache_control is gated to the Anthropic family only.
    fn non_anthropic_model() -> Model {
        let mut m = Model::anthropic_claude_sonnet_4_6();
        m.api = "openai-completions".into();
        m.provider = "openai".into();
        m
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn ctx_with_history() -> Context {
        Context {
            system_prompt: Some("You are a helpful assistant.".into()),
            messages: vec![
                Message::user_text("first turn"),
                Message::user_text("second turn"),
                Message::user_text("newest turn"),
            ],
            tools: vec![tool("read_file"), tool("write_file")],
        }
    }

    // OCEAN-188: Anthropic reports cache_creation/cache_read separately from
    // input_tokens, so the total footprint must add both cache buckets on top
    // of input + output. With input=100, output=50, cache_write=512,
    // cache_read=200 the total is 862, not the 150 that input+output alone gave.
    #[test]
    fn anthropic_total_tokens_includes_both_cache_buckets() {
        let usage = Usage {
            input: 100,
            output: 50,
            cache_write: 512,
            cache_read: 200,
            ..Default::default()
        };
        assert_eq!(
            total_tokens(&usage),
            862,
            "total must be input + output + cache_write + cache_read"
        );
    }

    // No caching active: cache buckets are zero, so the total collapses back to
    // input + output — no regression for the non-cached path.
    #[test]
    fn anthropic_total_tokens_without_cache_is_input_plus_output() {
        let usage = Usage {
            input: 377,
            output: 65,
            ..Default::default()
        };
        assert_eq!(total_tokens(&usage), 442);
    }

    // OCEAN-159: on the Anthropic family, build_body must attach an ephemeral
    // cache_control breakpoint on (1) the last tool definition, (2) the system
    // block, and (3) a rolling message near the end of history — so the stable
    // prefix caches and only the freshest turn is re-billed.
    #[test]
    fn anthropic_build_body_emits_cache_control_at_intended_positions() {
        let body = build_body(&anthropic_model(), &ctx_with_history(), &StreamOptions::default());

        // (1) Last tool carries the breakpoint; earlier tools do not.
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);
        assert!(
            tools[0].get("cache_control").is_none(),
            "only the LAST tool should carry cache_control: {body}"
        );
        assert_eq!(
            tools[1]["cache_control"]["type"], "ephemeral",
            "last tool must carry an ephemeral cache breakpoint: {body}"
        );

        // (2) System prompt became a text-block array with a breakpoint.
        let system = body["system"].as_array().expect("system must be a block array");
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are a helpful assistant.");
        assert_eq!(
            system[0]["cache_control"]["type"], "ephemeral",
            "system block must carry an ephemeral cache breakpoint: {body}"
        );

        // (3) Rolling message breakpoint on the second-to-last message; the
        // newest turn stays fresh (uncached).
        let msgs = body["messages"].as_array().expect("messages array");
        let n = msgs.len();
        let stable = &msgs[n - 2]["content"];
        let stable_last = stable.as_array().expect("content array").last().unwrap();
        assert_eq!(
            stable_last["cache_control"]["type"], "ephemeral",
            "the stable (second-to-last) message must carry the rolling breakpoint: {body}"
        );
        let newest = &msgs[n - 1]["content"];
        let newest_last = newest.as_array().expect("content array").last().unwrap();
        assert!(
            newest_last.get("cache_control").is_none(),
            "the newest turn must NOT be cached (stays fresh): {body}"
        );

        // Never exceed Anthropic's 4-breakpoint limit.
        let serialized = serde_json::to_string(&body).unwrap();
        let count = serialized.matches("\"cache_control\"").count();
        assert!(count <= 4, "must not exceed 4 cache breakpoints, found {count}: {body}");
    }

    // Non-Anthropic models must get a clean body with no cache_control anywhere,
    // and the system prompt stays a plain string (no array conversion).
    #[test]
    fn non_anthropic_build_body_emits_no_cache_control() {
        let body = build_body(&non_anthropic_model(), &ctx_with_history(), &StreamOptions::default());
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains("cache_control"),
            "non-Anthropic models must not receive cache_control: {body}"
        );
        assert!(
            body["system"].is_string(),
            "non-Anthropic system prompt must stay a plain string: {body}"
        );
    }

    // OCEAN-198: the stop_reason mapper lives inline in the stream loop
    // (lines ~583), so it isn't a free function. This helper mirrors that exact
    // match so the mapping is unit-testable; if the inline arm ever drifts from
    // this, the test guarding it here will need to be updated in lockstep — which
    // is the point of pinning every documented variant.
    fn map_stop_reason(reason: &str) -> StopReason {
        match reason {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::Length,
            "end_turn" | "stop_sequence" => StopReason::Stop,
            _ => StopReason::Stop,
        }
    }

    // OCEAN-198: every stop_reason Anthropic can send maps to the right
    // StopReason, and an unknown/unexpected value falls back to a sane Stop
    // rather than panicking. `pause_turn` (a newer server-tool reason) and a
    // garbage value both land on Stop.
    #[test]
    fn anthropic_stop_reason_mapping_covers_all_variants_and_unknown() {
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(map_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::Stop);
        // Unknown / newer reasons must not panic — default to Stop.
        assert_eq!(map_stop_reason("pause_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("refusal"), StopReason::Stop);
        assert_eq!(map_stop_reason("totally_unexpected"), StopReason::Stop);
        assert_eq!(map_stop_reason(""), StopReason::Stop);
    }

    // OCEAN-188 / OCEAN-198: the message_delta usage decode must read
    // output_tokens off the wire, and message_start usage must read the input +
    // both cache buckets — the exact fields the stream loop folds into Usage.
    // This feeds the REAL Anthropic wire shape through the serde structs the loop
    // uses.
    #[test]
    fn anthropic_usage_delta_decodes_all_token_fields() {
        let wire = r#"{
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 512
        }"#;
        let u: UsageDelta = serde_json::from_str(wire).expect("usage delta decodes");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read_input_tokens, 200);
        assert_eq!(u.cache_creation_input_tokens, 512);
    }

    // OCEAN-198: a usage payload missing the cache fields decodes to zeros (via
    // serde defaults), NOT a parse error — so a non-cached turn doesn't panic the
    // decode path.
    #[test]
    fn anthropic_usage_delta_missing_cache_fields_defaults_to_zero() {
        let wire = r#"{"input_tokens": 42, "output_tokens": 7}"#;
        let u: UsageDelta = serde_json::from_str(wire).expect("partial usage decodes");
        assert_eq!(u.input_tokens, 42);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_input_tokens, 0, "missing cache_read must default to 0");
        assert_eq!(u.cache_creation_input_tokens, 0, "missing cache_write must default to 0");
    }

    // OCEAN-198: an entirely empty usage object still decodes (all defaults) —
    // the stream loop must never panic on a usage frame that omits everything.
    #[test]
    fn anthropic_usage_delta_empty_object_is_all_zeros() {
        let u: UsageDelta = serde_json::from_str("{}").expect("empty usage decodes");
        assert_eq!(total_tokens(&Usage::default()), 0);
        assert_eq!(u.input_tokens + u.output_tokens, 0);
    }

    // OCEAN-101 / OCEAN-198: an unrecognized SSE event type must decode to the
    // `Other` catch-all (handled as a no-op in the loop), NOT a deserialize
    // error that would otherwise abort the stream. A future/unknown event name
    // is tolerated gracefully.
    #[test]
    fn anthropic_unknown_sse_event_decodes_to_other() {
        let wire = r#"{"type": "message_resume", "foo": 1}"#;
        let ev: SseEvent = serde_json::from_str(wire).expect("unknown event must not fail to parse");
        assert!(matches!(ev, SseEvent::Other), "unknown event type must map to Other");
    }

    // OCEAN-101 / OCEAN-198: an unmapped content_block_start kind (e.g.
    // redacted_thinking, server_tool_use) decodes to BlockStart::Other, which the
    // loop logs-and-drops instead of panicking; an unmapped block_delta kind
    // likewise decodes to BlockDelta::Other. Proves the `#[serde(other)]`
    // catch-alls are wired.
    #[test]
    fn anthropic_unmapped_block_kinds_decode_to_other() {
        // A redacted_thinking block_start is an Other block (no Content variant).
        let block = r#"{"type": "redacted_thinking", "data": "xyz"}"#;
        let bs: BlockStart = serde_json::from_str(block).expect("unmapped block kind decodes");
        assert!(matches!(bs, BlockStart::Other), "unmapped block kind must map to Other");

        // An unmapped block_delta kind likewise decodes to Other, not an error.
        let delta = r#"{"type": "citations_delta", "citation": {}}"#;
        let bd: BlockDelta = serde_json::from_str(delta).expect("unmapped delta kind decodes");
        assert!(matches!(bd, BlockDelta::Other), "unmapped delta kind must map to Other");
    }

    // OCEAN-198: a tool_use input_json_delta assembled across multiple fragments
    // joins into one parseable JSON object — the exact buffer-then-parse the
    // stream loop performs at content_block_stop. Mirrors the loop's json_buf
    // accumulation + final from_str.
    #[test]
    fn anthropic_tool_call_args_assemble_across_deltas() {
        // Fragments as they'd arrive on successive input_json_delta frames.
        let frags = ["{\"path\":", "\"/tmp/a\",", "\"recursive\":", "true}"];
        let mut json_buf = String::new();
        for f in frags {
            let wire = json!({"type": "input_json_delta", "partial_json": f}).to_string();
            let d: BlockDelta = serde_json::from_str(&wire).expect("delta decodes");
            match d {
                BlockDelta::InputJsonDelta { partial_json } => json_buf.push_str(&partial_json),
                _ => panic!("expected InputJsonDelta"),
            }
        }
        let parsed: Value = serde_json::from_str(&json_buf).expect("assembled args must parse");
        assert_eq!(parsed["path"], "/tmp/a");
        assert_eq!(parsed["recursive"], true);
    }

    // OCEAN-198: an empty arg buffer (a tool call with no streamed input) must
    // finalize to `{}`, never panic — mirroring the loop's
    // `if json_buf.is_empty() { {} }` guard. And a MALFORMED partial buffer must
    // degrade to `{}` via unwrap_or, not blow up the turn.
    #[test]
    fn anthropic_tool_call_empty_and_malformed_args_default_to_empty_object() {
        // Empty buffer → {}.
        let empty = String::new();
        let args: Value = if empty.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&empty).unwrap_or(Value::Object(Default::default()))
        };
        assert_eq!(args, json!({}), "empty tool args must finalize to an empty object");

        // Truncated/malformed JSON → {} (no panic), exactly like the loop's
        // unwrap_or fallback.
        let bad = "{\"path\": \"/tmp".to_string();
        let args2: Value = serde_json::from_str(&bad).unwrap_or(Value::Object(Default::default()));
        assert_eq!(args2, json!({}), "malformed tool args must fall back to an empty object, not panic");
    }

    // With a single message there is no stable history yet, so the rolling
    // message breakpoint is skipped — but tools + system still cache the prefix.
    #[test]
    fn anthropic_build_body_skips_message_breakpoint_for_single_message() {
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::user_text("only turn")],
            tools: vec![tool("read_file")],
        };
        let body = build_body(&anthropic_model(), &ctx, &StreamOptions::default());
        let msgs = body["messages"].as_array().expect("messages array");
        let only = msgs[0]["content"].as_array().expect("content array");
        assert!(
            only.last().unwrap().get("cache_control").is_none(),
            "a lone message must not carry a rolling breakpoint: {body}"
        );
        // System + tool breakpoints are still present.
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }
}
