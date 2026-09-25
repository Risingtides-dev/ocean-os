//! Retained-size and slow-client MEASUREMENTS (ROADMAP "Reliability and scale").
//!
//! Measurement only. Nothing in this module changes a bound, a capacity, or a
//! code path: it drives the production buses and the production router and
//! reports what they retain and how a slow SSE consumer is treated. The numbers
//! are recorded in
//! `docs/specs/2026-09-25-retained-size-and-slow-client-measurements.md`.
//!
//! Every fast test asserts the invariant it prints, so CI keeps the recorded
//! behavior honest. Print the tables with:
//!
//! ```text
//! cargo test -p ocean-daemon retained_size_measurements -- --nocapture --test-threads=1
//! cargo test -p ocean-daemon retained_size_measurements -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The second command adds the larger payload sweep (up to 2 MiB tool outputs),
//! which is `#[ignore]`d because it serializes a few hundred MiB in a debug build.
//!
//! Sizes are serialized JSON bytes (the same `serde_json` encoding the bus uses
//! for its replay byte budget and the SSE handlers put on the wire), never RSS.

use std::{
    net::SocketAddr,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use chrono::Utc;
use ocean_agent_sdk::{
    AgentSessionId, AgentTurnEvent, AgentTurnId, AgentTurnStatus, ToolCall, ToolCallId, ToolResult,
};
use ocean_core::{RoomKey, RoomMessageKind, RoomParticipantKind};
use ocean_store::RoomStore;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpSocket, TcpStream},
    sync::broadcast::error::TryRecvError,
};

use crate::{
    bus::{
        AgentEventBus, EventBus, AGENT_EVENT_REPLAY_BUFFER, AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS,
        AGENT_EVENT_REPLAY_MAX_BYTES,
    },
    emit_agent,
    persistent_rooms::{
        publish_room_access_wake_on, publish_room_read_cursor_wake_on, publish_room_wake,
        publish_room_wake_on, with_rooms, RoomAccessWakeBus, RoomReadCursorWakeBus, RoomWakeBus,
    },
    tests::{fake_convene_state, AUTO_CONVENE_ENV_LOCK},
    AppState,
};

/// Production broadcast capacity of the `AgentEventBus` (`/v1/agent/events`).
/// `main` passes a literal; `production_bus_capacities_match_main` pins it.
const PROD_AGENT_BUS_CAPACITY: usize = 1024;
/// Production broadcast capacity of the legacy `EventBus` (`/v1/events`).
const PROD_LEGACY_BUS_CAPACITY: usize = 1024;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

// ── synthetic workload ──────────────────────────────────────────────────────

/// One agent turn's event shape: `TurnStarted`, `deltas` assistant text deltas,
/// `tools` × (`ToolCallStarted` with `args_bytes` of arguments +
/// `ToolCallFinished` with `output_bytes` of output), then `TurnFinished`. This
/// matches the bridge in `agent_turn`: text arrives as many small deltas, and a
/// tool's full rendered output rides `ToolCallFinished` (the 32 KiB transcript
/// cap in `ocean-runtime` does not apply to the live event).
#[derive(Clone, Copy, Debug)]
struct TurnShape {
    deltas: usize,
    delta_bytes: usize,
    tools: usize,
    args_bytes: usize,
    output_bytes: usize,
}

impl TurnShape {
    const fn with_output(output_bytes: usize) -> Self {
        Self {
            deltas: 200,
            delta_bytes: 24,
            tools: 4,
            args_bytes: 256,
            output_bytes,
        }
    }

    fn events_per_turn(self) -> usize {
        2 + self.deltas + 2 * self.tools
    }
}

fn payload(len: usize) -> String {
    "x".repeat(len)
}

fn turn_events(session_id: AgentSessionId, shape: TurnShape) -> Vec<AgentTurnEvent> {
    let turn_id = AgentTurnId::new_v4();
    let mut events = Vec::with_capacity(shape.events_per_turn());
    events.push(AgentTurnEvent::TurnStarted {
        turn_id,
        session_id,
        model: Some("measure-model".into()),
    });
    let delta = payload(shape.delta_bytes);
    for _ in 0..shape.deltas {
        events.push(AgentTurnEvent::AssistantTextDelta {
            session_id,
            turn_id,
            delta: delta.clone(),
        });
    }
    for _ in 0..shape.tools {
        let call_id = ToolCallId::new_v4();
        events.push(AgentTurnEvent::ToolCallStarted {
            session_id,
            turn_id,
            call: ToolCall {
                id: call_id.clone(),
                name: "bash".into(),
                args_json: serde_json::json!({ "command": payload(shape.args_bytes) }),
            },
        });
        events.push(AgentTurnEvent::ToolCallFinished {
            session_id,
            turn_id,
            call_id,
            result: ToolResult {
                ok: true,
                output: payload(shape.output_bytes),
                metadata_json: None,
            },
        });
    }
    events.push(AgentTurnEvent::TurnFinished {
        session_id,
        turn_id,
        status: AgentTurnStatus::Completed,
        error: None,
        wall_ms: Some(1_234),
        output_tokens: Some(1_000),
        input_tokens: Some(10_000),
        cache_read_tokens: None,
        tokens_per_second: Some(42.0),
        context_usage: None,
    });
    events
}

fn encoded(event: &AgentTurnEvent) -> usize {
    serde_json::to_vec(event).expect("event serializes").len()
}

/// Serialized bytes of every event in one turn, in emission order. UUIDs are
/// fixed-width, so every turn of a shape has exactly these sizes.
fn turn_event_sizes(shape: TurnShape) -> Vec<usize> {
    turn_events(AgentSessionId::new_v4(), shape)
        .iter()
        .map(encoded)
        .collect()
}

// ── what a bus is holding ───────────────────────────────────────────────────

#[derive(Debug, Default)]
struct AgentRing {
    events: usize,
    bytes: usize,
    turn_finished: usize,
    tool_finished: usize,
    /// Terminal-floor envelopes no longer in the ring (count-bounded, NOT
    /// counted against the 32 MiB byte budget).
    floor_only: usize,
    floor_only_bytes: usize,
}

fn agent_ring(bus: &AgentEventBus) -> AgentRing {
    let mut ring = AgentRing::default();
    {
        let history = bus.history.lock().expect("history lock");
        for envelope in history.iter() {
            ring.events += 1;
            ring.bytes += envelope.encoded_bytes;
            match envelope.event {
                AgentTurnEvent::TurnFinished { .. } => ring.turn_finished += 1,
                AgentTurnEvent::ToolCallFinished { .. } => ring.tool_finished += 1,
                _ => {}
            }
        }
    }
    let (merged, _rx) = bus.subscribe_with_full_replay();
    ring.floor_only = merged.len() - ring.events;
    // Floor-only envelopes sort ahead of every ring envelope.
    ring.floor_only_bytes = merged
        .iter()
        .take(ring.floor_only)
        .map(|envelope| envelope.encoded_bytes)
        .sum();
    ring
}

/// Legacy `EventBus` replay history, measured through its public seam: capture
/// every emitted envelope live, then find the oldest id `subscribe_with_replay`
/// still anchors on. Retained = that envelope plus everything after it.
struct LegacyTap {
    rx: tokio::sync::broadcast::Receiver<ocean_core::EventEnvelope>,
    seen: std::collections::VecDeque<ocean_core::EventEnvelope>,
}

impl LegacyTap {
    fn new(bus: &EventBus) -> Self {
        Self {
            rx: bus.subscribe(),
            seen: std::collections::VecDeque::new(),
        }
    }

    /// Drain after every emit so the tap itself never lags.
    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(envelope) => {
                    self.seen.push_back(envelope);
                    // The history ceiling is 256; keep a margin above it.
                    while self.seen.len() > 1024 {
                        self.seen.pop_front();
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(error) => panic!("legacy tap must never lag: {error:?}"),
            }
        }
    }

    /// (retained envelopes, retained serialized bytes)
    fn retained(&self, bus: &EventBus) -> (usize, usize) {
        let Some(last) = self.seen.back() else {
            return (0, 0);
        };
        for (index, envelope) in self.seen.iter().enumerate() {
            let (replay, _rx) = bus.subscribe_with_replay(Some(envelope.id));
            if !replay.is_empty() || envelope.id == last.id {
                let bytes = self
                    .seen
                    .iter()
                    .skip(index)
                    .map(|envelope| serde_json::to_vec(envelope).expect("envelope").len())
                    .sum();
                return (replay.len() + 1, bytes);
            }
        }
        (0, 0)
    }
}

fn fmt_bytes(bytes: usize) -> String {
    if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ── 1. retained memory: agent replay ring + legacy history ─────────────────

struct RetentionRow {
    output_bytes: usize,
    sessions: usize,
    turns_per_session: usize,
    events_per_turn: usize,
    bytes_per_turn: usize,
    emitted_events: usize,
    ring: AgentRing,
    legacy_events: usize,
    legacy_bytes: usize,
}

fn measure_retention(shape: TurnShape, sessions: usize, turns_per_session: usize) -> RetentionRow {
    let agent = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    let legacy = EventBus::new(PROD_LEGACY_BUS_CAPACITY);
    let mut tap = LegacyTap::new(&legacy);
    let session_ids: Vec<AgentSessionId> =
        (0..sessions).map(|_| AgentSessionId::new_v4()).collect();
    let mut emitted = 0;
    // Sessions take turns round-robin, the way concurrent sessions interleave.
    for _ in 0..turns_per_session {
        for session_id in &session_ids {
            for event in turn_events(*session_id, shape) {
                emit_agent(&legacy, &agent, *session_id, event);
                emitted += 1;
                tap.drain();
            }
        }
    }
    let (legacy_events, legacy_bytes) = tap.retained(&legacy);
    RetentionRow {
        output_bytes: shape.output_bytes,
        sessions,
        turns_per_session,
        events_per_turn: shape.events_per_turn(),
        bytes_per_turn: turn_event_sizes(shape).iter().sum(),
        emitted_events: emitted,
        ring: agent_ring(&agent),
        legacy_events,
        legacy_bytes,
    }
}

fn binding_cap(row: &RetentionRow) -> &'static str {
    if row.ring.events == AGENT_EVENT_REPLAY_BUFFER {
        "count (2,048)"
    } else if row.ring.events < row.emitted_events {
        "bytes (32 MiB)"
    } else {
        "none"
    }
}

fn print_retention(title: &str, rows: &[RetentionRow]) {
    println!("\n{title}");
    println!(
        "| tool output K | sessions × turns | events/turn | bytes/turn | emitted | ring events | ring bytes | cap binding | TurnFinished in ring | floor-only | legacy events | legacy bytes |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for row in rows {
        println!(
            "| {} | {} × {} | {} | {} | {} | {} | {} | {} | {} | {} ({}) | {} | {} |",
            fmt_bytes(row.output_bytes),
            row.sessions,
            row.turns_per_session,
            row.events_per_turn,
            fmt_bytes(row.bytes_per_turn),
            row.emitted_events,
            row.ring.events,
            fmt_bytes(row.ring.bytes),
            binding_cap(row),
            row.ring.turn_finished,
            row.ring.floor_only,
            fmt_bytes(row.ring.floor_only_bytes),
            row.legacy_events,
            fmt_bytes(row.legacy_bytes),
        );
    }
}

fn assert_retention_invariants(row: &RetentionRow) {
    assert!(row.ring.events <= AGENT_EVENT_REPLAY_BUFFER);
    assert!(row.ring.bytes <= AGENT_EVENT_REPLAY_MAX_BYTES);
    assert!(row.legacy_events <= 256, "legacy history is clamped to 256");
    assert!(row.ring.floor_only <= AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS);
    // The ring is filled to within one event of whichever cap binds.
    let largest_event = row.output_bytes + 512;
    if row.ring.events < row.emitted_events && row.ring.events < AGENT_EVENT_REPLAY_BUFFER {
        assert!(
            row.ring.bytes + largest_event > AGENT_EVENT_REPLAY_MAX_BYTES,
            "a byte-bound ring must be within one event of 32 MiB, held {}",
            row.ring.bytes
        );
    }
}

#[test]
fn agent_replay_ring_and_legacy_history_retained_bytes() {
    let mut rows = Vec::new();
    for (output, turns) in [(KIB, 20), (32 * KIB, 20), (256 * KIB, 20), (MIB, 12)] {
        rows.push(measure_retention(TurnShape::with_output(output), 1, turns));
    }
    // Eight interleaved sessions: the ring is global, so per-session retention
    // is that session's share of it, and every session keeps a floor terminal.
    rows.push(measure_retention(TurnShape::with_output(32 * KIB), 8, 5));
    print_retention(
        "Agent replay ring (/v1/agent/events) and legacy history (/v1/events)",
        &rows,
    );
    for row in &rows {
        assert_retention_invariants(row);
    }
    // 1 KiB outputs: 214-event turns hit the COUNT cap long before 32 MiB.
    assert_eq!(rows[0].ring.events, AGENT_EVENT_REPLAY_BUFFER);
    assert!(rows[0].ring.bytes < 2 * MIB);
    // 256 KiB outputs: still count-bound (four outputs per 210-event turn).
    assert_eq!(rows[2].ring.events, AGENT_EVENT_REPLAY_BUFFER);
    // 1 MiB outputs: the BYTE cap binds; far fewer than 2,048 events stay.
    assert!(rows[3].ring.events < AGENT_EVENT_REPLAY_BUFFER);
    assert!(rows[3].ring.bytes > 31 * MIB);
    // Legacy history never carries tool output (ToolCallFinished mirrors to a
    // payload-free ToolEnded), so it stays small whatever K is.
    assert!(rows.iter().all(|row| row.legacy_bytes < 256 * KIB));
}

/// Large-payload sweep: bash and web_fetch capture up to 2 MiB, and the live
/// `ToolCallFinished` carries all of it.
#[test]
#[ignore = "heavy sweep; run with --ignored --nocapture (see module docs)"]
fn agent_replay_ring_retained_bytes_large_payload_sweep() {
    let mut rows = Vec::new();
    for (output, turns) in [
        (KIB, 50),
        (32 * KIB, 50),
        (256 * KIB, 50),
        (MIB, 20),
        (2 * MIB, 20),
    ] {
        rows.push(measure_retention(TurnShape::with_output(output), 1, turns));
    }
    rows.push(measure_retention(TurnShape::with_output(MIB), 16, 2));
    print_retention("Large-payload sweep", &rows);
    for row in &rows {
        assert_retention_invariants(row);
    }
}

/// The per-session terminal floor is bounded by session COUNT, not bytes, and
/// sits outside the 32 MiB budget. Measure what one floor entry costs.
#[test]
fn terminal_floor_entry_size() {
    let bus = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    let sessions = 64;
    for _ in 0..sessions {
        for event in turn_events(AgentSessionId::new_v4(), TurnShape::with_output(0)) {
            bus.emit(event);
        }
    }
    // Push every terminal out of the ring with another session's deltas.
    let noisy = AgentSessionId::new_v4();
    for _ in 0..AGENT_EVENT_REPLAY_BUFFER {
        bus.emit(AgentTurnEvent::AssistantTextDelta {
            session_id: noisy,
            turn_id: AgentTurnId::new_v4(),
            delta: payload(24),
        });
    }
    let ring = agent_ring(&bus);
    println!(
        "\nterminal floor: {} floor-only entries hold {} ({} per TurnFinished, error=None); cap {} sessions → ≈ {} at the cap",
        ring.floor_only,
        fmt_bytes(ring.floor_only_bytes),
        ring.floor_only_bytes / ring.floor_only.max(1),
        AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS,
        fmt_bytes(ring.floor_only_bytes / ring.floor_only.max(1) * AGENT_EVENT_REPLAY_FLOOR_MAX_SESSIONS),
    );
    assert_eq!(ring.floor_only, sessions);
    assert_eq!(ring.events, AGENT_EVENT_REPLAY_BUFFER);
}

// ── 2. capacity thresholds and what a stalled receiver pins ─────────────────

/// Returns (unread after `capacity` sends, first recv result after `capacity+1`).
fn lag_threshold<T: Clone>(
    capacity: usize,
    mut subscribe: impl FnMut() -> tokio::sync::broadcast::Receiver<T>,
    mut send: impl FnMut(),
) -> (usize, Result<T, TryRecvError>) {
    let mut at_capacity = subscribe();
    for _ in 0..capacity {
        send();
    }
    let unread = at_capacity.len();
    assert!(
        at_capacity.try_recv().is_ok(),
        "exactly `capacity` unread messages must not lag"
    );
    drop(at_capacity);
    let mut over = subscribe();
    for _ in 0..=capacity {
        send();
    }
    (unread, over.try_recv())
}

#[test]
fn broadcast_capacity_lag_thresholds() {
    let session_id = AgentSessionId::new_v4();
    let delta = || AgentTurnEvent::AssistantTextDelta {
        session_id,
        turn_id: AgentTurnId::new_v4(),
        delta: "d".into(),
    };

    let agent = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    let agent_result = lag_threshold(
        PROD_AGENT_BUS_CAPACITY,
        || agent.subscribe_with_replay(None).1,
        || {
            agent.emit(delta());
        },
    );

    let legacy = EventBus::new(PROD_LEGACY_BUS_CAPACITY);
    let legacy_result = lag_threshold(
        PROD_LEGACY_BUS_CAPACITY,
        || legacy.subscribe(),
        || {
            legacy.emit(ocean_core::EventEnvelope::new(
                ocean_core::OceanEvent::AssistantDelta { text: "d".into() },
            ))
        },
    );

    let mut store = ocean_store::SqliteRoomStore::open_in_memory().expect("store");
    let room = RoomKey::new("threshold");
    store
        .create(room.clone(), "threshold", None, Utc::now())
        .expect("room");
    let message = store
        .append_message(
            &room,
            "human",
            RoomParticipantKind::Human,
            RoomMessageKind::Message,
            "hi",
            Utc::now(),
        )
        .expect("message");
    let room_wakes = RoomWakeBus::default();
    let room_result = lag_threshold(
        256,
        || room_wakes.test_subscribe(&room),
        || publish_room_wake_on(&room_wakes, &room, &message),
    );
    let access = RoomAccessWakeBus::default();
    let access_result = lag_threshold(
        64,
        || access.subscribe(),
        || publish_room_access_wake_on(&access, &room),
    );
    let cursor = RoomReadCursorWakeBus::default();
    let cursor_result = lag_threshold(
        64,
        || cursor.subscribe(),
        || publish_room_read_cursor_wake_on(&cursor, &room),
    );

    println!("\nbroadcast lag thresholds (receiver that never reads)");
    println!("| bus | capacity | unread at capacity | recv after capacity+1 sends |");
    println!("|---|---|---|---|");
    let rows: [(&str, usize, usize, String); 5] = [
        (
            "AgentEventBus (/v1/agent/events)",
            PROD_AGENT_BUS_CAPACITY,
            agent_result.0,
            format!("{:?}", agent_result.1.as_ref().map(|_| ())),
        ),
        (
            "EventBus (/v1/events)",
            PROD_LEGACY_BUS_CAPACITY,
            legacy_result.0,
            format!("{:?}", legacy_result.1.as_ref().map(|_| ())),
        ),
        (
            "RoomWakeBus (per room)",
            256,
            room_result.0,
            format!("{:?}", room_result.1.as_ref().map(|_| ())),
        ),
        (
            "RoomAccessWakeBus",
            64,
            access_result.0,
            format!("{:?}", access_result.1.as_ref().map(|_| ())),
        ),
        (
            "RoomReadCursorWakeBus",
            64,
            cursor_result.0,
            format!("{:?}", cursor_result.1.as_ref().map(|_| ())),
        ),
    ];
    for (name, capacity, unread, first) in &rows {
        println!("| {name} | {capacity} | {unread} | {first} |");
    }
    assert!(matches!(agent_result.1, Err(TryRecvError::Lagged(1))));
    assert!(matches!(legacy_result.1, Err(TryRecvError::Lagged(1))));
    assert!(matches!(room_result.1, Err(TryRecvError::Lagged(1))));
    assert!(matches!(access_result.1, Err(TryRecvError::Lagged(1))));
    assert!(matches!(cursor_result.1, Err(TryRecvError::Lagged(1))));
    for (_, capacity, unread, _) in &rows {
        assert_eq!(unread, capacity);
    }
}

/// `main` builds both event buses with literal capacities; keep the recorded
/// numbers tied to the source that ships.
#[test]
fn production_bus_capacities_match_main() {
    let main = include_str!("main.rs");
    assert!(main.contains(&format!("AgentEventBus::new({PROD_AGENT_BUS_CAPACITY})")));
    assert!(main.contains(&format!(
        "events: EventBus::new({PROD_LEGACY_BUS_CAPACITY})"
    )));
    let rooms = include_str!("persistent_rooms.rs");
    assert!(rooms.contains("Self::new(256)"), "RoomWakeBus default");
    assert_eq!(rooms.matches("Self::new(64)").count(), 2, "access + cursor");
}

/// A receiver that stops reading keeps the last `capacity` envelopes alive in
/// the broadcast ring. Those are a SECOND deep copy (`emit` clones into the
/// replay ring and sends the original), counted by neither replay cap. Slots
/// are shared, so N stalled receivers pin the same ≤1,024 envelopes, not N×.
#[test]
fn stalled_receiver_pins_broadcast_slots_outside_the_replay_budget() {
    stalled_receiver_rows(&[(KIB, 10), (32 * KIB, 10), (256 * KIB, 10)]);
}

#[test]
#[ignore = "emits ~20 MiB of 1 MiB events; run with --ignored --nocapture"]
fn stalled_receiver_pins_broadcast_slots_large_payloads() {
    stalled_receiver_rows(&[(MIB, 5)]);
}

fn stalled_receiver_rows(rows: &[(usize, usize)]) {
    println!("\nbytes pinned in the AgentEventBus broadcast ring by one stalled receiver");
    println!("| tool output K | turns emitted | unread | pinned envelopes | pinned bytes | replay ring bytes | total live copies |");
    println!("|---|---|---|---|---|---|---|");
    for &(output, turns) in rows {
        let shape = TurnShape::with_output(output);
        let bus = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
        let (_replay, stalled) = bus.subscribe_with_replay(None);
        let session_id = AgentSessionId::new_v4();
        for _ in 0..turns {
            for event in turn_events(session_id, shape) {
                bus.emit(event);
            }
        }
        let emitted = turns * shape.events_per_turn();
        let unread = stalled.len();
        let pinned = unread.min(PROD_AGENT_BUS_CAPACITY);
        let sizes = turn_event_sizes(shape);
        // The pinned slots are the newest `pinned` emissions.
        let pinned_bytes: usize = (0..pinned)
            .map(|back| sizes[(emitted - 1 - back) % sizes.len()])
            .sum();
        let ring = agent_ring(&bus);
        println!(
            "| {} | {turns} | {unread} | {pinned} | {} | {} | {} |",
            fmt_bytes(output),
            fmt_bytes(pinned_bytes),
            fmt_bytes(ring.bytes),
            fmt_bytes(pinned_bytes + ring.bytes),
        );
        assert_eq!(unread, emitted, "len() counts every unread send");
        assert_eq!(pinned, PROD_AGENT_BUS_CAPACITY);
    }
}

// ── 3. per-connection replay materialization ────────────────────────────────

/// Every `/v1/agent/events` connect snapshots the ring under the history lock.
/// `subscribe_with_replay` and `subscribe_with_full_replay` both call
/// `merged_ordered()`, which deep-clones every retained envelope, even for a
/// plain connect with no `Last-Event-ID` that then discards the clone. Measure
/// the bytes one connection materializes and how long the lock is held.
#[test]
fn connect_materializes_the_whole_replay_ring() {
    let bus = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    let heavy = AgentSessionId::new_v4();
    let quiet = AgentSessionId::new_v4();
    let turn_id = AgentTurnId::new_v4();
    for _ in 0..40 {
        bus.emit(AgentTurnEvent::ToolCallFinished {
            session_id: heavy,
            turn_id,
            call_id: ToolCallId::new_v4(),
            result: ToolResult {
                ok: true,
                output: payload(MIB),
                metadata_json: None,
            },
        });
    }
    for event in turn_events(quiet, TurnShape::with_output(KIB)) {
        bus.emit(event);
    }
    let ring = agent_ring(&bus);

    // `?replay=1&session_id=<quiet>`: clone everything, then scope.
    let (full, _rx) = bus.subscribe_with_full_replay();
    let cloned: usize = full.iter().map(|envelope| envelope.encoded_bytes).sum();
    let (frames, _ids) = crate::agent_replay_frames(full, Some(quiet), false);
    let quiet_frames_bytes: usize = frames.iter().map(|frame| frame.data.len()).sum();
    let (frames_heavy, _ids) = {
        let (full, _rx) = bus.subscribe_with_full_replay();
        crate::agent_replay_frames(full, Some(heavy), false)
    };
    let heavy_frames_bytes: usize = frames_heavy.iter().map(|frame| frame.data.len()).sum();

    // Plain connect (no anchor, no replay) timing, full ring vs empty ring.
    let time_connect = |bus: &AgentEventBus| {
        let started = Instant::now();
        for _ in 0..10 {
            let (replay, _rx) = bus.subscribe_with_replay(None);
            assert!(replay.is_empty());
        }
        started.elapsed() / 10
    };
    let empty = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    let full_ring_connect = time_connect(&bus);
    let empty_ring_connect = time_connect(&empty);

    println!(
        "\nper-connection replay materialization (ring = {} in {} events)",
        fmt_bytes(ring.bytes),
        ring.events
    );
    println!("| connect | ring clone under history lock | serialized in-scope replay frames |");
    println!("|---|---|---|");
    println!(
        "| `?session_id=<quiet>&replay=1` (quiet session, {} own events) | {} | {} |",
        TurnShape::with_output(KIB).events_per_turn(),
        fmt_bytes(cloned),
        fmt_bytes(quiet_frames_bytes)
    );
    println!(
        "| `?session_id=<heavy>&replay=1` | {} | {} |",
        fmt_bytes(cloned),
        fmt_bytes(heavy_frames_bytes)
    );
    println!(
        "| plain connect, no `Last-Event-ID` | {} (discarded) | 0 B |",
        fmt_bytes(cloned)
    );
    println!(
        "plain connect lock hold (debug build, this machine): full ring {full_ring_connect:?}, empty ring {empty_ring_connect:?}"
    );
    assert_eq!(cloned, ring.bytes, "full replay clones exactly the ring");
    assert!(cloned > 30 * MIB);
    assert!(quiet_frames_bytes < 256 * KIB);
    assert!(heavy_frames_bytes > 30 * MIB);
}

// ── 4. slow SSE clients through the real router ────────────────────────────

/// Socket buffer size requested on both ends so the OS absorbs little and the
/// measurement is driven by the daemon's buffers, not by loopback autotuning.
const SMALL_SOCKET_BUFFER: u32 = 16 * 1024;

async fn serve(state: AppState) -> SocketAddr {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .set_send_buffer_size(SMALL_SOCKET_BUFFER)
        .expect("SO_SNDBUF");
    socket
        .bind("127.0.0.1:0".parse().expect("addr"))
        .expect("bind");
    let listener = socket.listen(64).expect("listen");
    let addr = listener.local_addr().expect("local addr");
    let shutdown = state.shutdown.clone();
    let app = crate::app_router(crate::cors::cors_layer(Vec::new())).with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    addr
}

#[derive(Debug)]
struct SseFrame {
    event: String,
    data: String,
}

/// Minimal HTTP/1.1 chunked SSE reader over a raw socket, so the test controls
/// exactly when (and whether) the client reads.
struct SseClient {
    reader: BufReader<TcpStream>,
    pending: String,
}

impl SseClient {
    async fn connect(addr: SocketAddr, path: &str) -> Self {
        let socket = TcpSocket::new_v4().expect("socket");
        socket
            .set_recv_buffer_size(SMALL_SOCKET_BUFFER)
            .expect("SO_RCVBUF");
        let mut stream = socket.connect(addr).await.expect("connect");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).await.expect("status line");
        assert!(status.starts_with("HTTP/1.1 200"), "{path}: {status:?}");
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("header");
            if line == "\r\n" {
                break;
            }
        }
        Self {
            reader,
            pending: String::new(),
        }
    }

    async fn read_chunk(&mut self) -> bool {
        let mut size_line = String::new();
        if self
            .reader
            .read_line(&mut size_line)
            .await
            .expect("chunk size")
            == 0
        {
            return false;
        }
        let size = usize::from_str_radix(size_line.trim(), 16).expect("hex chunk size");
        if size == 0 {
            return false;
        }
        let mut body = vec![0; size + 2];
        self.reader.read_exact(&mut body).await.expect("chunk body");
        body.truncate(size);
        self.pending
            .push_str(std::str::from_utf8(&body).expect("utf8 SSE"));
        true
    }

    /// Next non-comment SSE frame, or `None` if the stream ended.
    async fn next_frame(&mut self) -> Option<SseFrame> {
        loop {
            while let Some(end) = self.pending.find("\n\n") {
                let block: String = self.pending.drain(..end + 2).collect();
                let mut frame = SseFrame {
                    event: "message".into(),
                    data: String::new(),
                };
                let mut fields = false;
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        frame.event = value.trim().to_string();
                        fields = true;
                    } else if let Some(value) = line.strip_prefix("data:") {
                        frame.data.push_str(value.trim_start());
                        fields = true;
                    } else if line.starts_with("id:") {
                        fields = true;
                    }
                }
                if fields {
                    return Some(frame);
                }
            }
            if !self.read_chunk().await {
                return None;
            }
        }
    }

    async fn next_frame_within(&mut self, limit: Duration) -> SseFrame {
        tokio::time::timeout(limit, self.next_frame())
            .await
            .expect("SSE frame timed out")
            .expect("SSE stream ended")
    }
}

const MARKER: &str = "MEASURE-END";

#[derive(Debug, Default)]
struct SlowTally {
    delivered: usize,
    lag_frames: usize,
    /// Legacy rail only: sum of `lagged by N` across lag frames.
    reported_dropped: u64,
    delivered_before_first_lag: Option<usize>,
}

async fn drain_until_marker(client: &mut SseClient, data_event: &str) -> SlowTally {
    let mut tally = SlowTally::default();
    loop {
        let frame = client.next_frame_within(Duration::from_secs(20)).await;
        if frame.data.contains(MARKER) {
            return tally;
        }
        if frame.event == data_event {
            tally.delivered += 1;
        } else if frame.event == "error" {
            tally.lag_frames += 1;
            tally
                .delivered_before_first_lag
                .get_or_insert(tally.delivered);
            if let Some(rest) = frame.data.split("lagged by ").nth(1) {
                tally.reported_dropped += rest
                    .split(' ')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
    }
}

/// One producer, one fast `/v1/agent/events` client that paces the producer,
/// and two clients (agent rail + legacy rail) that read nothing until the burst
/// is over. Answers: does the slow client drop, disconnect, or grow memory, and
/// does it slow the fast client or the producer?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_sse_clients_lag_and_drop_without_backpressure() {
    const BURST: usize = 3 * PROD_AGENT_BUS_CAPACITY;
    const CHUNK_BYTES: usize = 4 * KIB;
    const PACE: usize = 64;

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut state = {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        fake_convene_state(&tmp)
    };
    state.agent_events = AgentEventBus::new(PROD_AGENT_BUS_CAPACITY);
    state.events = EventBus::new(PROD_LEGACY_BUS_CAPACITY);
    let addr = serve(state.clone()).await;

    let mut fast = SseClient::connect(addr, "/v1/agent/events?all=1").await;
    let mut slow_agent = SseClient::connect(addr, "/v1/agent/events?all=1").await;
    let mut slow_legacy = SseClient::connect(addr, "/v1/events").await;

    let (seen_tx, mut seen_rx) = tokio::sync::watch::channel(0usize);
    let fast_reader = tokio::spawn(async move {
        let mut chunks = 0usize;
        let mut lag_frames = 0usize;
        loop {
            let frame = fast.next_frame_within(Duration::from_secs(20)).await;
            if frame.data.contains(MARKER) {
                return (chunks, lag_frames);
            }
            match frame.event.as_str() {
                "tool_call_chunk" => {
                    chunks += 1;
                    let _ = seen_tx.send(chunks);
                }
                "error" => lag_frames += 1,
                _ => {}
            }
        }
    });

    let session_id = AgentSessionId::new_v4();
    let turn_id = AgentTurnId::new_v4();
    let call_id = ToolCallId::new_v4();
    let chunk = payload(CHUNK_BYTES);
    let mut max_emit = Duration::ZERO;
    let mut pace_wait = Duration::ZERO;
    let burst_started = Instant::now();
    for index in 0..BURST {
        let started = Instant::now();
        emit_agent(
            &state.events,
            &state.agent_events,
            session_id,
            AgentTurnEvent::ToolCallChunk {
                session_id,
                turn_id,
                call_id: call_id.clone(),
                chunk: chunk.clone(),
            },
        );
        max_emit = max_emit.max(started.elapsed());
        if (index + 1) % PACE == 0 {
            let waited = Instant::now();
            tokio::time::timeout(
                Duration::from_secs(20),
                seen_rx.wait_for(|seen| *seen > index),
            )
            .await
            .expect("fast client fell behind the paced producer")
            .expect("fast reader alive");
            pace_wait += waited.elapsed();
        }
    }
    emit_agent(
        &state.events,
        &state.agent_events,
        session_id,
        AgentTurnEvent::AssistantTextDelta {
            session_id,
            turn_id,
            delta: MARKER.into(),
        },
    );
    let burst_elapsed = burst_started.elapsed();

    let (fast_chunks, fast_lags) = tokio::time::timeout(Duration::from_secs(20), fast_reader)
        .await
        .expect("fast reader finished")
        .expect("fast reader panicked");
    let agent = drain_until_marker(&mut slow_agent, "tool_call_chunk").await;
    let legacy = drain_until_marker(&mut slow_legacy, "tool_output").await;
    let lag_occurrences = state.sse_lag_events.load(Ordering::Relaxed);
    let dropped_total = state.sse_events_dropped.load(Ordering::Relaxed);
    state.shutdown.cancel();

    println!(
        "\nslow SSE clients through app_router (burst {BURST} × {} ToolCallChunk, bus capacity {PROD_AGENT_BUS_CAPACITY}, socket buffers {} requested)",
        fmt_bytes(CHUNK_BYTES),
        fmt_bytes(SMALL_SOCKET_BUFFER as usize)
    );
    println!("| client | delivered | lag frames | delivered before first lag | dropped (reported) | still connected after burst |");
    println!("|---|---|---|---|---|---|");
    println!(
        "| fast /v1/agent/events (paces producer) | {fast_chunks} | {fast_lags} | – | 0 | yes |"
    );
    println!(
        "| stalled /v1/agent/events | {} | {} | {:?} | {} (frame carries no count) | yes (received marker) |",
        agent.delivered,
        agent.lag_frames,
        agent.delivered_before_first_lag,
        BURST - agent.delivered
    );
    println!(
        "| stalled /v1/events | {} | {} | {:?} | {} | yes (received marker) |",
        legacy.delivered,
        legacy.lag_frames,
        legacy.delivered_before_first_lag,
        legacy.reported_dropped
    );
    println!(
        "producer: {BURST} emits in {burst_elapsed:?} (of which {pace_wait:?} waiting on the fast client), slowest single emit {max_emit:?}"
    );
    println!("daemon counters: sse_lag_events_total={lag_occurrences}, sse_events_dropped_total={dropped_total}");

    // Fast client: every event, no lag, despite two stalled neighbours.
    assert_eq!(fast_chunks, BURST);
    assert_eq!(fast_lags, 0);
    // Stalled clients: lagged and lost events, but were NOT disconnected.
    assert!(agent.lag_frames >= 1);
    assert!(agent.delivered < BURST);
    assert!(legacy.lag_frames >= 1);
    // Legacy accounting is exact: delivered + reported-dropped == emitted.
    assert_eq!(
        legacy.delivered as u64 + legacy.reported_dropped,
        BURST as u64
    );
    assert_eq!(dropped_total, legacy.reported_dropped);
    assert_eq!(
        lag_occurrences,
        (agent.lag_frames + legacy.lag_frames) as u64
    );
}

/// Room SSE: the wake bus carries hints only and the tail re-pages SQLite, so a
/// stalled client should lose nothing. Measure it end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_room_sse_client_is_lossless_and_does_not_block_posting() {
    const MESSAGES: usize = 1_500;
    const BODY_BYTES: usize = KIB;
    const PACE: usize = 32;

    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        fake_convene_state(&tmp)
    };
    let room = RoomKey::new("measure-room");
    with_rooms(&state, |store| {
        store
            .create(room.clone(), "measure-room", None, Utc::now())
            .expect("room");
    });
    let addr = serve(state.clone()).await;
    let path = format!("/v1/rooms/persistent/{}/events", room.as_str());
    let mut fast = SseClient::connect(addr, &path).await;
    let mut slow = SseClient::connect(addr, &path).await;

    let (seen_tx, mut seen_rx) = tokio::sync::watch::channel(None::<u64>);
    let fast_reader = tokio::spawn(async move {
        let mut seqs = Vec::new();
        while seqs.len() < MESSAGES {
            let frame = fast.next_frame_within(Duration::from_secs(20)).await;
            if frame.event == "room_message" {
                let message: ocean_core::RoomMessage =
                    serde_json::from_str(&frame.data).expect("room message");
                seqs.push(message.seq);
                let _ = seen_tx.send(Some(message.seq));
            }
        }
        seqs
    });

    let body = payload(BODY_BYTES);
    let mut max_append = Duration::ZERO;
    let started = Instant::now();
    let mut last_seq = 0;
    for index in 0..MESSAGES {
        let one = Instant::now();
        let message = with_rooms(&state, |store| {
            store.append_message(
                &room,
                "human",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                &body,
                Utc::now(),
            )
        })
        .expect("append");
        publish_room_wake(&state, &room, &message);
        max_append = max_append.max(one.elapsed());
        last_seq = message.seq;
        if (index + 1) % PACE == 0 {
            tokio::time::timeout(
                Duration::from_secs(20),
                seen_rx.wait_for(|seen| seen.is_some_and(|seq| seq >= last_seq)),
            )
            .await
            .expect("fast room client fell behind")
            .expect("fast reader alive");
        }
    }
    let elapsed = started.elapsed();
    let fast_seqs = tokio::time::timeout(Duration::from_secs(20), fast_reader)
        .await
        .expect("fast reader finished")
        .expect("fast reader panicked");

    let mut slow_seqs = Vec::new();
    let mut error_frames = 0;
    while slow_seqs.last() != Some(&last_seq) {
        let frame = slow.next_frame_within(Duration::from_secs(20)).await;
        match frame.event.as_str() {
            "room_message" => {
                let message: ocean_core::RoomMessage =
                    serde_json::from_str(&frame.data).expect("room message");
                slow_seqs.push(message.seq);
            }
            "error" => error_frames += 1,
            _ => {}
        }
    }
    state.shutdown.cancel();

    println!(
        "\nroom SSE through app_router ({MESSAGES} × {} messages; RoomWakeBus capacity 256, tail mpsc 64)",
        fmt_bytes(BODY_BYTES)
    );
    println!("| client | delivered | gaps | error frames |");
    println!("|---|---|---|---|");
    let gaps = |seqs: &[u64]| seqs.windows(2).filter(|w| w[1] != w[0] + 1).count();
    println!("| fast | {} | {} | 0 |", fast_seqs.len(), gaps(&fast_seqs));
    println!(
        "| stalled until burst end | {} | {} | {error_frames} |",
        slow_seqs.len(),
        gaps(&slow_seqs)
    );
    println!("producer: {MESSAGES} appends+wakes in {elapsed:?}, slowest {max_append:?}");

    assert_eq!(fast_seqs.len(), MESSAGES);
    assert_eq!(gaps(&fast_seqs), 0);
    assert_eq!(
        slow_seqs.len(),
        MESSAGES,
        "stalled room client lost nothing"
    );
    assert_eq!(gaps(&slow_seqs), 0);
    assert_eq!(error_frames, 0);
}
