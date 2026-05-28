# Ocean Runtime — Concurrency Audit

Read-only audit of `ocean-daemon`, `ocean-agent`, `ocean-agent-sdk`, `ocean-core`,
and `ocean-providers` in `workspace/ocean-os`. Goal: determine whether the runtime
can execute concurrent agent turns safely, since that underpins the multi-agent
roadmap (Tides Mesh / workbench). Triggered by a bug found while building
ocean-voice (PR #22, issue #23).

## Bottom line

**The runtime already executes turns in true parallel — but that concurrency is
currently unsafe and unobservable.** It is not that concurrency is missing; it is
that three gaps make it incorrect to rely on:

1. **Session state race — data loss (P0).** Same-session concurrent turns lose
   history via an unguarded load → modify → save.
2. **Event attribution is impossible (P1).** A single global event broadcast with
   regenerated ids means no client can tell which turn an event belongs to (#23).
3. **The product agent stream is near-silent mid-turn (P2).** `/v1/agent/turns`
   wires no event sink, so incremental deltas and tool events never reach the bus.

For a single operator running one turn at a time, the system is fine today. For
multiple agents/clients — the actual roadmap — all three are blocking.

## What works (verified safe)

- **Parallel execution, no global lock.** `AppState.runtime` is `Arc<AgentRuntime>`
  with no `Mutex`/`RwLock`; handlers don't hold any lock across
  `state.runtime.prompt(...).await`. `ocean-daemon/src/main.rs:44-50, 951`.
- **Runtime is immutable + thread-safe.** `AgentRuntime` holds only config/model/
  credential/provider values; `prompt(&self, …)`; each turn spawns its own agent
  loop via `tokio::spawn(run_agent_with_history(...))`.
  `ocean-agent/src/lib.rs:34-41, 72, 221-224`.
- **Provider layer is safe.** Per-turn `AgentConfig`, credentials cloned per call,
  no shared HTTP client / token / counter in ocean-rs code.
  `ocean-agent/src/lib.rs:208-217`, `ocean-providers/src/lib.rs:114-118`.
- **Daemon registries are safe.** `requests`/`permissions` are
  `Arc<RwLock<HashMap<…>>>` keyed by UUID, with short-lived locks never held across
  `.await`. `ocean-daemon/src/main.rs:52-53`.
- **No global mutable statics** in core/agent/providers.

## Findings

### P0 — Session persistence race (data loss)
`session::load()` → mutate in memory → `session::save()` with **no locking, no
atomic read-modify-write, no version check** (`ocean-agent/src/lib.rs:189-195,
294-295, 493-508`). Two turns on the **same `session_id`** both read the file, both
run, both write — the later write clobbers the earlier turn's messages. The SDK even
models `AgentSession.active_turn` (`ocean-agent-sdk/src/lib.rs:140`), implying
single-flight-per-session was intended, but nothing enforces it.

### P1 — Event attribution impossible (expands #23)
The event bus is a single `tokio::sync::broadcast` channel (capacity 1024) shared by
all turns; subscribers get everything (`ocean-daemon/src/main.rs:76-94, 108`). On the
agent stream, `ocean_to_agent_event` assigns a **fresh** `turn_id`
(`AgentTurnId(Uuid::new_v4())`) to every event (`main.rs:1181-1238`), and
`agent_to_ocean_event` drops the original on the way in (`main.rs:1128-`). So a
client cannot match stream events to its turn, and concurrent turns interleave
unrecoverably. This is exactly why ocean-voice must run single-flight.

### P2 — Product agent stream is near-silent mid-turn
`agent_turn` builds `PromptControl::yolo(true)`, whose `event_sink` is `None`, and
never calls `.with_event_sink()` (`ocean-agent/src/lib.rs:323-348`;
`ocean-daemon/src/main.rs:950`). The runtime *does* emit incremental `TextDelta` and
tool events to its sink (`ocean-agent/src/lib.rs:230-282`), but on the agent path
that sink is null. So `/v1/agent/turns` emits only `session_created`, `turn_started`,
a **single** end-of-turn `assistant_text_delta` (the full `res.stdout`), and
`turn_finished`. No streaming, **no `tool_call_started`/`tool_call_finished`**.
(The legacy `/v1/prompt` + `/v1/requests` path *does* wire a sink, so it streams —
and as a side effect double-emits the final text: streamed deltas **plus** a bulk
`res.stdout` reemit. Minor, but worth fixing alongside.)

## Impact on ocean-voice (the client)
- The single-flight guard (PR #22) is **correct and must stay** until P1/P2 land.
- **Spoken per-tool status phrases don't fire** on the agent path (P2): no
  `tool_call_started` is delivered, so only the periodic "still working" waiting
  phrases play. Graceful, but the per-tool flavor is dead until P2. (Switching
  ocean-voice to the legacy `/v1/requests` + `/v1/events` path would restore tool
  events today, but at the cost of the older event shape — recommend waiting for P2.)

## Fix plan (prioritized, daemon-side)

- **P0 — Serialize per session.** Acquire a per-`session_id` async lock around
  load → run → save (e.g. a `DashMap<SessionId, Arc<tokio::sync::Mutex<()>>>`, or
  fold the lock into a session manager). Same-session turns serialize; different
  sessions stay parallel. Longer term: a real session store (SQLite) with atomic
  transactions. Add the lock at `ocean-agent/src/lib.rs:189-295`.
- **P1 — Stable id + scoped stream (supersedes #23).** Preserve the turn id
  end-to-end (thread it through `EventEnvelope`, which already carries `session_id`/
  `request_id`) and/or add server-side filtering: `GET /v1/agent/events?session_id=…`
  (and/or `?turn_id=…`).
- **P2 — Wire the agent-path event sink.** Pass an event sink into the
  `runtime.prompt` call from `agent_turn` that forwards runtime events (deltas, tool
  start/finish) onto the bus as `AgentTurnEvent`s, and drop the redundant full-stdout
  reemit to avoid doubling.
- **P3 — Concurrency tests.** Spawn N concurrent turns on the same and different
  sessions; assert no lost messages, correct per-turn attribution, and that tool
  events are observed.

## Tracking
- **P0 — #24**: same-session save race (silent history loss). Highest severity.
- **P1 — #23**: event attribution (stable `turn_id` / scoped stream).
- **P2 — #25**: `/v1/agent/turns` event sink (streaming + tool events).
