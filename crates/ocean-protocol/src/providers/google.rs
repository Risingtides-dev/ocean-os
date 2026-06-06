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
    StreamOptions, ThinkingLevel, Usage,
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

/// Classify a Gemini `finishReason` into the unified `StopReason`, flagging the
/// abnormal/blocking reasons so they surface as an error rather than masquerading
/// as a clean stop. Gemini returns reasons like `SAFETY`, `RECITATION`,
/// `PROHIBITED_CONTENT`, `BLOCKLIST`, `SPII`, `MALFORMED_FUNCTION_CALL`,
/// `IMAGE_SAFETY` — all of which previously mapped to a normal `Stop`, so a
/// content-filtered response with empty content looked like a successful empty
/// completion (OCEAN-101 silent-drop).
///
/// Returns `(StopReason, is_blocking)`. When `is_blocking` is true the caller
/// surfaces an error so the operator/agent sees *why* the turn produced nothing
/// instead of receiving a clean-but-empty assistant message.
fn classify_finish_reason(reason: &str) -> (StopReason, bool) {
    match reason {
        // Normal completions.
        "STOP" | "FINISH_REASON_UNSPECIFIED" | "" => (StopReason::Stop, false),
        "MAX_TOKENS" => (StopReason::Length, false),
        // Abnormal / blocking terminations — the model stopped because content
        // was filtered, recited, malformed, etc. These must not look like a
        // clean stop; surface them.
        _ => (StopReason::Error, true),
    }
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
                // The structured/textual tool output stays in the
                // functionResponse part. The Gemini functionResponse schema
                // carries the tool's text/JSON result; it has no slot for image
                // bytes.
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": tr.tool_name,
                            "response": {"output": text, "is_error": tr.is_error}
                        }
                    }]
                }));
                // OCEAN-132: tool-result images (browser / computer-use
                // screenshots come back as Content::Image) were silently dropped
                // — only `.as_text()` was collected above, so the model never saw
                // the screenshot ("I can't see any screenshot"). Gemini reads
                // images from inlineData parts, so follow the functionResponse
                // with a user-role content carrying each image as an inlineData
                // part (mirroring how user-message images are encoded above).
                let image_parts: Vec<Value> = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Image { data, mime_type } => Some(json!({
                            "inlineData": {"mimeType": mime_type, "data": data}
                        })),
                        _ => None,
                    })
                    .collect();
                if !image_parts.is_empty() {
                    out.push(json!({"role": "user", "parts": image_parts}));
                }
            }
        }
    }
    out
}

/// Maps the operator-chosen `ThinkingLevel` into a Gemini `thinkingBudget`
/// (token count). Gemini 2.x thinking models accept
/// `generationConfig.thinkingConfig.thinkingBudget`: a token budget where `0`
/// disables thinking and `-1` is automatic; the upper range is model-dependent.
/// We mirror the per-level token budgets the Anthropic provider uses
/// (`thinking_budget`) so the same operator level produces a comparable
/// reasoning allowance across providers. `Off` returns `None` so nothing is
/// emitted (parity with OCEAN-134's openai.rs).
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

/// Injects the reasoning budget onto the Gemini request body under
/// `generationConfig.thinkingConfig`, using the REST shape the v1beta
/// `generateContent` endpoint expects:
/// `generationConfig.thinkingConfig.thinkingBudget` (token count).
///
/// We deliberately do NOT set `includeThoughts`. With that flag on, Gemini
/// streams "thought" parts (`text` + a `thought: true` marker), and this
/// provider's stream loop emits every non-empty `part.text` as a normal
/// `TextDelta` — so the reasoning summary would leak into the visible
/// assistant answer. OCEAN-139's scope is only to request the thinking
/// *budget* so the operator's reasoning level takes effect; surfacing thought
/// summaries as proper Thinking blocks is OCEAN-140's concern.
///
/// Before OCEAN-139 the Gemini provider silently dropped the operator's
/// thinking level — `build_body` set `temperature`/`tools` but never emitted
/// `thinkingConfig`, so Medium/High was inert on Gemini while it worked on
/// Anthropic (budget_tokens), Codex (effort), and OpenAI (reasoning_effort,
/// OCEAN-134). `Off`/unset emits nothing.
fn apply_reasoning(body: &mut Value, level: ThinkingLevel) {
    let Some(budget) = thinking_budget(level) else {
        return;
    };
    // generationConfig may already exist (temperature); merge into it rather
    // than clobbering it.
    if !body["generationConfig"].is_object() {
        body["generationConfig"] = json!({});
    }
    body["generationConfig"]["thinkingConfig"] = json!({
        "thinkingBudget": budget,
    });
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
    if let Some(level) = options.reasoning {
        apply_reasoning(&mut body, level);
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
            // Track an abnormal/blocking finishReason (SAFETY, RECITATION, …) so
            // it surfaces as a real error instead of a clean empty completion.
            let mut block_reason: Option<String> = None;

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
                    Err(e) => {
                        tracing::debug!(error = %e, "skipping unparseable Gemini SSE frame");
                        continue;
                    }
                };
                if let Some(m) = chunk.model_version { response_model = Some(m); }
                if let Some(u) = chunk.usage_metadata {
                    usage.input = u.prompt_token_count;
                    usage.output = u.candidates_token_count;
                    usage.total_tokens = u.total_token_count;
                }
                for cand in chunk.candidates {
                    if let Some(reason) = cand.finish_reason {
                        let (mapped, is_blocking) = classify_finish_reason(&reason);
                        stop = mapped;
                        if is_blocking {
                            tracing::warn!(
                                finish_reason = %reason,
                                "Gemini terminated abnormally; surfacing as error"
                            );
                            block_reason = Some(reason);
                        }
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

            // A blocking finishReason (SAFETY / RECITATION / BLOCKLIST / …) that
            // produced no usable content is an error, not a clean stop. Surface it
            // so the caller sees *why* the turn yielded nothing rather than a
            // silently-empty success (OCEAN-101).
            if let Some(reason) = &block_reason {
                if !text_started && tool_blocks.is_empty() {
                    let am = AssistantMessage {
                        content: vec![],
                        api: api.clone(),
                        provider: provider.clone(),
                        model: response_model.clone().unwrap_or_else(|| model_id.clone()),
                        usage: usage.clone(),
                        stop_reason: StopReason::Error,
                        error_message: Some(format!(
                            "Gemini blocked the response (finishReason: {reason})"
                        )),
                        timestamp: now_ms(),
                    };
                    yield Ok(AssistantMessageEvent::Error { reason: StopReason::Error, error: am });
                    return;
                }
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

    // OCEAN-132: tool-result image parity. A Message::ToolResult carrying a
    // Content::Image (browser / computer-use screenshot) must reach the model as
    // a Gemini inlineData part — previously only `.as_text()` was collected into
    // functionResponse.output, so the screenshot was silently dropped and the
    // model replied "I can't see any screenshot".
    #[test]
    fn tool_result_image_is_encoded_as_inline_data_part() {
        let messages = vec![Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "screenshot".into(),
            content: vec![
                Content::text("captured viewport"),
                Content::Image {
                    data: "AAECAwQ=".into(),
                    mime_type: "image/png".into(),
                },
            ],
            is_error: false,
            timestamp: now_ms(),
        })];

        let out = convert_messages(&messages);

        // functionResponse still carries the textual output…
        let fr = out
            .iter()
            .find(|c| c["parts"][0].get("functionResponse").is_some())
            .expect("functionResponse missing");
        assert_eq!(
            fr["parts"][0]["functionResponse"]["response"]["output"],
            "captured viewport"
        );

        // …and the image is present somewhere as an inlineData part (NOT dropped).
        let image = out
            .iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .find(|p| p.get("inlineData").is_some())
            .expect("inlineData part missing — tool-result image was dropped");
        assert_eq!(image["inlineData"]["mimeType"], "image/png");
        assert_eq!(image["inlineData"]["data"], "AAECAwQ=");
    }

    // A text-only tool result stays exactly as before: one functionResponse
    // content, no stray inlineData part appended.
    #[test]
    fn text_only_tool_result_stays_text_only() {
        let messages = vec![Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "read_file".into(),
            content: vec![Content::text("file contents here")],
            is_error: false,
            timestamp: now_ms(),
        })];

        let out = convert_messages(&messages);
        assert_eq!(out.len(), 1, "no extra image content should be appended");
        assert_eq!(
            out[0]["parts"][0]["functionResponse"]["response"]["output"],
            "file contents here"
        );
        let has_inline = out
            .iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .any(|p| p.get("inlineData").is_some());
        assert!(!has_inline, "text-only result must not emit inlineData");
    }

    // OCEAN-101: a content-filter / abnormal finishReason must NOT map to a clean
    // `Stop`. Previously every non-MAX_TOKENS reason fell through to `Stop`, so a
    // SAFETY/RECITATION block looked like a successful empty completion.
    #[test]
    fn blocking_finish_reasons_classify_as_error() {
        for reason in [
            "SAFETY",
            "RECITATION",
            "PROHIBITED_CONTENT",
            "BLOCKLIST",
            "SPII",
            "MALFORMED_FUNCTION_CALL",
            "IMAGE_SAFETY",
            "OTHER",
        ] {
            let (stop, blocking) = classify_finish_reason(reason);
            assert_eq!(
                stop,
                StopReason::Error,
                "{reason} must surface as Error, not a silent Stop"
            );
            assert!(blocking, "{reason} must be flagged as blocking");
        }
    }

    // OCEAN-139: reasoning parity (Gemini side of OCEAN-134). `build_body` must
    // translate `options.reasoning` into
    // `generationConfig.thinkingConfig.thinkingBudget`, and must omit
    // thinkingConfig entirely when no reasoning level is set (or it's Off).
    fn empty_context() -> Context {
        Context {
            messages: vec![Message::User {
                content: vec![Content::text("hi")],
                timestamp: now_ms(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn build_body_omits_thinking_config_when_unset() {
        let body = build_body(&empty_context(), &StreamOptions::default());
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "thinkingConfig must not be sent when options.reasoning is None: {body}"
        );
    }

    #[test]
    fn build_body_omits_thinking_config_when_off() {
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let body = build_body(&empty_context(), &options);
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "ThinkingLevel::Off must not emit thinkingConfig: {body}"
        );
    }

    #[test]
    fn build_body_emits_thinking_config_when_set() {
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let body = build_body(&empty_context(), &options);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 16384,
            "Gemini must receive generationConfig.thinkingConfig.thinkingBudget: {body}"
        );
        // OCEAN-139: must NOT request thought parts. includeThoughts would make
        // Gemini stream `thought: true` text parts, which this provider's loop
        // emits as normal TextDelta — leaking the reasoning summary into the
        // visible answer. Surfacing thoughts as Thinking blocks is OCEAN-140.
        assert!(
            body["generationConfig"]["thinkingConfig"]
                .get("includeThoughts")
                .is_none(),
            "includeThoughts must NOT be sent — it leaks thought parts as visible text: {body}"
        );
    }

    #[test]
    fn build_body_maps_thinking_levels_to_budgets() {
        for (level, expected) in [
            (ThinkingLevel::Minimal, 1024u32),
            (ThinkingLevel::Low, 2048),
            (ThinkingLevel::Medium, 8192),
            (ThinkingLevel::High, 16384),
            (ThinkingLevel::Xhigh, 24576),
        ] {
            let options = StreamOptions {
                reasoning: Some(level),
                ..Default::default()
            };
            let body = build_body(&empty_context(), &options);
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingBudget"], expected,
                "level {level:?} mismapped: {body}"
            );
        }
    }

    // thinkingConfig must merge into an existing generationConfig (temperature),
    // not clobber it.
    #[test]
    fn build_body_preserves_temperature_alongside_thinking_config() {
        let options = StreamOptions {
            temperature: Some(0.7),
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        };
        let body = build_body(&empty_context(), &options);
        let temp = body["generationConfig"]["temperature"]
            .as_f64()
            .expect("temperature must survive alongside thinkingConfig");
        assert!(
            (temp - 0.7).abs() < 1e-6,
            "temperature must survive alongside thinkingConfig: {body}"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 8192,
            "thinkingConfig must be present alongside temperature: {body}"
        );
    }

    // Normal terminations stay normal.
    #[test]
    fn normal_finish_reasons_are_not_blocking() {
        assert_eq!(classify_finish_reason("STOP"), (StopReason::Stop, false));
        assert_eq!(classify_finish_reason(""), (StopReason::Stop, false));
        assert_eq!(
            classify_finish_reason("FINISH_REASON_UNSPECIFIED"),
            (StopReason::Stop, false)
        );
        assert_eq!(
            classify_finish_reason("MAX_TOKENS"),
            (StopReason::Length, false)
        );
    }
}
