//! Event buses — the daemon's broadcast/pub-sub backbone.
//!
//! Two parallel buses:
//! - `EventBus` — legacy `OceanEvent` broadcast (SSE `/v1/events`).
//! - `AgentEventBus` — full-fidelity `AgentTurnEvent` broadcast (SSE
//!   `/v1/agent/events`), with `Last-Event-ID` replay via a bounded ring buffer.
//!
//! Both provide `subscribe_with_replay` for atomic subscribe + snapshot so no
//! event slips between joining the live broadcast and catching up.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Deref,
    sync::{Arc, Mutex, MutexGuard},
};

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};
use ocean_core::EventEnvelope;
use tokio::sync::broadcast;
use uuid::Uuid;

// ── constants ───────────────────────────────────────────────────────────────

/// Maximum recent agent-event count retained for `Last-Event-ID` replay.
/// Count alone is not a byte bound: tool completions and extension payloads can
/// be large, so [`AGENT_EVENT_REPLAY_MAX_BYTES`] is enforced at the same time.
pub(crate) const AGENT_EVENT_REPLAY_BUFFER: usize = 2048;

/// Maximum serialized bytes retained by the global agent-event replay ring.
/// Live broadcast delivery remains full fidelity; when either replay limit is
/// exceeded, oldest envelopes are evicted until both hold. An individual event
/// larger than this ceiling is delivered live but is not replay-retained.
pub(crate) const AGENT_EVENT_REPLAY_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Maximum number of per-session terminal-event floor entries retained by the
/// agent-event replay ring (TASK-62). The floor keeps the LATEST terminal event
/// (`TurnFinished`) for a session even after the global ring evicts it under
/// load from other sessions, so a reconnecting scoped client always recovers
/// its turn-end. One entry per session bounds the map by active session count;
/// this ceiling is a hard backstop against an unbounded churn of distinct
/// session ids — when exceeded, the oldest terminal (smallest sequence) is
/// dropped and the eviction is logged at warn.
pub(crate) const AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS: usize = 1024;

/// Shared SSE keep-alive interval for both the legacy `/v1/events` rail and the
/// `/v1/agent/events` rail. Set to 3s (down from axum's 15s default) per
/// OCEAN-305 so the TUI's scope-change watcher — which only wakes on incoming
/// lines, including keepalive comments — re-scopes within ~3s instead of ~15s.
/// OCEAN-368 standardized both rails on this single documented contract; keep
/// them in sync via this constant.
pub(crate) const SSE_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Default)]
struct SerializedByteCounter {
    bytes: usize,
}

impl std::io::Write for SerializedByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_event_bytes(event: &AgentTurnEvent) -> usize {
    let mut counter = SerializedByteCounter::default();
    serde_json::to_writer(&mut counter, event)
        .map(|()| counter.bytes)
        .unwrap_or(usize::MAX)
}

// ── EventBus ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct EventBus {
    tx: broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<EventEnvelope>>>,
    history_limit: usize,
}

impl EventBus {
    pub(crate) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            history: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(128)))),
            history_limit: capacity.clamp(1, 256),
        }
    }

    // Used by the daemon unit tests; the live `/v1/events` handler now uses
    // `subscribe_with_replay` (OCEAN-129).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// OCEAN-129: atomically subscribe to the live broadcast and snapshot the
    /// history buffer under the same lock so no event slips through the seam.
    /// When `last_event_id` is present and still buffered, returns the buffered
    /// envelopes strictly AFTER it (in emission order) to replay before the live
    /// stream attaches; otherwise the replay vec is empty (id aged out / no
    /// header) and the caller just attaches the live stream as before.
    pub(crate) fn subscribe_with_replay(
        &self,
        last_event_id: Option<Uuid>,
    ) -> (Vec<EventEnvelope>, broadcast::Receiver<EventEnvelope>) {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let rx = self.tx.subscribe();
        let replay = match last_event_id {
            Some(want) => match history.iter().position(|env| env.id == want) {
                Some(pos) => history.iter().skip(pos + 1).cloned().collect(),
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        (replay, rx)
    }

    pub(crate) fn emit(&self, event: EventEnvelope) {
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            history.push_back(event.clone());
            while history.len() > self.history_limit {
                history.pop_front();
            }
        }

        // `broadcast::send` errors only when there are zero live receivers; the
        // event is still buffered for late subscribers and also lives in
        // `history`, so this is expected (no SSE client connected) and logged at
        // debug — not a dropped event (OCEAN-87).
        if let Err(err) = self.tx.send(event) {
            tracing::debug!(?err, "EventBus: no active subscribers for event");
        }
    }
}

// ── AgentEventBus ────────────────────────────────────────────────────────────

/// Parallel broadcast bus that carries `AgentTurnEvent`s with full fidelity
/// (turn_id, call_id, thinking deltas, tool chunks). The legacy `OceanEvent`
/// bus still ships, but `/v1/agent/events` subscribes here so the TUI can
/// render real-time streaming output without the lossy round-trip.
///
/// OCEAN-129: the bus also keeps a bounded in-memory ring buffer of recent
/// envelopes keyed by id so a reconnecting SSE client carrying a
/// `Last-Event-ID` header can be replayed the events it missed while away,
/// before it attaches to the live broadcast.
#[derive(Clone)]
pub(crate) struct AgentEventBus {
    tx: broadcast::Sender<AgentEventEnvelope>,
    // Exposed to the crate so the daemon's inline SSE-replay tests can assert on
    // the bounded ring buffer directly (bounds/eviction/ordering).
    pub(crate) history: Arc<Mutex<AgentReplayHistory>>,
    history_limit: usize,
    history_byte_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplayGapBounds {
    pub(crate) oldest: Option<Uuid>,
    pub(crate) newest: Option<Uuid>,
}

#[derive(Clone)]
pub(crate) struct AgentEventEnvelope {
    pub(crate) id: Uuid,
    pub(crate) event: AgentTurnEvent,
    pub(crate) encoded_bytes: usize,
    /// Monotonic emission sequence, assigned under the history lock. Ids are
    /// random UUIDs and carry no ordering, so replay ordering — and merging a
    /// resurrected terminal-floor entry back into the ring snapshot in true
    /// emission order — keys off this instead (TASK-62).
    pub(crate) seq: u64,
}

pub(crate) struct AgentReplayHistory {
    envelopes: VecDeque<AgentEventEnvelope>,
    encoded_bytes: usize,
    /// Per-session latest terminal event (`TurnFinished`), retained even after
    /// the ring evicts it, so a reconnecting scoped client always recovers its
    /// turn-end (TASK-62). Not counted against `encoded_bytes`: it is bounded by
    /// session count (`floor_limit`), not payload bytes.
    floor: HashMap<AgentSessionId, AgentEventEnvelope>,
    floor_limit: usize,
    /// Next monotonic emission sequence. Assigned under the same lock that
    /// guards `envelopes`, so ordering is total and consistent with push order.
    next_seq: u64,
}

/// The only reconnect-critical terminal event on the agent bus. Permission
/// prompts/decisions ride the legacy `OceanEvent` / `/v1/events` rail (which
/// keeps its own history), not this bus, so `TurnFinished` is the sole terminal
/// the floor must protect for `/v1/agent/events` reconnect correctness (TASK-62).
fn terminal_floor_session(event: &AgentTurnEvent) -> Option<AgentSessionId> {
    match event {
        AgentTurnEvent::TurnFinished { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

impl AgentReplayHistory {
    fn with_capacity(capacity: usize, floor_limit: usize) -> Self {
        Self {
            envelopes: VecDeque::with_capacity(capacity),
            encoded_bytes: 0,
            floor: HashMap::new(),
            floor_limit,
            next_seq: 0,
        }
    }

    fn recompute_encoded_bytes(&mut self) {
        self.encoded_bytes = self.envelopes.iter().fold(0usize, |total, envelope| {
            total.saturating_add(envelope.encoded_bytes)
        });
    }

    /// Record `envelope` as the latest terminal for `session`, replacing any
    /// older terminal for it. Enforces the session-count ceiling by evicting the
    /// oldest (smallest-sequence) terminal when a NEW session pushes past it.
    fn record_floor(&mut self, session: AgentSessionId, envelope: AgentEventEnvelope) {
        let is_new_session = self.floor.insert(session, envelope).is_none();
        if is_new_session && self.floor.len() > self.floor_limit {
            if let Some((&oldest_session, _)) = self.floor.iter().min_by_key(|(_, env)| env.seq) {
                self.floor.remove(&oldest_session);
                tracing::warn!(
                    floor_limit = self.floor_limit,
                    "AgentReplayHistory: terminal floor exceeded session cap; \
                     evicted oldest session's terminal"
                );
            }
        }
    }

    /// Snapshot the ring merged with any floor terminals no longer present in
    /// the ring, returned in true emission (sequence) order. Floor entries that
    /// survive eviction are always older than every ring entry (eviction is
    /// strictly oldest-first), so they sort ahead; sorting by `seq` keeps the
    /// batch identical to plain ring order whenever the floor adds nothing.
    fn merged_ordered(&self) -> Vec<AgentEventEnvelope> {
        let ring_ids: HashSet<Uuid> = self.envelopes.iter().map(|env| env.id).collect();
        let mut merged: Vec<AgentEventEnvelope> = self.envelopes.iter().cloned().collect();
        for env in self.floor.values() {
            if !ring_ids.contains(&env.id) {
                merged.push(env.clone());
            }
        }
        merged.sort_by_key(|env| env.seq);
        merged
    }
}

impl Deref for AgentReplayHistory {
    type Target = VecDeque<AgentEventEnvelope>;

    fn deref(&self) -> &Self::Target {
        &self.envelopes
    }
}

impl AgentEventBus {
    pub(crate) fn new(capacity: usize) -> Self {
        Self::new_with_history_limits(
            capacity,
            AGENT_EVENT_REPLAY_BUFFER,
            AGENT_EVENT_REPLAY_MAX_BYTES,
        )
    }

    fn new_with_history_limits(
        capacity: usize,
        history_limit: usize,
        history_byte_limit: usize,
    ) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            history: Arc::new(Mutex::new(AgentReplayHistory::with_capacity(
                history_limit.min(256),
                AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS,
            ))),
            history_limit,
            history_byte_limit,
        }
    }

    /// Test-only: shrink the per-session terminal-floor cap so the bound is
    /// exercisable without emitting thousands of distinct sessions.
    #[cfg(test)]
    fn with_floor_limit(self, floor_limit: usize) -> Self {
        self.lock_history().floor_limit = floor_limit;
        self
    }

    fn lock_history(&self) -> MutexGuard<'_, AgentReplayHistory> {
        match self.history.lock() {
            Ok(history) => history,
            Err(poison) => {
                let mut history = poison.into_inner();
                history.recompute_encoded_bytes();
                history
            }
        }
    }

    pub(crate) fn emit(&self, event: AgentTurnEvent) -> Uuid {
        // This is an estimate of retained replay bytes and the exact JSON body
        // shape SSE will later serialize (excluding the envelope UUID/SSE
        // framing). A serialization failure is conservatively treated as
        // oversized: the event stays live but is not retained for replay.
        // Serialize outside the history lock to keep the critical section short.
        let encoded_bytes = serialized_event_bytes(&event);
        let id = Uuid::new_v4();

        // Record into the bounded replay ring BEFORE broadcasting so that a
        // client which subscribes (and snapshots the buffer) concurrently with
        // this emit can never observe the live event without also finding it in
        // the replay buffer — closing the gap/dupe seam (OCEAN-129). Count and
        // serialized-byte limits are enforced together under the history lock.
        // The monotonic sequence and per-session terminal floor are assigned and
        // updated under this same lock so ordering and floor state stay
        // consistent with the ring (TASK-62).
        let envelope = {
            let mut history = self.lock_history();
            let seq = history.next_seq;
            history.next_seq = history.next_seq.wrapping_add(1);
            let envelope = AgentEventEnvelope {
                id,
                event,
                encoded_bytes,
                seq,
            };

            // Resurrection floor: keep the LATEST terminal per session so ring
            // eviction under multi-session load can never lose a reconnecting
            // client's turn-end. Updated before eviction runs — even if this very
            // envelope is immediately evicted by the byte cap, its floor copy
            // survives for replay.
            if let Some(session) = terminal_floor_session(&envelope.event) {
                history.record_floor(session, envelope.clone());
            }

            history.encoded_bytes = history.encoded_bytes.saturating_add(envelope.encoded_bytes);
            history.envelopes.push_back(envelope.clone());
            while history.envelopes.len() > self.history_limit
                || history.encoded_bytes > self.history_byte_limit
            {
                let Some(evicted) = history.envelopes.pop_front() else {
                    break;
                };
                history.encoded_bytes = history.encoded_bytes.saturating_sub(evicted.encoded_bytes);
            }
            envelope
        };

        // `broadcast::send` errors only when there are no live receivers (no SSE
        // client subscribed to `/v1/agent/events`). That's expected during idle
        // periods, so debug — not warn. Per-subscriber *lag* (a slow client that
        // overflows the ring buffer) surfaces on the RECEIVE side as
        // `Lagged(n)`, which the SSE handlers log at warn (OCEAN-87).
        let id = envelope.id;
        if self.tx.send(envelope).is_err() {
            tracing::debug!("AgentEventBus: no active subscribers for event");
        }
        id
    }

    /// Atomically subscribe to the live broadcast and snapshot the replay
    /// buffer under the same lock, so no event can slip between the two. If
    /// `last_event_id` is present and still in the buffer, returns the buffered
    /// envelopes strictly AFTER it (in emission order) for replay; otherwise the
    /// replay vector is empty (the id aged out, or no header was sent), and the
    /// caller just attaches the live stream — matching pre-OCEAN-129 behavior.
    ///
    /// Holding the `history` lock across `self.tx.subscribe()` is the seam
    /// guarantee: `emit` takes the same lock before it sends, so every event is
    /// either already in the snapshot (and will be replayed) or will arrive on
    /// the freshly-created live receiver — never both, never neither. Replayed
    /// ids are still deduped against the live tail by the handler as a belt-and-
    /// suspenders measure.
    pub(crate) fn subscribe_with_replay(
        &self,
        last_event_id: Option<Uuid>,
    ) -> (
        Vec<AgentEventEnvelope>,
        broadcast::Receiver<AgentEventEnvelope>,
    ) {
        let history = self.lock_history();
        let rx = self.tx.subscribe();
        // Merge the resurrection floor into the ring snapshot in emission order
        // so a still-buffered anchor replays past it, and a floor-only terminal
        // (evicted from the ring) is still delivered after the anchor (TASK-62).
        let merged = history.merged_ordered();
        let replay = match last_event_id {
            Some(want) => match merged.iter().position(|env| env.id == want) {
                // Found: replay everything strictly after it.
                Some(pos) => merged[pos + 1..].to_vec(),
                // Not found (aged out / unknown id): no replay.
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        (replay, rx)
    }

    /// Checked replay used by public SSE resumptions. A supplied but absent
    /// anchor is never treated as live-only success: callers receive retained
    /// diagnostics and must emit a typed reset-required gap.
    pub(crate) fn subscribe_with_replay_checked(
        &self,
        last_event_id: Uuid,
        expected_session: Option<ocean_agent_sdk::AgentSessionId>,
    ) -> (
        Result<Vec<AgentEventEnvelope>, ReplayGapBounds>,
        broadcast::Receiver<AgentEventEnvelope>,
    ) {
        let history = self.lock_history();
        let rx = self.tx.subscribe();
        // Snapshot ring + resurrection floor in emission order, then anchor/scope
        // over the merged batch. A session's floored `TurnFinished` that the ring
        // has since evicted is now a valid, in-scope replay target and widens the
        // reset-required gap bounds instead of vanishing (TASK-62).
        let merged = history.merged_ordered();
        let in_scope = |env: &&AgentEventEnvelope| {
            expected_session
                .map(|session_id| env.event.session_id() == Some(session_id))
                .unwrap_or(true)
        };
        let replay = match merged
            .iter()
            .position(|env| env.id == last_event_id && in_scope(&env))
        {
            Some(pos) => Ok(merged[pos + 1..].to_vec()),
            None => {
                let mut scoped = merged.iter().filter(in_scope);
                let oldest = scoped.next().map(|env| env.id);
                let newest = scoped.next_back().map(|env| env.id).or(oldest);
                Err(ReplayGapBounds { oldest, newest })
            }
        };
        (replay, rx)
    }

    /// Publish a session-scoped synchronization barrier and return its replay
    /// id. Callers hold the session operation lease while emitting this fence
    /// and reading/mutating the matching snapshot, so every later session event
    /// is necessarily replayed after the returned id.
    pub(crate) fn emit_session_fence(
        &self,
        session_id: ocean_agent_sdk::AgentSessionId,
    ) -> ocean_core::SessionEventFence {
        let id = self.emit(AgentTurnEvent::Extension {
            extension: "ocean.session_sync_fence".into(),
            payload: serde_json::json!({}),
            scope: Some(session_id),
        });
        ocean_core::SessionEventFence { event_id: Some(id) }
    }

    /// OCEAN-305: atomically subscribe to the live broadcast and snapshot the
    /// ENTIRE replay buffer (in emission order) for replay — for a client that
    /// has no `Last-Event-ID` anchor at all because it was previously connected
    /// unscoped (and thus deliberately received nothing session-bearing it
    /// could anchor on). The caller is expected to apply the same per-event
    /// session scoping it applies to the live stream, so a session-scoped
    /// subscriber only ever sees its own session's buffered events.
    ///
    /// Holding the `history` lock across `self.tx.subscribe()` preserves the
    /// same seam guarantee as [`subscribe_with_replay`]: every event is either
    /// in the snapshot (replayed) or arrives on the fresh live receiver —
    /// never both, never neither.
    pub(crate) fn subscribe_with_full_replay(
        &self,
    ) -> (
        Vec<AgentEventEnvelope>,
        broadcast::Receiver<AgentEventEnvelope>,
    ) {
        let history = self.lock_history();
        let rx = self.tx.subscribe();
        // Merge the resurrection floor so a full-replay recovery (the OCEAN-305
        // first-turn path) also surfaces a session's floored `TurnFinished` that
        // the ring already evicted (TASK-62). The caller's per-event session
        // scope filters this to the reconnecting session, exactly as for live.
        let replay = history.merged_ordered();
        (replay, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{AgentTurnId, AgentTurnStatus, ToolCallId, ToolResult};
    use tokio::sync::broadcast::error::TryRecvError;

    fn turn_finished(session_id: AgentSessionId) -> AgentTurnEvent {
        AgentTurnEvent::TurnFinished {
            session_id,
            turn_id: AgentTurnId::new_v4(),
            status: AgentTurnStatus::Completed,
            error: None,
            wall_ms: None,
            output_tokens: None,
            input_tokens: None,
            cache_read_tokens: None,
            tokens_per_second: None,
            context_usage: None,
        }
    }

    fn delta(session_id: AgentSessionId, text: &str) -> AgentTurnEvent {
        AgentTurnEvent::AssistantTextDelta {
            session_id,
            turn_id: AgentTurnId::new_v4(),
            delta: text.into(),
        }
    }

    /// Characterize finite byte pressure at the bounded downstream bus. The
    /// producer never blocks: a slow capacity-2 subscriber lags while the replay
    /// ring keeps only the newest payloads that fit its byte ceiling, including
    /// an event emitted after the subscriber disconnects.
    #[test]
    fn agent_bus_large_payloads_lag_slow_receiver_and_replay_after_disconnect() {
        const LIVE_EVENTS: usize = 8;
        const OUTPUT_BYTES: usize = 1024 * 1024;
        const REPLAY_BYTE_LIMIT: usize = 3 * (OUTPUT_BYTES + 1024);

        let bus = AgentEventBus::new_with_history_limits(2, 32, REPLAY_BYTE_LIMIT);
        let producer = bus.clone();
        let (_, mut slow_rx) = bus.subscribe_with_full_replay();
        let session_id = AgentSessionId::new_v4();
        let turn_id = AgentTurnId::new_v4();

        let event = || AgentTurnEvent::ToolCallFinished {
            session_id,
            turn_id,
            call_id: ToolCallId::new_v4(),
            result: ToolResult {
                ok: true,
                output: "x".repeat(OUTPUT_BYTES),
                metadata_json: None,
            },
        };

        for _ in 0..LIVE_EVENTS {
            producer.emit(event());
        }
        match slow_rx.try_recv() {
            Err(TryRecvError::Lagged(skipped)) => {
                assert_eq!(skipped, LIVE_EVENTS as u64 - 2);
            }
            Ok(_) => panic!("capacity-2 subscriber unexpectedly received an event without lag"),
            Err(error) => panic!("capacity-2 subscriber should lag, got {error:?}"),
        }

        drop(slow_rx);
        producer.emit(event());

        let (replay, _) = bus.subscribe_with_full_replay();
        assert_eq!(
            replay.len(),
            3,
            "byte cap should retain only the newest events"
        );
        assert!(
            bus.lock_history().encoded_bytes <= REPLAY_BYTE_LIMIT,
            "replay bytes must stay within the configured ceiling"
        );
        for envelope in replay {
            let AgentTurnEvent::ToolCallFinished { result, .. } = envelope.event else {
                panic!("expected tool-call completion");
            };
            assert_eq!(result.output.len(), OUTPUT_BYTES);
        }
    }

    #[test]
    fn serialized_event_counter_matches_json_payload_length() {
        let event = AgentTurnEvent::AssistantTextDelta {
            session_id: AgentSessionId::new_v4(),
            turn_id: AgentTurnId::new_v4(),
            delta: "hello".into(),
        };
        assert_eq!(
            serialized_event_bytes(&event),
            serde_json::to_vec(&event).unwrap().len()
        );
    }

    #[test]
    fn agent_bus_delivers_single_oversized_event_live_without_replay_retention() {
        let bus = AgentEventBus::new_with_history_limits(2, 32, 1024);
        let (_, mut live_rx) = bus.subscribe_with_full_replay();
        let output = "x".repeat(2048);
        bus.emit(AgentTurnEvent::ToolCallFinished {
            session_id: AgentSessionId::new_v4(),
            turn_id: AgentTurnId::new_v4(),
            call_id: ToolCallId::new_v4(),
            result: ToolResult {
                ok: true,
                output: output.clone(),
                metadata_json: None,
            },
        });

        let live = live_rx
            .try_recv()
            .expect("oversized event must remain live");
        let AgentTurnEvent::ToolCallFinished { result, .. } = live.event else {
            panic!("expected tool-call completion");
        };
        assert_eq!(result.output, output);
        let history = bus.lock_history();
        assert!(history.is_empty());
        assert_eq!(history.encoded_bytes, 0);
    }

    #[test]
    fn checked_replay_distinguishes_unknown_anchor_from_empty_success() {
        let bus = AgentEventBus::new_with_history_limits(4, 4, usize::MAX);
        let session_id = AgentSessionId::new_v4();
        let first = bus.emit(AgentTurnEvent::AssistantTextDelta {
            session_id,
            turn_id: AgentTurnId::new_v4(),
            delta: "first".into(),
        });
        let fence = bus
            .emit_session_fence(session_id)
            .event_id
            .expect("retained fence");
        let (after_fence, _) = bus.subscribe_with_replay_checked(fence, Some(session_id));
        assert!(after_fence.expect("known anchor").is_empty());

        let foreign_session = AgentSessionId::new_v4();
        let (foreign_gap, _) = bus.subscribe_with_replay_checked(fence, Some(foreign_session));
        let Err(foreign_bounds) = foreign_gap else {
            panic!("a globally valid anchor from another session must require reset");
        };
        assert_eq!(foreign_bounds.oldest, None);
        assert_eq!(foreign_bounds.newest, None);

        let missing = Uuid::new_v4();
        let (gap, _) = bus.subscribe_with_replay_checked(missing, Some(session_id));
        let Err(bounds) = gap else {
            panic!("unknown anchor must be reset-required");
        };
        assert_eq!(bounds.oldest, Some(first));
        assert_eq!(bounds.newest, Some(fence));
    }

    // TASK-62 (a): a quiet session's terminal survives ring eviction driven by a
    // chatty neighbor, and the `?replay` full-history path still recovers it.
    #[test]
    fn terminal_floor_survives_ring_eviction_for_quiet_session() {
        let bus = AgentEventBus::new_with_history_limits(64, 4, usize::MAX);
        let quiet = AgentSessionId::new_v4();
        let chatty = AgentSessionId::new_v4();

        let quiet_terminal = bus.emit(turn_finished(quiet));
        // Overrun the 4-slot ring with the chatty session's deltas.
        for i in 0..8 {
            bus.emit(delta(chatty, &format!("chatty-{i}")));
        }

        // The terminal is gone from the ring itself...
        assert!(
            bus.lock_history()
                .iter()
                .all(|env| env.id != quiet_terminal),
            "chatty burst should have evicted the quiet terminal from the ring"
        );

        // ...but the floor-backed full replay still surfaces exactly one copy.
        let (replay, _rx) = bus.subscribe_with_full_replay();
        let recovered: Vec<_> = replay
            .iter()
            .filter(|env| env.event.session_id() == Some(quiet))
            .collect();
        assert_eq!(
            recovered.len(),
            1,
            "quiet session's terminal must be recovered"
        );
        assert_eq!(recovered[0].id, quiet_terminal);
        assert!(matches!(
            recovered[0].event,
            AgentTurnEvent::TurnFinished { .. }
        ));
    }

    // TASK-62 (b): a newer terminal for the same session replaces the older floor
    // entry; only the latest is retained and replayed.
    #[test]
    fn terminal_floor_keeps_only_latest_terminal_per_session() {
        let bus = AgentEventBus::new_with_history_limits(64, 4, usize::MAX);
        let session = AgentSessionId::new_v4();
        let noise = AgentSessionId::new_v4();

        let first_terminal = bus.emit(turn_finished(session));
        for i in 0..8 {
            bus.emit(delta(noise, &format!("n1-{i}")));
        }
        let second_terminal = bus.emit(turn_finished(session));
        for i in 0..8 {
            bus.emit(delta(noise, &format!("n2-{i}")));
        }
        assert_ne!(first_terminal, second_terminal);

        let (replay, _rx) = bus.subscribe_with_full_replay();
        let terminals: Vec<_> = replay
            .iter()
            .filter(|env| {
                env.event.session_id() == Some(session)
                    && matches!(env.event, AgentTurnEvent::TurnFinished { .. })
            })
            .map(|env| env.id)
            .collect();
        assert_eq!(
            terminals,
            vec![second_terminal],
            "only the newest terminal should survive"
        );

        let history = bus.lock_history();
        assert_eq!(history.floor.len(), 1);
        assert_eq!(
            history.floor.get(&session).map(|env| env.id),
            Some(second_terminal)
        );
    }

    // TASK-62 (c): a deliberately-lagged subscriber loses the terminal live, but
    // the floor-backed reconnect (the recovery the `LiveLag` reset frame
    // instructs) re-delivers it.
    #[test]
    fn lagged_subscriber_recovers_terminal_via_floor_backed_reconnect() {
        // capacity-2 broadcast ring so a stalled subscriber overflows quickly;
        // 4-slot replay ring so the chatty burst also evicts the terminal.
        let bus = AgentEventBus::new_with_history_limits(2, 4, usize::MAX);
        let quiet = AgentSessionId::new_v4();
        let chatty = AgentSessionId::new_v4();

        let (_replay, mut rx) = bus.subscribe_with_replay(None);
        let quiet_terminal = bus.emit(turn_finished(quiet));
        for i in 0..8 {
            bus.emit(delta(chatty, &format!("chatty-{i}")));
        }

        // The stalled live subscriber lags; the skipped frames included the
        // quiet session's terminal.
        match rx.try_recv() {
            Err(TryRecvError::Lagged(_)) => {}
            Ok(_) => panic!("stalled subscriber unexpectedly received an event without lag"),
            Err(error) => panic!("expected the stalled subscriber to lag, got {error:?}"),
        }

        // Recovery reconnect (full replay) re-delivers the terminal from the floor.
        let (recovered, _rx) = bus.subscribe_with_full_replay();
        assert!(
            recovered.iter().any(|env| env.id == quiet_terminal
                && matches!(env.event, AgentTurnEvent::TurnFinished { .. })),
            "floor-backed reconnect must recover the lagged terminal"
        );
    }

    // TASK-62 (d): merged replay is ordered by emission sequence and never
    // duplicates a terminal held in both the ring and the floor.
    #[test]
    fn merged_replay_is_ordered_and_dedup_free() {
        // Evicted-terminal case: floored entry sorts to the front, appears once.
        let bus = AgentEventBus::new_with_history_limits(64, 4, usize::MAX);
        let quiet = AgentSessionId::new_v4();
        let chatty = AgentSessionId::new_v4();
        let quiet_terminal = bus.emit(turn_finished(quiet));
        for i in 0..8 {
            bus.emit(delta(chatty, &format!("chatty-{i}")));
        }
        let (replay, _rx) = bus.subscribe_with_full_replay();
        let seqs: Vec<_> = replay.iter().map(|env| env.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "merged replay must be in emission order");
        assert_eq!(
            replay.first().map(|env| env.id),
            Some(quiet_terminal),
            "the resurrected terminal is oldest and sorts to the front"
        );
        let unique: HashSet<_> = replay.iter().map(|env| env.id).collect();
        assert_eq!(
            unique.len(),
            replay.len(),
            "no duplicate ids in merged replay"
        );

        // In-ring case: a terminal still in the ring must not be doubled by its
        // floor copy.
        let live = AgentEventBus::new_with_history_limits(64, 16, usize::MAX);
        let session = AgentSessionId::new_v4();
        let terminal = live.emit(turn_finished(session));
        live.emit(delta(session, "after"));
        let (replay, _rx) = live.subscribe_with_full_replay();
        assert_eq!(
            replay.iter().filter(|env| env.id == terminal).count(),
            1,
            "a terminal in both ring and floor must appear exactly once"
        );
    }

    // TASK-62 (e): the floor is bounded by its session cap; the oldest session's
    // terminal is evicted, newest retained.
    #[test]
    fn terminal_floor_respects_session_cap() {
        let bus = AgentEventBus::new_with_history_limits(256, 4, usize::MAX).with_floor_limit(2);
        let mut sessions = Vec::new();
        for _ in 0..5 {
            let session = AgentSessionId::new_v4();
            sessions.push(session);
            bus.emit(turn_finished(session));
            // Evict this terminal from the ring so it lives only in the floor.
            let noise = AgentSessionId::new_v4();
            for i in 0..8 {
                bus.emit(delta(noise, &format!("n-{i}")));
            }
        }

        let history = bus.lock_history();
        assert!(
            history.floor.len() <= 2,
            "floor must stay within its session cap, got {}",
            history.floor.len()
        );
        assert!(
            history.floor.contains_key(sessions.last().unwrap()),
            "the newest session's terminal must be retained"
        );
        assert!(
            !history.floor.contains_key(&sessions[0]),
            "the oldest session's terminal must be evicted past the cap"
        );
    }

    #[test]
    fn agent_bus_poison_recovery_recomputes_bytes_before_eviction() {
        let event = AgentTurnEvent::AssistantTextDelta {
            session_id: AgentSessionId::new_v4(),
            turn_id: AgentTurnId::new_v4(),
            delta: "x".repeat(2048),
        };
        let encoded_bytes = serialized_event_bytes(&event);
        let bus = AgentEventBus::new_with_history_limits(2, 32, encoded_bytes + 1);

        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut history = bus.history.lock().unwrap();
            history.envelopes.push_back(AgentEventEnvelope {
                id: Uuid::new_v4(),
                event: event.clone(),
                encoded_bytes,
                seq: 0,
            });
            // Simulate a panic after the deque mutation but before aggregate
            // accounting. Recovery must rebuild the total from the deque.
            history.encoded_bytes = 0;
            panic!("intentional history poison");
        }));
        assert!(poison_result.is_err());

        bus.emit(event);
        let history = bus.lock_history();
        assert_eq!(history.len(), 1, "recovered accounting must evict one copy");
        assert_eq!(history.encoded_bytes, encoded_bytes);
        assert!(history.encoded_bytes <= encoded_bytes + 1);
    }
}
