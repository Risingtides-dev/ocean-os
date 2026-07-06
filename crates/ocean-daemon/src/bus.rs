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
    sync::{Arc, Mutex},
};

use ocean_core::EventEnvelope;
use ocean_agent_sdk::AgentTurnEvent;
use tokio::sync::broadcast;
use uuid::Uuid;

// ── constants ───────────────────────────────────────────────────────────────

/// How many recent agent events the bus retains for `Last-Event-ID` replay
/// (OCEAN-129). Each envelope is a small enum value plus a UUID — well under a
/// few KB even for the largest variants (tool chunks / thinking deltas) — so
/// 2048 entries caps the buffer at a handful of MB while covering a generous
/// reconnect window (a full streaming turn is typically a few hundred events).
/// When the buffer overflows, the oldest entries are evicted; a client whose
/// `Last-Event-ID` has already aged out simply gets the live stream with no
/// replay (same as the pre-OCEAN-129 behavior), so memory stays bounded.
pub(crate) const AGENT_EVENT_REPLAY_BUFFER: usize = 2048;

/// Shared SSE keep-alive interval for both the legacy `/v1/events` rail and the
/// `/v1/agent/events` rail. Set to 3s (down from axum's 15s default) per
/// OCEAN-305 so the TUI's scope-change watcher — which only wakes on incoming
/// lines, including keepalive comments — re-scopes within ~3s instead of ~15s.
/// OCEAN-368 standardized both rails on this single documented contract; keep
/// them in sync via this constant.
pub(crate) const SSE_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

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
    pub(crate) history: Arc<Mutex<VecDeque<AgentEventEnvelope>>>,
    history_limit: usize,
}

#[derive(Clone)]
pub(crate) struct AgentEventEnvelope {
    pub(crate) id: Uuid,
    pub(crate) event: AgentTurnEvent,
}

impl AgentEventBus {
    pub(crate) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            history: Arc::new(Mutex::new(VecDeque::with_capacity(
                AGENT_EVENT_REPLAY_BUFFER.min(256),
            ))),
            history_limit: AGENT_EVENT_REPLAY_BUFFER,
        }
    }

    pub(crate) fn emit(&self, event: AgentTurnEvent) {
        let envelope = AgentEventEnvelope {
            id: Uuid::new_v4(),
            event,
        };

        // Record into the bounded replay ring BEFORE broadcasting so that a
        // client which subscribes (and snapshots the buffer) concurrently with
        // this emit can never observe the live event without also finding it in
        // the replay buffer — closing the gap/dupe seam (OCEAN-129).
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            history.push_back(envelope.clone());
            while history.len() > self.history_limit {
                history.pop_front();
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
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let rx = self.tx.subscribe();
        let replay = history.iter().cloned().collect();
        (replay, rx)
    }
}
