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
    pub context_window: u32,
    pub max_tokens: u32,
}

impl Model {
    /// Anthropic Claude Sonnet 4.6.
    pub fn anthropic_claude_sonnet_4_6() -> Self {
        Self {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
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
            context_window: 1_000_000,
            max_tokens: 8_192,
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
