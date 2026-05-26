//! Shared protocol types for Ocean daemon clients.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Stable identifier for a streamed event.
pub type EventId = Uuid;

/// Stable identifier for a permission approval item.
pub type PermissionId = Uuid;

/// Stable identifier for a request.
pub type RequestId = Uuid;

/// Stable identifier for a session.
pub type SessionId = Uuid;

/// Health payload returned by `GET /health`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
    pub version: String,
    pub backend: String,
}

/// Lifecycle state for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    #[default]
    Queued,
    Running,
    WaitingForPermission,
    Cancelling,
    Cancelled,
    Completed,
    Errored,
}

impl RequestState {
    /// Returns true when no later request-control action should mutate this state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RequestState::Cancelled | RequestState::Completed | RequestState::Errored
        )
    }

    /// Returns true when a client cancellation request can be accepted.
    pub fn is_cancellable(self) -> bool {
        matches!(
            self,
            RequestState::Queued
                | RequestState::Running
                | RequestState::WaitingForPermission
                | RequestState::Cancelling
        )
    }
}

/// Payload for `POST /v1/prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub cwd: String,
}

/// Response payload for `POST /v1/prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    pub wall_ms: u64,
    pub stdout: String,
    pub stderr: String,
    #[serde(default)]
    pub cwd: String,
}

/// Summary item returned by `GET /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(alias = "session_id")]
    pub id: SessionId,
    pub model: String,
    pub turns: u32,
    pub title: String,
}

/// Runtime state exposed by `GET /v1/sessions/{id}` for command-center clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunState {
    Stored,
    Running,
    WaitingForPermission,
    Cancelling,
    Cancelled,
    Completed,
    Errored,
}

/// One display-ready transcript entry derived from a persisted session message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTranscriptEntry {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Tool-call or tool-result context derived from persisted transcript messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionToolContext {
    pub kind: String,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub text: String,
}

/// Detailed session transcript returned by `GET /v1/sessions/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(alias = "session_id")]
    pub id: SessionId,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub model: String,
    pub provider: String,
    pub turns: u32,
    pub title: String,
    pub state: SessionRunState,
    pub resumable: bool,
    #[serde(default)]
    pub active_requests: Vec<RequestId>,
    #[serde(default)]
    pub pending_permissions: Vec<PermissionId>,
    #[serde(default)]
    pub transcript: Vec<SessionTranscriptEntry>,
    #[serde(default)]
    pub tool_context: Vec<SessionToolContext>,
    /// Raw persisted messages for clients that need provider-specific detail.
    #[serde(default)]
    pub messages: Vec<Value>,
}

/// Response payload for `GET /v1/sessions/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response payload for `POST /v1/requests`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestCreateResponse {
    pub ok: bool,
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub state: RequestState,
    pub message: String,
}

/// Current status snapshot for a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestStatus {
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub state: RequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_id: Option<PermissionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Response payload for `GET /v1/requests`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestsResponse {
    pub ok: bool,
    #[serde(default)]
    pub requests: Vec<RequestStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Pending permission request observable by daemon clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub permission_id: PermissionId,
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub tool: String,
    pub reason: String,
    pub args: Value,
    pub created_at: DateTime<Utc>,
}

/// Response payload for `GET /v1/permissions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionsResponse {
    pub ok: bool,
    #[serde(default)]
    pub permissions: Vec<PermissionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A client decision for a permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Body for `POST /v1/permissions/{id}/decision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDecisionRequest {
    pub permission_id: PermissionId,
    #[serde(flatten)]
    pub decision: PermissionDecision,
}

/// Body for `POST /v1/requests/{id}/cancel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelRequest {
    pub request_id: RequestId,
}

/// Response for request-control endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestControlResponse {
    pub ok: bool,
    pub request_id: RequestId,
    pub state: RequestState,
    pub message: String,
}

/// Response for permission-decision endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionControlResponse {
    pub ok: bool,
    pub permission_id: PermissionId,
    pub message: String,
}

/// Event stream payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OceanEvent {
    SessionCreated,
    UserMessage {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    ToolStarted {
        tool: String,
        args: Value,
    },
    ToolOutput {
        tool: String,
        text: String,
        is_error: bool,
    },
    ToolEnded {
        tool: String,
        is_error: bool,
    },
    PermissionRequest {
        tool: String,
        reason: String,
        args: Value,
    },
    PermissionDecision {
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TurnFinished {
        ok: bool,
        wall_ms: u64,
    },
    Cancelled {
        reason: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Envelope shared by the SSE stream and persisted event logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_id: Option<PermissionId>,
    #[serde(flatten)]
    pub event: OceanEvent,
}

impl EventEnvelope {
    /// Build a new event envelope with the current time.
    pub fn new(event: OceanEvent) -> Self {
        Self {
            id: Uuid::new_v4(),
            at: Utc::now(),
            session_id: None,
            request_id: None,
            permission_id: None,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_request_defaults_for_legacy_clients() {
        let request: PromptRequest = serde_json::from_str(r#"{"prompt":"hello"}"#).unwrap();

        assert_eq!(request.prompt, "hello");
        assert_eq!(request.request_id, None);
        assert_eq!(request.session_id, None);
        assert_eq!(request.max_turns, None);
        assert!(!request.yolo);
    }

    #[test]
    fn event_envelope_flattens_event_fields() {
        let session_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let permission_id = Uuid::new_v4();
        let envelope = EventEnvelope {
            id: Uuid::new_v4(),
            at: Utc::now(),
            session_id: Some(session_id),
            request_id: Some(request_id),
            permission_id: Some(permission_id),
            event: OceanEvent::ToolStarted {
                tool: "bash".to_string(),
                args: serde_json::json!({"cmd": "ls"}),
            },
        };

        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["type"], "tool_started");
        assert_eq!(json["session_id"], session_id.to_string());
        assert_eq!(json["request_id"], request_id.to_string());
        assert_eq!(json["permission_id"], permission_id.to_string());
        assert_eq!(json["tool"], "bash");

        let roundtrip: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, envelope);
    }

    #[test]
    fn permission_decision_roundtrip() {
        let decision = PermissionDecisionRequest {
            permission_id: Uuid::new_v4(),
            decision: PermissionDecision::Deny {
                reason: Some("not now".into()),
            },
        };

        let json = serde_json::to_value(&decision).unwrap();
        assert_eq!(json["decision"], "deny");
        assert_eq!(json["reason"], "not now");

        let roundtrip: PermissionDecisionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, decision);
    }

    #[test]
    fn request_state_helpers_mark_control_boundaries() {
        assert!(RequestState::Queued.is_cancellable());
        assert!(RequestState::Running.is_cancellable());
        assert!(RequestState::WaitingForPermission.is_cancellable());
        assert!(RequestState::Cancelling.is_cancellable());

        assert!(RequestState::Cancelled.is_terminal());
        assert!(RequestState::Completed.is_terminal());
        assert!(RequestState::Errored.is_terminal());

        assert!(!RequestState::Completed.is_cancellable());
        assert!(!RequestState::Running.is_terminal());
    }
}
