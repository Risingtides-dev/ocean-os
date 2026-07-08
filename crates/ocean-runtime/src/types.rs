use std::sync::Arc;

use async_trait::async_trait;
use ocean_protocol::{Content, Message, Model, Provider, StreamOptions, ThinkingLevel, Tool};
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
    /// One or more validated `surface_patch` operations the agent emitted for a
    /// canvas. The agent loop forwards this onto the event bus; Slice 3 wires the
    /// daemon to wrap each patch in a [`SurfacePatchEnvelope`] (stamping
    /// session/surface/actor/timestamp) and stream it over `/v1/agent/events`.
    /// The GPUI ledger (Slice 4/6) owns the authoritative revision; the runtime
    /// only reports the patches it accepted.
    ///
    /// [`SurfacePatchEnvelope`]: ocean_agent_sdk::surface::SurfacePatchEnvelope
    SurfacePatch {
        canvas_id: String,
        patches: Vec<ocean_agent_sdk::surface::SurfacePatch>,
    },
    /// A validated `slack_canvas` operation the agent emitted (OCEAN-214 ph2).
    /// The agent loop forwards this onto the event bus; the Slack canvas bridge
    /// (`ocean-agents`, a later phase) round-trips the op to the real Slack Canvas
    /// API (`canvases.create`/`canvases.edit`/`canvases.read`) and, for `read`,
    /// populates the agent-facing contents. The runtime only emits the validated,
    /// contracted op here — it does not itself call Slack.
    SlackCanvas {
        op: ocean_agent_sdk::slack_canvas::SlackCanvasOp,
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

/// How a tool may be scheduled relative to the other tool calls in the same
/// assistant batch.
///
/// The agent loop executes a batch of tool calls by walking it in order and
/// running maximal runs of consecutive [`Shared`](Concurrency::Shared) tools
/// *concurrently*, while an [`Exclusive`](Concurrency::Exclusive) tool acts as a
/// barrier: everything before it finishes first, it runs alone, and everything
/// after it waits. This mirrors oh-my-pi's `executeToolCalls` scheduler — the
/// read-heavy common case (several `read`/`grep`/`glob` in one batch) fans out,
/// while a mutating tool (`write`/`edit`/`bash`) never races a neighbour it
/// could observe or corrupt.
///
/// The default is [`Exclusive`](Concurrency::Exclusive) — the *safe* default:
/// a tool parallelizes only when it explicitly opts in by returning `Shared`,
/// so a new side-effecting tool is never accidentally run concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// Free of side effects a sibling could observe; safe to run concurrently
    /// with other `Shared` tools. Pure reads: `read`, `ls`, `grep`, `glob`,
    /// `web_fetch`.
    Shared,
    /// Mutates state or has side effects; must run alone — a full barrier
    /// against every other tool in the batch. The default.
    Exclusive,
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
    /// How this tool may be scheduled against its batch-mates. Defaults to
    /// [`Concurrency::Exclusive`] (the safe default — a tool runs alone unless it
    /// opts into concurrency). Pure read-only tools override this to
    /// [`Concurrency::Shared`] so a batch of reads fans out. See [`Concurrency`].
    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
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
    /// Total wall-clock deadline for a single turn's LLM stream
    /// (provider request + full stream consumption). `None` falls back to
    /// [`AgentConfig::DEFAULT_TURN_TIMEOUT_SECS`]. A hung or slow provider that
    /// exceeds this window aborts the turn with [`crate::error::AgentError::Timeout`].
    pub turn_timeout_secs: Option<u32>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub system_prompt: String,
    pub permission: Arc<dyn PermissionPolicy>,
    /// Session this run belongs to, when known. Every [`AgentEvent`] the loop
    /// emits is stamped with this id so downstream consumers (daemon SSE bridge,
    /// SDK) can route events without re-attaching session context themselves.
    /// `None` for ad-hoc runs (tests, embedded use) that have no session.
    pub session_id: Option<String>,
    /// Optional provider override. When `None` (the production default) the loop
    /// dispatches through [`ocean_protocol::stream_simple`], picking the real
    /// provider from `model.api`. When `Some`, the loop streams from this
    /// provider instead — the injection seam that lets end-to-end tests drive
    /// the real agent loop against a scripted [`Provider`] with no network or
    /// credentials. See `with_provider` and the e2e tests in `tests/`.
    pub provider: Option<Arc<dyn Provider>>,
}

impl AgentConfig {
    /// Default total per-turn deadline when `turn_timeout_secs` is `None`.
    pub const DEFAULT_TURN_TIMEOUT_SECS: u32 = 300;

    pub fn new(model: Model, system_prompt: impl Into<String>) -> Self {
        Self {
            model,
            thinking_level: ThinkingLevel::Off,
            stream_options: StreamOptions::default(),
            max_turns: 32,
            turn_timeout_secs: None,
            tools: Vec::new(),
            system_prompt: system_prompt.into(),
            permission: Arc::new(AllowAllPolicy),
            session_id: None,
            provider: None,
        }
    }

    /// Attach the session id this run belongs to. Stamped onto every emitted
    /// [`AgentEvent`].
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Resolved per-turn deadline in seconds (configured value or the default).
    pub fn turn_timeout_secs(&self) -> u32 {
        self.turn_timeout_secs
            .unwrap_or(Self::DEFAULT_TURN_TIMEOUT_SECS)
    }

    pub fn with_turn_timeout_secs(mut self, secs: Option<u32>) -> Self {
        self.turn_timeout_secs = secs;
        self
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

    /// Inject a provider for the loop to stream from instead of the real
    /// `stream_simple` dispatch. The injection seam that lets tests drive the
    /// agent loop end-to-end against a scripted [`Provider`] — no network, no
    /// credentials. Production never sets this (the field stays `None`).
    pub fn with_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }
}

/// Events emitted by the agent loop, mirroring `AgentEvent` in TS.
///
/// Every variant carries a `session_id: Option<String>` — the session the run
/// belongs to, stamped by the loop from [`AgentConfig::session_id`] at every
/// emit site. Downstream consumers (the daemon SSE bridge, the SDK) can route
/// events by this id natively instead of re-attaching session context after the
/// fact. It is `None` only for ad-hoc runs with no session (tests, embedded use).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart {
        session_id: Option<String>,
    },
    AgentEnd {
        session_id: Option<String>,
        messages: Vec<Message>,
    },
    TurnStart {
        session_id: Option<String>,
    },
    TurnEnd {
        session_id: Option<String>,
    },
    AssistantMessage {
        session_id: Option<String>,
        message: Message,
    },
    UserMessage {
        session_id: Option<String>,
        message: Message,
    },
    /// Streaming text chunk while the assistant types.
    TextDelta {
        session_id: Option<String>,
        delta: String,
    },
    /// Streaming thinking chunk.
    ThinkingDelta {
        session_id: Option<String>,
        delta: String,
    },
    ToolExecutionStart {
        session_id: Option<String>,
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionEnd {
        session_id: Option<String>,
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        content: Vec<Content>,
        /// Structured tool-result metadata produced by the tool
        /// (`AgentToolResult.details`): exit codes, counts, perf, etc.
        /// `Value::Null` when the tool produced no structured detail.
        details: Value,
    },
    /// Permission denied for a tool call (the loop appended an error tool result).
    PermissionDenied {
        session_id: Option<String>,
        tool_name: String,
        reason: String,
    },
    /// The agent wants the client to mount or update an interactive component.
    /// Clients maintain a component registry per session.
    Render {
        session_id: Option<String>,
        id: String,
        kind: String,
        props: Value,
        replace: bool,
    },
    /// The agent wants the client to remove a previously rendered component.
    Unmount {
        session_id: Option<String>,
        id: String,
    },
    /// A browser tool started (`active: true`) or browser work wound down
    /// (`active: false`). Drives the extension side-panel auto-focus handoff.
    BrowserActivity {
        session_id: Option<String>,
        active: bool,
    },
    /// The agent applied one or more validated `surface_patch` operations to a
    /// canvas (GPUI Masterbuild Slice 3). The daemon stamps each patch into a
    /// `SurfacePatchEnvelope` and relays it onto `/v1/agent/events` as
    /// `AgentTurnEvent::SurfacePatch`, scoped to this session.
    SurfacePatch {
        session_id: Option<String>,
        canvas_id: String,
        patches: Vec<ocean_agent_sdk::surface::SurfacePatch>,
    },
    /// The agent emitted a validated `slack_canvas` op (OCEAN-214 ph2). The agent
    /// loop forwards it onto the event bus scoped to this session; the Slack canvas
    /// bridge (`ocean-agents`, a later phase) consumes it and round-trips to the
    /// Slack Canvas API. Mirrors [`AgentEvent::SurfacePatch`].
    SlackCanvas {
        session_id: Option<String>,
        op: ocean_agent_sdk::slack_canvas::SlackCanvasOp,
    },
}

impl AgentEvent {
    /// The session this event belongs to, if the run had one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            AgentEvent::AgentStart { session_id }
            | AgentEvent::AgentEnd { session_id, .. }
            | AgentEvent::TurnStart { session_id }
            | AgentEvent::TurnEnd { session_id }
            | AgentEvent::AssistantMessage { session_id, .. }
            | AgentEvent::UserMessage { session_id, .. }
            | AgentEvent::TextDelta { session_id, .. }
            | AgentEvent::ThinkingDelta { session_id, .. }
            | AgentEvent::ToolExecutionStart { session_id, .. }
            | AgentEvent::ToolExecutionEnd { session_id, .. }
            | AgentEvent::PermissionDenied { session_id, .. }
            | AgentEvent::Render { session_id, .. }
            | AgentEvent::Unmount { session_id, .. }
            | AgentEvent::BrowserActivity { session_id, .. }
            | AgentEvent::SurfacePatch { session_id, .. }
            | AgentEvent::SlackCanvas { session_id, .. } => session_id.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_protocol::Model;

    fn cfg() -> AgentConfig {
        AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
    }

    #[test]
    fn turn_timeout_defaults_to_300s_when_unset() {
        let c = cfg();
        assert_eq!(c.turn_timeout_secs, None, "field starts unset");
        assert_eq!(
            c.turn_timeout_secs(),
            AgentConfig::DEFAULT_TURN_TIMEOUT_SECS,
            "unset resolves to the default"
        );
        assert_eq!(AgentConfig::DEFAULT_TURN_TIMEOUT_SECS, 300);
    }

    #[test]
    fn with_turn_timeout_secs_overrides_the_default() {
        let c = cfg().with_turn_timeout_secs(Some(45));
        assert_eq!(c.turn_timeout_secs, Some(45));
        assert_eq!(
            c.turn_timeout_secs(),
            45,
            "explicit value wins over default"
        );
    }

    #[test]
    fn with_turn_timeout_secs_none_falls_back_to_default() {
        // Passing None (e.g. OCEAN_TURN_TIMEOUT_SECS unset upstream) must keep
        // the resolved deadline at the default, never zero.
        let c = cfg().with_turn_timeout_secs(None);
        assert_eq!(
            c.turn_timeout_secs(),
            AgentConfig::DEFAULT_TURN_TIMEOUT_SECS
        );
    }
}
