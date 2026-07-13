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
    collections::VecDeque,
    ops::Deref,
    sync::{Arc, Mutex, MutexGuard},
};

use ocean_agent_sdk::AgentTurnEvent;
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

    pub(crate) fn recent(&self, limit: usize) -> Vec<EventEnvelope> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        history.iter().rev().take(limit).cloned().collect()
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

#[derive(Clone)]
pub(crate) struct AgentEventEnvelope {
    pub(crate) id: Uuid,
    pub(crate) event: AgentTurnEvent,
    pub(crate) encoded_bytes: usize,
}

pub(crate) struct AgentReplayHistory {
    envelopes: VecDeque<AgentEventEnvelope>,
    encoded_bytes: usize,
}

impl AgentReplayHistory {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            envelopes: VecDeque::with_capacity(capacity),
            encoded_bytes: 0,
        }
    }

    fn recompute_encoded_bytes(&mut self) {
        self.encoded_bytes = self.envelopes.iter().fold(0usize, |total, envelope| {
            total.saturating_add(envelope.encoded_bytes)
        });
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
            ))),
            history_limit,
            history_byte_limit,
        }
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

    pub(crate) fn emit(&self, event: AgentTurnEvent) {
        // This is an estimate of retained replay bytes and the exact JSON body
        // shape SSE will later serialize (excluding the envelope UUID/SSE
        // framing). A serialization failure is conservatively treated as
        // oversized: the event stays live but is not retained for replay.
        let encoded_bytes = serialized_event_bytes(&event);
        let envelope = AgentEventEnvelope {
            id: Uuid::new_v4(),
            event,
            encoded_bytes,
        };

        // Record into the bounded replay ring BEFORE broadcasting so that a
        // client which subscribes (and snapshots the buffer) concurrently with
        // this emit can never observe the live event without also finding it in
        // the replay buffer — closing the gap/dupe seam (OCEAN-129). Count and
        // serialized-byte limits are enforced together under the history lock.
        {
            let mut history = self.lock_history();
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
        }

        // `broadcast::send` errors only when there are no live receivers (no SSE
        // client subscribed to `/v1/agent/events`). That's expected during idle
        // periods, so debug — not warn. Per-subscriber *lag* (a slow client that
        // overflows the ring buffer) surfaces on the RECEIVE side as
        // `Lagged(n)`, which the SSE handlers log at warn (OCEAN-87).
        if self.tx.send(envelope).is_err() {
            tracing::debug!("AgentEventBus: no active subscribers for event");
        }
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
        let replay = match last_event_id {
            Some(want) => match history.iter().position(|env| env.id == want) {
                // Found: replay everything strictly after it.
                Some(pos) => history.iter().skip(pos + 1).cloned().collect(),
                // Not found (aged out / unknown id): no replay.
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        (replay, rx)
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
        let replay = history.iter().cloned().collect();
        (replay, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{AgentSessionId, AgentTurnId, ToolCallId, ToolResult};
    use tokio::sync::broadcast::error::TryRecvError;

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
