use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        thinking_signature: Option<String>,
    },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into() }
    }
    pub fn as_text(&self) -> Option<&str> {
        if let Content::Text { text } = self {
            Some(text)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// OCEAN-164: reasoning/thinking tokens billed by the provider on reasoning
    /// models. This is a SUBSET of `output` (and therefore of `total_tokens`),
    /// not an additional bucket — every provider already counts reasoning inside
    /// its output/total figures:
    ///   - OpenAI Chat Completions: `completion_tokens_details.reasoning_tokens`
    ///     ⊆ `completion_tokens`
    ///   - OpenAI/Codex Responses: `output_tokens_details.reasoning_tokens`
    ///     ⊆ `output_tokens`
    ///   - Gemini: `thoughtsTokenCount` ⊆ `totalTokenCount`
    ///   - Anthropic: thinking is billed inside `output_tokens`; there is no
    ///     separate usage field, so this stays 0 on Anthropic.
    ///
    /// It is surfaced so the HUD can show how much of `output` was reasoning;
    /// it must NOT be added into `total_tokens` (that would double-count).
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    #[serde(rename = "user")]
    User {
        content: Vec<Content>,
        #[serde(default = "now_ms")]
        timestamp: i64,
    },
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<Content>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error_message: Option<String>,
    #[serde(default = "now_ms")]
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<Content>,
    pub is_error: bool,
    #[serde(default = "now_ms")]
    pub timestamp: i64,
}

impl Message {
    pub fn user_text(s: impl Into<String>) -> Self {
        Message::User {
            content: vec![Content::text(s)],
            timestamp: now_ms(),
        }
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Tool schema definition matching the unified ocean-protocol Tool interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool parameters (object schema).
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

/// How a provider should present its credential on the request wire.
///
/// Carries no secret material — the token still arrives via
/// [`StreamOptions::api_key`]. The enum only selects the header convention.
///
/// Defaults to [`AuthMethod::ApiKey`], the Anthropic Messages convention of an
/// `x-api-key: <secret>` header, so existing callers using default options keep
/// their historical wire shape. [`AuthMethod::Bearer`] is for OAuth-style
/// providers (e.g. Claude Code plan OAuth, OpenAI Codex OAuth) whose credential
/// is an access token sent as `authorization: Bearer <secret>` instead of an
/// API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// API-key auth: `x-api-key: <secret>`. Anthropic default; preserved for
    /// backward compatibility.
    #[default]
    ApiKey,
    /// OAuth bearer auth: `authorization: Bearer <secret>`.
    Bearer,
}

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    /// Cancellation token honored by the provider HTTP client and SSE loop.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// Override base URL (used by OpenAI-compatible passthrough providers).
    pub base_url: Option<String>,
    /// Optional custom request headers.
    pub headers: std::collections::BTreeMap<String, String>,
    /// How to present `api_key` on the wire. Defaults to [`AuthMethod::ApiKey`],
    /// preserving the historical Anthropic `x-api-key` behavior for callers
    /// using default options.
    pub auth: AuthMethod,
}

/// Model descriptor — analogous to the Model<TApi> interface in ocean-protocol.
#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    /// Whether the model's API accepts image content parts. When false the
    /// encoders degrade `Content::Image` to a short text placeholder instead
    /// of emitting `image_url` parts — strict text-only OpenAI-compatible
    /// backends (DeepSeek, Kimi, MiniMax) reject unknown content variants with
    /// a 400, which permanently bricks any session whose history contains an
    /// image (OCEAN-386).
    pub supports_images: bool,
    pub context_window: u32,
    pub max_tokens: u32,
}

impl Model {
    /// Anthropic Claude Sonnet 5 — current Sonnet generation. Verified live
    /// against api.anthropic.com (2026-07-08): the undated id is recognized on
    /// both API-key and Claude-subscription OAuth auth.
    pub fn anthropic_claude_sonnet_5() -> Self {
        Self {
            id: "claude-sonnet-5".into(),
            name: "Claude Sonnet 5".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 16_384,
        }
    }

    /// Anthropic Claude Opus 4.8 — current Opus generation (verified 2026-07-08).
    pub fn anthropic_claude_opus_4_8() -> Self {
        Self {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 16_384,
        }
    }

    /// Anthropic Claude Haiku 4.5 — current Haiku generation (verified
    /// 2026-07-08; the undated alias resolves).
    pub fn anthropic_claude_haiku_4_5() -> Self {
        Self {
            id: "claude-haiku-4-5".into(),
            name: "Claude Haiku 4.5".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 16_384,
        }
    }

    /// Anthropic Claude Fable 5 — Mythos-class tier above Opus (verified
    /// recognized on subscription OAuth 2026-07-08).
    pub fn anthropic_claude_fable_5() -> Self {
        Self {
            id: "claude-fable-5".into(),
            name: "Claude Fable 5".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 16_384,
        }
    }

    /// Anthropic Claude Sonnet 4.6 (LEGACY — kept so sessions pinned to the
    /// old id still resolve; not in the public model menu).
    pub fn anthropic_claude_sonnet_4_6() -> Self {
        Self {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 8_192,
        }
    }

    pub fn anthropic_claude_opus_4_7() -> Self {
        Self {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_images: true,
            context_window: 200_000,
            max_tokens: 8_192,
        }
    }

    pub fn openai_gpt_4o_mini() -> Self {
        Self {
            id: "gpt-4o-mini".into(),
            name: "GPT-4o mini".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            supports_images: true,
            context_window: 128_000,
            max_tokens: 16_384,
        }
    }

    pub fn openai_gpt_4o() -> Self {
        Self {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            supports_images: true,
            context_window: 128_000,
            max_tokens: 16_384,
        }
    }

    pub fn gemini_2_0_flash() -> Self {
        Self {
            id: "gemini-2.0-flash".into(),
            name: "Gemini 2.0 Flash".into(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            reasoning: false,
            supports_images: true,
            context_window: 1_000_000,
            max_tokens: 8_192,
        }
    }

    /// Codex (Responses API) over a ChatGPT/Codex OAuth subscription token.
    /// Used for `gpt-5.x` driven without an API key.
    pub fn codex(id: impl Into<String>, context_window: u32, max_tokens: u32) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            api: "codex-responses".into(),
            provider: "openai-codex".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            reasoning: true,
            supports_images: true,
            context_window,
            max_tokens,
        }
    }

    /// OpenAI-compatible passthrough: any provider whose API matches OpenAI Chat
    /// Completions (OpenRouter, Together, Groq, Cerebras, DeepSeek, Fireworks,
    /// xAI, etc.). Construct via `openai_compat("groq", "...", "...", ...)`
    /// or by overriding `base_url`.
    pub fn openai_compat(
        provider: impl Into<String>,
        id: impl Into<String>,
        base_url: impl Into<String>,
        context_window: u32,
        max_tokens: u32,
    ) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            api: "openai-completions".into(),
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: false,
            // Text-only by default: the compat lane serves DeepSeek / Kimi /
            // MiniMax / custom gateways, none of which take image parts. A
            // caller that KNOWS its compat endpoint is vision-capable flips
            // this after construction.
            supports_images: false,
            context_window,
            max_tokens,
        }
    }
}

/// Events emitted by streaming completions, matching `AssistantMessageEvent` in ocean-protocol.
#[derive(Debug, Clone)]
pub enum AssistantMessageEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolCallStart {
        content_index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        id: String,
        name: String,
        arguments: Value,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}
