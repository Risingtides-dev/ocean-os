use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use ocean_core::{
    PermissionId, PermissionStatus, PromptRequest, RequestId, RequestState, RequestStatus, RoomKey,
    SessionId,
};
use ocean_runtime::PermissionDecision as AgentPermissionDecision;
use tokio::{
    sync::{oneshot, RwLock},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub(super) type RequestRegistry = Arc<RwLock<HashMap<RequestId, RequestControl>>>;
pub(super) type PermissionRegistry = Arc<RwLock<HashMap<PermissionId, PermissionWaiter>>>;

pub(super) struct RequestControl {
    pub(super) status: RequestStatus,
    pub(super) cancel: CancellationToken,
    pub(super) handle: Option<JoinHandle<()>>,
    /// Per-turn secret bound to the submitter (OCEAN-185, P0). Set from the
    /// turn's `decision_token`; the gating policy copies it into every
    /// `PermissionWaiter` this turn raises, and the decision POST must present
    /// it. `None` = the turn was submitted without binding (a legacy/internal
    /// turn). Never serialized onto the public `/v1/events` SSE. Held here so the
    /// turn record owns the secret; the enforcement read is on the waiter.
    #[allow(dead_code)]
    pub(super) decision_token: Option<String>,
    /// Immutable room-agent authority that admitted this request. Ordinary
    /// requests remain `None`; mutations use this metadata to cancel only
    /// superseded generations for the exact room/member pair.
    pub(super) room_agent_authority: Option<RoomAgentRequestAuthority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RoomAgentRequestAuthority {
    pub(super) room: RoomKey,
    pub(super) agent_member_id: String,
    pub(super) generation: u64,
    pub(super) admission_id: String,
    pub(super) decision_id: String,
    pub(super) approved_definition_digest: String,
    pub(super) session_id: SessionId,
}

pub(super) struct PermissionWaiter {
    pub(super) status: PermissionStatus,
    pub(super) sender: Option<oneshot::Sender<AgentPermissionDecision>>,
    /// The turn's `decision_token` (OCEAN-185), copied from the owning
    /// `RequestControl` when the waiter is registered. The decision handler
    /// constant-time-compares the POSTed token against this; a missing/wrong
    /// token is rejected 403. `None` = the gated turn was submitted unbound
    /// (legacy client). NEVER placed in `status` or any SSE payload.
    pub(super) decision_token: Option<String>,
}

impl RequestControl {
    /// Whether this request has reached a terminal lifecycle state.
    pub(super) fn is_terminal(&self) -> bool {
        self.status.state.is_terminal()
    }

    /// Best-effort "when did this become final" timestamp for age comparison.
    pub(super) fn terminal_at(&self) -> DateTime<Utc> {
        self.status
            .finished_at
            .or(self.status.updated_at)
            .or(self.status.started_at)
            .unwrap_or_else(Utc::now)
    }
}

impl PermissionWaiter {
    /// A waiter whose decision channel has been consumed is effectively done —
    /// it's normally removed on decision/cancel, so a lingering `None`-sender
    /// entry is a leak. Pending waiters (`Some`) are never reaped by age.
    pub(super) fn is_terminal(&self) -> bool {
        self.sender.is_none()
    }

    pub(super) fn terminal_at(&self) -> DateTime<Utc> {
        self.status.created_at
    }
}

pub(super) async fn requests_snapshot(requests: &RequestRegistry) -> Vec<RequestStatus> {
    let mut requests = requests
        .read()
        .await
        .values()
        .map(|control| control.status.clone())
        .collect::<Vec<_>>();
    requests.sort_by_key(|status| status.started_at);
    requests.reverse();
    requests
}

pub(super) async fn pending_permissions_snapshot(
    permissions: &PermissionRegistry,
) -> Vec<PermissionStatus> {
    let mut pending = permissions
        .read()
        .await
        .values()
        .map(|waiter| waiter.status.clone())
        .collect::<Vec<_>>();
    pending.sort_by_key(|status| status.created_at);
    pending.reverse();
    pending
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod room_agent_tests {
    use super::*;
    use std::collections::HashMap;

    fn request(request_id: RequestId, session_id: SessionId) -> PromptRequest {
        PromptRequest {
            prompt: String::new(),
            images: None,
            request_id: Some(request_id),
            session_id: Some(session_id),
            create_if_missing: true,
            max_turns: None,
            yolo: false,
            cwd: "/tmp".into(),
            project_id: None,
            client_type: Some("room".into()),
            decision_token: None,
        }
    }

    fn authority(session_id: SessionId) -> RoomAgentRequestAuthority {
        RoomAgentRequestAuthority {
            room: RoomKey::new("room-a"),
            agent_member_id: "agent-a".into(),
            generation: 7,
            admission_id: "admission-a".into(),
            decision_id: "decision-a".into(),
            approved_definition_digest: "sha256:a".into(),
            session_id,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn room_registration_linearizes_validation_audit_and_visibility() {
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let request_id = RequestId::new_v4();
        let session_id = SessionId::new_v4();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let registration_requests = requests.clone();
        let registration = tokio::spawn(async move {
            let mut prompt = request(request_id, session_id);
            register_room_agent_request_checked(
                &registration_requests,
                &mut prompt,
                "room admission",
                authority(session_id),
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ()>(())
                },
            )
            .await
        });
        entered_rx.recv().unwrap();

        let mutation_requests = requests.clone();
        let mutation = tokio::spawn(async move {
            let registry = mutation_requests.write().await;
            registry
                .get(&request_id)
                .map(|control| control.status.state)
        });
        tokio::task::yield_now().await;
        assert!(
            !mutation.is_finished(),
            "mutation must wait on admission gate"
        );
        release_tx.send(()).unwrap();
        registration.await.unwrap().unwrap();
        assert_eq!(mutation.await.unwrap(), Some(RequestState::Running));
        let registry = requests.read().await;
        let stored = registry[&request_id]
            .room_agent_authority
            .as_ref()
            .expect("authority metadata");
        assert_eq!(stored.generation, 7);
        assert_eq!(stored.admission_id, "admission-a");
    }

    #[tokio::test]
    async fn failed_admission_audit_removes_queued_request() {
        let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new()));
        let request_id = RequestId::new_v4();
        let session_id = SessionId::new_v4();
        let mut prompt = request(request_id, session_id);
        let result = register_room_agent_request_checked(
            &requests,
            &mut prompt,
            "room admission",
            authority(session_id),
            || Err::<(), _>("audit failed"),
        )
        .await;
        assert_eq!(result.unwrap_err(), "audit failed");
        assert!(requests.read().await.is_empty());
    }
}

pub(super) async fn register_running_request(
    requests: &RequestRegistry,
    req: &mut PromptRequest,
    message: impl Into<String>,
    state_value: RequestState,
) -> (RequestId, CancellationToken) {
    register_request(requests, req, message, state_value, None).await
}

pub(super) async fn register_room_agent_request_checked<E>(
    requests: &RequestRegistry,
    req: &mut PromptRequest,
    message: impl Into<String>,
    authority: RoomAgentRequestAuthority,
    validate_and_audit: impl FnOnce() -> Result<(), E>,
) -> Result<(RequestId, CancellationToken), E> {
    let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
    req.request_id = Some(request_id);
    let cancel = CancellationToken::new();
    let now = Utc::now();
    let mut registry = requests.write().await;
    registry.insert(
        request_id,
        RequestControl {
            status: RequestStatus {
                request_id,
                session_id: req.session_id,
                state: RequestState::Queued,
                permission_id: None,
                message: Some(message.into()),
                started_at: Some(now),
                updated_at: Some(now),
                finished_at: None,
            },
            cancel: cancel.clone(),
            handle: None,
            decision_token: req.decision_token.clone(),
            room_agent_authority: Some(authority),
        },
    );
    if let Err(error) = validate_and_audit() {
        registry.remove(&request_id);
        return Err(error);
    }
    if let Some(control) = registry.get_mut(&request_id) {
        control.status.state = RequestState::Running;
        control.status.updated_at = Some(Utc::now());
    }
    Ok((request_id, cancel))
}

async fn register_request(
    requests: &RequestRegistry,
    req: &mut PromptRequest,
    message: impl Into<String>,
    state_value: RequestState,
    room_agent_authority: Option<RoomAgentRequestAuthority>,
) -> (RequestId, CancellationToken) {
    let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
    req.request_id = Some(request_id);
    let cancel = CancellationToken::new();
    let now = Utc::now();

    requests.write().await.insert(
        request_id,
        RequestControl {
            status: RequestStatus {
                request_id,
                session_id: req.session_id,
                state: state_value,
                permission_id: None,
                message: Some(message.into()),
                started_at: Some(now),
                updated_at: Some(now),
                finished_at: None,
            },
            cancel: cancel.clone(),
            handle: None,
            // OCEAN-185: bind the turn's permission gate to the submitter. The
            // token rides the request body (authenticated submit path) and is
            // copied into every PermissionWaiter; it is NEVER emitted on the
            // public /v1/events SSE.
            decision_token: req.decision_token.clone(),
            room_agent_authority,
        },
    );

    (request_id, cancel)
}

pub(super) async fn attach_request_handle(
    requests: &RequestRegistry,
    request_id: RequestId,
    handle: JoinHandle<()>,
) {
    let mut requests = requests.write().await;
    if let Some(control) = requests.get_mut(&request_id) {
        control.handle = Some(handle);
    }
}

pub(super) async fn cancel_permission_waiter(
    permissions: &PermissionRegistry,
    permission_id: PermissionId,
    request_id: RequestId,
) {
    let waiter = {
        let mut permissions = permissions.write().await;
        permissions.remove(&permission_id)
    };

    if let Some(mut waiter) = waiter {
        if waiter.status.request_id != request_id {
            return;
        }
        if let Some(sender) = waiter.sender.take() {
            let _ = sender.send(AgentPermissionDecision::Deny {
                reason: "request cancelled while waiting for permission".into(),
            });
        }
    }
}

pub(super) async fn update_request_permission_result(
    requests: &RequestRegistry,
    request_id: RequestId,
    permission_id: PermissionId,
    decision: AgentPermissionDecision,
) {
    let mut requests = requests.write().await;
    let Some(control) = requests.get_mut(&request_id) else {
        return;
    };

    if control.status.state.is_terminal()
        || matches!(control.status.state, RequestState::Cancelling)
    {
        return;
    }

    control.status.state = RequestState::Running;
    control.status.permission_id = None;
    control.status.message = Some(match decision {
        AgentPermissionDecision::Allow => format!("permission {permission_id} allowed"),
        AgentPermissionDecision::AllowSession => {
            format!("permission {permission_id} allowed for session")
        }
        AgentPermissionDecision::Deny { ref reason } => {
            format!("permission {permission_id} denied: {reason}")
        }
    });
    control.status.updated_at = Some(Utc::now());
}

/// Drive a request to its terminal registry state, exactly once, and run
/// `on_finalize` UNDER the registry write lock at the instant of the transition.
///
/// `on_finalize(final_state)` is the exact-once turn finalizer's atomic hook
/// (TASK-61): it fires IFF *this* call performs the terminal transition — either
/// the cancel-settle branch (`Cancelling` → `Cancelled`) or the normal
/// desired-state branch (`Running` → `desired_state`). A call that finds
/// the entry already terminal (a late twin — normal completion racing the orphan
/// guard, or vice versa) returns the existing state and does NOT invoke the hook,
/// so the terminal frame is emitted exactly once.
///
/// Because the hook runs while this function still holds the write guard — before
/// the mutated `status.state` is visible to any `requests.read().await` reader —
/// the registry transition and whatever the hook emits (the agent-bus
/// `TurnFinished` frame) are atomic from a concurrent reader's perspective: a
/// reader that observes the entry terminal is guaranteed the hook has already
/// run. This closes the stale-projection window where `GET /v1/agent/sessions`
/// (which derives `active_turn` from this registry) could report a turn cleared
/// before the events stream delivered its `TurnFinished`. The hook MUST stay
/// synchronous and non-blocking (no `.await`): it runs under the write lock, so
/// blocking there would stall every registry reader. `emit_agent` / the event
/// buses satisfy this (a `std::sync::Mutex` push + a non-blocking
/// `broadcast::send`), and they never re-enter the registry, so there is no lock
/// inversion.
pub(super) async fn update_request_finished(
    requests: &RequestRegistry,
    request_id: RequestId,
    session_id: Option<SessionId>,
    desired_state: RequestState,
    message: String,
    on_finalize: impl FnOnce(RequestState),
) -> Option<RequestState> {
    let mut requests = requests.write().await;
    let control = requests.get_mut(&request_id)?;
    let status = &mut control.status;

    if matches!(status.state, RequestState::Cancelling) {
        status.session_id = session_id.or(status.session_id);
        status.state = RequestState::Cancelled;
        status.message = Some(
            "cancel requested; runtime completed after cancellation request and output was ignored"
                .into(),
        );
        status.updated_at = Some(Utc::now());
        status.finished_at = Some(Utc::now());
        let _ = control.handle.take();
        on_finalize(RequestState::Cancelled);
        return Some(RequestState::Cancelled);
    }

    if status.state.is_terminal() {
        let _ = control.handle.take();
        return Some(status.state);
    }

    status.session_id = session_id.or(status.session_id);
    status.state = desired_state;
    status.message = Some(message);
    status.updated_at = Some(Utc::now());
    status.finished_at = Some(Utc::now());
    let _ = control.handle.take();
    on_finalize(desired_state);
    Some(desired_state)
}
