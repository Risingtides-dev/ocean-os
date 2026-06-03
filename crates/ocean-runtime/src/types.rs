use std::sync::Arc;

use async_trait::async_trait;
use ocean_protocol::{Content, Message, Model, StreamOptions, ThinkingLevel, Tool};
use serde_json::Value;

/// Side-effect events a tool may emit during execution. The agent loop forwards
/// these onto the event bus after the tool returns. This avoids threading a
/// mutable event sender through every tool's `execute` signature.
#[derive(Debug, Clone)]
pub enum ToolSideEffect {
    Render {
        id: String,
        kind: String,
        props: Value,
        replace: bool,
    },
    Unmount {
        id: String,
    },
    /// Signals the daemon that a browser tool started (`active: true`) or the
    /// turn's browser work is winding down (`active: false`). The daemon relays
    /// this onto the SSE bus as `browser-active` / `browser-idle` so the
    /// extension side panel can auto-focus / release.
    BrowserActivity {
        active: bool,
    },
}

/// A live result returned from a tool execution.
#[derive(Debug, Clone, Default)]
pub struct AgentToolResult {
    pub content: Vec<Content>,
    pub details: Value,
    pub terminate: bool,
    /// Optional side-effect events the tool wants emitted after its result.
    /// The agent loop emits these onto the event bus during
    /// `ToolExecutionEnd` handling.
    pub side_effects: Vec<ToolSideEffect>,
}

impl AgentToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(s)],
            details: Value::Null,
            terminate: false,
            side_effects: Vec::new(),
        }
    }
}

/// Permission outcome for a tool call. Returned by a [`PermissionPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    /// Allow this call and remember the choice for the rest of the run.
    AllowSession,
    /// Deny this call; emit an error tool result with `reason`.
    Deny {
        reason: String,
    },
}

/// User-supplied permission policy. Implementations may prompt interactively or
/// consult a static allow-list.
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    async fn check(&self, tool_name: &str, args: &Value) -> PermissionDecision;
}

/// Always-allow policy — useful for tests and non-interactive runs.
pub struct AllowAllPolicy;

#[async_trait]
impl PermissionPolicy for AllowAllPolicy {
    async fn check(&self, _tool_name: &str, _args: &Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// Tool execution trait — analog of `AgentTool.execute` in TS.
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    /// Whether the tool requires user permission by default. Read-only tools
    /// (`read`, `ls`, `grep`, `glob`) return `false`; mutating or side-effecting
    /// tools (`bash`, `write`, `edit`) return `true`.
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, tool_call_id: &str, args: Value) -> Result<AgentToolResult, String>;
}

pub fn tool_def(t: &dyn AgentTool) -> Tool {
    Tool {
        name: t.name().to_string(),
        description: t.description().to_string(),
        parameters: t.parameters(),
    }
}

/// Agent configuration controlling the loop.
#[derive(Clone)]
pub struct AgentConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub max_turns: u32,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub system_prompt: String,
    pub permission: Arc<dyn PermissionPolicy>,
}

impl AgentConfig {
    pub fn new(model: Model, system_prompt: impl Into<String>) -> Self {
        Self {
            model,
            thinking_level: ThinkingLevel::Off,
            stream_options: StreamOptions::default(),
            max_turns: 32,
            tools: Vec::new(),
            system_prompt: system_prompt.into(),
            permission: Arc::new(AllowAllPolicy),
        }
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_turns(mut self, n: u32) -> Self {
        self.max_turns = n;
        self
    }

    pub fn with_permission(mut self, p: Arc<dyn PermissionPolicy>) -> Self {
        self.permission = p;
        self
    }

    pub fn with_thinking(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }
}

/// Events emitted by the agent loop, mirroring `AgentEvent` in TS.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    TurnEnd,
    AssistantMessage {
        message: Message,
    },
    UserMessage {
        message: Message,
    },
    /// Streaming text chunk while the assistant types.
    TextDelta {
        delta: String,
    },
    /// Streaming thinking chunk.
    ThinkingDelta {
        delta: String,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        content: Vec<Content>,
    },
    /// Permission denied for a tool call (the loop appended an error tool result).
    PermissionDenied {
        tool_name: String,
        reason: String,
    },
    /// The agent wants the client to mount or update an interactive component.
    /// Clients maintain a component registry per session.
    Render {
        id: String,
        kind: String,
        props: Value,
        replace: bool,
    },
    /// The agent wants the client to remove a previously rendered component.
    Unmount {
        id: String,
    },
    /// A browser tool started (`active: true`) or browser work wound down
    /// (`active: false`). Drives the extension side-panel auto-focus handoff.
    BrowserActivity {
        active: bool,
    },
}
