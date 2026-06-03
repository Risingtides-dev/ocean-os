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
    /// The project this turn belongs to. When set with an empty `cwd`, the
    /// daemon binds the turn to the project's `workspace_root` — letting a thin
    /// client steer by project id without re-resolving directory paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<uuid::Uuid>,
    /// Identifies the client surface so the agent can tailor responses.
    /// Known values: "tui", "surface-web", "surface-gpui", "surface-native", "cli", "leo-voice"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<String>,
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
        /// Model id driving this turn (e.g. "deepseek-v4-pro"). Lets clients
        /// show the live model and reflect a mid-session model swap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Incremental assistant text delta.  Clients append to their output buffer.
    AssistantTextDelta {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        delta: String,
    },
    /// Incremental assistant thinking delta (extended-thinking models only).
    /// Clients should display this in a separate, collapsed surface from the
    /// main assistant text — it is not part of the public turn output.
    ThinkingDelta {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        delta: String,
    },
    /// A tool call has been dispatched.
    ToolCallStarted {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        call: ToolCall,
    },
    /// Incremental output from a running tool call.
    ToolCallChunk {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        call_id: ToolCallId,
        chunk: String,
    },
    /// A tool call has completed.
    ToolCallFinished {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        call_id: ToolCallId,
        result: ToolResult,
    },
    /// The turn has ended.
    TurnFinished {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        status: AgentTurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Wall-clock duration for the turn, when known by the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wall_ms: Option<u64>,
        /// Output token count for the turn. Real provider usage when available,
        /// else an estimate from visible text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Input (prompt) token count for the turn, summed across rounds, from
        /// real provider usage. `None`/0 when the provider reported none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Cache-read (prompt-cache hit) tokens for the turn, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        /// Output tokens per second (output_tokens / wall time).
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
    /// The agent wants the client to mount or update an interactive component.
    /// Clients maintain a component registry per session and render these
    /// as live UI elements (kanban, forms, tables, etc.).
    ComponentRender {
        session_id: AgentSessionId,
        /// Opaque component id chosen by the agent.
        component_id: String,
        /// Component kind: "kanban", "form", "table", "progress", "markdown", etc.
        kind: String,
        /// Component-specific JSON props. Schema defined per-kind in
        /// docs/AGENT_RENDER_PROTOCOL.md.
        props: Value,
        /// If true, overwrite any existing component with this component_id.
        replace: bool,
    },
    /// The agent wants the client to unmount (remove) a previously rendered
    /// component.
    ComponentUnmount {
        session_id: AgentSessionId,
        component_id: String,
    },
    /// A browser tool started (`active: true`) or browser work wound down
    /// (`active: false`). The extension side panel listens for this to
    /// auto-focus while Ocean drives the browser, and release afterward.
    BrowserActivity {
        session_id: AgentSessionId,
        active: bool,
    },
    /// Catch-all for unexpected or extension events.  Includes the raw payload.
    Extension { extension: String, payload: Value },
}

// ---------------------------------------------------------------------------
// Longhouse — non-hierarchical multi-agent coordination (the "underwater
// building" deck renders these). LonghouseEvents ride inside
// `AgentTurnEvent::Extension { extension: "longhouse", payload }` so they
// stream over the existing `/v1/agent/events` SSE contract without breaking
// any client that doesn't understand them. See docs/LONGHOUSE_ORCHESTRATION.md
// and the Living Deck section of the vision doc.
// ---------------------------------------------------------------------------

/// The standing departments of the longhouse. Each is a *federation* — a room
/// in the building, a domain of competence. Queries route to the federation
/// whose domain matches; couriers and the steward of that federation live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Federation {
    /// Builds & maintains the automations/workflows the other departments run on.
    /// Most council sessions that produce *new capability* convene here.
    Dev,
    /// Notion CRM, outreach, deal motion.
    Sales,
    /// Content Lab — the production pipeline.
    Content,
    /// Campaign Hub — creator ops + the campaign databases.
    Campaign,
    /// Cross-cutting / not yet routed to a department.
    Commons,
}

/// What an agent *is* within the longhouse. The load-bearing split: couriers
/// **exercise** (do the work), stewards **oversee + recall** (watch runs, catch
/// failures, assess, carry human feedback back, can restart/revoke a run).
/// This mirrors the grant/exercise/revoke separation in the orchestration doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// A worker that carries out departmental tasks (draft, clip, update, build).
    Courier,
    /// The department's IT/overseer: watches cron + automation runs, ensures they
    /// fire and recover, assesses results, proposes improvements, relays feedback.
    Steward,
    /// Per-topic: emits the single binding `Converged` — only when the daemon's
    /// own quorum state already says converged. Ratifies, never overrides.
    Firekeeper,
    /// Per-topic: verifies *process* (did competent members weigh in? was quorum
    /// genuinely credentialed?) without voting on content. Can procedurally veto.
    Validator,
}

/// Why a topic was convened — the three entry points into the longhouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConveneTrigger {
    /// A council was convened to deliberate a question.
    Deliberation,
    /// A user (operator) asked for a task or an answer.
    UserRequest,
    /// A scheduled cron / event-driven automation fired.
    Cron,
}

/// A typed mark posted to a topic's blackboard (the stigmergic signal field).
/// Aggregate weight + decay are computed by the daemon, never an LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkKind {
    /// A candidate answer / plan.
    Proposal,
    /// Support for a proposal (raises its net weight).
    Endorse,
    /// Opposition to a proposal (cross-inhibition; lowers net weight).
    Inhibit,
    /// A fact / source / tool-result contributed to the deliberation.
    Evidence,
    /// A free-form note.
    Note,
}

/// One member seated in a convened topic. The deck renders one agent sprite per
/// `LonghouseMember`, skinned by `federation` + `role` + `model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LonghouseMember {
    /// Stable id for this seated agent (for the deck to track its sprite).
    pub agent_id: Uuid,
    /// Which department this agent belongs to.
    pub federation: Federation,
    /// Courier / steward / firekeeper / validator.
    pub role: AgentRole,
    /// Model driving this agent (e.g. "claude-opus-4-7", "kimi-k2.6").
    /// Lets the deck color sprites by provider and shows the model menu live.
    pub model: String,
    /// Human-facing label (e.g. "Sales Courier 3").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A typed mark on a topic's blackboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub mark_id: Uuid,
    /// The agent that posted it.
    pub author: Uuid,
    pub kind: MarkKind,
    /// For endorse/inhibit: the proposal this mark targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Uuid>,
    /// Short human-facing summary of the mark's content (the deck shows this on
    /// the blackboard slab; full content lives in the agent's transcript).
    pub summary: String,
}

/// Running tally for one proposal — the daemon-computed "pheromone level".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalTally {
    pub proposal: Uuid,
    /// Net credential-weighted support (endorse − inhibit), after decay.
    pub net_weight: f32,
}

/// Why a topic closed without a single converged answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    /// The hard deadline fired with no clear leader.
    Timeout,
    /// Two+ proposals were irreconcilable.
    Split,
    /// The token budget ceiling was hit.
    BudgetExhausted,
    /// A run was recalled by a steward / operator.
    Recalled,
}

/// The longhouse coordination events. Carried as the `payload` of
/// `AgentTurnEvent::Extension { extension: "longhouse", .. }`.
///
/// The deck (the underwater-building UI) renders these spatially: a topic is a
/// lit room, members swim in, marks land on the blackboard, the room brightens
/// toward quorum, and `Converged` floods it with light.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "lh_type", rename_all = "snake_case")]
pub enum LonghouseEvent {
    /// A topic opened. The matching department room lights up.
    TopicConvened {
        topic_id: Uuid,
        /// The board this topic deliberates on.
        board_id: Uuid,
        /// Which department room hosts it.
        federation: Federation,
        /// What kind of work this is.
        trigger: ConveneTrigger,
        /// Short human-facing description of the question/task.
        title: String,
        /// Hard deadline (epoch ms) after which the daemon forces resolution.
        deadline_ms: i64,
    },
    /// The members routed into the topic. Sprites spawn, one per member.
    Convened {
        topic_id: Uuid,
        members: Vec<LonghouseMember>,
    },
    /// A member posted a mark to the blackboard.
    MarkPosted { topic_id: Uuid, mark: Mark },
    /// The daemon recomputed quorum after a mark. Drives the room's quorum meter.
    QuorumUpdated {
        topic_id: Uuid,
        tallies: Vec<ProposalTally>,
        /// The current front-runner proposal, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leader: Option<Uuid>,
        /// 0.0–1.0: how close the leader is to crossing the quorum threshold.
        distance_to_quorum: f32,
    },
    /// A title (firekeeper/validator) was bound to a member for this topic.
    RoleGranted {
        topic_id: Uuid,
        agent_id: Uuid,
        role: AgentRole,
    },
    /// A title was pulled / a member was recalled (the steward/War-Chief action).
    RoleRevoked {
        topic_id: Uuid,
        agent_id: Uuid,
        reason: String,
    },
    /// A member accrued a soft strike (degraded output / ignored the board).
    Warned {
        topic_id: Uuid,
        agent_id: Uuid,
        strike: u8,
        reason: String,
    },
    /// The single binding resolution — emitted by the firekeeper (or the daemon
    /// on the no-tie fast path) only once the daemon's quorum state says so.
    Converged {
        topic_id: Uuid,
        /// The winning proposal.
        decision: Uuid,
        /// Who signed the call (for the immutable decision log).
        by: Uuid,
    },
    /// The topic ended without a converged answer.
    Aborted { topic_id: Uuid, reason: AbortReason },
    /// The topic closed; titles return to escrow, the room dims.
    TopicClosed { topic_id: Uuid },
    /// Steward heartbeat about a department's automation/cron runs. Lets the deck
    /// show a department's "are the automations healthy" state outside of any
    /// single deliberation (the steward at its station, runs green/red).
    RunHealth {
        federation: Federation,
        /// Total automation runs the steward is overseeing.
        runs_total: u32,
        /// How many are currently healthy / green.
        runs_healthy: u32,
        /// Optional note (e.g. "restarted nightly clip job").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl LonghouseEvent {
    /// The extension tag used when wrapping this in `AgentTurnEvent::Extension`.
    pub const EXTENSION: &'static str = "longhouse";

    /// Wrap this longhouse event as an `AgentTurnEvent::Extension` so it can be
    /// published on the existing agent event bus / `/v1/agent/events` SSE.
    pub fn into_turn_event(self) -> AgentTurnEvent {
        AgentTurnEvent::Extension {
            extension: Self::EXTENSION.to_string(),
            payload: serde_json::to_value(self).unwrap_or(Value::Null),
        }
    }

    /// Try to extract a `LonghouseEvent` from an `AgentTurnEvent` if it is a
    /// longhouse extension. Returns `None` for any other event (clients ignore
    /// unrecognised extensions). This is what the deck calls on each SSE line.
    pub fn from_turn_event(event: &AgentTurnEvent) -> Option<Self> {
        match event {
            AgentTurnEvent::Extension { extension, payload } if extension == Self::EXTENSION => {
                serde_json::from_value(payload.clone()).ok()
            }
            _ => None,
        }
    }
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
            model: Some("deepseek-v4-pro".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.starts_with(r#"{"type":"turn_started","#));
        assert!(json.contains("\"turn_id\""));
        assert!(json.contains("\"session_id\""));
    }

    #[test]
    fn longhouse_event_rides_extension_and_round_trips() {
        let topic = Uuid::new_v4();
        let ev = LonghouseEvent::TopicConvened {
            topic_id: topic,
            board_id: Uuid::new_v4(),
            federation: Federation::Sales,
            trigger: ConveneTrigger::UserRequest,
            title: "status of the Warner campaign".into(),
            deadline_ms: 1_700_000_000_000,
        };
        // Wrap into the SSE-carried AgentTurnEvent::Extension.
        let wrapped = ev.clone().into_turn_event();
        match &wrapped {
            AgentTurnEvent::Extension { extension, .. } => {
                assert_eq!(extension, "longhouse");
            }
            _ => panic!("expected Extension"),
        }
        // It serializes as a normal agent event (so SSE clients see it).
        let json = serde_json::to_string(&wrapped).unwrap();
        assert!(json.contains(r#""type":"extension""#));
        assert!(json.contains(r#""lh_type":"topic_convened""#));
        assert!(json.contains("Warner"));

        // The deck extracts it back; unrelated events return None.
        let back = LonghouseEvent::from_turn_event(&wrapped).expect("should extract");
        match back {
            LonghouseEvent::TopicConvened { federation, trigger, .. } => {
                assert_eq!(federation, Federation::Sales);
                assert_eq!(trigger, ConveneTrigger::UserRequest);
            }
            _ => panic!("wrong variant"),
        }
        let not_lh = AgentTurnEvent::TurnStarted {
            turn_id: AgentTurnId::new_v4(),
            session_id: AgentSessionId::new_v4(),
            model: None,
        };
        assert!(LonghouseEvent::from_turn_event(&not_lh).is_none());
    }

    #[test]
    fn agent_turn_event_deserializes_round_trip() {
        let event = AgentTurnEvent::ToolCallFinished {
            session_id: AgentSessionId::new_v4(),
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
            client_type: Some("surface-web".into()),
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
