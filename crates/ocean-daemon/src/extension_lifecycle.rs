//! Pure Stage A lifecycle adaptation and boot-local retention.
//!
//! This module deliberately has no process, transport, registry, route, or live
//! producer wiring. It converts explicit authoritative-source facts into the
//! closed metadata-only SDK envelope and retains only encoded-safe frames.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Mutex,
};

use ocean_agent_sdk::extension_lifecycle::{
    encode_frame, DaemonStartedMetadata, DaemonStopReason, DaemonStoppingMetadata, EmptyMetadata,
    EventFrame, LifecycleEvent, LifecycleEventKind, LifecycleMetadata, LifecycleScope,
    MillisecondTimestamp, PermissionOutcome, PermissionRequestedMetadata,
    PermissionResolvedMetadata, ProtocolName, ProtocolV1, Sequence, ToolFinishedMetadata, ToolName,
    ToolOutcome, ToolStartedMetadata, TurnFinishedMetadata, TurnOutcome,
};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const BOOT_RING_MAX_EVENTS: usize = 2_048;
pub(crate) const BOOT_RING_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Deterministic host inputs used for one emitted envelope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EventStamp {
    pub(crate) event_id: Uuid,
    pub(crate) occurred_at: MillisecondTimestamp,
}

/// Host-derived identifiers associated with an authoritative source fact.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SourceScope {
    pub(crate) project_id: Option<Uuid>,
    pub(crate) project_registered: bool,
    pub(crate) session_id: Option<Uuid>,
    pub(crate) turn_id: Option<Uuid>,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) permission_id: Option<Uuid>,
}

impl SourceScope {
    fn lifecycle(self, tool_call_id: Option<Uuid>) -> LifecycleScope {
        LifecycleScope {
            project_id: self.project_id.filter(|_| self.project_registered),
            session_id: self.session_id,
            turn_id: self.turn_id,
            request_id: self.request_id,
            tool_call_id,
            permission_id: self.permission_id,
        }
    }
}

/// Terminal input authority for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalSource {
    NormalCompletion,
    OrphanSettlement,
}

/// Final request state read by the one terminal finalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalRequestState {
    Completed,
    Cancelled,
    Failed,
    OtherTerminalFailure,
}

/// Permission-policy terminal input. `AllowSession` is compatibility-only at
/// the lifecycle wire and maps to the same `allowed` metadata as `Allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionResolution {
    Allow,
    AllowSession,
    Deny,
    RequestCancelled,
    WaiterClosed,
}

/// Runtime details needed for the one honest cancellation bit. All other
/// details remain private and are discarded structurally.
#[derive(Debug, Clone)]
pub(crate) struct ToolEndDetails {
    pub(crate) cancelled: bool,
    pub(crate) private: Value,
}

/// Exhaustive pure input vocabulary for current Stage A source authorities.
#[derive(Debug, Clone)]
pub(crate) enum LifecycleSource {
    DaemonStarted {
        daemon_version: String,
        stamp: EventStamp,
    },
    ExplicitSessionCreated {
        succeeded: bool,
        scope: SourceScope,
        stamp: EventStamp,
        title: String,
        cwd: String,
    },
    OrdinaryTurnAdmission {
        admitted: bool,
        new_session: bool,
        scope: SourceScope,
        session_stamp: EventStamp,
        turn_stamp: EventStamp,
        title: String,
        cwd: String,
    },
    PermissionWaiting {
        scope: SourceScope,
        tool_name: ToolName,
        stamp: EventStamp,
        arguments: Value,
        reason: String,
    },
    PermissionResolved {
        scope: SourceScope,
        resolution: PermissionResolution,
        stamp: EventStamp,
        reason: Option<String>,
    },
    ToolExecutionStart {
        scope: SourceScope,
        runtime_tool_call_id: String,
        host_tool_call_id: Uuid,
        tool_name: ToolName,
        started_at_ms: u64,
        stamp: EventStamp,
        arguments: Value,
    },
    ToolExecutionEnd {
        scope: SourceScope,
        runtime_tool_call_id: String,
        is_error: bool,
        rendered_output: Vec<u8>,
        details: ToolEndDetails,
        ended_at_ms: u64,
        stamp: EventStamp,
    },
    /// Existing client compatibility pair for runtime permission denial. No
    /// tool executed, so this source must never become lifecycle tool facts.
    CompatibilityPermissionDenied {
        tool_name: String,
        reason: String,
    },
    TurnTerminal {
        scope: SourceScope,
        source: TerminalSource,
        final_state: FinalRequestState,
        duration_ms: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        stamp: EventStamp,
        error: Option<String>,
    },
    /// Reserved schema compatibility signal. There is no Stage A producer.
    SessionStoppedCompatibility,
    DaemonStopping {
        stamp: EventStamp,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DiagnosticCode {
    UnmatchedToolEnd,
    DuplicateToolStart,
    UnmatchedPermissionResolution,
    DuplicatePermissionRequest,
    TurnAlreadyFinalized,
    CompatibilityPermissionDenied,
    SessionStoppedUnproduced,
    OversizedEvent,
    InvalidEventId,
}

impl DiagnosticCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::UnmatchedToolEnd => "unmatched_tool_end",
            Self::DuplicateToolStart => "duplicate_tool_start",
            Self::UnmatchedPermissionResolution => "unmatched_permission_resolution",
            Self::DuplicatePermissionRequest => "duplicate_permission_request",
            Self::TurnAlreadyFinalized => "turn_already_finalized",
            Self::CompatibilityPermissionDenied => "compatibility_permission_denied",
            Self::SessionStoppedUnproduced => "session_stopped_unproduced",
            Self::OversizedEvent => "oversized_event",
            Self::InvalidEventId => "invalid_event_id",
        }
    }
}

#[derive(Debug, Clone)]
struct ToolCorrelation {
    host_tool_call_id: Uuid,
    tool_name: ToolName,
    started_at_ms: u64,
}

#[derive(Debug, Clone)]
struct RetainedEvent {
    event: LifecycleEvent,
    encoded_bytes: usize,
}

/// In-memory boot ring with both ratified retention bounds.
#[derive(Debug, Default)]
struct BootRing {
    events: VecDeque<RetainedEvent>,
    encoded_bytes: usize,
}

impl BootRing {
    fn push(&mut self, event: LifecycleEvent, encoded_bytes: usize) {
        self.encoded_bytes += encoded_bytes;
        self.events.push_back(RetainedEvent {
            event,
            encoded_bytes,
        });
        while self.events.len() > BOOT_RING_MAX_EVENTS || self.encoded_bytes > BOOT_RING_MAX_BYTES {
            if let Some(evicted) = self.events.pop_front() {
                self.encoded_bytes -= evicted.encoded_bytes;
            }
        }
    }
}

/// Pure adapter and boot-local correlation state.
#[derive(Default)]
struct TerminalGuard {
    finalized_requests: Mutex<HashSet<Uuid>>,
}

impl TerminalGuard {
    fn claim(&self, request_id: Uuid) -> bool {
        self.finalized_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request_id)
    }
}

pub(crate) struct LifecycleAdapter {
    daemon_boot_id: Uuid,
    next_sequence: u64,
    ring: BootRing,
    tools: HashMap<String, ToolCorrelation>,
    open_permissions: HashSet<Uuid>,
    terminal_guard: TerminalGuard,
    diagnostics: HashMap<DiagnosticCode, u64>,
}

impl LifecycleAdapter {
    pub(crate) fn new(daemon_boot_id: Uuid) -> Self {
        Self {
            daemon_boot_id,
            next_sequence: 1,
            ring: BootRing::default(),
            tools: HashMap::new(),
            open_permissions: HashSet::new(),
            terminal_guard: TerminalGuard::default(),
            diagnostics: HashMap::new(),
        }
    }

    /// Adapt one source fact. The vector has two events only for a successfully
    /// admitted ordinary new-session turn; every other source yields zero/one.
    pub(crate) fn adapt(&mut self, source: LifecycleSource) -> Vec<LifecycleEvent> {
        match source {
            LifecycleSource::DaemonStarted {
                daemon_version,
                stamp,
            } => self.one(
                LifecycleEventKind::DaemonStarted,
                LifecycleScope::default(),
                LifecycleMetadata::DaemonStarted(DaemonStartedMetadata { daemon_version }),
                stamp,
            ),
            LifecycleSource::ExplicitSessionCreated {
                succeeded,
                scope,
                stamp,
                title: _,
                cwd: _,
            } => self.explicit_create_session(succeeded, scope, stamp),
            LifecycleSource::OrdinaryTurnAdmission {
                admitted,
                new_session,
                scope,
                session_stamp,
                turn_stamp,
                title: _,
                cwd: _,
            } => self.ordinary_session_admission(
                admitted,
                new_session,
                scope,
                session_stamp,
                turn_stamp,
            ),
            LifecycleSource::PermissionWaiting {
                scope,
                tool_name,
                stamp,
                arguments: _,
                reason: _,
            } => self.permission_waiting(scope, tool_name, stamp),
            LifecycleSource::PermissionResolved {
                scope,
                resolution,
                stamp,
                reason: _,
            } => self.permission_resolved(scope, resolution, stamp),
            LifecycleSource::ToolExecutionStart {
                scope,
                runtime_tool_call_id,
                host_tool_call_id,
                tool_name,
                started_at_ms,
                stamp,
                arguments: _,
            } => self.tool_started(
                scope,
                runtime_tool_call_id,
                host_tool_call_id,
                tool_name,
                started_at_ms,
                stamp,
            ),
            LifecycleSource::ToolExecutionEnd {
                scope,
                runtime_tool_call_id,
                is_error,
                rendered_output,
                details,
                ended_at_ms,
                stamp,
            } => self.tool_finished(
                scope,
                &runtime_tool_call_id,
                is_error,
                rendered_output.len() as u64,
                details,
                ended_at_ms,
                stamp,
            ),
            LifecycleSource::CompatibilityPermissionDenied {
                tool_name: _,
                reason: _,
            } => {
                self.count(DiagnosticCode::CompatibilityPermissionDenied);
                Vec::new()
            }
            LifecycleSource::TurnTerminal {
                scope,
                source,
                final_state,
                duration_ms,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                stamp,
                error: _,
            } => self.finalize_turn(
                scope,
                source,
                final_state,
                duration_ms,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                stamp,
            ),
            LifecycleSource::SessionStoppedCompatibility => {
                self.count(DiagnosticCode::SessionStoppedUnproduced);
                Vec::new()
            }
            LifecycleSource::DaemonStopping { stamp } => self.one(
                LifecycleEventKind::DaemonStopping,
                LifecycleScope::default(),
                LifecycleMetadata::DaemonStopping(DaemonStoppingMetadata {
                    reason: DaemonStopReason::GracefulShutdown,
                }),
                stamp,
            ),
        }
    }

    /// Successful explicit create emits only the session fact.
    fn explicit_create_session(
        &mut self,
        succeeded: bool,
        scope: SourceScope,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        if !succeeded {
            return Vec::new();
        }
        self.one(
            LifecycleEventKind::SessionStarted,
            scope.lifecycle(None),
            LifecycleMetadata::Empty(EmptyMetadata {}),
            stamp,
        )
    }

    /// Ordinary mapping is admitted-only: a new session precedes the turn;
    /// resumed admission emits only the turn; rejection emits neither.
    fn ordinary_session_admission(
        &mut self,
        admitted: bool,
        new_session: bool,
        scope: SourceScope,
        session_stamp: EventStamp,
        turn_stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        if !admitted {
            return Vec::new();
        }
        let mut events = Vec::with_capacity(usize::from(new_session) + 1);
        if new_session {
            events.extend(self.one(
                LifecycleEventKind::SessionStarted,
                scope.lifecycle(None),
                LifecycleMetadata::Empty(EmptyMetadata {}),
                session_stamp,
            ));
        }
        events.extend(self.one(
            LifecycleEventKind::TurnStarted,
            scope.lifecycle(None),
            LifecycleMetadata::Empty(EmptyMetadata {}),
            turn_stamp,
        ));
        events
    }

    fn permission_waiting(
        &mut self,
        scope: SourceScope,
        tool_name: ToolName,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let Some(permission_id) = scope.permission_id else {
            self.count(DiagnosticCode::DuplicatePermissionRequest);
            return Vec::new();
        };
        if !self.open_permissions.insert(permission_id) {
            self.count(DiagnosticCode::DuplicatePermissionRequest);
            return Vec::new();
        }
        self.one(
            LifecycleEventKind::PermissionRequested,
            scope.lifecycle(None),
            LifecycleMetadata::PermissionRequested(PermissionRequestedMetadata { tool_name }),
            stamp,
        )
    }

    fn permission_resolved(
        &mut self,
        scope: SourceScope,
        resolution: PermissionResolution,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let Some(permission_id) = scope.permission_id else {
            self.count(DiagnosticCode::UnmatchedPermissionResolution);
            return Vec::new();
        };
        if !self.open_permissions.remove(&permission_id) {
            self.count(DiagnosticCode::UnmatchedPermissionResolution);
            return Vec::new();
        }
        let outcome = match resolution {
            PermissionResolution::Allow | PermissionResolution::AllowSession => {
                PermissionOutcome::Allowed
            }
            PermissionResolution::Deny => PermissionOutcome::Denied,
            PermissionResolution::RequestCancelled | PermissionResolution::WaiterClosed => {
                PermissionOutcome::Cancelled
            }
        };
        self.one(
            LifecycleEventKind::PermissionResolved,
            scope.lifecycle(None),
            LifecycleMetadata::PermissionResolved(PermissionResolvedMetadata { outcome }),
            stamp,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_started(
        &mut self,
        scope: SourceScope,
        runtime_tool_call_id: String,
        host_tool_call_id: Uuid,
        tool_name: ToolName,
        started_at_ms: u64,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        if self.tools.contains_key(&runtime_tool_call_id) {
            self.count(DiagnosticCode::DuplicateToolStart);
            return Vec::new();
        }
        self.tools.insert(
            runtime_tool_call_id,
            ToolCorrelation {
                host_tool_call_id,
                tool_name: tool_name.clone(),
                started_at_ms,
            },
        );
        self.one(
            LifecycleEventKind::ToolStarted,
            scope.lifecycle(Some(host_tool_call_id)),
            LifecycleMetadata::ToolStarted(ToolStartedMetadata { tool_name }),
            stamp,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_finished(
        &mut self,
        scope: SourceScope,
        runtime_tool_call_id: &str,
        is_error: bool,
        output_bytes: u64,
        details: ToolEndDetails,
        ended_at_ms: u64,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let Some(correlation) = self.tools.remove(runtime_tool_call_id) else {
            self.count(DiagnosticCode::UnmatchedToolEnd);
            return Vec::new();
        };
        let outcome = if details.cancelled {
            ToolOutcome::Cancelled
        } else if is_error {
            ToolOutcome::Error
        } else {
            ToolOutcome::Success
        };
        let _ = details.private;
        self.one(
            LifecycleEventKind::ToolFinished,
            scope.lifecycle(Some(correlation.host_tool_call_id)),
            LifecycleMetadata::ToolFinished(ToolFinishedMetadata {
                tool_name: correlation.tool_name,
                outcome,
                duration_ms: ended_at_ms.saturating_sub(correlation.started_at_ms),
                output_bytes,
            }),
            stamp,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_turn(
        &mut self,
        scope: SourceScope,
        source: TerminalSource,
        final_state: FinalRequestState,
        duration_ms: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let Some(request_id) = scope.request_id else {
            self.count(DiagnosticCode::TurnAlreadyFinalized);
            return Vec::new();
        };
        if !self.terminal_guard.claim(request_id) {
            self.count(DiagnosticCode::TurnAlreadyFinalized);
            return Vec::new();
        }
        let outcome = match (final_state, source) {
            (FinalRequestState::Completed, _) => TurnOutcome::Completed,
            (FinalRequestState::Cancelled, _) => TurnOutcome::Cancelled,
            (_, TerminalSource::OrphanSettlement) => TurnOutcome::Abandoned,
            (FinalRequestState::Failed | FinalRequestState::OtherTerminalFailure, _) => {
                TurnOutcome::Failed
            }
        };
        self.one(
            LifecycleEventKind::TurnFinished,
            scope.lifecycle(None),
            LifecycleMetadata::TurnFinished(TurnFinishedMetadata {
                outcome,
                duration_ms,
                input_tokens,
                output_tokens,
                cache_read_tokens,
            }),
            stamp,
        )
    }

    fn one(
        &mut self,
        kind: LifecycleEventKind,
        scope: LifecycleScope,
        metadata: LifecycleMetadata,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        if stamp.event_id.get_version_num() != 4 {
            self.count(DiagnosticCode::InvalidEventId);
            return Vec::new();
        }
        let event = LifecycleEvent {
            protocol: ProtocolName,
            version: ProtocolV1,
            frame: EventFrame,
            daemon_boot_id: self.daemon_boot_id,
            sequence: Sequence(self.next_sequence),
            event_id: stamp.event_id,
            occurred_at: stamp.occurred_at,
            kind,
            scope,
            metadata,
        };
        let Ok(encoded) = encode_frame(&event) else {
            self.count(DiagnosticCode::OversizedEvent);
            return Vec::new();
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.ring.push(event.clone(), encoded.len());
        vec![event]
    }

    fn count(&mut self, code: DiagnosticCode) {
        *self.diagnostics.entry(code).or_default() += 1;
    }

    #[cfg(test)]
    fn diagnostic_count(&self, code: DiagnosticCode) -> u64 {
        self.diagnostics.get(&code).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn retained(&self) -> impl Iterator<Item = &LifecycleEvent> {
        self.ring.events.iter().map(|retained| &retained.event)
    }
}

/// Immutable effective activation scope used for pure delivery tests.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActivationScope {
    pub(crate) global: bool,
    pub(crate) projects: HashSet<Uuid>,
}

impl ActivationScope {
    pub(crate) fn eligible(&self, event: &LifecycleEvent) -> bool {
        match event.kind {
            LifecycleEventKind::DaemonStarted | LifecycleEventKind::DaemonStopping => {
                self.global || !self.projects.is_empty()
            }
            _ => match event.scope.project_id {
                Some(project_id) => self.projects.contains(&project_id),
                None => self.global,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use ocean_agent_sdk::extension_lifecycle::{
        LifecycleMetadata, PermissionOutcome, ToolOutcome, TurnOutcome,
    };

    use super::*;

    fn id(value: u128) -> Uuid {
        const VERSION_MASK: u128 = 0x0000_0000_0000_f000_0000_0000_0000_0000;
        const VARIANT_MASK: u128 = 0x0000_0000_0000_0000_c000_0000_0000_0000;
        const VERSION_4: u128 = 0x0000_0000_0000_4000_0000_0000_0000_0000;
        const RFC_4122: u128 = 0x0000_0000_0000_0000_8000_0000_0000_0000;
        Uuid::from_u128((value & !VERSION_MASK & !VARIANT_MASK) | VERSION_4 | RFC_4122)
    }

    fn stamp(value: u128, millis: i64) -> EventStamp {
        EventStamp {
            event_id: id(value),
            occurred_at: MillisecondTimestamp::new(
                Utc.timestamp_millis_opt(millis)
                    .single()
                    .expect("timestamp"),
            ),
        }
    }

    fn scope(request: u128) -> SourceScope {
        SourceScope {
            project_id: Some(id(10)),
            project_registered: true,
            session_id: Some(id(11)),
            turn_id: Some(id(12)),
            request_id: Some(id(request)),
            permission_id: None,
        }
    }

    fn tool(name: &str) -> ToolName {
        ToolName::new(name).expect("tool")
    }

    #[test]
    fn nine_produced_kinds_map_and_reserved_session_stopped_never_emits() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let mut events = Vec::new();
        events.extend(adapter.adapt(LifecycleSource::DaemonStarted {
            daemon_version: "0.1.0".to_owned(),
            stamp: stamp(101, 0),
        }));
        events.extend(adapter.adapt(LifecycleSource::ExplicitSessionCreated {
            succeeded: true,
            scope: scope(13),
            stamp: stamp(102, 1),
            title: "PRIVATE".to_owned(),
            cwd: "/secret".to_owned(),
        }));
        events.extend(adapter.adapt(LifecycleSource::OrdinaryTurnAdmission {
            admitted: true,
            new_session: false,
            scope: scope(13),
            session_stamp: stamp(199, 1),
            turn_stamp: stamp(103, 2),
            title: String::new(),
            cwd: String::new(),
        }));
        let permission_scope = SourceScope {
            permission_id: Some(id(14)),
            ..scope(13)
        };
        events.extend(adapter.adapt(LifecycleSource::PermissionWaiting {
            scope: permission_scope,
            tool_name: tool("read"),
            stamp: stamp(104, 3),
            arguments: Value::Null,
            reason: String::new(),
        }));
        events.extend(adapter.adapt(LifecycleSource::PermissionResolved {
            scope: permission_scope,
            resolution: PermissionResolution::Allow,
            stamp: stamp(105, 4),
            reason: None,
        }));
        events.extend(adapter.adapt(LifecycleSource::ToolExecutionStart {
            scope: scope(13),
            runtime_tool_call_id: "opaque".to_owned(),
            host_tool_call_id: id(15),
            tool_name: tool("read"),
            started_at_ms: 10,
            stamp: stamp(106, 5),
            arguments: Value::Null,
        }));
        events.extend(adapter.adapt(LifecycleSource::ToolExecutionEnd {
            scope: scope(13),
            runtime_tool_call_id: "opaque".to_owned(),
            is_error: false,
            rendered_output: vec![0; 12],
            details: ToolEndDetails {
                cancelled: false,
                private: Value::Null,
            },
            ended_at_ms: 14,
            stamp: stamp(107, 6),
        }));
        events.extend(adapter.adapt(LifecycleSource::TurnTerminal {
            scope: scope(13),
            source: TerminalSource::NormalCompletion,
            final_state: FinalRequestState::Completed,
            duration_ms: 7,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            stamp: stamp(108, 7),
            error: None,
        }));
        assert!(adapter
            .adapt(LifecycleSource::SessionStoppedCompatibility)
            .is_empty());
        events.extend(adapter.adapt(LifecycleSource::DaemonStopping {
            stamp: stamp(109, 8),
        }));

        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            [
                LifecycleEventKind::DaemonStarted,
                LifecycleEventKind::SessionStarted,
                LifecycleEventKind::TurnStarted,
                LifecycleEventKind::PermissionRequested,
                LifecycleEventKind::PermissionResolved,
                LifecycleEventKind::ToolStarted,
                LifecycleEventKind::ToolFinished,
                LifecycleEventKind::TurnFinished,
                LifecycleEventKind::DaemonStopping,
            ]
        );
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::SessionStoppedUnproduced),
            1
        );
        assert!(!events
            .iter()
            .any(|event| event.kind == LifecycleEventKind::SessionStopped));
    }

    #[test]
    fn ordinary_and_explicit_session_admission_helpers_preserve_truthful_order() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let rejected = adapter.adapt(LifecycleSource::OrdinaryTurnAdmission {
            admitted: false,
            new_session: true,
            scope: scope(20),
            session_stamp: stamp(1, 0),
            turn_stamp: stamp(2, 1),
            title: "secret".to_owned(),
            cwd: "/secret".to_owned(),
        });
        assert!(rejected.is_empty());

        let resumed = adapter.adapt(LifecycleSource::OrdinaryTurnAdmission {
            admitted: true,
            new_session: false,
            scope: scope(21),
            session_stamp: stamp(3, 2),
            turn_stamp: stamp(4, 3),
            title: String::new(),
            cwd: String::new(),
        });
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].kind, LifecycleEventKind::TurnStarted);

        let fresh = adapter.adapt(LifecycleSource::OrdinaryTurnAdmission {
            admitted: true,
            new_session: true,
            scope: scope(22),
            session_stamp: stamp(5, 4),
            turn_stamp: stamp(6, 5),
            title: String::new(),
            cwd: String::new(),
        });
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh[0].kind, LifecycleEventKind::SessionStarted);
        assert_eq!(fresh[1].kind, LifecycleEventKind::TurnStarted);

        let failed_create = adapter.adapt(LifecycleSource::ExplicitSessionCreated {
            succeeded: false,
            scope: scope(23),
            stamp: stamp(7, 6),
            title: String::new(),
            cwd: String::new(),
        });
        assert!(failed_create.is_empty());
        let created = adapter.adapt(LifecycleSource::ExplicitSessionCreated {
            succeeded: true,
            scope: scope(23),
            stamp: stamp(8, 7),
            title: String::new(),
            cwd: String::new(),
        });
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, LifecycleEventKind::SessionStarted);
    }

    #[test]
    fn permission_waiting_resolution_pair_is_exactly_once_for_all_terminal_inputs() {
        for (index, resolution, expected) in [
            (1, PermissionResolution::Allow, PermissionOutcome::Allowed),
            (
                2,
                PermissionResolution::AllowSession,
                PermissionOutcome::Allowed,
            ),
            (3, PermissionResolution::Deny, PermissionOutcome::Denied),
            (
                4,
                PermissionResolution::RequestCancelled,
                PermissionOutcome::Cancelled,
            ),
            (
                5,
                PermissionResolution::WaiterClosed,
                PermissionOutcome::Cancelled,
            ),
        ] {
            let mut adapter = LifecycleAdapter::new(id(1));
            let permission_scope = SourceScope {
                permission_id: Some(id(index)),
                ..scope(30 + index)
            };
            assert_eq!(
                adapter
                    .adapt(LifecycleSource::PermissionWaiting {
                        scope: permission_scope,
                        tool_name: tool("bash"),
                        stamp: stamp(10 + index, 0),
                        arguments: serde_json::json!({"secret": "ARG_SENTINEL"}),
                        reason: "REASON_SENTINEL".to_owned(),
                    })
                    .len(),
                1
            );
            let resolved = adapter.adapt(LifecycleSource::PermissionResolved {
                scope: permission_scope,
                resolution,
                stamp: stamp(20 + index, 1),
                reason: Some("REASON_SENTINEL".to_owned()),
            });
            match &resolved[0].metadata {
                LifecycleMetadata::PermissionResolved(metadata) => {
                    assert_eq!(metadata.outcome, expected)
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(adapter
                .adapt(LifecycleSource::PermissionResolved {
                    scope: permission_scope,
                    resolution,
                    stamp: stamp(30 + index, 2),
                    reason: None,
                })
                .is_empty());
            assert_eq!(
                adapter.diagnostic_count(DiagnosticCode::UnmatchedPermissionResolution),
                1
            );
        }
    }

    #[test]
    fn tool_uuid_correlation_cancellation_and_unmatched_end_are_honest() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let started = adapter.adapt(LifecycleSource::ToolExecutionStart {
            scope: scope(40),
            runtime_tool_call_id: "opaque-runtime-id".to_owned(),
            host_tool_call_id: id(41),
            tool_name: tool("bash"),
            started_at_ms: 100,
            stamp: stamp(42, 0),
            arguments: serde_json::json!({"command": "ARG_SENTINEL"}),
        });
        assert_eq!(started[0].scope.tool_call_id, Some(id(41)));
        let finished = adapter.adapt(LifecycleSource::ToolExecutionEnd {
            scope: scope(40),
            runtime_tool_call_id: "opaque-runtime-id".to_owned(),
            is_error: true,
            rendered_output: b"RESULT_SENTINEL".to_vec(),
            details: ToolEndDetails {
                cancelled: true,
                private: serde_json::json!({"secret": "DETAIL_SENTINEL"}),
            },
            ended_at_ms: 104,
            stamp: stamp(43, 1),
        });
        assert_eq!(finished[0].scope.tool_call_id, Some(id(41)));
        match &finished[0].metadata {
            LifecycleMetadata::ToolFinished(metadata) => {
                assert_eq!(metadata.outcome, ToolOutcome::Cancelled);
                assert_eq!(metadata.duration_ms, 4);
                assert_eq!(metadata.output_bytes, 15);
            }
            other => panic!("unexpected {other:?}"),
        }
        let encoded = String::from_utf8(encode_frame(&finished[0]).expect("encode")).expect("utf8");
        for sentinel in ["ARG_SENTINEL", "RESULT_SENTINEL", "DETAIL_SENTINEL"] {
            assert!(!encoded.contains(sentinel), "{encoded}");
        }

        assert!(adapter
            .adapt(LifecycleSource::ToolExecutionEnd {
                scope: scope(40),
                runtime_tool_call_id: "missing".to_owned(),
                is_error: false,
                rendered_output: Vec::new(),
                details: ToolEndDetails {
                    cancelled: false,
                    private: Value::Null,
                },
                ended_at_ms: 0,
                stamp: stamp(44, 2),
            })
            .is_empty());
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::UnmatchedToolEnd),
            1
        );
        assert_eq!(
            DiagnosticCode::UnmatchedToolEnd.as_str(),
            "unmatched_tool_end"
        );
    }

    #[test]
    fn compatibility_permission_denial_never_fabricates_execution() {
        let mut adapter = LifecycleAdapter::new(id(1));
        assert!(adapter
            .adapt(LifecycleSource::CompatibilityPermissionDenied {
                tool_name: "bash".to_owned(),
                reason: "PRIVATE".to_owned(),
            })
            .is_empty());
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::CompatibilityPermissionDenied),
            1
        );
        assert_eq!(adapter.retained().count(), 0);
    }

    #[test]
    fn all_terminal_sources_and_cancellation_races_finalize_exactly_once() {
        for (index, source, state, expected) in [
            (
                1,
                TerminalSource::NormalCompletion,
                FinalRequestState::Completed,
                TurnOutcome::Completed,
            ),
            (
                2,
                TerminalSource::NormalCompletion,
                FinalRequestState::Cancelled,
                TurnOutcome::Cancelled,
            ),
            (
                3,
                TerminalSource::NormalCompletion,
                FinalRequestState::Failed,
                TurnOutcome::Failed,
            ),
            (
                4,
                TerminalSource::NormalCompletion,
                FinalRequestState::OtherTerminalFailure,
                TurnOutcome::Failed,
            ),
            (
                5,
                TerminalSource::OrphanSettlement,
                FinalRequestState::Failed,
                TurnOutcome::Abandoned,
            ),
            (
                6,
                TerminalSource::OrphanSettlement,
                FinalRequestState::Cancelled,
                TurnOutcome::Cancelled,
            ),
        ] {
            let mut adapter = LifecycleAdapter::new(id(1));
            let terminal = adapter.adapt(LifecycleSource::TurnTerminal {
                scope: scope(50 + index),
                source,
                final_state: state,
                duration_ms: 12,
                input_tokens: Some(1),
                output_tokens: Some(2),
                cache_read_tokens: None,
                stamp: stamp(60 + index, 0),
                error: Some("ERROR_SENTINEL".to_owned()),
            });
            match &terminal[0].metadata {
                LifecycleMetadata::TurnFinished(metadata) => {
                    assert_eq!(metadata.outcome, expected)
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: scope(50 + index),
                    source: TerminalSource::OrphanSettlement,
                    final_state: FinalRequestState::Failed,
                    duration_ms: 13,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(70 + index, 1),
                    error: None,
                })
                .is_empty());
            assert_eq!(
                adapter.diagnostic_count(DiagnosticCode::TurnAlreadyFinalized),
                1
            );
        }
    }

    #[test]
    fn deterministic_sequence_timestamp_and_correlation_inputs_are_preserved() {
        let boot = id(80);
        let event_id = id(81);
        let occurred_at = MillisecondTimestamp::new(
            Utc.timestamp_millis_opt(1_700_000_000_123)
                .single()
                .expect("timestamp"),
        );
        let mut adapter = LifecycleAdapter::new(boot);
        let event = adapter
            .adapt(LifecycleSource::DaemonStarted {
                daemon_version: "0.1.0".to_owned(),
                stamp: EventStamp {
                    event_id,
                    occurred_at,
                },
            })
            .pop()
            .expect("event");
        assert_eq!(event.daemon_boot_id, boot);
        assert_eq!(event.sequence, Sequence(1));
        assert_eq!(event.event_id, event_id);
        assert_eq!(event.occurred_at, occurred_at);
    }

    #[test]
    fn metadata_only_wire_strips_every_forbidden_source_sentinel() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let events = adapter.adapt(LifecycleSource::OrdinaryTurnAdmission {
            admitted: true,
            new_session: true,
            scope: SourceScope {
                project_id: None,
                project_registered: false,
                ..scope(90)
            },
            session_stamp: stamp(91, 0),
            turn_stamp: stamp(92, 1),
            title: "PROMPT_TRANSCRIPT_TITLE_SENTINEL".to_owned(),
            cwd: "/PATH_SECRET_SENTINEL".to_owned(),
        });
        let encoded = events
            .iter()
            .flat_map(|event| encode_frame(event).expect("encode"))
            .collect::<Vec<_>>();
        let encoded = String::from_utf8(encoded).expect("utf8");
        for sentinel in [
            "PROMPT_TRANSCRIPT_TITLE_SENTINEL",
            "PATH_SECRET_SENTINEL",
            "arguments",
            "results",
            "headers",
            "environment",
            "canvas",
        ] {
            assert!(!encoded.contains(sentinel), "{encoded}");
        }
    }

    #[test]
    fn project_scope_is_captured_at_publication_and_delivery_does_not_widen() {
        let project = id(100);
        let other = id(101);
        let mut adapter = LifecycleAdapter::new(id(1));
        let registered = adapter
            .adapt(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: SourceScope {
                    project_id: Some(project),
                    project_registered: true,
                    ..scope(102)
                },
                stamp: stamp(103, 0),
                title: String::new(),
                cwd: String::new(),
            })
            .pop()
            .expect("event");
        let unregistered = adapter
            .adapt(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: SourceScope {
                    project_id: Some(other),
                    project_registered: false,
                    ..scope(104)
                },
                stamp: stamp(105, 1),
                title: String::new(),
                cwd: String::new(),
            })
            .pop()
            .expect("event");
        assert_eq!(registered.scope.project_id, Some(project));
        assert_eq!(unregistered.scope.project_id, None);

        let project_only = ActivationScope {
            global: false,
            projects: HashSet::from([project]),
        };
        assert!(project_only.eligible(&registered));
        assert!(!project_only.eligible(&unregistered));
        let disabled = ActivationScope::default();
        assert!(!disabled.eligible(&registered));
    }

    #[test]
    fn concurrent_session_order_is_global_sequence_and_per_scope_filterable() {
        let a = id(110);
        let b = id(111);
        let mut adapter = LifecycleAdapter::new(id(1));
        for (index, project) in [a, b, a, b].into_iter().enumerate() {
            adapter.adapt(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: SourceScope {
                    project_id: Some(project),
                    project_registered: true,
                    ..scope(120 + index as u128)
                },
                stamp: stamp(130 + index as u128, index as i64),
                title: String::new(),
                cwd: String::new(),
            });
        }
        let retained = adapter.retained().collect::<Vec<_>>();
        assert_eq!(
            retained
                .iter()
                .map(|event| event.sequence.0)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let a_scope = ActivationScope {
            global: false,
            projects: HashSet::from([a]),
        };
        assert_eq!(
            retained
                .iter()
                .filter(|event| a_scope.eligible(event))
                .count(),
            2
        );
    }

    #[test]
    fn terminal_guard_is_atomic_under_competing_sources() {
        let guard = std::sync::Arc::new(TerminalGuard::default());
        let request_id = id(999);
        let claims = std::thread::scope(|scope| {
            (0..16)
                .map(|_| {
                    let guard = std::sync::Arc::clone(&guard);
                    scope.spawn(move || guard.claim(request_id))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().expect("claim thread"))
                .collect::<Vec<_>>()
        });
        assert_eq!(claims.into_iter().filter(|claimed| *claimed).count(), 1);
    }

    #[test]
    fn boot_ring_evicts_oldest_at_byte_bound() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let event = adapter
            .adapt(LifecycleSource::DaemonStarted {
                daemon_version: "0.1.0".to_owned(),
                stamp: stamp(998, 0),
            })
            .pop()
            .expect("event");
        let mut ring = BootRing::default();
        for _ in 0..130 {
            ring.push(event.clone(), 65_536);
        }
        assert!(ring.encoded_bytes <= BOOT_RING_MAX_BYTES);
        assert_eq!(ring.events.len(), 128);
    }

    #[test]
    fn boot_ring_evicts_oldest_at_count_bound_and_keeps_sequence() {
        let mut adapter = LifecycleAdapter::new(id(1));
        for index in 0..(BOOT_RING_MAX_EVENTS + 2) {
            adapter.adapt(LifecycleSource::DaemonStarted {
                daemon_version: "0.1.0".to_owned(),
                stamp: stamp(1_000 + index as u128, index as i64),
            });
        }
        let retained = adapter.retained().collect::<Vec<_>>();
        assert_eq!(retained.len(), BOOT_RING_MAX_EVENTS);
        assert_eq!(retained.first().expect("first").sequence, Sequence(3));
        assert_eq!(
            retained.last().expect("last").sequence,
            Sequence((BOOT_RING_MAX_EVENTS + 2) as u64)
        );
        assert!(adapter.ring.encoded_bytes <= BOOT_RING_MAX_BYTES);
    }
}
