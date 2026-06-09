//! `ocean-protocol` — Unified multi-provider LLM API.
//!
//! Supports Anthropic Messages, OpenAI Chat Completions, Google Gemini, and
//! any OpenAI-compatible endpoint. All providers share the same
//! `Message`/`Tool`/`AssistantMessageEvent` types so the agent loop is
//! provider-agnostic.

pub mod error;
pub mod http;
pub mod providers;
pub mod retry;
pub mod stream;
pub mod types;

pub use error::{Error, Result};
pub use providers::{
    anthropic::AnthropicProvider, codex::CodexProvider, google::GoogleProvider,
    openai::OpenAiProvider, Provider,
};
pub use stream::AssistantMessageEventStream;
pub use types::{
    now_ms, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, Tool, ToolResultMessage, Usage,
};

/// Entry point that mirrors `streamSimple()` in ocean-protocol TS: pick the provider
/// implementation from `model.api` and return a stream of message events.
///
/// `openai-completions` covers OpenAI and every OpenAI-compatible passthrough
/// (OpenRouter, Together, Groq, Cerebras, DeepSeek, Fireworks, xAI, etc.) —
/// just set `Model::base_url` (or `StreamOptions::base_url`) accordingly.
pub async fn stream_simple(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<AssistantMessageEventStream> {
    match model.api.as_str() {
        "anthropic-messages" => {
            AnthropicProvider::new()
                .stream(model, context, options)
                .await
        }
        "openai-completions" => OpenAiProvider::new().stream(model, context, options).await,
        "codex-responses" => CodexProvider::new().stream(model, context, options).await,
        "google-generative-ai" => GoogleProvider::new().stream(model, context, options).await,
        other => Err(Error::UnsupportedProvider(other.into())),
    }
}
