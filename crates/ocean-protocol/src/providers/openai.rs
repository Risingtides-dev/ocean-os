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
                let text = content
                    .iter()
                    .filter_map(|c| c.as_text().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({"role": "user", "content": text}));
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

            let mut text_buf = String::new();
            let mut text_started = false;
            let mut text_index: usize = 0;
            let mut tool_calls: std::collections::BTreeMap<usize, PartialToolCall> = Default::default();
            let mut tool_started: std::collections::BTreeSet<usize> = Default::default();
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
                    Err(_) => continue,
                };
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
                        if let Some(c) = delta.content {
                            if !c.is_empty() {
                                if !text_started {
                                    text_started = true;
                                    yield Ok(AssistantMessageEvent::TextStart { content_index: text_index });
                                }
                                text_buf.push_str(&c);
                                yield Ok(AssistantMessageEvent::TextDelta {
                                    content_index: text_index,
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
                                    if !tool_started.contains(&tc.index) {
                                        tool_started.insert(tc.index);
                                        let block_index = text_index
                                            + if text_started { 1 } else { 0 }
                                            + tool_started.len()
                                            - 1;
                                        yield Ok(AssistantMessageEvent::ToolCallStart {
                                            content_index: block_index,
                                            id: entry.id.clone(),
                                            name: entry.name.clone(),
                                        });
                                    }
                                    let block_index = text_index
                                        + if text_started { 1 } else { 0 }
                                        + tc.index;
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

            if text_started {
                yield Ok(AssistantMessageEvent::TextEnd {
                    content_index: text_index,
                    content: text_buf.clone(),
                });
                text_index += 1;
            }

            let mut out_content: Vec<Content> = Vec::new();
            if text_started {
                out_content.push(Content::Text { text: text_buf.clone() });
            }
            for (i, tc) in tool_calls {
                let args: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args).unwrap_or(Value::Object(Default::default()))
                };
                let block_index = text_index + i;
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
