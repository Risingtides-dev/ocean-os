//! Ocean Agent SDK — first-class coding-agent primitives for the Ocean runtime.
//!
//! This crate defines the typed product vocabulary shared by the Ocean daemon and
//! all Ocean clients (TUI, CLI, future SDK consumers).  It is intentionally
//! separate from `ocean-core` so the product contract is explicit and isolated.
//!
//! # Design principles
//!
//! - **Session-scoped turns**: every agent action belongs to an `AgentSession` and
//!   an `AgentTurn`.  A session accumulates context across turns.
//!
//! - **Foreground only, explicit tools**: tool calls run immediately and are
//!   streamed to the client.  No background autonomy in V0.
//!
//! - **Typed event stream**: `AgentTurnEvent` is the canonical SSE payload type.
//!   Clients deserialize each `data:` line as JSON.
//!
//! - **SQLite for durability**: sessions, turns, and event logs are the primary
//!   store.  JSONL export is available for debugging but is not authoritative.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable identifier for an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionId(pub Uuid);
impl AgentSessionId {
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn inner(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for AgentSessionId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl From<AgentSessionId> for Uuid {
    fn from(id: AgentSessionId) -> Uuid {
        id.0
    }
}

impl std::fmt::Display for AgentSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for an agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentTurnId(pub Uuid);
impl AgentTurnId {
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn inner(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for AgentTurnId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl From<AgentTurnId> for Uuid {
    fn from(id: AgentTurnId) -> Uuid {
        id.0
    }
}

impl std::fmt::Display for AgentTurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a single tool call within a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallId(pub Uuid);
impl ToolCallId {
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn inner(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for ToolCallId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl From<ToolCallId> for Uuid {
    fn from(id: ToolCallId) -> Uuid {
        id.0
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Human-readable tool name — matches the tool's registered name.
pub type ToolName = String;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A coding-agent session.  Sessions are the top-level container for a
/// collaborative operator–agent conversation scoped to a working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: AgentSessionId,
    /// Short title derived from the first user instruction.
    pub title: String,
    /// Working directory the session operates in.
    pub cwd: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Turn that is currently running in this session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<AgentTurnId>,
}

// ---------------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------------

/// Status of an agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// An agent turn — one operator instruction plus its execution lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurn {
    pub id: AgentTurnId,
    pub session_id: AgentSessionId,
    pub prompt: String,
    pub status: AgentTurnStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Request / Response payloads
// ---------------------------------------------------------------------------

/// Request payload for `POST /v1/agent/turns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnRequest {
    /// Session to submit the turn in.  If omitted a new session is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AgentSessionId>,
    /// The operator's instruction.
    pub prompt: String,
    /// Working directory for the turn.  Required.
    pub cwd: String,
    /// Optional guidance hints passed to the agent (e.g. "focus on tests").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
    /// Optional room identifier for Track-0 room-scoped turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
}

/// Response payload for `POST /v1/agent/turns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnResponse {
    pub ok: bool,
    pub turn_id: AgentTurnId,
    pub session_id: AgentSessionId,
    pub status: AgentTurnStatus,
    /// Included so clients can correlate the HTTP response with the SSE stream.
    pub event_id_prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Summary item returned by `GET /v1/agent/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionSummary {
    pub id: AgentSessionId,
    pub title: String,
    pub cwd: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<AgentTurnId>,
    pub turn_count: u32,
}

/// Response payload for `GET /v1/agent/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionsResponse {
    pub ok: bool,
    #[serde(default)]
    pub sessions: Vec<AgentSessionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response payload for `GET /v1/agent/sessions/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<AgentSession>,
    /// Turns in reverse chronological order (newest first).
    #[serde(default)]
    pub turns: Vec<AgentTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool primitives
// ---------------------------------------------------------------------------

/// One tool call dispatched by the agent during a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub args_json: Value,
}

/// Result of a completed tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
    /// Optional structured metadata (exit code, file count, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<Value>,
}

// ---------------------------------------------------------------------------
// Event stream
// ---------------------------------------------------------------------------

/// All events emitted on `GET /v1/agent/events` and as SSE data payloads.
///
/// Each event is a JSON object string on a single SSE `data:` line.
/// Clients should ignore unrecognised event kinds to remain forward-compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentTurnEvent {
    /// The turn has started processing.
    TurnStarted {
        turn_id: AgentTurnId,
        session_id: AgentSessionId,
    },
    /// Incremental assistant text delta.  Clients append to their output buffer.
    AssistantTextDelta { turn_id: AgentTurnId, delta: String },
    /// Incremental assistant thinking delta (extended-thinking models only).
    /// Clients should display this in a separate, collapsed surface from the
    /// main assistant text — it is not part of the public turn output.
    ThinkingDelta { turn_id: AgentTurnId, delta: String },
    /// A tool call has been dispatched.
    ToolCallStarted {
        turn_id: AgentTurnId,
        call: ToolCall,
    },
    /// Incremental output from a running tool call.
    ToolCallChunk {
        turn_id: AgentTurnId,
        call_id: ToolCallId,
        chunk: String,
    },
    /// A tool call has completed.
    ToolCallFinished {
        turn_id: AgentTurnId,
        call_id: ToolCallId,
        result: ToolResult,
    },
    /// The turn has ended.
    TurnFinished {
        turn_id: AgentTurnId,
        status: AgentTurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Wall-clock duration for the turn, when known by the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wall_ms: Option<u64>,
        /// Approximate visible output token count, when provider usage is not exposed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Approximate visible output tokens per second.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_per_second: Option<f64>,
    },
    /// A session was created as a side-effect of submitting a turn with
    /// `session_id: null`.
    SessionCreated {
        session_id: AgentSessionId,
        title: String,
        cwd: String,
    },
    /// Catch-all for unexpected or extension events.  Includes the raw payload.
    Extension { extension: String, payload: Value },
}

// ---------------------------------------------------------------------------
// Backward-compatibility notes
// ---------------------------------------------------------------------------
// The /v1/agent/* slice is the **product** API.  It replaces the mental model
// of sessions + requests + raw events for TUI/CLI consumers.
//
// Backward-compat map:
//
//   /v1/agent/turns   → equivalent to POST /v1/prompt + implicit session
//   /v1/agent/sessions → equivalent to GET /v1/sessions but with turn metadata
//   /v1/agent/events  → equivalent to GET /v1/events but scoped to AgentTurnEvents
//   GET /v1/requests  → still available; maps to in-flight AgentTurnStatus records
//   GET /v1/sessions  → still available; low-level session persistence view
//   GET /v1/events    → still available; raw OceanEvent stream
//
// V0 policy: all tool calls are foreground-only (no permission ceremony).
// A future /v1/agent/permissions layer can be added without changing events.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_turn_event_serializes_to_tagged_json() {
        let event = AgentTurnEvent::TurnStarted {
            turn_id: AgentTurnId::new_v4(),
            session_id: AgentSessionId::new_v4(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.starts_with(r#"{"type":"turn_started","#));
        assert!(json.contains("\"turn_id\""));
        assert!(json.contains("\"session_id\""));
    }

    #[test]
    fn agent_turn_event_deserializes_round_trip() {
        let event = AgentTurnEvent::ToolCallFinished {
            turn_id: AgentTurnId::new_v4(),
            call_id: ToolCallId::new_v4(),
            result: ToolResult {
                ok: true,
                output: "hello world".into(),
                metadata_json: None,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentTurnEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentTurnEvent::ToolCallFinished { .. }));
    }

    #[test]
    fn agent_turn_request_serde() {
        let req = AgentTurnRequest {
            session_id: None,
            prompt: "list the src directory".into(),
            cwd: "/home/user/project".into(),
            guidance: Some(vec!["be concise".into()]),
            room_id: Some("pm".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"prompt\""));
        assert!(json.contains("\"cwd\""));
        assert!(json.contains("\"room_id\":\"pm\""));
        let back: AgentTurnRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.prompt, "list the src directory");
        assert_eq!(back.room_id.as_deref(), Some("pm"));
    }

    #[test]
    fn agent_session_id_display() {
        let id = AgentSessionId::new_v4();
        let s = id.to_string();
        assert_eq!(s.len(), 36); // standard UUID string
    }
}
