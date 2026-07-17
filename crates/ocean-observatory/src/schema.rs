use crate::Cursor;
use serde::{Deserialize, Serialize};
pub const SCHEMA_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub cursor: Cursor,
    pub event_id: String,
    pub observatory_id: String,
    pub daemon_instance_id: String,
    pub occurred_at: String,
    pub recorded_at: String,
    pub kind: EventKind,
    pub truth: TruthProvenance,
    pub producer: Producer,
    pub topology: Topology,
    pub correlation: Correlation,
    pub visibility: Visibility,
    pub payload: EventPayload,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    DaemonStarted,
    DaemonStopping,
    ExecutionAdmitted,
    ExecutionPhaseChanged,
    ExecutionHeartbeat,
    ExecutionFinished,
    ToolStarted,
    ToolFinished,
    PermissionWaiting,
    PermissionResolved,
    ModelRerouted,
    TopologyAttestationRejected,
    StreamReset,
    StreamGap,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruthProvenance {
    HostObserved,
    ExtensionAttested,
    Derived,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Producer {
    pub kind: ProducerKind,
    pub id: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    Daemon,
    Extension,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topology {
    pub execution_id: String,
    pub root_execution_id: String,
    pub parent_execution_id: Option<String>,
    pub edge_id: Option<String>,
    pub session_id: String,
    pub turn_id: String,
    pub request_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Correlation {
    pub tool_call_id: Option<String>,
    pub permission_id: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Metadata,
    Content,
    ExtensionProducer,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum EventPayload {
    DaemonStarted {
        version: String,
    },
    DaemonStopping {
        reason: Option<String>,
    },
    ExecutionAdmitted {
        phase: ExecutionPhase,
        labels: Vec<String>,
    },
    ExecutionPhaseChanged {
        from_phase: ExecutionPhase,
        to_phase: ExecutionPhase,
    },
    ExecutionHeartbeat {},
    ExecutionFinished {
        phase: ExecutionPhase,
        duration_millis: u64,
        error_classification: Option<String>,
    },
    ToolStarted {
        tool_name: String,
        model_alias: String,
    },
    ToolFinished {
        tool_name: String,
        duration_millis: u64,
        outcome: ToolOutcome,
        byte_count: u64,
    },
    PermissionWaiting {
        reason_code: String,
    },
    PermissionResolved {
        reason_code: String,
        outcome: PermissionOutcome,
        duration_millis: u64,
    },
    ModelRerouted {
        from_model: String,
        to_model: String,
        reason: String,
    },
    TopologyAttestationRejected {
        reason: String,
    },
    StreamReset {
        reason: String,
    },
    StreamGap {
        from_cursor: Cursor,
        to_cursor: Cursor,
        reason: String,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Admitted,
    Running,
    Finished,
    Error,
    Canceled,
    TimedOut,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success,
    Error,
    Skipped,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Approved,
    Denied,
}
