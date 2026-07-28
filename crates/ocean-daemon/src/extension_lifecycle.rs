//! Pure Stage A lifecycle adaptation and boot-local retention.
//!
//! This module deliberately has no process, transport, registry, route, or live
//! producer wiring. It converts explicit authoritative-source facts into the
//! closed metadata-only SDK envelope and retains only encoded-safe frames.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};

use ocean_agent_sdk::extension_lifecycle::{
    encode_frame, DaemonStartedMetadata, DaemonStopReason, DaemonStoppingMetadata, DaemonVersion,
    EmptyMetadata, EventFrame, FrameError, LifecycleEvent, LifecycleEventKind, LifecycleMetadata,
    LifecycleScope, MillisecondTimestamp, PermissionOutcome, PermissionRequestedMetadata,
    PermissionResolvedMetadata, ProtocolName, ProtocolV1, Sequence, ToolFinishedMetadata, ToolName,
    ToolOutcome, ToolStartedMetadata, TurnFinishedMetadata, TurnOutcome,
};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const BOOT_RING_MAX_EVENTS: usize = 2_048;
pub(crate) const BOOT_RING_MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACTIVE_TOOL_CORRELATIONS: usize = BOOT_RING_MAX_EVENTS;
const MAX_OPEN_PERMISSIONS: usize = BOOT_RING_MAX_EVENTS;
const MAX_ACTIVE_TERMINAL_AUTHORITIES: usize = BOOT_RING_MAX_EVENTS;
const MAX_RUNTIME_TOOL_CALL_ID_BYTES: usize = 1_024;

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
#[derive(Clone)]
pub(crate) struct ToolEndDetails {
    pub(crate) cancelled: bool,
    pub(crate) private: Value,
}

/// Exhaustive pure input vocabulary for current Stage A source authorities.
#[derive(Clone)]
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
        authority: TerminalAuthority,
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
    InvalidEvent,
    InvalidEventId,
    SequenceExhausted,
    BookkeepingCapacity,
    InvalidCorrelationScope,
    InvalidRuntimeToolId,
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
            Self::InvalidEvent => "invalid_event",
            Self::InvalidEventId => "invalid_event_id",
            Self::SequenceExhausted => "sequence_exhausted",
            Self::BookkeepingCapacity => "bookkeeping_capacity",
            Self::InvalidCorrelationScope => "invalid_correlation_scope",
            Self::InvalidRuntimeToolId => "invalid_runtime_tool_id",
        }
    }
}

#[derive(Debug, Clone)]
struct ToolCorrelation {
    host_tool_call_id: Uuid,
    tool_name: ToolName,
    started_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RequestCorrelationScope {
    turn_id: Uuid,
    request_id: Uuid,
}

impl RequestCorrelationScope {
    fn from_source(scope: SourceScope) -> Option<Self> {
        Some(Self {
            turn_id: scope.turn_id?,
            request_id: scope.request_id?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolCorrelationKey {
    request: RequestCorrelationScope,
    runtime_tool_call_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PermissionCorrelationKey {
    request: RequestCorrelationScope,
    permission_id: Uuid,
}

#[derive(Debug, Clone)]
struct PreparedEvent {
    event: LifecycleEvent,
    encoded_bytes: usize,
    next_sequence: Option<u64>,
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

/// Cloneable exactly-once authority minted once when a request is registered.
///
/// Normal completion and orphan/panic settlement must carry clones of this
/// same value. No terminal adaptation API exists without it.
#[derive(Clone)]
pub(crate) struct TerminalAuthority {
    request: RequestCorrelationScope,
    claimed: Arc<AtomicBool>,
}

impl TerminalAuthority {
    fn try_claim(&self) -> bool {
        self.claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

pub(crate) struct LifecycleAdapter {
    daemon_boot_id: Uuid,
    next_sequence: Option<u64>,
    ring: BootRing,
    tools: HashMap<ToolCorrelationKey, ToolCorrelation>,
    open_permissions: HashSet<PermissionCorrelationKey>,
    terminal_authorities: HashMap<RequestCorrelationScope, Weak<AtomicBool>>,
    diagnostics: HashMap<DiagnosticCode, u64>,
}

impl LifecycleAdapter {
    pub(crate) fn new(daemon_boot_id: Uuid) -> Self {
        Self {
            daemon_boot_id,
            next_sequence: Some(1),
            ring: BootRing::default(),
            tools: HashMap::new(),
            open_permissions: HashSet::new(),
            terminal_authorities: HashMap::new(),
            diagnostics: HashMap::new(),
        }
    }

    /// Register one admitted request and mint its request-scoped terminal authority.
    ///
    /// Re-registration while any racing owner remains returns the same atomic
    /// authority. Dead weak entries are pruned before enforcing the active cap,
    /// so sequential traffic does not accumulate boot-lifetime tombstones.
    pub(crate) fn register_request(&mut self, scope: SourceScope) -> Option<TerminalAuthority> {
        let Some(request) = RequestCorrelationScope::from_source(scope) else {
            self.count(DiagnosticCode::InvalidCorrelationScope);
            return None;
        };
        self.terminal_authorities
            .retain(|_, authority| authority.strong_count() != 0);
        if let Some(claimed) = self
            .terminal_authorities
            .get(&request)
            .and_then(Weak::upgrade)
        {
            return Some(TerminalAuthority { request, claimed });
        }
        if self.terminal_authorities.len() == MAX_ACTIVE_TERMINAL_AUTHORITIES {
            self.count(DiagnosticCode::BookkeepingCapacity);
            return None;
        }
        let claimed = Arc::new(AtomicBool::new(false));
        self.terminal_authorities
            .insert(request, Arc::downgrade(&claimed));
        Some(TerminalAuthority { request, claimed })
    }

    /// Adapt one source fact. The vector has two events only for a successfully
    /// admitted ordinary new-session turn; every other source yields zero/one.
    pub(crate) fn adapt(&mut self, source: LifecycleSource) -> Vec<LifecycleEvent> {
        match source {
            LifecycleSource::DaemonStarted {
                daemon_version,
                stamp,
            } => {
                let Ok(daemon_version) = DaemonVersion::new(daemon_version) else {
                    self.count(DiagnosticCode::InvalidEvent);
                    return Vec::new();
                };
                self.one(
                    LifecycleEventKind::DaemonStarted,
                    LifecycleScope::default(),
                    LifecycleMetadata::DaemonStarted(DaemonStartedMetadata { daemon_version }),
                    stamp,
                )
            }
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
                authority,
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
                authority,
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
        let (Some(request), Some(permission_id)) = (
            RequestCorrelationScope::from_source(scope),
            scope.permission_id,
        ) else {
            self.count(DiagnosticCode::InvalidCorrelationScope);
            return Vec::new();
        };
        let key = PermissionCorrelationKey {
            request,
            permission_id,
        };
        if self.open_permissions.contains(&key) {
            self.count(DiagnosticCode::DuplicatePermissionRequest);
            return Vec::new();
        }
        if self.open_permissions.len() == MAX_OPEN_PERMISSIONS {
            self.count(DiagnosticCode::BookkeepingCapacity);
            return Vec::new();
        }
        let prepared = match self.prepare_one(
            LifecycleEventKind::PermissionRequested,
            scope.lifecycle(None),
            LifecycleMetadata::PermissionRequested(PermissionRequestedMetadata { tool_name }),
            stamp,
        ) {
            Ok(prepared) => prepared,
            Err(code) => {
                self.count(code);
                return Vec::new();
            }
        };
        self.open_permissions.insert(key);
        self.commit_one(prepared)
    }

    fn permission_resolved(
        &mut self,
        scope: SourceScope,
        resolution: PermissionResolution,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let (Some(request), Some(permission_id)) = (
            RequestCorrelationScope::from_source(scope),
            scope.permission_id,
        ) else {
            self.count(DiagnosticCode::UnmatchedPermissionResolution);
            return Vec::new();
        };
        let key = PermissionCorrelationKey {
            request,
            permission_id,
        };
        if !self.open_permissions.contains(&key) {
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
        let prepared = match self.prepare_one(
            LifecycleEventKind::PermissionResolved,
            scope.lifecycle(None),
            LifecycleMetadata::PermissionResolved(PermissionResolvedMetadata { outcome }),
            stamp,
        ) {
            Ok(prepared) => prepared,
            Err(code) => {
                self.count(code);
                return Vec::new();
            }
        };
        self.open_permissions.remove(&key);
        self.commit_one(prepared)
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
        let Some(request) = RequestCorrelationScope::from_source(scope) else {
            self.count(DiagnosticCode::InvalidCorrelationScope);
            return Vec::new();
        };
        if runtime_tool_call_id.is_empty()
            || runtime_tool_call_id.len() > MAX_RUNTIME_TOOL_CALL_ID_BYTES
        {
            self.count(DiagnosticCode::InvalidRuntimeToolId);
            return Vec::new();
        }
        let key = ToolCorrelationKey {
            request,
            runtime_tool_call_id,
        };
        if self.tools.contains_key(&key) {
            self.count(DiagnosticCode::DuplicateToolStart);
            return Vec::new();
        }
        if self.tools.len() == MAX_ACTIVE_TOOL_CORRELATIONS {
            self.count(DiagnosticCode::BookkeepingCapacity);
            return Vec::new();
        }
        let prepared = match self.prepare_one(
            LifecycleEventKind::ToolStarted,
            scope.lifecycle(Some(host_tool_call_id)),
            LifecycleMetadata::ToolStarted(ToolStartedMetadata {
                tool_name: tool_name.clone(),
            }),
            stamp,
        ) {
            Ok(prepared) => prepared,
            Err(code) => {
                self.count(code);
                return Vec::new();
            }
        };
        self.tools.insert(
            key,
            ToolCorrelation {
                host_tool_call_id,
                tool_name,
                started_at_ms,
            },
        );
        self.commit_one(prepared)
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
        let Some(request) = RequestCorrelationScope::from_source(scope) else {
            self.count(DiagnosticCode::UnmatchedToolEnd);
            return Vec::new();
        };
        let key = ToolCorrelationKey {
            request,
            runtime_tool_call_id: runtime_tool_call_id.to_owned(),
        };
        let Some(correlation) = self.tools.get(&key).cloned() else {
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
        let prepared = match self.prepare_one(
            LifecycleEventKind::ToolFinished,
            scope.lifecycle(Some(correlation.host_tool_call_id)),
            LifecycleMetadata::ToolFinished(ToolFinishedMetadata {
                tool_name: correlation.tool_name,
                outcome,
                duration_ms: ended_at_ms.saturating_sub(correlation.started_at_ms),
                output_bytes,
            }),
            stamp,
        ) {
            Ok(prepared) => prepared,
            Err(code) => {
                self.count(code);
                return Vec::new();
            }
        };
        self.tools.remove(&key);
        self.commit_one(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_turn(
        &mut self,
        scope: SourceScope,
        authority: TerminalAuthority,
        source: TerminalSource,
        final_state: FinalRequestState,
        duration_ms: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        let Some(request) = RequestCorrelationScope::from_source(scope) else {
            self.count(DiagnosticCode::InvalidCorrelationScope);
            return Vec::new();
        };
        let outcome = match (final_state, source) {
            (FinalRequestState::Completed, _) => TurnOutcome::Completed,
            (FinalRequestState::Cancelled, _) => TurnOutcome::Cancelled,
            (_, TerminalSource::OrphanSettlement) => TurnOutcome::Abandoned,
            (FinalRequestState::Failed | FinalRequestState::OtherTerminalFailure, _) => {
                TurnOutcome::Failed
            }
        };
        let prepared = match self.prepare_one(
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
        ) {
            Ok(prepared) => prepared,
            Err(code) => {
                self.count(code);
                return Vec::new();
            }
        };
        if authority.request != request {
            self.count(DiagnosticCode::InvalidCorrelationScope);
            return Vec::new();
        }
        if !authority.try_claim() {
            self.count(DiagnosticCode::TurnAlreadyFinalized);
            return Vec::new();
        }

        // Terminal settlement is the explicit cleanup boundary for abandoned
        // per-request tool and permission state. The shared atomic authority
        // remains alive only while a potential racing owner retains a clone.
        self.tools.retain(|key, _| key.request != request);
        self.open_permissions.retain(|key| key.request != request);
        self.commit_one(prepared)
    }

    fn one(
        &mut self,
        kind: LifecycleEventKind,
        scope: LifecycleScope,
        metadata: LifecycleMetadata,
        stamp: EventStamp,
    ) -> Vec<LifecycleEvent> {
        match self.prepare_one(kind, scope, metadata, stamp) {
            Ok(prepared) => self.commit_one(prepared),
            Err(code) => {
                self.count(code);
                Vec::new()
            }
        }
    }

    fn prepare_one(
        &self,
        kind: LifecycleEventKind,
        scope: LifecycleScope,
        metadata: LifecycleMetadata,
        stamp: EventStamp,
    ) -> Result<PreparedEvent, DiagnosticCode> {
        if stamp.event_id.get_version_num() != 4 {
            return Err(DiagnosticCode::InvalidEventId);
        }
        let sequence = self
            .next_sequence
            .ok_or(DiagnosticCode::SequenceExhausted)?;
        let event = LifecycleEvent {
            protocol: ProtocolName,
            version: ProtocolV1,
            frame: EventFrame,
            daemon_boot_id: self.daemon_boot_id,
            sequence: Sequence(sequence),
            event_id: stamp.event_id,
            occurred_at: stamp.occurred_at,
            kind,
            scope,
            metadata,
        };
        let encoded = encode_frame(&event).map_err(|error| match error {
            FrameError::TooLarge { .. } => DiagnosticCode::OversizedEvent,
            FrameError::InvalidFraming | FrameError::InvalidFrame => DiagnosticCode::InvalidEvent,
        })?;
        Ok(PreparedEvent {
            event,
            encoded_bytes: encoded.len(),
            next_sequence: sequence.checked_add(1),
        })
    }

    fn commit_one(&mut self, prepared: PreparedEvent) -> Vec<LifecycleEvent> {
        self.next_sequence = prepared.next_sequence;
        self.ring
            .push(prepared.event.clone(), prepared.encoded_bytes);
        vec![prepared.event]
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
            turn_id: Some(id(request + 10_000)),
            request_id: Some(id(request)),
            permission_id: None,
        }
    }

    fn invalid_stamp(millis: i64) -> EventStamp {
        EventStamp {
            event_id: Uuid::nil(),
            occurred_at: MillisecondTimestamp::new(
                Utc.timestamp_millis_opt(millis)
                    .single()
                    .expect("timestamp"),
            ),
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
        let terminal_authority = adapter.register_request(scope(13)).expect("authority");
        events.extend(adapter.adapt(LifecycleSource::TurnTerminal {
            scope: scope(13),
            authority: terminal_authority,
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
    fn identical_opaque_tool_ids_are_isolated_per_request_and_project_out_of_order() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let scope_a = SourceScope {
            project_id: Some(id(201)),
            ..scope(202)
        };
        let scope_b = SourceScope {
            project_id: Some(id(203)),
            ..scope(204)
        };
        for (scope, host_id, stamp_id) in [(scope_a, id(205), 206), (scope_b, id(207), 208)] {
            assert_eq!(
                adapter
                    .adapt(LifecycleSource::ToolExecutionStart {
                        scope,
                        runtime_tool_call_id: "same-opaque-id".to_owned(),
                        host_tool_call_id: host_id,
                        tool_name: tool("read"),
                        started_at_ms: 10,
                        stamp: stamp(stamp_id, 0),
                        arguments: Value::Null,
                    })
                    .len(),
                1
            );
        }
        assert_eq!(adapter.tools.len(), 2);

        let finish_b = adapter.adapt(LifecycleSource::ToolExecutionEnd {
            scope: scope_b,
            runtime_tool_call_id: "same-opaque-id".to_owned(),
            is_error: false,
            rendered_output: Vec::new(),
            details: ToolEndDetails {
                cancelled: false,
                private: Value::Null,
            },
            ended_at_ms: 12,
            stamp: stamp(209, 1),
        });
        let finish_a = adapter.adapt(LifecycleSource::ToolExecutionEnd {
            scope: scope_a,
            runtime_tool_call_id: "same-opaque-id".to_owned(),
            is_error: false,
            rendered_output: Vec::new(),
            details: ToolEndDetails {
                cancelled: false,
                private: Value::Null,
            },
            ended_at_ms: 13,
            stamp: stamp(210, 2),
        });
        assert_eq!(finish_b[0].scope.tool_call_id, Some(id(207)));
        assert_eq!(finish_b[0].scope.project_id, Some(id(203)));
        assert_eq!(finish_a[0].scope.tool_call_id, Some(id(205)));
        assert_eq!(finish_a[0].scope.project_id, Some(id(201)));
        assert!(adapter.tools.is_empty());
    }

    #[test]
    fn rejected_publications_do_not_orphan_consume_or_block_valid_retries() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let permission_scope = SourceScope {
            permission_id: Some(id(301)),
            ..scope(302)
        };
        assert!(adapter
            .adapt(LifecycleSource::PermissionWaiting {
                scope: permission_scope,
                tool_name: tool("bash"),
                stamp: invalid_stamp(0),
                arguments: Value::Null,
                reason: String::new(),
            })
            .is_empty());
        assert!(adapter.open_permissions.is_empty());
        assert!(adapter
            .adapt(LifecycleSource::PermissionResolved {
                scope: permission_scope,
                resolution: PermissionResolution::Allow,
                stamp: stamp(303, 1),
                reason: None,
            })
            .is_empty());
        assert_eq!(
            adapter
                .adapt(LifecycleSource::PermissionWaiting {
                    scope: permission_scope,
                    tool_name: tool("bash"),
                    stamp: stamp(304, 2),
                    arguments: Value::Null,
                    reason: String::new(),
                })
                .len(),
            1
        );
        assert!(adapter
            .adapt(LifecycleSource::PermissionResolved {
                scope: permission_scope,
                resolution: PermissionResolution::Allow,
                stamp: invalid_stamp(3),
                reason: None,
            })
            .is_empty());
        assert_eq!(adapter.open_permissions.len(), 1);
        assert_eq!(
            adapter
                .adapt(LifecycleSource::PermissionResolved {
                    scope: permission_scope,
                    resolution: PermissionResolution::Allow,
                    stamp: stamp(305, 4),
                    reason: None,
                })
                .len(),
            1
        );

        let tool_scope = scope(310);
        assert!(adapter
            .adapt(LifecycleSource::ToolExecutionStart {
                scope: tool_scope,
                runtime_tool_call_id: "retry-tool".to_owned(),
                host_tool_call_id: id(311),
                tool_name: tool("read"),
                started_at_ms: 0,
                stamp: invalid_stamp(5),
                arguments: Value::Null,
            })
            .is_empty());
        assert!(adapter.tools.is_empty());
        assert!(adapter
            .adapt(LifecycleSource::ToolExecutionEnd {
                scope: tool_scope,
                runtime_tool_call_id: "retry-tool".to_owned(),
                is_error: false,
                rendered_output: Vec::new(),
                details: ToolEndDetails {
                    cancelled: false,
                    private: Value::Null,
                },
                ended_at_ms: 1,
                stamp: stamp(312, 6),
            })
            .is_empty());
        assert_eq!(
            adapter
                .adapt(LifecycleSource::ToolExecutionStart {
                    scope: tool_scope,
                    runtime_tool_call_id: "retry-tool".to_owned(),
                    host_tool_call_id: id(311),
                    tool_name: tool("read"),
                    started_at_ms: 0,
                    stamp: stamp(313, 7),
                    arguments: Value::Null,
                })
                .len(),
            1
        );
        assert!(adapter
            .adapt(LifecycleSource::ToolExecutionEnd {
                scope: tool_scope,
                runtime_tool_call_id: "retry-tool".to_owned(),
                is_error: false,
                rendered_output: Vec::new(),
                details: ToolEndDetails {
                    cancelled: false,
                    private: Value::Null,
                },
                ended_at_ms: 1,
                stamp: invalid_stamp(8),
            })
            .is_empty());
        assert_eq!(adapter.tools.len(), 1);
        assert_eq!(
            adapter
                .adapt(LifecycleSource::ToolExecutionEnd {
                    scope: tool_scope,
                    runtime_tool_call_id: "retry-tool".to_owned(),
                    is_error: false,
                    rendered_output: Vec::new(),
                    details: ToolEndDetails {
                        cancelled: false,
                        private: Value::Null,
                    },
                    ended_at_ms: 2,
                    stamp: stamp(314, 9),
                })
                .len(),
            1
        );

        let terminal_scope = scope(320);
        let terminal_authority = adapter.register_request(terminal_scope).expect("authority");
        assert!(adapter
            .adapt(LifecycleSource::TurnTerminal {
                scope: terminal_scope,
                authority: terminal_authority.clone(),
                source: TerminalSource::NormalCompletion,
                final_state: FinalRequestState::Completed,
                duration_ms: 1,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                stamp: invalid_stamp(10),
                error: None,
            })
            .is_empty());
        assert!(!terminal_authority.claimed.load(Ordering::Acquire));
        assert_eq!(
            adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: terminal_scope,
                    authority: terminal_authority,
                    source: TerminalSource::NormalCompletion,
                    final_state: FinalRequestState::Completed,
                    duration_ms: 1,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(321, 11),
                    error: None,
                })
                .len(),
            1
        );
    }

    #[test]
    fn invalid_or_oversized_daemon_versions_leave_all_bookkeeping_unchanged() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let invalid = adapter.adapt(LifecycleSource::DaemonStarted {
            daemon_version: "latest".to_owned(),
            stamp: stamp(330, 0),
        });
        let oversized = adapter.adapt(LifecycleSource::DaemonStarted {
            daemon_version: format!("1.0.0+{}", "a".repeat(70_000)),
            stamp: stamp(331, 1),
        });
        assert!(invalid.is_empty());
        assert!(oversized.is_empty());
        assert!(adapter.tools.is_empty());
        assert!(adapter.open_permissions.is_empty());
        assert!(adapter.terminal_authorities.is_empty());
        assert_eq!(adapter.next_sequence, Some(1));
        assert_eq!(adapter.retained().count(), 0);
        assert_eq!(adapter.diagnostic_count(DiagnosticCode::InvalidEvent), 2);
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
            let terminal_scope = scope(50 + index);
            let authority = adapter.register_request(terminal_scope).expect("authority");
            let terminal = adapter.adapt(LifecycleSource::TurnTerminal {
                scope: terminal_scope,
                authority: authority.clone(),
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
                    scope: terminal_scope,
                    authority,
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
    fn more_than_ring_capacity_sequential_requests_each_finalize_once() {
        const REQUESTS: usize = BOOT_RING_MAX_EVENTS + 257;
        let mut adapter = LifecycleAdapter::new(id(1));
        let mut published = 0;

        for index in 0..REQUESTS {
            let request_scope = scope(100_000 + index as u128);
            let authority = adapter
                .register_request(request_scope)
                .expect("sequential request authority");
            published += adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: request_scope,
                    authority: authority.clone(),
                    source: TerminalSource::NormalCompletion,
                    final_state: FinalRequestState::Completed,
                    duration_ms: 1,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(200_000 + index as u128, index as i64),
                    error: None,
                })
                .len();
            published += adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: request_scope,
                    authority,
                    source: TerminalSource::OrphanSettlement,
                    final_state: FinalRequestState::Failed,
                    duration_ms: 2,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(300_000 + index as u128, index as i64),
                    error: None,
                })
                .len();
            assert!(adapter.terminal_authorities.len() <= 1);
        }

        assert_eq!(published, REQUESTS);
        assert_eq!(adapter.retained().count(), BOOT_RING_MAX_EVENTS);
        assert!(adapter.ring.encoded_bytes <= BOOT_RING_MAX_BYTES);
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::TurnAlreadyFinalized),
            REQUESTS as u64
        );
    }

    #[test]
    fn normal_orphan_and_panic_settlement_owners_share_one_authority() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let request_scope = scope(400_000);
        let authority = adapter.register_request(request_scope).expect("authority");
        let sources = [
            TerminalSource::NormalCompletion,
            TerminalSource::OrphanSettlement,
            // Panic handling converges through the orphan-settlement authority.
            TerminalSource::OrphanSettlement,
        ];
        let published = sources
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                adapter
                    .adapt(LifecycleSource::TurnTerminal {
                        scope: request_scope,
                        authority: authority.clone(),
                        source,
                        final_state: FinalRequestState::Failed,
                        duration_ms: index as u64,
                        input_tokens: None,
                        output_tokens: None,
                        cache_read_tokens: None,
                        stamp: stamp(400_001 + index as u128, index as i64),
                        error: None,
                    })
                    .len()
            })
            .sum::<usize>();

        assert_eq!(published, 1);
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::TurnAlreadyFinalized),
            2
        );
    }

    #[test]
    fn distinct_request_authorities_never_collide_or_accept_a_foreign_scope() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let scope_a = scope(500_000);
        let scope_b = scope(500_001);
        let authority_a = adapter.register_request(scope_a).expect("authority a");
        let authority_b = adapter.register_request(scope_b).expect("authority b");

        assert!(adapter
            .adapt(LifecycleSource::TurnTerminal {
                scope: scope_b,
                authority: authority_a.clone(),
                source: TerminalSource::NormalCompletion,
                final_state: FinalRequestState::Completed,
                duration_ms: 1,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                stamp: stamp(500_002, 0),
                error: None,
            })
            .is_empty());
        assert!(!authority_a.claimed.load(Ordering::Acquire));
        assert!(!authority_b.claimed.load(Ordering::Acquire));

        let mut published = 0;
        for (request_scope, authority, stamp_id) in [
            (scope_a, authority_a, 500_003),
            (scope_b, authority_b, 500_004),
        ] {
            published += adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: request_scope,
                    authority,
                    source: TerminalSource::NormalCompletion,
                    final_state: FinalRequestState::Completed,
                    duration_ms: 1,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(stamp_id, 1),
                    error: None,
                })
                .len();
        }
        assert_eq!(published, 2);
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::InvalidCorrelationScope),
            1
        );
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
    fn terminal_settlement_cleans_abandoned_request_state_and_rejects_shared_retry() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let request_scope = SourceScope {
            permission_id: Some(id(401)),
            ..scope(402)
        };
        assert_eq!(
            adapter
                .adapt(LifecycleSource::PermissionWaiting {
                    scope: request_scope,
                    tool_name: tool("bash"),
                    stamp: stamp(403, 0),
                    arguments: Value::Null,
                    reason: String::new(),
                })
                .len(),
            1
        );
        assert_eq!(
            adapter
                .adapt(LifecycleSource::ToolExecutionStart {
                    scope: request_scope,
                    runtime_tool_call_id: "abandoned".to_owned(),
                    host_tool_call_id: id(404),
                    tool_name: tool("bash"),
                    started_at_ms: 0,
                    stamp: stamp(405, 1),
                    arguments: Value::Null,
                })
                .len(),
            1
        );
        assert_eq!(adapter.open_permissions.len(), 1);
        assert_eq!(adapter.tools.len(), 1);
        let authority = adapter.register_request(request_scope).expect("authority");

        assert_eq!(
            adapter
                .adapt(LifecycleSource::TurnTerminal {
                    scope: request_scope,
                    authority: authority.clone(),
                    source: TerminalSource::OrphanSettlement,
                    final_state: FinalRequestState::Failed,
                    duration_ms: 2,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    stamp: stamp(406, 2),
                    error: None,
                })
                .len(),
            1
        );
        assert!(adapter.open_permissions.is_empty());
        assert!(adapter.tools.is_empty());
        assert!(adapter
            .adapt(LifecycleSource::TurnTerminal {
                scope: request_scope,
                authority,
                source: TerminalSource::NormalCompletion,
                final_state: FinalRequestState::Completed,
                duration_ms: 3,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                stamp: stamp(407, 3),
                error: None,
            })
            .is_empty());
    }

    #[test]
    fn active_bookkeeping_caps_fail_closed_without_eviction() {
        let mut guarded = LifecycleAdapter::new(id(1));
        let mut authorities = Vec::with_capacity(MAX_ACTIVE_TERMINAL_AUTHORITIES);
        for index in 0..MAX_ACTIVE_TERMINAL_AUTHORITIES {
            authorities.push(
                guarded
                    .register_request(scope(10_000 + index as u128))
                    .expect("authority within cap"),
            );
        }
        assert_eq!(
            guarded.terminal_authorities.len(),
            MAX_ACTIVE_TERMINAL_AUTHORITIES
        );
        assert!(guarded.register_request(scope(99_999)).is_none());
        assert_eq!(
            guarded.terminal_authorities.len(),
            MAX_ACTIVE_TERMINAL_AUTHORITIES
        );
        drop(authorities);
        assert!(guarded.register_request(scope(99_999)).is_some());
        assert_eq!(guarded.terminal_authorities.len(), 1);

        let mut adapter = LifecycleAdapter::new(id(1));
        for index in 0..MAX_ACTIVE_TOOL_CORRELATIONS {
            let request = RequestCorrelationScope::from_source(scope(20_000 + index as u128))
                .expect("request scope");
            adapter.tools.insert(
                ToolCorrelationKey {
                    request,
                    runtime_tool_call_id: format!("tool-{index}"),
                },
                ToolCorrelation {
                    host_tool_call_id: id(30_000 + index as u128),
                    tool_name: tool("read"),
                    started_at_ms: 0,
                },
            );
        }
        assert!(adapter
            .adapt(LifecycleSource::ToolExecutionStart {
                scope: scope(50_000),
                runtime_tool_call_id: "over-cap".to_owned(),
                host_tool_call_id: id(50_001),
                tool_name: tool("read"),
                started_at_ms: 0,
                stamp: stamp(50_002, 0),
                arguments: Value::Null,
            })
            .is_empty());
        assert_eq!(adapter.tools.len(), MAX_ACTIVE_TOOL_CORRELATIONS);
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::BookkeepingCapacity),
            1
        );

        let mut permissions = LifecycleAdapter::new(id(1));
        for index in 0..MAX_OPEN_PERMISSIONS {
            let source = scope(70_000 + index as u128);
            permissions
                .open_permissions
                .insert(PermissionCorrelationKey {
                    request: RequestCorrelationScope::from_source(source).expect("request scope"),
                    permission_id: id(80_000 + index as u128),
                });
        }
        let over_scope = SourceScope {
            permission_id: Some(id(90_001)),
            ..scope(90_000)
        };
        assert!(permissions
            .adapt(LifecycleSource::PermissionWaiting {
                scope: over_scope,
                tool_name: tool("bash"),
                stamp: stamp(90_002, 0),
                arguments: Value::Null,
                reason: String::new(),
            })
            .is_empty());
        assert_eq!(permissions.open_permissions.len(), MAX_OPEN_PERMISSIONS);
        assert_eq!(
            permissions.diagnostic_count(DiagnosticCode::BookkeepingCapacity),
            1
        );
    }

    #[test]
    fn sequence_exhaustion_emits_u64_max_once_then_fails_closed() {
        let mut adapter = LifecycleAdapter::new(id(1));
        adapter.next_sequence = Some(u64::MAX);
        let last = adapter.adapt(LifecycleSource::DaemonStarted {
            daemon_version: "1.0.0".to_owned(),
            stamp: stamp(60_000, 0),
        });
        assert_eq!(last[0].sequence, Sequence(u64::MAX));
        assert_eq!(adapter.next_sequence, None);
        assert!(adapter
            .adapt(LifecycleSource::DaemonStopping {
                stamp: stamp(60_001, 1),
            })
            .is_empty());
        assert_eq!(adapter.retained().count(), 1);
        assert_eq!(
            adapter.diagnostic_count(DiagnosticCode::SequenceExhausted),
            1
        );
    }

    #[test]
    fn terminal_authority_lives_until_every_racing_owner_drops() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let request_scope = scope(998);
        let authority = adapter.register_request(request_scope).expect("authority");
        let racing_owner = authority.clone();
        let request = RequestCorrelationScope::from_source(request_scope).expect("request");
        let retained = adapter
            .terminal_authorities
            .get(&request)
            .expect("registered weak authority")
            .clone();

        drop(authority);
        assert!(retained.upgrade().is_some());
        drop(racing_owner);
        assert!(retained.upgrade().is_none());

        assert!(adapter.register_request(scope(999)).is_some());
        assert_eq!(adapter.terminal_authorities.len(), 1);
        assert!(!adapter.terminal_authorities.contains_key(&request));
    }

    #[test]
    fn terminal_authority_compare_exchange_allows_one_competing_owner() {
        let mut adapter = LifecycleAdapter::new(id(1));
        let request_scope = scope(999);
        let request = RequestCorrelationScope::from_source(request_scope).expect("request");
        let authority = adapter.register_request(request_scope).expect("authority");
        let claims = std::thread::scope(|scope| {
            (0..16)
                .map(|_| {
                    let authority = authority.clone();
                    scope.spawn(move || {
                        assert_eq!(authority.request, request);
                        authority.try_claim()
                    })
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
