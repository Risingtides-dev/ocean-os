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
use crate::retry::{classify_status, parse_retry_after, retry_config, with_retry, Attempt};
use crate::stream::AssistantMessageEventStream;
use crate::types::{
    now_ms, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, Usage,
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

/// Map an OpenAI (Chat Completions) `finish_reason` to a [`StopReason`].
///
/// `tool_calls` → ToolUse and `length` → Length are normal terminations.
/// `content_filter` (OCEAN-142) is the safety filter cutting the turn off; with
/// no accompanying refusal delta (OCEAN-101) it used to collapse into the
/// catch-all clean `Stop`, hiding the truncation. We surface it as
/// `StopReason::Error`, mirroring Gemini's `classify_finish_reason` in
/// `google.rs`, so the operator/agent sees the turn was filtered rather than a
/// silent, possibly-truncated stop. Everything else stays a clean `Stop`.
fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::Length,
        "content_filter" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

/// OCEAN-142: decide whether a finished turn must be surfaced as an Error event
/// rather than a normal Done.
///
/// A `content_filter` finish_reason maps to [`StopReason::Error`]. If the turn
/// also produced no usable content, the runtime's agent loop would treat a Done
/// (even one carrying `StopReason::Error`) as a clean, empty success — so the
/// safety filter would never reach the user. In that empty/blocked case the
/// stream must emit an `AssistantMessageEvent::Error` instead, mirroring the
/// in-stream error-frame path and Gemini's blocking path. A partial-but-useful
/// turn (some text or tool calls arrived before the filter) is preserved and
/// must NOT be turned into an error.
fn is_blocking_empty_turn(stop: StopReason, has_usable_content: bool) -> bool {
    stop == StopReason::Error && !has_usable_content
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
    // OCEAN-158: cached-prompt tokens live under `prompt_tokens_details.cached_tokens`
    // on the Chat Completions API. Decode them so the cache-read HUD shows a true
    // value instead of a structural 0.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    // OCEAN-164: reasoning tokens live under `completion_tokens_details.reasoning_tokens`
    // on the Chat Completions API. They are already part of `completion_tokens`,
    // so we decode them only to surface the reasoning subset (never re-add to total).
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize, Debug, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Debug, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

pub struct OpenAiProvider {
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        // OCEAN-221: streaming SSE client with connect + idle (read) timeouts
        // and NO total request timeout, so long completions are never cut off.
        Self {
            client: crate::http::streaming_client(),
        }
    }
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn persisted_model_id(requested: &str, response: Option<&str>, preserve_requested: bool) -> String {
    if preserve_requested {
        requested.to_string()
    } else {
        response.unwrap_or(requested).to_string()
    }
}

fn tool_wire(t: &crate::types::Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

fn convert_messages(
    system_prompt: Option<&str>,
    messages: &[Message],
    model: &Model,
    dynamic_declarations: &[crate::types::DynamicToolDeclaration],
) -> Vec<Value> {
    let supports_images = model.supports_images;
    let replay_kimi_reasoning = model.supports_dynamic_tools();
    let mut out: Vec<Value> = Vec::new();
    if let Some(sp) = system_prompt {
        out.push(json!({"role": "system", "content": sp}));
    }
    for (message_index, m) in messages.iter().enumerate() {
        for declaration in dynamic_declarations
            .iter()
            .filter(|declaration| declaration.before_message == message_index)
        {
            out.push(json!({
                "role": "system",
                "tools": declaration.tools.iter().map(tool_wire).collect::<Vec<_>>()
            }));
        }
        match m {
            Message::User { content, .. } => {
                // If any image is present AND the model takes image parts, emit
                // the content-array form so vision turns survive. Otherwise keep
                // the text form — strict text-only backends (DeepSeek, Kimi,
                // MiniMax) reject unknown content variants with a 400, which
                // permanently bricks any session whose history holds an image
                // (OCEAN-386).
                let has_image = content.iter().any(|c| matches!(c, Content::Image { .. }));
                if has_image && supports_images {
                    let parts: Vec<Value> = content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(json!({"type": "text", "text": text})),
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
                    // Text form. Each image (only reachable on a no-vision
                    // model) degrades to a bracketed note so the model still
                    // learns an attachment existed instead of silently losing
                    // the turn's meaning.
                    let mut text = String::new();
                    for c in content {
                        match c {
                            Content::Text { text: t } => text.push_str(t),
                            Content::Image { mime_type, .. } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(&format!(
                                    "[image attached ({mime_type}) — omitted: this model has no image input]"
                                ));
                            }
                            _ => {}
                        }
                    }
                    out.push(json!({"role": "user", "content": text}));
                }
            }
            Message::Assistant(a) => {
                let mut text = String::new();
                let mut reasoning = String::new();
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
                        // Kimi K3 requires the complete prior assistant message,
                        // including `reasoning_content`, on multi-round tool calls.
                        // Reconstruct it only for same-provider, same-model K3
                        // history; every other Chat Completions backend retains
                        // Ocean's explicit private-reasoning drop.
                        Content::Thinking { thinking, .. }
                            if replay_kimi_reasoning
                                && a.provider == "kimi"
                                && a.model == "kimi-k3" =>
                        {
                            reasoning.push_str(thinking);
                        }
                        Content::Thinking { .. } => {}
                        // Images never appear in assistant content on this API.
                        Content::Image { .. } => {}
                    }
                }
                let mut msg = json!({"role": "assistant", "content": text});
                if !reasoning.is_empty() {
                    msg["reasoning_content"] = json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Message::ToolResult(tr) => {
                let mut text = tr
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join("");
                let has_images = tr
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::Image { .. }));
                // On a no-vision model a screenshot tool result would otherwise
                // vanish without a trace — leave a note in the tool text so the
                // model knows the tool DID return something visual (OCEAN-386).
                if has_images && !supports_images {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("[screenshot omitted: this model has no image input]");
                }
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "content": text,
                }));
                // OpenAI Chat Completions cannot carry image parts inside a
                // `role:tool` message — only text. Browser / computer-use tools
                // return screenshots as Content::Image tool results, and the
                // text-only `role:tool` message above silently drops them, so on
                // OpenAI the model never sees the screenshot (OCEAN-131).
                //
                // The standard workaround: keep the textual tool result as the
                // `role:tool` message (above), then immediately follow it with a
                // `role:user` message carrying the image(s) as image_url
                // data-URL parts — the same encoding used for user-message
                // images (OCEAN-99). This is what surfaces the screenshot to the
                // model. Text-only tool results add no extra message, and a
                // no-vision model gets no image message at all (gated above).
                let image_parts: Vec<Value> = if supports_images {
                    tr.content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Image { data, mime_type } => Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", mime_type, data)
                                }
                            })),
                            _ => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                if !image_parts.is_empty() {
                    out.push(json!({"role": "user", "content": image_parts}));
                }
            }
        }
    }
    for declaration in dynamic_declarations
        .iter()
        .filter(|declaration| declaration.before_message == messages.len())
    {
        out.push(json!({
            "role": "system",
            "tools": declaration.tools.iter().map(tool_wire).collect::<Vec<_>>()
        }));
    }
    out
}

/// Maps the operator-chosen `ThinkingLevel` into the OpenAI Chat Completions
/// `reasoning_effort` enum, which only understands `minimal | low | medium | high`.
/// `Xhigh` has no distinct OpenAI level, so it folds into `high`.
fn openai_reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh => Some("high"),
    }
}

/// DeepSeek's effort scale differs from OpenAI's: it accepts only `high | max`,
/// and documents that `low`/`medium` map up to `high` while `xhigh` maps to
/// `max` (per the DeepSeek thinking-mode guide). DeepSeek also requires the
/// `thinking` toggle to actually engage the reasoner.
fn deepseek_reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal
        | ThinkingLevel::Low
        | ThinkingLevel::Medium
        | ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("max"),
    }
}

/// Injects the reasoning-effort request param onto an OpenAI-compatible body,
/// using the right wire shape for the routed backend.
///
/// Before OCEAN-134 this was a no-op: every openai-completions model (OpenAI
/// o-series, DeepSeek reasoner/v4, MiniMax M2, Kimi) silently dropped the
/// thinking level the user picked. The decoder already reads `reasoning`/
/// `reasoning_content` deltas — it just never asked for them.
///
/// Param names diverge per backend, so we gate by `model.provider` rather than
/// blasting one param everywhere (an unknown field 400s on stricter gateways):
///   - OpenAI o-series      → top-level `reasoning_effort` (minimal|low|medium|high)
///   - DeepSeek (V4)        → `thinking: {type, reasoning_effort}` (see below)
///   - other openai-compatible backends → left untouched (no agreed param)
///
/// DeepSeek's shape (verified live against api.deepseek.com 2026-07-12):
///
///   - The effort nests INSIDE the thinking object —
///     `thinking: {"type": "enabled", "reasoning_effort": "high"}`. A top-level
///     `reasoning_effort` is not part of DeepSeek's contract, and DeepSeek
///     silently DROPS unknown top-level fields rather than 400ing (probed with a
///     junk param: HTTP 200), so the old top-level form could not be trusted to
///     do anything. Send the documented shape.
///
///   - Thinking is ON BY DEFAULT. A request with no `thinking` object still
///     burns reasoning tokens (probed: ~1.8k–3.1k on a plain math prompt). So
///     `ThinkingLevel::Off` MUST send an explicit `{"type": "disabled"}` — an
///     early return leaves the reasoner running and the user pays for thinking
///     they turned off. This is why the fn no longer bails on Off.
fn apply_reasoning(body: &mut Value, model: &Model, level: ThinkingLevel) {
    match model.provider.as_str() {
        "openai" => {
            if level == ThinkingLevel::Off {
                return;
            }
            if let Some(effort) = openai_reasoning_effort(level) {
                body["reasoning_effort"] = json!(effort);
            }
        }
        "kimi" if model.supports_dynamic_tools() => {
            if level != ThinkingLevel::Off {
                // K3 currently accepts only `max`; omit the field when Ocean's
                // thinking control is Off rather than inventing a disable value.
                body["reasoning_effort"] = json!("max");
            }
        }
        "deepseek" => match deepseek_reasoning_effort(level) {
            Some(effort) => {
                body["thinking"] = json!({"type": "enabled", "reasoning_effort": effort});
            }
            // Off — must be explicit; see the note above.
            None => {
                body["thinking"] = json!({"type": "disabled"});
            }
        },
        // MiniMax, Kimi, OpenRouter passthrough, and arbitrary `openai_compat`
        // backends have no common reasoning-effort param — sending one risks a
        // 400. They already stream reasoning by default, so we leave the body
        // alone rather than guess a param.
        _ => {}
    }
}

fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Result<Value> {
    if !context.dynamic_tool_declarations.is_empty() && !model.supports_dynamic_tools() {
        return Err(Error::UnsupportedProvider(format!(
            "dynamic tool declarations require exact kimi-k3 route, got {}/{}",
            model.provider, model.id
        )));
    }
    let mut previous_index = 0usize;
    let mut declared_names = std::collections::HashSet::new();
    let mut declared_count = 0usize;
    let mut declared_bytes = 0usize;
    for (position, declaration) in context.dynamic_tool_declarations.iter().enumerate() {
        if declaration.tools.is_empty()
            || declaration.before_message > context.messages.len()
            || (position > 0 && declaration.before_message < previous_index)
        {
            return Err(Error::Other(
                "dynamic tool declarations require nonempty groups in stable transcript order"
                    .into(),
            ));
        }
        previous_index = declaration.before_message;
        for tool in &declaration.tools {
            if !declared_names.insert(tool.name.as_str()) {
                return Err(Error::Other(format!(
                    "dynamic tool `{}` is declared more than once",
                    tool.name
                )));
            }
            declared_count = declared_count.saturating_add(1);
            declared_bytes = declared_bytes.saturating_add(serde_json::to_vec(tool)?.len());
        }
    }
    if declared_count > 16 || declared_bytes > 512 * 1024 {
        return Err(Error::Other(
            "dynamic tool declarations exceed the 16-tool/512-KiB request bounds".into(),
        ));
    }
    let messages = convert_messages(
        context.system_prompt.as_deref(),
        &context.messages,
        model,
        &context.dynamic_tool_declarations,
    );
    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(t) = options.temperature {
        // K3 fixes temperature at 1.0 and documents that clients should omit it.
        if !model.supports_dynamic_tools() {
            body["temperature"] = json!(t);
        }
    }
    if let Some(m) = options.max_tokens {
        // OCEAN-141: real api.openai.com models (o-series, gpt-5-class) on the
        // Chat Completions path REJECT `max_tokens` with HTTP 400 "Unsupported
        // parameter: 'max_tokens'" — it is deprecated there in favor of
        // `max_completion_tokens` and is outright incompatible with o-series.
        // The retry layer treats 400 as fatal, so every turn dies on the token
        // cap. Other openai-compatible backends (DeepSeek/Kimi/MiniMax) still
        // accept the legacy `max_tokens`, so we gate by `model.provider` — the
        // SAME per-backend dispatch `apply_reasoning` (OCEAN-134) uses — rather
        // than blasting one param everywhere.
        let cap_param = match model.provider.as_str() {
            "openai" => "max_completion_tokens",
            "kimi" if model.supports_dynamic_tools() => "max_completion_tokens",
            _ => "max_tokens",
        };
        body[cap_param] = json!(m);
    }
    if let Some(level) = options.reasoning {
        apply_reasoning(&mut body, model, level);
    }
    if !context.tools.is_empty() {
        let tools: Vec<Value> = context.tools.iter().map(tool_wire).collect();
        body["tools"] = json!(tools);
    }
    if context.tool_choice != crate::types::ToolChoice::Auto {
        body["tool_choice"] = json!(context.tool_choice.as_str());
    }
    Ok(body)
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

/// Picks the bearer token for an openai-completions turn.
///
/// The `OPENAI_API_KEY` env fallback is gated on the routed backend ACTUALLY being
/// OpenAI. Every openai-compatible vendor (DeepSeek, MiniMax, Kimi, GLM, and any
/// custom `base_url`) shares this adapter, so an ungated fallback would bearer-auth
/// the user's OpenAI secret to THAT vendor's host whenever its own key was missing —
/// a cross-provider credential leak, not merely a confusing error. It also used to
/// report `MissingApiKey("openai")` on a DeepSeek turn; the error now names the
/// provider that is actually missing a key.
///
/// `openai_env` is passed in rather than read here so this stays pure and testable
/// without racing on process env in a parallel test run.
fn resolve_api_key(
    provider: &str,
    explicit: Option<&str>,
    openai_env: Option<String>,
) -> Result<String> {
    if let Some(key) = explicit {
        return Ok(key.to_string());
    }
    if provider == "openai" {
        if let Some(key) = openai_env {
            return Ok(key);
        }
    }
    Err(Error::MissingApiKey(provider.to_string()))
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        let api_key = resolve_api_key(
            &model.provider,
            options.api_key.as_deref(),
            std::env::var("OPENAI_API_KEY").ok(),
        )?;
        let base_url = options
            .base_url
            .clone()
            .unwrap_or_else(|| model.base_url.clone());
        if model.supports_dynamic_tools()
            && base_url.trim_end_matches('/') != "https://api.moonshot.ai/v1"
        {
            return Err(Error::UnsupportedProvider(
                "Kimi K3 dynamic tools require the official Moonshot endpoint".into(),
            ));
        }
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let body = build_body(model, context, options)?;
        crate::prompt_capture::capture_request_body(&model.api, &model.provider, &model.id, &body);
        let cancel = options.cancel.clone();
        let extra_headers: BTreeMap<String, String> = options.headers.clone();

        let resp = with_retry(retry_config(), cancel.as_ref(), |_| {
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
        // K3 lifecycle reconstruction keys durable search evidence to the exact
        // routed model. Persist that route identity even if Moonshot reports a
        // dated/snapshot response model string.
        let preserve_requested_model = model.supports_dynamic_tools();
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
                        model: persisted_model_id(
                        &model_id,
                        response_model.as_deref(),
                        preserve_requested_model,
                    ),
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
                    if let Some(d) = u.prompt_tokens_details {
                        usage.cache_read = d.cached_tokens;
                    }
                    if let Some(d) = u.completion_tokens_details {
                        usage.reasoning = d.reasoning_tokens;
                    }
                }
                for choice in chunk.choices {
                    if let Some(reason) = choice.finish_reason {
                        stop = map_finish_reason(reason.as_str());
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

            // OCEAN-142: the safety filter cut the turn off (`content_filter`
            // finish_reason → StopReason::Error via map_finish_reason). If it
            // produced NO usable content (no text — including promoted thinking
            // above — and no tool calls), falling through to Done with
            // error_message: None would let the runtime treat it as a clean,
            // empty success (agent_loop only returns an error for the Error
            // event, not for a Done carrying StopReason::Error). So mirror the
            // in-stream error-frame path (above) and Gemini's blocking path
            // (google.rs) — emit an Error event with a clear message so the
            // filtering is actually surfaced to the user. A partial-but-useful
            // turn (some text/tool calls arrived before the filter) is preserved
            // and falls through to Done unchanged.
            let has_usable_content =
                !text_buf.is_empty() || !thinking_buf.is_empty() || has_tool_calls;
            if is_blocking_empty_turn(stop, has_usable_content) {
                tracing::warn!("OpenAI content filter blocked the response");
                let am = AssistantMessage {
                    content: vec![],
                    api: api.clone(),
                    provider: provider.clone(),
                    model: persisted_model_id(
                        &model_id,
                        response_model.as_deref(),
                        preserve_requested_model,
                    ),
                    usage: usage.clone(),
                    stop_reason: StopReason::Error,
                    error_message: Some(
                        "OpenAI content filter blocked the response (finish_reason: content_filter)"
                            .to_string(),
                    ),
                    timestamp: now_ms(),
                };
                yield Ok(AssistantMessageEvent::Error { reason: StopReason::Error, error: am });
                return;
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
                model: persisted_model_id(
                    &model_id,
                    response_model.as_deref(),
                    preserve_requested_model,
                ),
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

    fn test_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
        super::build_body(model, context, options).expect("request body should build")
    }
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

        let out = convert_messages(None, &messages, &openai_model(), &[]);
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
            image["image_url"]["url"], "data:image/png;base64,AAECAwQ=",
            "image_url data-URL malformed"
        );
    }

    // Text-only user messages keep the simple string form (no regression).
    #[test]
    fn text_only_user_stays_string() {
        let messages = vec![Message::user_text("hello")];
        let out = convert_messages(None, &messages, &openai_model(), &[]);
        assert_eq!(out[0]["content"], "hello");
    }

    // OCEAN-131: tool-result image parity. OpenAI's `role:tool` message can only
    // carry text, so a screenshot returned as a Content::Image tool result was
    // silently dropped and never reached the model. The encoder must keep the
    // textual `role:tool` message AND follow it with a `role:user` message
    // carrying the image as an image_url data-URL part so the model sees it.
    #[test]
    fn tool_result_image_is_followed_by_user_image_url() {
        let messages = vec![Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: "call_123".into(),
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

        let out = convert_messages(None, &messages, &openai_model(), &[]);
        // Two messages: the tool message, then a following user image message.
        assert_eq!(
            out.len(),
            2,
            "expected tool message + user image message, got {out:?}"
        );

        // The tool message keeps the text, unchanged.
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call_123");
        assert_eq!(out[0]["content"], "here is the screenshot");

        // The following user message carries the image as an image_url data-URL —
        // the image is NOT dropped.
        assert_eq!(out[1]["role"], "user");
        let parts = out[1]["content"]
            .as_array()
            .expect("image message content must be an array");
        let image = parts
            .iter()
            .find(|p| p["type"] == "image_url")
            .expect("image_url part missing — tool-result image was dropped");
        assert_eq!(
            image["image_url"]["url"], "data:image/png;base64,AAECAwQ=",
            "image_url data-URL malformed"
        );
    }

    // OCEAN-386: a no-vision model (DeepSeek / Kimi / MiniMax via
    // `openai_compat`) rejects `image_url` parts with a 400, permanently
    // bricking any session whose history holds an image. With
    // `supports_images: false` a user image must degrade to the string form
    // with a bracketed placeholder — no `image_url` anywhere in the payload.
    #[test]
    fn user_image_degrades_to_text_placeholder_without_vision() {
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

        let out = convert_messages(None, &messages, &deepseek_model(), &[]);
        assert_eq!(out.len(), 1);
        let content = &out[0]["content"];
        let text = content.as_str().expect("expected string content form");
        assert!(text.starts_with("describe this"), "text lost: {text}");
        assert!(
            text.contains("[image attached (image/png)"),
            "placeholder missing: {text}"
        );
        assert!(
            !serde_json::to_string(&out).unwrap().contains("image_url"),
            "image_url leaked into a no-vision payload: {out:?}"
        );
    }

    // OCEAN-386: on a no-vision model a screenshot tool result must NOT grow a
    // following `role:user` image message; the tool text carries a note so the
    // model knows the tool returned something visual.
    #[test]
    fn tool_result_image_is_gated_without_vision() {
        let messages = vec![Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: "call_123".into(),
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

        let out = convert_messages(None, &messages, &deepseek_model(), &[]);
        assert_eq!(out.len(), 1, "no follow-up image message: {out:?}");
        assert_eq!(out[0]["role"], "tool");
        let text = out[0]["content"].as_str().expect("tool text");
        assert!(text.starts_with("here is the screenshot"));
        assert!(
            text.contains("[screenshot omitted"),
            "omission note missing: {text}"
        );
        assert!(
            !serde_json::to_string(&out).unwrap().contains("image_url"),
            "image_url leaked into a no-vision payload: {out:?}"
        );
    }

    // A text-only tool result stays a single text-only `role:tool` message —
    // no spurious following user message.
    #[test]
    fn text_only_tool_result_stays_single_tool_message() {
        let messages = vec![Message::ToolResult(crate::types::ToolResultMessage {
            tool_call_id: "call_456".into(),
            tool_name: "read_file".into(),
            content: vec![Content::text("file contents")],
            is_error: false,
            timestamp: now_ms(),
        })];

        let out = convert_messages(None, &messages, &openai_model(), &[]);
        assert_eq!(
            out.len(),
            1,
            "text-only tool result must not add a user message"
        );
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["content"], "file contents");
    }

    // OCEAN-140: a replayed assistant turn carrying a Content::Thinking block must
    // be handled by an EXPLICIT match arm, not the old silent `_ => {}`. Chat
    // Completions has no input shape for assistant reasoning, so the documented
    // behavior is an intentional drop: the thinking text must NOT leak into the
    // assistant message content or tool_calls, while the text + tool call survive.
    #[test]
    fn assistant_thinking_is_explicitly_dropped() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "secret chain of thought".into(),
                    thinking_signature: Some("sig-abc".into()),
                },
                Content::text("the answer is 42"),
                Content::ToolCall {
                    id: "call_1".into(),
                    name: "calc".into(),
                    arguments: serde_json::json!({"x": 1}),
                },
            ],
            api: "chat".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: now_ms(),
        })];

        let out = convert_messages(None, &messages, &openai_model(), &[]);
        assert_eq!(out.len(), 1, "expected a single assistant message");
        let msg = &out[0];
        assert_eq!(msg["role"], "assistant");

        // Visible text survives; thinking text does NOT leak into it.
        assert_eq!(msg["content"], "the answer is 42");
        let serialized = serde_json::to_string(msg).unwrap();
        assert!(
            !serialized.contains("secret chain of thought"),
            "thinking text must not appear anywhere in the encoded message: {serialized}"
        );
        assert!(
            !serialized.contains("sig-abc"),
            "thinking signature must not appear in the encoded message: {serialized}"
        );

        // The tool call still rides along.
        let calls = msg["tool_calls"].as_array().expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "calc");
    }

    // OCEAN-101: a mid-stream error frame must decode into `Chunk.error` so the
    // loop can surface it. Previously this frame parsed with empty `choices` and
    // the turn ended as a clean empty success, hiding the failure.
    #[test]
    fn in_stream_error_frame_is_captured() {
        let raw = r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":429}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("chunk parses");
        let err = chunk
            .error
            .expect("error object must be captured, not dropped");
        let desc = err.describe();
        assert!(desc.contains("rate limited"), "message lost: {desc}");
        assert!(desc.contains("rate_limit_error"), "type lost: {desc}");
        assert!(desc.contains("429"), "code lost: {desc}");
    }

    // OCEAN-142: a `content_filter` finish_reason means the safety filter cut
    // the turn off. With no refusal delta (OCEAN-101) it must NOT collapse into
    // a clean Stop — it has to surface as an error, like Gemini does. The other
    // finish reasons keep their existing mapping.
    #[test]
    fn content_filter_finish_reason_is_error_not_stop() {
        // The exact wire path: a chunk choice carrying only `finish_reason`.
        let raw = r#"{"index":0,"finish_reason":"content_filter"}"#;
        let choice: ChunkChoice = serde_json::from_str(raw).expect("choice parses");
        let reason = choice.finish_reason.expect("finish_reason present");
        assert_eq!(
            map_finish_reason(&reason),
            StopReason::Error,
            "content_filter must surface as an error, not a silent clean Stop",
        );

        // Existing mappings are untouched.
        assert_eq!(map_finish_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_finish_reason("length"), StopReason::Length);
        assert_eq!(map_finish_reason("stop"), StopReason::Stop);
        assert_eq!(map_finish_reason("anything_else"), StopReason::Stop);
    }

    // OCEAN-142: changing the StopReason label is not enough — the runtime's
    // agent loop only RETURNS an error for an `AssistantMessageEvent::Error`
    // event; a `Done` carrying `StopReason::Error` is treated as a completed
    // turn. So a bare `content_filter` with no usable content must take the
    // blocking branch that emits an Error event, exactly like Gemini's blocking
    // path. This asserts the actual user-visible decision, not just the label.
    #[test]
    fn bare_content_filter_takes_blocking_error_branch() {
        // Full path: decode the wire finish_reason, map it, and ask whether the
        // empty turn must surface as an Error event.
        let raw = r#"{"index":0,"finish_reason":"content_filter"}"#;
        let choice: ChunkChoice = serde_json::from_str(raw).expect("choice parses");
        let stop = map_finish_reason(&choice.finish_reason.unwrap());

        // No text, no thinking, no tool calls → blocked/empty → must error.
        assert!(
            is_blocking_empty_turn(stop, /* has_usable_content */ false),
            "bare content_filter with no content must emit an Error event, \
             not a clean Done",
        );

        // A partial-but-useful turn (content arrived before the filter) is
        // preserved, NOT turned into an error.
        assert!(
            !is_blocking_empty_turn(stop, /* has_usable_content */ true),
            "a content_filter turn that produced usable content must be \
             preserved, not errored",
        );

        // Normal terminations never take the blocking branch, even when empty.
        assert!(!is_blocking_empty_turn(StopReason::Stop, false));
        assert!(!is_blocking_empty_turn(StopReason::ToolUse, true));
        assert!(!is_blocking_empty_turn(StopReason::Length, false));
    }

    // OCEAN-134: reasoning-effort parity. `build_body` must translate
    // `options.reasoning` into the right wire param for the routed backend, and
    // must omit it entirely when no reasoning level is set (or it's Off).
    fn openai_model() -> Model {
        Model::openai_gpt_4o()
    }

    fn deepseek_model() -> Model {
        Model::openai_compat(
            "deepseek",
            "deepseek-reasoner",
            "https://api.deepseek.com/v1",
            128_000,
            8_192,
        )
    }

    fn kimi_model(id: &str) -> Model {
        Model::openai_compat(
            "kimi",
            id,
            "https://api.moonshot.ai/v1",
            if id == "kimi-k3" { 1_000_000 } else { 256_000 },
            8_192,
        )
    }

    fn dynamic_tool(name: &str) -> crate::types::Tool {
        crate::types::Tool {
            name: name.into(),
            description: format!("{name} description"),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn kimi_k3_persists_exact_requested_route_over_response_snapshot() {
        assert_eq!(
            persisted_model_id("kimi-k3", Some("kimi-k3-2026-07-16"), true),
            "kimi-k3"
        );
        assert_eq!(
            persisted_model_id("gpt-4o", Some("gpt-4o-2024-08-06"), false),
            "gpt-4o-2024-08-06"
        );
    }

    #[test]
    fn kimi_k3_dynamic_tools_are_a_contentless_system_message() {
        let context = Context {
            messages: vec![Message::user_text("use a calculator")],
            tools: vec![dynamic_tool("search_tools")],
            dynamic_tool_declarations: vec![crate::types::DynamicToolDeclaration {
                tools: vec![dynamic_tool("calculator")],
                before_message: 1,
            }],
            ..Default::default()
        };
        let body = test_body(&kimi_model("kimi-k3"), &context, &StreamOptions::default());
        assert_eq!(body["tools"][0]["function"]["name"], "search_tools");
        let messages = body["messages"].as_array().expect("messages array");
        let injected = messages.last().expect("dynamic system message");
        assert_eq!(injected["role"], "system");
        assert_eq!(injected["tools"][0]["function"]["name"], "calculator");
        assert!(
            injected.get("content").is_none(),
            "Kimi rejects content: {injected}"
        );
        assert_eq!(
            injected.as_object().expect("system object").len(),
            2,
            "dynamic declaration must contain only role and tools: {injected}"
        );
    }

    #[test]
    fn kimi_k3_retains_dynamic_declaration_before_historical_target_call() {
        let assistant_call = |id: &str, name: &str| {
            Message::Assistant(AssistantMessage {
                content: vec![Content::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: json!({}),
                }],
                api: "openai-completions".into(),
                provider: "kimi".into(),
                model: "kimi-k3".into(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: now_ms(),
            })
        };
        let tool_result = |id: &str, name: &str| {
            Message::ToolResult(crate::types::ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: name.into(),
                content: vec![Content::text("ok")],
                is_error: false,
                timestamp: now_ms(),
            })
        };
        let context = Context {
            messages: vec![
                Message::user_text("calculate"),
                assistant_call("search-1", "search_tools"),
                tool_result("search-1", "search_tools"),
                assistant_call("calc-1", "calculator"),
                tool_result("calc-1", "calculator"),
            ],
            tools: vec![dynamic_tool("search_tools")],
            dynamic_tool_declarations: vec![crate::types::DynamicToolDeclaration {
                tools: vec![dynamic_tool("calculator")],
                before_message: 3,
            }],
            ..Default::default()
        };

        let body = test_body(&kimi_model("kimi-k3"), &context, &StreamOptions::default());
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "search-1");
        assert_eq!(messages[3]["role"], "system");
        assert_eq!(messages[3]["tools"][0]["function"]["name"], "calculator");
        assert!(messages[3].get("content").is_none());
        assert_eq!(messages[4]["role"], "assistant");
        assert_eq!(
            messages[4]["tool_calls"][0]["function"]["name"],
            "calculator"
        );
        assert_eq!(messages[5]["tool_call_id"], "calc-1");
    }

    #[test]
    fn kimi_k3_serializes_multiple_declaration_epochs_and_tool_choice_none() {
        let assistant_call = |id: &str, name: &str| {
            Message::Assistant(AssistantMessage {
                content: vec![Content::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: json!({}),
                }],
                api: "openai-completions".into(),
                provider: "kimi".into(),
                model: "kimi-k3".into(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: now_ms(),
            })
        };
        let tool_result = |id: &str, name: &str| {
            Message::ToolResult(crate::types::ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: name.into(),
                content: vec![Content::text("ok")],
                is_error: false,
                timestamp: now_ms(),
            })
        };
        let context = Context {
            messages: vec![
                Message::user_text("run two tools"),
                assistant_call("search-1", "search_tools"),
                tool_result("search-1", "search_tools"),
                assistant_call("echo-1", "echo"),
                tool_result("echo-1", "echo"),
                assistant_call("search-2", "search_tools"),
                tool_result("search-2", "search_tools"),
                assistant_call("gated-1", "gated"),
            ],
            dynamic_tool_declarations: vec![
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("echo")],
                    before_message: 3,
                },
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("gated")],
                    before_message: 7,
                },
            ],
            tool_choice: crate::types::ToolChoice::None,
            ..Default::default()
        };

        let body = test_body(&kimi_model("kimi-k3"), &context, &StreamOptions::default());
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(messages[3]["role"], "system");
        assert_eq!(messages[3]["tools"][0]["function"]["name"], "echo");
        assert!(messages[3].get("content").is_none());
        assert_eq!(messages[8]["role"], "system");
        assert_eq!(messages[8]["tools"][0]["function"]["name"], "gated");
        assert!(messages[8].get("content").is_none());
    }

    #[test]
    fn invalid_dynamic_declaration_groups_fail_closed() {
        let model = kimi_model("kimi-k3");
        let invalid = [
            vec![crate::types::DynamicToolDeclaration {
                tools: Vec::new(),
                before_message: 0,
            }],
            vec![crate::types::DynamicToolDeclaration {
                tools: vec![dynamic_tool("echo")],
                before_message: 1,
            }],
            vec![
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("echo")],
                    before_message: 1,
                },
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("gated")],
                    before_message: 0,
                },
            ],
        ];
        for declarations in invalid {
            let messages = if declarations.len() == 1 && !declarations[0].tools.is_empty() {
                Vec::new()
            } else {
                vec![Message::user_text("one")]
            };
            let context = Context {
                messages,
                dynamic_tool_declarations: declarations,
                ..Default::default()
            };
            super::build_body(&model, &context, &StreamOptions::default())
                .expect_err("invalid declaration group must fail closed");
        }

        let duplicate = Context {
            dynamic_tool_declarations: vec![
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("echo")],
                    before_message: 0,
                },
                crate::types::DynamicToolDeclaration {
                    tools: vec![dynamic_tool("echo")],
                    before_message: 0,
                },
            ],
            ..Default::default()
        };
        super::build_body(&model, &duplicate, &StreamOptions::default())
            .expect_err("duplicate declaration must fail closed");
    }

    #[test]
    fn dynamic_tools_fail_closed_for_kimi_k2_and_other_models() {
        let context = Context {
            dynamic_tool_declarations: vec![crate::types::DynamicToolDeclaration {
                tools: vec![dynamic_tool("calculator")],
                before_message: 0,
            }],
            ..Default::default()
        };
        for model in [kimi_model("kimi-k2.6"), openai_model()] {
            let error = super::build_body(&model, &context, &StreamOptions::default())
                .expect_err("unsupported dynamic declaration must fail closed");
            assert!(matches!(error, Error::UnsupportedProvider(_)));
        }
    }

    #[test]
    fn kimi_k2_wire_stays_unchanged_without_dynamic_tools() {
        let options = StreamOptions {
            temperature: Some(0.6),
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let body = test_body(&kimi_model("kimi-k2.6"), &Context::default(), &options);
        assert!(
            (body["temperature"].as_f64().expect("temperature") - 0.6).abs() < 1e-6,
            "K2 temperature remains on the wire: {body}"
        );
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body["messages"].as_array().expect("messages").is_empty());
    }

    #[test]
    fn kimi_k3_omits_temperature_sets_max_reasoning_and_replays_same_model_thinking() {
        let history = Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "preserved reasoning".into(),
                    thinking_signature: None,
                },
                Content::text("visible"),
            ],
            api: "openai-completions".into(),
            provider: "kimi".into(),
            model: "kimi-k3".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        });
        let context = Context {
            messages: vec![history],
            ..Default::default()
        };
        let options = StreamOptions {
            temperature: Some(0.2),
            max_tokens: Some(8_192),
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let body = test_body(&kimi_model("kimi-k3"), &context, &options);
        assert!(
            body.get("temperature").is_none(),
            "K3 temperature is fixed: {body}"
        );
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_completion_tokens"], 8_192);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            "preserved reasoning"
        );
        assert_eq!(body["messages"][0]["content"], "visible");

        let k2_body = test_body(
            &kimi_model("kimi-k2.6"),
            &context,
            &StreamOptions::default(),
        );
        assert!(
            k2_body["messages"][0].get("reasoning_content").is_none(),
            "K3 reasoning must not replay cross-model: {k2_body}"
        );
    }

    #[test]
    fn build_body_omits_reasoning_when_unset() {
        let body = test_body(
            &openai_model(),
            &Context::default(),
            &StreamOptions::default(),
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must not be sent when options.reasoning is None: {body}"
        );
        assert!(
            body.get("thinking").is_none(),
            "thinking must not be sent: {body}"
        );
    }

    #[test]
    fn build_body_omits_reasoning_when_off() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let body = test_body(&openai_model(), &Context::default(), &opts);
        assert!(
            body.get("reasoning_effort").is_none(),
            "ThinkingLevel::Off must not emit reasoning_effort: {body}"
        );
    }

    #[test]
    fn build_body_emits_openai_reasoning_effort() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let body = test_body(&openai_model(), &Context::default(), &opts);
        assert_eq!(
            body["reasoning_effort"], "high",
            "OpenAI o-series must receive top-level reasoning_effort: {body}"
        );
        // OpenAI does not use the DeepSeek `thinking` toggle.
        assert!(
            body.get("thinking").is_none(),
            "thinking toggle is DeepSeek-only: {body}"
        );
    }

    #[test]
    fn build_body_maps_openai_levels() {
        for (level, expected) in [
            (ThinkingLevel::Minimal, "minimal"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Xhigh, "high"),
        ] {
            let opts = StreamOptions {
                reasoning: Some(level),
                ..Default::default()
            };
            let body = test_body(&openai_model(), &Context::default(), &opts);
            assert_eq!(
                body["reasoning_effort"], expected,
                "level {level:?} mismapped: {body}"
            );
        }
    }

    // DeepSeek nests the effort inside the thinking object. A top-level
    // `reasoning_effort` is NOT part of its contract, and DeepSeek silently drops
    // unknown top-level fields (verified live: a junk param returns HTTP 200), so
    // the old shape failed open — no error, no guarantee the effort was read.
    #[test]
    fn build_body_nests_deepseek_reasoning_effort_inside_thinking() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let body = test_body(&deepseek_model(), &Context::default(), &opts);
        assert_eq!(
            body["thinking"]["type"], "enabled",
            "DeepSeek must enable the thinking toggle: {body}"
        );
        // DeepSeek maps low/medium/high all up to "high".
        assert_eq!(
            body["thinking"]["reasoning_effort"], "high",
            "DeepSeek effort must nest inside `thinking`: {body}"
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "DeepSeek must not receive a top-level reasoning_effort: {body}"
        );
    }

    #[test]
    fn build_body_maps_deepseek_xhigh_to_max() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };
        let body = test_body(&deepseek_model(), &Context::default(), &opts);
        assert_eq!(
            body["thinking"]["reasoning_effort"], "max",
            "DeepSeek xhigh must map to max: {body}"
        );
    }

    // Thinking is ON BY DEFAULT at DeepSeek — a request carrying no `thinking`
    // object still burns reasoning tokens (probed live: ~1.8k-3.1k on a plain
    // math prompt). So `Off` has to say so out loud, or the user pays for
    // reasoning they explicitly turned off. Regression guard for that silent cost.
    #[test]
    fn build_body_explicitly_disables_deepseek_thinking_when_off() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let body = test_body(&deepseek_model(), &Context::default(), &opts);
        assert_eq!(
            body["thinking"]["type"], "disabled",
            "DeepSeek thinking-off must be explicit, not an omitted param: {body}"
        );
        assert!(
            body["thinking"].get("reasoning_effort").is_none(),
            "disabled thinking must not carry an effort: {body}"
        );
    }

    // The leak: a keyless DeepSeek turn with OPENAI_API_KEY set in the environment
    // used to bearer-auth the OpenAI secret straight to api.deepseek.com. The env
    // fallback must be reachable ONLY when the routed backend is really OpenAI.
    #[test]
    fn openai_env_key_never_leaks_to_a_compat_backend() {
        for provider in ["deepseek", "minimax", "kimi", "glm", "openai-compatible"] {
            let err = resolve_api_key(provider, None, Some("sk-openai-secret".into()))
                .expect_err("keyless compat backend must not fall back to OPENAI_API_KEY");
            match err {
                Error::MissingApiKey(p) => assert_eq!(
                    p, provider,
                    "error must name the provider actually missing a key"
                ),
                other => panic!("expected MissingApiKey, got {other:?}"),
            }
        }
    }

    #[test]
    fn openai_env_key_still_serves_a_real_openai_turn() {
        let key = resolve_api_key("openai", None, Some("sk-openai-secret".into()))
            .expect("openai must still accept the env fallback");
        assert_eq!(key, "sk-openai-secret");
        // An explicitly resolved key always wins, whatever the backend.
        let key = resolve_api_key("deepseek", Some("sk-deepseek"), Some("sk-openai".into()))
            .expect("explicit key must be used");
        assert_eq!(key, "sk-deepseek");
    }

    // OpenAI has no thinking toggle — Off there stays an omission.
    #[test]
    fn build_body_omits_reasoning_for_openai_when_off() {
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let body = test_body(&openai_model(), &Context::default(), &opts);
        assert!(
            body.get("reasoning_effort").is_none() && body.get("thinking").is_none(),
            "OpenAI must receive no reasoning params when off: {body}"
        );
    }

    #[test]
    fn build_body_omits_reasoning_for_unknown_backend() {
        // MiniMax / Kimi / arbitrary openai-compat backends have no agreed param;
        // sending one risks a 400, so build_body must leave the body untouched.
        let model = Model::openai_compat(
            "minimax",
            "MiniMax-M2",
            "https://api.minimaxi.com/v1",
            128_000,
            8_192,
        );
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let body = test_body(&model, &Context::default(), &opts);
        assert!(
            body.get("reasoning_effort").is_none(),
            "unknown backend must not receive reasoning_effort: {body}"
        );
        assert!(
            body.get("thinking").is_none(),
            "unknown backend must not receive thinking: {body}"
        );
    }

    // OCEAN-141: token-cap param parity. Real api.openai.com models (o-series,
    // gpt-5-class) on the Chat Completions path reject the deprecated `max_tokens`
    // with HTTP 400 and require `max_completion_tokens`. `build_body` must emit
    // `max_completion_tokens` (NOT `max_tokens`) for the OpenAI-family provider,
    // gating on `model.provider` the same way `apply_reasoning` does.
    #[test]
    fn build_body_emits_max_completion_tokens_for_openai() {
        let opts = StreamOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        let body = test_body(&openai_model(), &Context::default(), &opts);
        assert_eq!(
            body["max_completion_tokens"], 1024,
            "OpenAI must receive max_completion_tokens: {body}"
        );
        // The legacy param must NOT be present — o-series 400s on it.
        assert!(
            body.get("max_tokens").is_none(),
            "OpenAI must not receive the deprecated max_tokens: {body}"
        );
    }

    // Other openai-compatible backends (DeepSeek/Kimi/MiniMax) still accept the
    // legacy `max_tokens`, so build_body must keep emitting it for them — flipping
    // them to max_completion_tokens would risk breaking those gateways.
    #[test]
    fn build_body_keeps_max_tokens_for_openai_compat_backend() {
        let opts = StreamOptions {
            max_tokens: Some(2048),
            ..Default::default()
        };
        let body = test_body(&deepseek_model(), &Context::default(), &opts);
        assert_eq!(
            body["max_tokens"], 2048,
            "openai-compatible backends must keep the legacy max_tokens: {body}"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "openai-compatible backends must not receive max_completion_tokens: {body}"
        );
    }

    // No token cap set → neither param is emitted.
    #[test]
    fn build_body_omits_token_cap_when_unset() {
        let body = test_body(
            &openai_model(),
            &Context::default(),
            &StreamOptions::default(),
        );
        assert!(
            body.get("max_tokens").is_none(),
            "max_tokens must be absent when unset: {body}"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "max_completion_tokens must be absent when unset: {body}"
        );
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

    // OCEAN-158: a usage payload carrying `prompt_tokens_details.cached_tokens`
    // must decode that count so it can populate usage.cache_read. Without the
    // detail field the cache-read HUD shows a structural 0 for OpenAI users even
    // when the prompt was cached.
    #[test]
    fn usage_decodes_cached_tokens_into_cache_read() {
        let raw = r#"{
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 40,
                "total_tokens": 1240,
                "prompt_tokens_details": {"cached_tokens": 1024}
            }
        }"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("usage chunk parses");
        let u = chunk.usage.expect("usage present");
        assert_eq!(
            u.prompt_tokens_details
                .expect("details present")
                .cached_tokens,
            1024,
            "cached_tokens must decode from prompt_tokens_details"
        );
    }

    // OCEAN-164: a usage payload carrying
    // `completion_tokens_details.reasoning_tokens` must decode that count so it
    // can populate usage.reasoning. Reasoning is already inside completion_tokens,
    // so this is surfaced only to show the reasoning subset of output.
    #[test]
    fn usage_decodes_reasoning_tokens() {
        let raw = r#"{
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 240,
                "total_tokens": 1440,
                "completion_tokens_details": {"reasoning_tokens": 200}
            }
        }"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("usage chunk parses");
        let u = chunk.usage.expect("usage present");
        assert_eq!(
            u.completion_tokens_details
                .expect("details present")
                .reasoning_tokens,
            200,
            "reasoning_tokens must decode from completion_tokens_details"
        );
    }

    // OCEAN-198: the finish_reason mapper must be exhaustive over OpenAI's wire
    // values AND fall back to a clean Stop for unknown/empty values, never
    // panicking. An unexpected string the API might add later must default to Stop.
    #[test]
    fn map_finish_reason_handles_unknown_and_empty() {
        // Every documented value.
        assert_eq!(map_finish_reason("stop"), StopReason::Stop);
        assert_eq!(map_finish_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_finish_reason("length"), StopReason::Length);
        assert_eq!(map_finish_reason("content_filter"), StopReason::Error);
        // Unknown / future / empty values must NOT panic — default to Stop.
        assert_eq!(map_finish_reason("function_call"), StopReason::Stop);
        assert_eq!(map_finish_reason(""), StopReason::Stop);
        assert_eq!(map_finish_reason("🤖"), StopReason::Stop);
    }

    // OCEAN-198: a single tool call delivered across several `delta.tool_calls`
    // fragments (id+name on the first, argument JSON split across the rest) must
    // assemble — by upstream `index` — into one complete call. This is the exact
    // accumulation the stream loop does into `PartialToolCall`, exercised through
    // the real wire decode of each chunk.
    #[test]
    fn tool_call_assembles_across_multiple_deltas() {
        // Frame 1: id + name, empty args. Frames 2-4: argument fragments.
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]}}]}"#,
        ];

        let mut acc: std::collections::BTreeMap<usize, PartialToolCall> = Default::default();
        for raw in frames {
            let chunk: Chunk = serde_json::from_str(raw).expect("tool-call chunk parses");
            let delta = chunk.choices[0].delta.as_ref().expect("delta present");
            for tc in &delta.tool_calls {
                let entry = acc.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    entry.id = id.clone();
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        entry.name = n.clone();
                    }
                    if let Some(a) = &f.arguments {
                        entry.args.push_str(a);
                    }
                }
            }
        }

        assert_eq!(acc.len(), 1, "all fragments belong to one call");
        let call = &acc[&0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "search");
        let parsed: Value = serde_json::from_str(&call.args).expect("assembled args must parse");
        assert_eq!(parsed["q"], "rust");
    }

    // OCEAN-198: parallel tool calls arrive INTERLEAVED across frames and must
    // stay separated by their upstream `index` — call 0's args never leak into
    // call 1. This guards the BTreeMap keying that makes OpenAI parallel tool
    // calls correct.
    #[test]
    fn parallel_tool_calls_keyed_by_index_do_not_collide() {
        let frames = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_A","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_B","function":{"name":"get_time","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"SF\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"tz\":\"UTC\"}"}}]}}]}"#,
        ];
        let mut acc: std::collections::BTreeMap<usize, PartialToolCall> = Default::default();
        for raw in frames {
            let chunk: Chunk = serde_json::from_str(raw).expect("chunk parses");
            let delta = chunk.choices[0].delta.as_ref().expect("delta present");
            for tc in &delta.tool_calls {
                let entry = acc.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    entry.id = id.clone();
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        entry.name = n.clone();
                    }
                    if let Some(a) = &f.arguments {
                        entry.args.push_str(a);
                    }
                }
            }
        }
        assert_eq!(acc.len(), 2, "two distinct parallel calls");
        assert_eq!(acc[&0].id, "call_A");
        assert_eq!(acc[&0].name, "get_weather");
        assert_eq!(acc[&0].args, "{\"city\":\"SF\"}");
        assert_eq!(acc[&1].id, "call_B");
        assert_eq!(acc[&1].name, "get_time");
        assert_eq!(acc[&1].args, "{\"tz\":\"UTC\"}");
    }

    // OCEAN-198: a tool call whose args never streamed must finalize to `{}`, and
    // a malformed/truncated arg buffer must degrade to `{}` via unwrap_or — never
    // panic the turn. This mirrors the loop's finalize-args fallback.
    #[test]
    fn tool_call_empty_and_malformed_args_default_to_empty_object() {
        let empty = String::new();
        let args: Value = if empty.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&empty).unwrap_or(Value::Object(Default::default()))
        };
        assert_eq!(args, json!({}), "empty args finalize to an empty object");

        let truncated = "{\"q\": \"ru".to_string();
        let args2: Value =
            serde_json::from_str(&truncated).unwrap_or(Value::Object(Default::default()));
        assert_eq!(
            args2,
            json!({}),
            "malformed args fall back to an empty object, not panic"
        );
    }

    // OCEAN-198: a chunk with NO usage field decodes cleanly to `None` (not an
    // error), so the loop leaves usage at its zero default instead of panicking.
    // A frame carrying only choices is the common case and must not require usage.
    #[test]
    fn chunk_without_usage_decodes_to_none() {
        let raw = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("usage-less chunk parses");
        assert!(
            chunk.usage.is_none(),
            "absent usage must decode to None, not error"
        );
    }

    // OCEAN-198: a usage object with no detail sub-objects decodes with both
    // details None — cache_read / reasoning then stay at the zero default instead
    // of the loop unwrapping a missing field.
    #[test]
    fn usage_without_details_leaves_cache_and_reasoning_zero() {
        let raw = r#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("usage chunk parses");
        let u = chunk.usage.expect("usage present");
        assert_eq!(u.prompt_tokens, 10);
        assert!(
            u.prompt_tokens_details.is_none(),
            "no cached-token detail → None"
        );
        assert!(
            u.completion_tokens_details.is_none(),
            "no reasoning detail → None"
        );
    }

    // OCEAN-101 / OCEAN-198: StreamError.describe must produce a useful string
    // across the field-presence matrix — full (type+message+code), message-only,
    // and the fully-empty frame (must still say *something*, never an empty
    // string or a panic).
    #[test]
    fn stream_error_describe_handles_partial_and_empty() {
        // Fully populated.
        let raw = r#"{"error":{"message":"boom","type":"server_error","code":"e_500"}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("parses");
        let d = chunk.error.unwrap().describe();
        assert!(
            d.contains("server_error") && d.contains("boom") && d.contains("e_500"),
            "full: {d}"
        );

        // Message only — no type, no code.
        let raw = r#"{"error":{"message":"just a message"}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("parses");
        let d = chunk.error.unwrap().describe();
        assert_eq!(
            d, "just a message",
            "message-only must be exactly the message: {d}"
        );

        // Completely empty error object — must still produce a non-empty,
        // human-readable fallback, not "".
        let raw = r#"{"error":{}}"#;
        let chunk: Chunk = serde_json::from_str(raw).expect("parses");
        let d = chunk.error.unwrap().describe();
        assert!(!d.is_empty(), "empty error must still describe itself");
        assert!(
            d.contains("in-stream error"),
            "empty error falls back to a generic message: {d}"
        );
    }

    // OCEAN-198: a structurally unexpected/garbage frame is NOT a hard failure —
    // the loop's `serde_json::from_str(...)` skip path relies on a malformed frame
    // returning Err so it can be logged-and-skipped rather than aborting the
    // stream. Assert both: a junk frame errors (→ skipped), and an empty `{}`
    // chunk decodes to an all-empty Chunk (→ harmless no-op), neither panics.
    #[test]
    fn malformed_frame_errors_and_empty_object_is_harmless() {
        // Not even JSON → Err, which the loop turns into a debug-log + skip.
        assert!(
            serde_json::from_str::<Chunk>("this is not json").is_err(),
            "a non-JSON frame must surface as a decode error so the loop skips it"
        );
        // A valid-but-empty object decodes to a Chunk with no choices/usage/error.
        let chunk: Chunk = serde_json::from_str("{}").expect("empty object decodes");
        assert!(chunk.choices.is_empty());
        assert!(chunk.usage.is_none());
        assert!(chunk.error.is_none());
    }
}
