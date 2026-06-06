//! Google Generative AI provider (`google-generative-ai`).
//!
//! Targets the v1beta `generativelanguage.googleapis.com` endpoint with the
//! `streamGenerateContent` method. Emits the unified `AssistantMessageEvent`
//! protocol like the other providers.
//!
//! The Google SSE format is a JSON array of "candidates" chunks rather than
//! discrete event names, but `eventsource-stream` still works because the
//! server sends `data:` framed records.

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
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
    #[serde(default)]
    model_version: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize, Debug)]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCall>,
}

#[derive(Deserialize, Debug)]
struct FunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize, Debug, Default)]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
    #[serde(default)]
    total_token_count: u64,
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m {
            Message::User { content, .. } => {
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(json!({"text": text})),
                        Content::Image { data, mime_type } => Some(json!({
                            "inlineData": {"mimeType": mime_type, "data": data}
                        })),
                        _ => None,
                    })
                    .collect();
                out.push(json!({"role": "user", "parts": parts}));
            }
            Message::Assistant(a) => {
                let mut parts: Vec<Value> = Vec::new();
                for c in &a.content {
                    match c {
                        Content::Text { text } => parts.push(json!({"text": text})),
                        Content::ToolCall {
                            name, arguments, ..
                        } => {
                            parts.push(json!({
                                "functionCall": {"name": name, "args": arguments}
                            }));
                        }
                        _ => {}
                    }
                }
                out.push(json!({"role": "model", "parts": parts}));
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": tr.tool_name,
                            "response": {"output": text, "is_error": tr.is_error}
                        }
                    }]
                }));
            }
        }
    }
    out
}

fn build_body(context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "contents": convert_messages(&context.messages),
    });
    if let Some(sp) = &context.system_prompt {
        body["systemInstruction"] = json!({"role": "system", "parts": [{"text": sp}]});
    }
    if let Some(t) = options.temperature {
        body["generationConfig"] = json!({"temperature": t});
    }
    if !context.tools.is_empty() {
        let decls: Vec<Value> = context
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    body
}

pub struct GoogleProvider {
    client: reqwest::Client,
}

impl GoogleProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GoogleProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .ok_or_else(|| Error::MissingApiKey("google".into()))?;
        let base_url = options
            .base_url
            .clone()
            .unwrap_or_else(|| model.base_url.clone());
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            base_url.trim_end_matches('/'),
            model.id,
            api_key,
        );
        let body = build_body(context, options);
        let cancel = options.cancel.clone();
        let extra_headers: BTreeMap<String, String> = options.headers.clone();

        let resp = with_retry(&RetryConfig::default(), cancel.as_ref(), |_| {
            let client = self.client.clone();
            let url = url.clone();
            let body = body.clone();
            let extra_headers = extra_headers.clone();
            async move {
                let mut req = client
                    .post(&url)
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
                        };
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
                let body_text = r.text().await.unwrap_or_default();
                let err = Error::ProviderError {
                    status: status.as_u16(),
                    body: body_text,
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
            let mut tool_blocks: Vec<(String, String, Value)> = Vec::new();
            let mut stop = StopReason::Stop;
            let mut usage = Usage::default();
            let mut response_model: Option<String> = None;

            while let Some(ev) = sse.next().await {
                if let Some(c) = &cancel_for_stream {
                    if c.is_cancelled() { yield Err(Error::Cancelled); return; }
                }
                let ev = match ev {
                    Ok(e) => e,
                    Err(e) => { yield Err(Error::InvalidResponse(format!("sse: {e}"))); return; }
                };
                if ev.data.is_empty() { continue; }
                let chunk: Chunk = match serde_json::from_str(&ev.data) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some(m) = chunk.model_version { response_model = Some(m); }
                if let Some(u) = chunk.usage_metadata {
                    usage.input = u.prompt_token_count;
                    usage.output = u.candidates_token_count;
                    usage.total_tokens = u.total_token_count;
                }
                for cand in chunk.candidates {
                    if let Some(reason) = cand.finish_reason {
                        stop = match reason.as_str() {
                            "STOP" => StopReason::Stop,
                            "MAX_TOKENS" => StopReason::Length,
                            _ => StopReason::Stop,
                        };
                    }
                    if let Some(content) = cand.content {
                        for part in content.parts {
                            if let Some(t) = part.text {
                                if !t.is_empty() {
                                    if !text_started {
                                        text_started = true;
                                        yield Ok(AssistantMessageEvent::TextStart { content_index: text_index });
                                    }
                                    text_buf.push_str(&t);
                                    yield Ok(AssistantMessageEvent::TextDelta { content_index: text_index, delta: t });
                                }
                            }
                            if let Some(fc) = part.function_call {
                                let id = format!("call_{}", tool_blocks.len() + 1);
                                let block_index = text_index + if text_started { 1 } else { 0 } + tool_blocks.len();
                                yield Ok(AssistantMessageEvent::ToolCallStart {
                                    content_index: block_index,
                                    id: id.clone(),
                                    name: fc.name.clone(),
                                });
                                yield Ok(AssistantMessageEvent::ToolCallEnd {
                                    content_index: block_index,
                                    id: id.clone(),
                                    name: fc.name.clone(),
                                    arguments: fc.args.clone(),
                                });
                                if fc.finish_reason_set_to_tool_use() { stop = StopReason::ToolUse; }
                                tool_blocks.push((id, fc.name, fc.args));
                            }
                        }
                    }
                }
            }

            if text_started {
                yield Ok(AssistantMessageEvent::TextEnd { content_index: text_index, content: text_buf.clone() });
                text_index += 1;
            }
            if !tool_blocks.is_empty() && stop == StopReason::Stop {
                stop = StopReason::ToolUse;
            }
            let mut out_content: Vec<Content> = Vec::new();
            if text_started {
                out_content.push(Content::Text { text: text_buf });
            }
            for (id, name, args) in tool_blocks {
                out_content.push(Content::ToolCall { id, name, arguments: args });
            }
            let _ = text_index;
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

// Helper marker — Gemini doesn't signal tool use in finish_reason; treat any
// function_call as implying ToolUse if no other stop reason is reported.
impl FunctionCall {
    fn finish_reason_set_to_tool_use(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::now_ms;

    // OCEAN-99: vision parity. A user message carrying a Content::Image must
    // serialize as a Gemini inlineData part, not be dropped to text-only.
    #[test]
    fn user_image_is_encoded_as_inline_data_part() {
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

        let out = convert_messages(&messages);
        assert_eq!(out.len(), 1);
        let parts = out[0]["parts"]
            .as_array()
            .expect("parts array missing");

        let has_text = parts.iter().any(|p| p["text"] == "describe this");
        assert!(has_text, "text part missing: {:?}", parts);

        let image = parts
            .iter()
            .find(|p| p.get("inlineData").is_some())
            .expect("inlineData part missing — image was dropped");
        assert_eq!(image["inlineData"]["mimeType"], "image/png");
        assert_eq!(image["inlineData"]["data"], "AAECAwQ=");
    }
}
