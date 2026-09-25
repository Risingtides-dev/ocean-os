# Retained-Size and Slow-Client Measurements

**Date:** 2026-09-25
**Baseline commit:** `79060d61` (`origin/main` when the measurements were taken)
**ROADMAP item:** "Reliability and scale" → *Add end-to-end retained-size and slow-client measurements before changing history/event architecture*
**Status:** Measurement only. No bound, capacity, code path, or architecture was changed. The measurements exist so a later decision about the per-turn MPSC policy, larger payload limits, or the replay path can cite numbers. This document does not make that decision.

Related earlier evidence: [`2026-07-12-ocean-event-payload-characterization.md`](2026-07-12-ocean-event-payload-characterization.md) mapped the event path and led to the 32 MiB replay byte cap. This document measures the path as it stands now, including rooms, the legacy rail, the persisted transcript, and real SSE sockets.

## Questions

1. **Retained memory.** How many serialized bytes does the daemon hold per session and per turn, in events and in history, as turns and tool-output sizes grow?
2. **Slow client.** When an SSE consumer is slow, does it lag and drop, get disconnected, or grow memory without bound? What are the exact capacity thresholds of the agent-event bus, the control (legacy) event bus, and the room buses? Does a slow client affect fast clients or the producer?

## Method

Everything is measured in **serialized JSON bytes**: the same `serde_json` encoding the agent bus uses for its replay byte budget and the SSE handlers put on the wire. RSS was not measured, and no figure here is an RSS figure.

The measurements are Rust tests that drive production code. Fast tests run in CI and assert every invariant they print. Heavier sweeps are `#[ignore]`d.

| File | What it drives |
|---|---|
| `crates/ocean-daemon/src/retained_size_measurements.rs` | The production `AgentEventBus`, `EventBus`, `RoomWakeBus`, `RoomAccessWakeBus`, and `RoomReadCursorWakeBus` at their production capacities. Events are published through `emit_agent`, the same function the turn bridge uses, so both rails see what they see in production. SSE clients connect over real TCP sockets to the production router: `app_router(cors_layer(..))`, served by `axum::serve`. |
| `crates/ocean-agent/src/session_retained_size_measurements.rs` | The real `compact_history`, `cap_session_history`, and `session::save` functions, called in the same load → compact → accept-save → round-checkpoint saves → final-save order that `run_turn_inner` uses. |

**Synthetic turn.** One turn is 210 events:

- `TurnStarted`
- 200 `AssistantTextDelta` events of 24 bytes each
- 4 tool calls, each a `ToolCallStarted` with 256 bytes of arguments plus a `ToolCallFinished` whose output is **K** bytes
- `TurnFinished`

This follows the bridge in `agent_turn`: text arrives as many small deltas, and a tool's full rendered output rides the live `ToolCallFinished`. The runtime's 32 KiB transcript cap does not apply to the live event. K is swept over 1 KiB, 32 KiB, 256 KiB, 1 MiB, and 2 MiB. The upper end is realistic because bash and web_fetch capture up to 2 MiB.

**Transcript turn.** One user message of 400 bytes, then 4 rounds each made of an assistant `toolCall` with 256 bytes of arguments and a `toolResult` capped the way the runtime caps it. The turn ends with a 1,600-byte assistant reply. That is 10 messages and 6 saves per turn: accept, 4 checkpoints, and final. The runtime's 32 KiB tool-result truncation is private to `ocean-runtime`, so the test mirrors it, marker included, as `RUNTIME_MAX_TOOL_RESULT_BYTES`.

**Slow client.** A slow client is a raw-socket HTTP/1.1 client that reads the response head and then reads nothing until the burst is over. Both ends request 16 KiB socket buffers so that loopback autotuning does not hide the daemon's own buffering. The fast client paces the producer: every 64 events, the producer waits until the fast client has seen them. This models a producer that is slower than a healthy client, so any loss a fast client showed would be real.

### Reproduce

```bash
# fast, CI-safe (each suite finishes in ~2-3 s in a debug build)
cargo test -p ocean-daemon retained_size_measurements -- --nocapture --test-threads=1
cargo test -p ocean-agent session_retained_size_measurements -- --nocapture --test-threads=1

# add the heavy sweeps (#[ignore]d; ~30-45 s each in a debug build)
cargo test -p ocean-daemon retained_size_measurements -- --include-ignored --nocapture --test-threads=1
cargo test -p ocean-agent session_retained_size_measurements -- --include-ignored --nocapture --test-threads=1
```

Environment: macOS 26.5.2 on an arm64 Mac mini (Mac16,10), with `rustc 1.97.0 (2d8144b78 2026-07-07)` and the debug test profile. Byte counts are deterministic: UUIDs have a fixed width and payloads are fixed. Timings and the "delivered before first lag" count depend on the OS and hyper, and are shown only as observed on this machine.

## Buffer inventory

| Buffer | Owner | Bounded by | Capacity | Byte-bounded? | Overflow behavior |
|---|---|---|---|---|---|
| Agent replay ring | `bus.rs` `AgentReplayHistory` | events **and** bytes | 2,048 events / 32 MiB serialized | **yes** | Oldest events are evicted. An event larger than 32 MiB is delivered live but not retained. |
| Agent terminal floor | `bus.rs` `AgentReplayHistory.floor` | session count | 1,024 sessions (latest `TurnFinished` each) | no, but one small event per session | Oldest session's terminal is evicted, with a warning. |
| Agent broadcast ring | `AgentEventBus` tokio `broadcast` | events | 1,024 (`main.rs` literal) | **no** | A receiver more than 1,024 behind gets `Lagged(n)`. The producer never blocks. |
| Legacy replay history | `bus.rs` `EventBus.history` | events | 256 (`capacity.clamp(1, 256)`) | **no** | Oldest is evicted. |
| Legacy broadcast ring | `EventBus` tokio `broadcast` | events | 1,024 (`main.rs` literal) | **no** | `Lagged(n)` |
| Room wake bus | `persistent_rooms.rs` `RoomWakeBus` | hints, per room | 256 | hints only (room key + seq) | `Lagged`; the tail re-pages SQLite. |
| Room access wake bus | `RoomAccessWakeBus` | hints, daemon-wide | 64 | hints only | `Lagged`; the tail re-reads the projection. |
| Room read-cursor wake bus | `RoomReadCursorWakeBus` | hints, daemon-wide | 64 | hints only | `Lagged`; the tail re-reads the projection. |
| Room tail → SSE queue | `room_message_tail` `mpsc` | messages | 64 (access 16, cursor 16) | no, but bounded by count | Tail task awaits (backpressure on its own task only). |
| Per-connection replay snapshot | `subscribe_with_replay*` → `merged_ordered()` | the replay ring | whole ring, up to 32 MiB | inherits | Transient, one per connect. See failure mode 3. |
| Per-connection hyper write buffer + socket | axum/hyper | hyper's HTTP/1 buffer limit | 94–102 × 4 KiB frames observed (~380–410 KiB) | effectively yes | hyper stops polling the stream, and the broadcast receiver falls behind. |
| Runtime → daemon per-turn queue | `main.rs` `unbounded_channel::<AgentEvent>` | nothing | unbounded | **no** | Not measured. See "Not measured". |
| Persisted session transcript | `ocean-agent` `session::save` | 200 messages (`MAX_SESSION_MESSAGES`), 32 KiB per tool-result text block (runtime), token-trigger elision (`compact_history`) | ≤ ~0.53 MiB (200k window) / ≤ ~2 MiB (1M window), measured | yes, indirectly | Oldest messages are dropped, and old tool results are elided to a marker. |
| Extension lifecycle boot ring | `extension_lifecycle.rs` | events and bytes | 2,048 / 8 MiB | yes | Out of scope here; listed because the turn bridge also publishes rendered tool output to it. |

The last-verified capacities are pinned by `production_bus_capacities_match_main`, which reads `main.rs` and `persistent_rooms.rs`. The thresholds themselves are measured, not read from source.

## Results

### 1. Retained replay bytes per turn and per session

`agent_replay_ring_and_legacy_history_retained_bytes` (fast) and `agent_replay_ring_retained_bytes_large_payload_sweep` (ignored):

| tool output K | sessions × turns | events/turn | bytes/turn | emitted | ring events | ring bytes | cap binding | `TurnFinished` in ring | floor-only | legacy events | legacy bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.0 KiB | 1 × 50 | 210 | 39.7 KiB | 10,500 | 2,048 | 388.4 KiB | count (2,048) | 10 | 0 | 256 | 53.9 KiB |
| 32.0 KiB | 1 × 50 | 210 | 163.7 KiB | 10,500 | 2,048 | 1.59 MiB | count (2,048) | 10 | 0 | 256 | 53.9 KiB |
| 256.0 KiB | 1 × 50 | 210 | 1.03 MiB | 10,500 | 2,048 | 10.34 MiB | count (2,048) | 10 | 0 | 256 | 53.9 KiB |
| 1.00 MiB | 1 × 20 | 210 | 4.03 MiB | 4,200 | 1,477 | 31.25 MiB | bytes (32 MiB) | 8 | 0 | 256 | 53.9 KiB |
| 2.00 MiB | 1 × 20 | 210 | 8.03 MiB | 4,200 | 637 | 30.11 MiB | bytes (32 MiB) | 4 | 0 | 256 | 53.9 KiB |
| 32.0 KiB | 8 × 5 (interleaved) | 210 | 163.7 KiB | 8,400 | 2,048 | 1.59 MiB | count (2,048) | 10 | 0 | 256 | 53.9 KiB |
| 1.00 MiB | 16 × 2 (interleaved) | 210 | 4.03 MiB | 6,720 | 1,477 | 31.25 MiB | bytes (32 MiB) | 8 | 8 (1.8 KiB) | 256 | 53.9 KiB |

What the rows show:

- **Per turn.** Retained bytes per turn are about `210 × ~190 B + 4 × K`, so tool output dominates from K ≈ 10 KiB up. At this event shape the **count cap binds for K up to about 830 KiB**, where 2,048 events equal 9.75 turns and 39 outputs fill 32 MiB. Above that the byte cap binds.
- **Replay depth.** In both regimes the ring holds **about 10 turns or fewer** of history in total. At K = 2 MiB it holds 4 turns.
- **Per session.** The ring is global: its window is shared by every session and is not multiplied by the session count. A session's retained share is its fraction of the last ≤ 2,048 events / ≤ 32 MiB. With 16 interleaved sessions, 8 of the 16 `TurnFinished` events had left the ring and survived only in the terminal floor.
- **Terminal floor.** A floor entry measured 228 B per `TurnFinished` (`error: None`), so at the 1,024-session cap the floor is about 228 KiB. It is outside the 32 MiB budget and bounded by count only. A `TurnFinished` that carries a long `error` string costs proportionally more (`terminal_floor_entry_size`).
- **Legacy rail.** The legacy history is flat at 256 events / 53.9 KiB for every K. `ToolCallFinished` mirrors to a payload-free `ToolEnded`, so the legacy rail never carries tool output. It does carry `ToolCallStarted` arguments (`ToolStarted.args`) and has no byte cap, so its bound is 256 × the largest tool-argument payload. For example, a `write` call carries the whole file. With 256-byte arguments that is small. Large arguments were not swept.

### 2. Capacity thresholds

`broadcast_capacity_lag_thresholds`: a receiver subscribes and never reads.

| bus | capacity | unread at capacity | first recv after capacity + 1 sends |
|---|---|---|---|
| `AgentEventBus` (`/v1/agent/events`) | 1,024 | 1,024 (no lag) | `Lagged(1)` |
| `EventBus` (`/v1/events`) | 1,024 | 1,024 (no lag) | `Lagged(1)` |
| `RoomWakeBus` (per room) | 256 | 256 (no lag) | `Lagged(1)` |
| `RoomAccessWakeBus` | 64 | 64 (no lag) | `Lagged(1)` |
| `RoomReadCursorWakeBus` | 64 | 64 (no lag) | `Lagged(1)` |

The thresholds are exact: a receiver may fall exactly `capacity` messages behind without loss, and one more send makes it lag.

### 3. What a stalled receiver pins outside the replay budget

`stalled_receiver_pins_broadcast_slots_outside_the_replay_budget` (fast) and `stalled_receiver_pins_broadcast_slots_large_payloads` (ignored): `emit` deep-clones each event into the replay ring and sends the original into the broadcast ring. A slot's value is freed only when every receiver has read it. So a receiver that stops reading keeps the newest 1,024 envelopes alive as a **second copy** that neither replay cap counts.

| tool output K | turns emitted | unread (`len()`) | pinned envelopes | pinned bytes | replay ring bytes | total live copies |
|---|---|---|---|---|---|---|
| 1.0 KiB | 10 | 2,100 | 1,024 | 194.2 KiB | 388.4 KiB | 582.6 KiB |
| 32.0 KiB | 10 | 2,100 | 1,024 | 814.2 KiB | 1.59 MiB | 2.39 MiB |
| 256.0 KiB | 10 | 2,100 | 1,024 | 5.17 MiB | 10.34 MiB | 15.51 MiB |
| 1.00 MiB | 5 (ignored test) | 1,050 | 1,024 | 20.17 MiB | 20.17 MiB | 40.34 MiB |

Slots are shared across receivers, so N stalled receivers pin the same ≤ 1,024 envelopes, not N× that. The pinned bytes are bounded by `1,024 × largest event`, not by any byte figure. With this event shape the pinned share stays below the replay ring. A stream dominated by large events is the worst case: with 2 MiB outputs that is up to about 2 GiB of pinned copies, which is arithmetic from the measured per-event size and was not allocated.

A production receiver always exists. The Observatory durability pump subscribes at boot, and when it lags it logs `observatory pump lagged; facts were lost`.

### 4. Per-connection replay materialization

`connect_materializes_the_whole_replay_ring`: the ring was filled with 40 × 1 MiB `ToolCallFinished` events from one busy session, followed by one 1 KiB turn from a quiet session. The ring then held 31.05 MiB in 241 events.

| connect | ring clone under the history lock | serialized in-scope replay frames |
|---|---|---|
| `?session_id=<quiet>&replay=1` (210 own events) | 31.05 MiB | 39.7 KiB |
| `?session_id=<heavy>&replay=1` | 31.05 MiB | 31.01 MiB |
| plain connect, no `Last-Event-ID` | 31.05 MiB (discarded unused) | 0 B |

`subscribe_with_replay`, `subscribe_with_replay_checked`, and `subscribe_with_full_replay` all call `merged_ordered()` before they look at the anchor or the scope. That call deep-clones every retained envelope while the history mutex is held. `emit` takes the same mutex. So each connect, reconnect, or scope switch:

- costs a transient copy of the whole ring (up to 32 MiB), even when the connection replays nothing;
- blocks every emitter for as long as the copy takes.

Observed lock hold for a plain connect was **0.9–1.2 ms with the full ring and 0.7–1.1 µs with an empty ring** across four runs (debug build, this machine; not asserted). A `?replay=1` connect for the heavy session also holds its serialized frames, so it peaks at roughly 2 × 31 MiB until the replay is written out. Concurrent reconnects multiply this per connection.

### 5. Slow SSE clients through the real router

`slow_sse_clients_lag_and_drop_without_backpressure`: a burst of 3,072 × 4 KiB `ToolCallChunk` events at production capacity 1,024, with 16 KiB socket buffers requested.

| client | delivered | lag frames | delivered before first lag | dropped | still connected after burst |
|---|---|---|---|---|---|
| fast `/v1/agent/events` (paces the producer) | 3,072 | 0 | – | 0 | yes |
| stalled `/v1/agent/events` | 1,117–1,125 | 1 | 94–102 | 1,947–1,955 (the lag frame carries no count) | yes |
| stalled `/v1/events` | 1,117 | 1 | 94 | 1,955 (reported in the frame) | yes |

Delivered counts are from three runs; the legacy row was identical in all three.

- **Producer.** 3,072 emits took 605–764 ms, of which 316–364 ms was waiting on the fast client. The slowest single emit took 118 µs–1.3 ms.
- **Daemon counters.** `sse_lag_events_total` = 2 and `sse_events_dropped_total` = 1,955.

`slow_room_sse_client_is_lossless_and_does_not_block_posting`: 1,500 × 1 KiB room messages on `/v1/rooms/persistent/{key}/events`.

| client | delivered | seq gaps | error frames |
|---|---|---|---|
| fast | 1,500 | 0 | 0 |
| stalled until burst end | 1,500 | 0 | 0 |

- **Producer.** 1,500 append + wake operations took 135–143 ms. The slowest took 235–320 µs.

## Observed failure modes

1. **Agent and legacy SSE: a slow client lags and silently loses events, and stays connected.** The stalled clients got the first 94–102 frames, which fit in hyper's write buffer and the socket buffers. Then came one lag frame, then the 1,024 envelopes still in the broadcast ring (1,023 chunks plus the end marker), then the live tail. In every run, delivered minus delivered-before-lag was exactly 1,023, one broadcast capacity. Nothing disconnects a slow client; only daemon shutdown ends the stream.
   - On `/v1/agent/events` the lag frame is the typed `live_lag` / `reset_required: true` gap, and it carries no drop count. The daemon counts only the occurrence.
   - On `/v1/events` the frame reports the exact count. `delivered + reported dropped == emitted` held exactly, and matched `sse_events_dropped_total`.
2. **No backpressure, and no cross-client interference.** Broadcast `send` never waits. The paced fast client received all 3,072 events with zero lag while two stalled neighbors were attached, and no emit took longer than 1.3 ms. A slow client's per-connection memory is bounded by hyper's write buffer (about 380–410 KiB observed) plus its position in the shared broadcast ring. It does not grow without bound.
3. **Connection churn costs O(retained ring) and stalls the producer.** Every agent-rail connect clones the whole replay ring under the lock that `emit` needs (§4). This is the one measured path where client behavior reaches the producer. It is bounded at 32 MiB per connect and scales with how often clients connect, not with how slowly they read.
4. **Broadcast-ring pinning is count-bounded, not byte-bounded.** A single stalled receiver, such as a lagging Observatory pump or a stalled SSE socket, keeps up to 1,024 extra envelope copies alive beyond the 32 MiB replay budget. That was 40.34 MiB total at 1 MiB outputs (§3).
5. **Rooms: lossless.** The stalled room client received all 1,500 messages in order, with no gaps and no error frame. The room tail blocks on its own 64-slot queue, its wake receiver lags past 256, and on resume it re-pages SQLite. SQLite is the authority and the broadcast carries hints only, so lag costs re-reads, not data. Posting was not slowed.
6. **Persisted transcripts are bounded, but their write volume is not small.** See §6.

### 6. Persisted session transcript

`persisted_session_bytes_by_turns_and_tool_output_size` (fast, 25 turns) and `persisted_session_write_volume_exact` (ignored, 60 turns, serialized at every save):

| context window | tool output K | turns | after turn 1 | after turn 10 | after turn 20 | max over run | messages | tool results elided | saves | bytes written (all saves) |
|---|---|---|---|---|---|---|---|---|---|---|
| 200,000 | 1.0 KiB | 60 | 11.0 KiB | 108.5 KiB | 216.7 KiB | 216.7 KiB | 200 | 0 | 360 | 63.54 MiB |
| 200,000 | 32.0 KiB | 60 | 135.0 KiB | 327.6 KiB | 399.8 KiB | 527.4 KiB | 200 | 232 | 360 | 134.35 MiB |
| 200,000 | 2.00 MiB | 60 | 135.3 KiB | 328.2 KiB | 400.4 KiB | 528.3 KiB | 200 | 232 | 360 | 134.56 MiB |
| 1,000,000 | 1.0 KiB | 60 | 11.0 KiB | 108.5 KiB | 216.7 KiB | 216.7 KiB | 200 | 0 | 360 | 63.54 MiB |
| 1,000,000 | 32.0 KiB | 60 | 135.0 KiB | 1.32 MiB | 1.48 MiB | 1.98 MiB | 200 | 181 | 360 | 475.10 MiB |
| 1,000,000 | 2.00 MiB | 60 | 135.3 KiB | 1.32 MiB | 1.48 MiB | 1.98 MiB | 200 | 181 | 360 | 476.12 MiB |

- **Size is bounded.** The session is not held in daemon memory between turns. Its file size is also the per-turn resident unit and the size of every save. Three bounds shape it:
  - the 200-message cap, which binds by turn 20 at 10 messages per turn;
  - the runtime's 32 KiB tool-result truncation, which is why 2 MiB outputs persist like 32 KiB ones;
  - `compact_history` elision at half the context window.

  Together they make the file saw-tooth below about **0.53 MiB for a 200k window and about 2 MiB for a 1M window**, whatever the turn count or live output size.
- **Write volume is the cost that grows.** At 6 full-file saves per turn, a 60-turn session wrote 64–476 MiB of pretty JSON, 1.1–7.9 MiB per turn, each save with an fsync.
- **Live event versus persisted copy.** The live `ToolCallFinished` for K = 2 MiB was 64 times larger than its persisted counterpart.

## What these numbers justify, and what they do not

The measurements **do** support these statements:

- The agent replay ring is bounded as documented. It holds about 10 turns or fewer of history, shared across all sessions, and the count cap binds below about 830 KiB per tool output.
- A slow SSE reader on the agent or legacy rail loses events, is told so with one lag frame, and is not disconnected. Its memory cost is bounded, and it does not slow the producer or other readers.
- Room SSE survives a stalled reader without loss.
- Persisted transcripts are bounded in size, and live tool-output size does not reach them past 32 KiB.
- Per-connect replay cloning and count-only broadcast pinning are two measured costs that sit outside the 32 MiB replay budget.

The measurements do **not** justify these conclusions:

- **No per-turn MPSC policy.** The runtime → daemon unbounded channel was not exercised. The bridge drains it with non-blocking `emit` calls, so it can grow only while the bridge task is starved or waiting on the history lock (failure mode 3). No measurement here shows that happening, and none shows it cannot.
- **No larger payload limits, and no smaller ones.** The numbers describe the current limits. They do not show that 32 MiB, 2,048, or 1,024 are too small or too large for real traffic, because real traffic shapes were not sampled.
- **No RSS or allocator behavior.** Serialized bytes are a proxy. In-memory `AgentTurnEvent` size, allocator slack, and fragmentation were not measured.
- **No production frequency.** How often clients reconnect, how often a reader stalls, and how large real tool outputs are were not measured. This document shows costs and thresholds, not how often they occur.
- **No verdict on whether a hazard needs fixing.** Failure modes 3 and 4 are reported as measured. Whether they matter depends on production frequencies this document does not have. Any change is a separate design decision.

## Not measured

- **The per-turn `unbounded_channel::<AgentEvent>`** in `agent_turn`, under a starved bridge.
- **Timing figures in release builds.** All timings above come from a debug build.
- **The Observatory pump's real lag rate** under SQLite write load.
- **Legacy-rail retention with large tool arguments.** A `write` call carrying a whole file was not swept.
- **The `/v1/agent/events` replay path under concurrent reconnect storms.** Only one connect at a time was timed.
- **HTTP/2 or proxied clients.** Every client here was a direct HTTP/1.1 loopback connection.
