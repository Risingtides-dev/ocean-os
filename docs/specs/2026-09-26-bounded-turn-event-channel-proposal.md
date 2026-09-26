# Bounded Per-Turn Event Channel — Proposal

**Date:** 2026-09-26
**Status:** PROPOSED. Needs an operator yes/no on slices 3–6. Slices 1–2 do not depend on the open questions and are **landed, pending review** on `perf/turn-channel-slices-1-2` (see §8). No bound, backpressure, coalescing, channel type, or metric exists yet.
**Baseline:** `origin/main` at `bd2f9abb`.
**ROADMAP item:** "Reliability and scale" → *Design a bounded policy for the runtime-to-daemon per-turn event channel*.
**Evidence:** [`2026-09-25-retained-size-and-slow-client-measurements.md`](2026-09-25-retained-size-and-slow-client-measurements.md) (cited below as **M**), plus the measurements in §2.

## Decision requested

Replace both unbounded per-turn `mpsc` hops with one shared type, `TurnEventChannel`. It has a per-hop budget of **8 MiB and 1,024 queued events**. When a hop is over budget, the producer waits (backpressure). It never drops. Adjacent text and thinking deltas are merged in the queue, up to **64 KiB** per merged delta. Nothing changes on the wire. Loss stays where it is today, at the broadcast fan-out, where the existing `live_lag` / `reset_required` frame already reports it.

## 1. Current design

"The per-turn channel" is two unbounded hops in a row. Both are created fresh for every turn and dropped at turn end.

| Hop | Channel | Created | Producers | Consumer |
|---|---|---|---|---|
| **H1** runtime → session layer | `tokio::mpsc::unbounded_channel::<AgentEvent>` | `crates/ocean-agent/src/lib.rs:2440` | `ocean-runtime` agent loop through `emit` (`agent_loop.rs:1871`): `UserMessage` 487, `AgentStart` 495, `TurnStart` 552, `ProviderRetrying` 656/838, `TextDelta` 762/865/1242, `ThinkingDelta` 772, `AssistantMessage` 876, `TurnEnd` 919/1217/1227, `PermissionDenied` 998, `ToolExecutionStart` 1095, `AgentEnd` 1262 (carries the **whole message history**), `ToolExecutionEnd` 1439 (carries the **full live tool output**), `Render` 1458, `Unmount` 1470, `BrowserActivity` 1479, `SurfacePatch` 1492, `SlackCanvas` 1505, `TurnCheckpoint` 1864 | The `run_prompt` loop at `lib.rs:2473`. It (a) clones **every** event into H2 (`lib.rs:2474`), (b) builds `stdout`/`stderr` and the failover flag `streamed_output`, and (c) on `TurnCheckpoint` runs `cap_session_history(clone)` + `session::save` **synchronously**, with an fsync, inside the receive loop (`lib.rs:2518–2530`). |
| **H2** session layer → daemon | `tokio::mpsc::unbounded_channel::<AgentEvent>` | `crates/ocean-daemon/src/main.rs:6984`, passed in by `.with_event_sink(event_tx)` (`main.rs:7375`) | the H1 forwarder (`ocean-agent/src/lib.rs:2474`), plus direct `sink.send` for `ModelRerouted` (`lib.rs:948`, `1174`) and the fake provider's `TextDelta` (`lib.rs:2057`) | The bridge task (`main.rs:6990–7324`). It maps each `AgentEvent` to an `AgentTurnEvent` and calls `AgentEventBus::emit` synchronously. It also publishes `ToolExecutionStart`/`End` to the extension lifecycle dispatcher. `TurnCheckpoint`, `AgentEnd`, `AssistantMessage`, `UserMessage`, `AgentStart/End`, and `TurnStart/End` are received and **discarded** (`main.rs:7309–7316`). |

Downstream of the bridge, and not part of this channel:

- `AgentEventBus::emit` (`bus.rs:327`) serializes the event to measure it, deep-clones it into the replay ring under a `std::sync::Mutex`, and then `broadcast::send`s it. The broadcast capacity is 1,024 and it never blocks (M §2).
- The consumers of that broadcast are:
  - agent SSE (`main.rs:7891`; a lag produces the `live_lag` frame at `main.rs:7998–8030`);
  - the Observatory durability pump (`observatory_adapter.rs:403`; a lag is logged as `facts were lost`; deltas are never facts, `observatory_adapter.rs:229–242`);
  - the replay ring and terminal floor (`bus.rs:188`).
- The extension lifecycle dispatcher (`extension_lifecycle.rs:1053`) has its own bounded per-service queue: 256 messages / 1 MiB (`extension_service.rs:72–73`). It uses `try_send`, and on overflow it sends a `Lag` control frame with `replay_available` (`extension_service.rs:2040–2064`). That is precedent for an explicit loss marker when the consumer is untrusted.

Four things **never enter the channel**:

- `SessionCreated`, `TurnStarted`, and `TurnFinished` are emitted by the daemon itself (`main.rs:6963`, `6974`, `7487`/`7500`). `TurnFinished` is emitted only **after** `bridge.await` (`main.rs:7449`). That is why the terminal event always follows every bridged event.
- Permission prompts go from `PromptControl` onto the legacy `EventBus` (`main.rs:2788`), not through H1/H2.

## 2. Fast producer, slow consumer: what happens today

Neither hop has a bound. A backlog is held in full and drained later. Nothing is dropped, and nothing slows the producer. The backlog equals the producer's output while the consumer is stalled. So there are two questions: how long can each consumer stall, and how many bytes can the producer emit in that time?

**Consumer stalls (measured).**

| Stall source | Hop | Measured | Method |
|---|---|---|---|
| Checkpoint `cap_session_history(clone)` + `session::save` + fsync at steady state (200 messages, 32 KiB tool results) | H1 | **file 539 KB: p50 9.1 ms, p90 10.0 ms, max 22.3 ms. File 1.03 MB: p50 11.0 ms, p90 12.8 ms, max 13.1 ms** (30 saves each) | `#[ignore]` test `checkpoint_save_stall_at_steady_state` in `crates/ocean-agent/src/session_retained_size_measurements.rs` (same `TurnShape`, `compact_history`, `cap_session_history`, `session::save`), release build, APFS, Mac16,10. Originally a scratch test; landed permanently by slice 1 (§8), which re-ran it. |
| `AgentEventBus::emit`, slowest single call under load | H2 | 118 µs – 1.3 ms, debug build | M §5 |
| History lock held by a full-ring `?replay=1&session_id=` connect (the only connect that still clones the ring after #496) | H2 | 0.9–1.2 ms for a 31 MiB ring. Re-run today: a plain connect holds the lock **320 ns** (full ring) / 233 ns (empty). | M §4, debug build. The full-clone figure was taken when a plain connect still cloned the ring, which is the same operation `?replay=1` still performs. Re-ran `connect_materializes_the_whole_replay_ring` at `bd2f9abb` for the plain-connect figure. |
| A tokio worker blocked by a sync call elsewhere, or a panicked bridge | H2 | unbounded | not measurable; a failure mode, not a rate |

**Producer volume (measured and derived).**

- **Deltas.** The synthetic turn in M has 200 deltas of 24 B. At provider streaming rates a 10–22 ms stall queues a handful of deltas, about 1 KB. H1 growth from checkpoint saves is negligible in normal operation.
- **Tool output.** This is the dominant volume, but only on **spill-off** turns. `SpillingTool` wraps every tool whenever `artifact_spill` is on (`capability.rs:472–497`). It replaces any text block over 24,000 B (`SPILL_THRESHOLD_BYTES`) with a 16,000 B head plus a notice. The `tui`, `cli`, `acp`, and `web` profiles have spill on (`harness_profile.rs:78–93`), so their live `ToolExecutionEnd` text is ≤ ~16 KB per block. The **voice** profile has spill off, so bash and web_fetch send up to 2 MiB raw (`bash.rs:17`, M §1). **Image blocks are never capped on the live path** in any profile. The runtime caps images at 256 KiB (`agent_loop.rs:1628`), but only in the transcript.
- **History-sized payloads that nobody downstream reads.** H1 forwards `AgentEnd { messages }` (the whole history, up to ~2 MiB serialized in a 1M window, M §6) and every `TurnCheckpoint` delta into H2. The bridge then discards them. This is pure waste on every turn, stalled or not.
- **Copies per large tool result.** One live `ToolExecutionEnd` exists as up to six copies at the same time:
  1. the runtime `Outcome`;
  2. the H1 item;
  3. the H2 clone (`lib.rs:2474`);
  4. `render_tool_output`;
  5. the lifecycle `rendered_output` `Vec`, which is used only for its length (`extension_lifecycle.rs:464`);
  6. the ring clone plus the broadcast original.

**Worst case today.** The per-turn bound is `Σ(event bytes emitted while the consumer is stalled)`, and nothing caps it. Stalls in the table are milliseconds, so in practice the backlog is small. It is still only an arithmetic bound: 24 concurrent turns (`DEFAULT_MAX_CONCURRENT_TURNS`, `main.rs:536`) × a voice turn with N parallel 2 MiB tools × 2 hops. Parallel tool segments have no count limit (`agent_loop.rs:1100–1125`). If the bridge stops (failure mode 2 below), the backlog grows for the rest of the turn and is bounded only by the turn's total output.

## 3. Failure modes that matter

1. **Memory growth per turn is unbounded in principle and small in practice.** Measured stalls are 1–22 ms. No stall long enough to matter has been observed. The risk is a stall that is not a rate: a blocked worker, a lock held across something slow, or a future bug. This proposal buys a hard ceiling in exchange for a code path that normally sits idle.
2. **A dead bridge is silent.** If the bridge task panics, `event_rx` is dropped and every `send` fails. The runtime ignores the error (`let _ =`), so the turn finishes with no live output. That behavior is already correct. The proposal keeps it: a closed channel never blocks.
3. **Tool output is the dominant volume only on voice turns and with images.** On spill-on profiles it is ≤ ~16 KB per block, so the 8 MiB budget below holds more than 500 of them.
4. **Unused payloads.** `AgentEnd` and `TurnCheckpoint` are the largest events in H2, and they are thrown away. Fixing this is independent of bounding and is the cheapest win.

**What must never be dropped or reordered.** Everything except the two delta kinds:

- `TurnCheckpoint`: durable transcript. Losing one loses persisted rounds.
- `ToolExecutionStart`/`End`: paired. The lifecycle correlation map and the Observatory facts depend on both halves.
- `PermissionDenied`: becomes a paired Started→Finished, OCEAN-317.
- `SurfacePatch`: non-idempotent ledger operations.
- `SlackCanvas`: an operation to fulfill.
- `Render` and `Unmount`: the component registry, including interactive `confirm`/`form`.
- `ModelRerouted` and `ProviderRetrying`: honesty signals.
- `BrowserActivity`, `AssistantMessage`, `AgentStart/End`, and `TurnStart/End`.

The terminal (`TurnFinished`) and permission prompts are already outside the channel.

**What can be coalesced.** `TextDelta` and `ThinkingDelta` only, and only when adjacent and of the same kind:

- Concatenating them loses no bytes.
- `stdout`, the `streamed_output` failover flag, and every client concatenate them anyway.
- The Observatory drops them.

`ToolCallChunk` is on the wire but has no producer today. If streaming tool output ships later, it joins this class and merges per `call_id`.

## 4. Recommended policy

**One type, `TurnEventChannel`, in `ocean-runtime`, used at both H1 and H2.**

The sender API is async:

- `send(ev).await`
- `send_delta(ev).await`
- `closed()`

The receiver API is `recv().await`. The state is a `Mutex<VecDeque<Item>>` with a byte counter and two `Notify`s. Each hop has these rules:

| Parameter | Value | Why this number |
|---|---|---|
| Byte budget | **8 MiB** queued, by in-memory payload estimate: string and byte lengths plus a 64 B per-event overhead, with `Value` fields walked recursively. | 4 × the largest bounded live event (2 MiB, bash/web_fetch `MAX_CAPTURE_BYTES`). A voice segment of four parallel 2 MiB tools never blocks. It is a quarter of the 32 MiB replay budget (M §1), so a single drained backlog cannot flush more than a quarter of the ring. Spill-on turns (≤ ~16 KB/block) never reach it. |
| Count budget | **1,024** queued items | Equal to the agent broadcast capacity (M §2, exact lag at capacity + 1). A drained backlog cannot, on its own, push a caught-up SSE reader into `Lagged`. The synthetic 210-event turn fits five times over, and the 200 deltas collapse to a few items under backlog. |
| Delta merge cap | **64 KiB** per merged delta | The extension frame ceiling (`MAX_FRAME_BYTES` = 65,536). No single SSE `data:` line grows past what a client already accepts from lifecycle frames. When the merged delta at the tail reaches 64 KiB, a new item starts. |
| Merge rule | Only onto the **tail** item, only same kind (`TextDelta`↔`TextDelta`, `ThinkingDelta`↔`ThinkingDelta`), and only same `session_id`. | Total order is preserved exactly. A fact is a hard merge barrier. |
| Overflow | The producer **awaits** space. It never drops and never evicts. | The only consumers are in-process and trusted. Waiting propagates cleanly through H2 → H1 → runtime → provider stream. The extension queue drops because its consumer is an untrusted process. That reason does not apply here. |
| Oversize event | Always admitted when the queue is **empty**. | Guarantees progress for an event over 8 MiB, such as a large image. Worst case per hop = 8 MiB + one event. |
| Cancellation | `send` races the turn's cancel token (biased) and returns `Cancelled`. | A full queue can never hold a cancelled turn open. |
| Closed receiver | `send` returns `Closed` immediately. The producer keeps today's ignore-and-continue behavior. | Keeps failure mode 2 as it is today. |

**Same-change tightening (no new behavior):**

- H1 stops forwarding the variants the bridge discards: `TurnCheckpoint`, `AgentEnd`, `AssistantMessage`, `UserMessage`, `AgentStart/End`, `TurnStart/End`. The exhaustive bridge `match` keeps them named.
- H1 **moves** each forwarded event into H2 instead of cloning it. It takes the few fields it needs (tool name, `is_error`, the delta text) before the move.

**Deadlock argument.** The wait graph is acyclic:

- The runtime waits only on H1.
- The H1 consumer waits only on H2 and its own fsync.
- The H2 consumer waits only on the history mutex and synchronous lifecycle publish. It never waits on any sender.

Permission decisions do not travel through either hop.

**Resulting worst case.** Per turn ≤ 2 × (8 MiB + one event). Per daemon ≤ 24 × that, about 384 MiB plus the largest events, against unbounded today. In normal operation queue depth stays near zero (§2).

**How each consumer observes loss.**

| Consumer | Loss in this channel | Observes |
|---|---|---|
| `ocean-agent` (`stdout`, checkpoints, failover flag) | none | nothing. Merged deltas concatenate to the same `stdout`. |
| Daemon bridge → `AgentEventBus` | none | fewer, larger delta events during backpressure only |
| SSE clients (TUI, surface, ACP) | none added | the existing `live_lag` / `reset_required` frame, unchanged, for broadcast-ring loss |
| Observatory pump | none added | the existing `Lagged` warn. Deltas were never facts. |
| Extension lifecycle services | none added | their existing `Lag` control frame |
| Operator | none | new counters: `turn_channel_backpressure_waits_total`, `turn_channel_backpressure_wait_ms`, `turn_channel_deltas_merged_total`, and `turn_channel_peak_bytes` (high-water mark) on `/metrics` |

## 5. Compatibility

- **Wire.** No change:
  - `docs/contracts/session-wire.json` keeps the same 17 `agent_event_types`;
  - `component-wire.json` is untouched;
  - no new SSE event name or error code.
- **Observable difference.** Only while a hop is backpressured, several adjacent `assistant_text_delta` or `thinking_delta` events arrive as one. Each is ≤ 64 KiB and its text is identical when concatenated. The event count per turn, and so the replay-ring depth, can only go **down**.
- **Replay.** Ring semantics, the terminal floor, `Last-Event-ID`, and `?replay=1` are unchanged. `TurnFinished` still follows `bridge.await`, so it stays after every bridged event.
- **TUI** (`ocean-tui/src/shell/client.rs:284–307`): concatenates deltas, and treats any `error` frame as a reset. Unaffected.
- **ACP** (`ocean-acp/src/main.rs:1553`): maps each delta to one ACP chunk. It gets fewer, larger chunks under backpressure. The ACP spec allows chunks of any size.
- **CLI:** reads `stdout` from `ocean-agent`. Byte-identical.
- **Surface** (`ocean-surface`, a separate repo): must be confirmed to concatenate deltas and not key UI on delta count. See Q3.
- **Runtime API.** `AgentConfig`/`run_agent_with_history` take the new sender instead of `mpsc::UnboundedSender<AgentEvent>`. `PromptControl::with_event_sink` changes type. Every caller is in-workspace.

## 6. Implementation plan

Each slice is one PR with its own tests. Every slice runs `cargo xtask ci`.

1. **Measure first: the channel harness.** *Save-stall half LANDED, pending review (§8). The stalled-consumer peak-bytes harness is not built yet.* Land the H1 save-stall measurement from §2 as an `#[ignore]` test. Add a runtime-level harness that runs the fake provider (`FAKE_TOOL_MODEL`) against a consumer stalled for a controlled time and records the peak queued bytes and items per hop.
   - *Accept:* the numbers land in the measurements doc's "Not measured" section, and that row closes.
2. **Stop forwarding discarded variants, and move instead of clone** (`ocean-agent/src/lib.rs:2474`). *LANDED, pending review (§8).*
   - *Tests:* a daemon test in which a turn's `AgentEnd`/`TurnCheckpoint` never reaches the bridge, and SSE output is byte-identical for a fake-tool turn.
   - *Accept:* peak H2 bytes on a 1M-window turn drop by the size of the history, measured by the slice 1 harness.
3. **Add `TurnEventChannel` as a standalone type in `ocean-runtime`.** No callers yet. Tests:
   - *G1, control never dropped:* property test with a random interleaving of facts and deltas, a stalled consumer, then a drain. Every fact arrives, in order, and exactly once. The concatenated text of each delta kind equals the input.
   - *G2, memory bounded:* stalled consumer. `queued_bytes() ≤ 8 MiB + max_event` and `len() ≤ 1,024` hold after every send, and the next `send` is `Pending`. Release one item and the next send completes.
   - *G3:* an oversize event into an empty queue is admitted (tokio timeout 1 s).
   - *G4:* a cancelled send on a full queue returns within 100 ms. A dropped receiver makes `send` return `Closed` without blocking.
   - *G5:* no merge across a fact, across kinds, or past 64 KiB.
   - *Mutation checks, each recorded in the PR:*
     - drop-on-full instead of await → G1 fails;
     - merge across a barrier → G1 order fails;
     - skip byte accounting on merge → G2 fails;
     - remove the await → G2 fails;
     - `>` instead of `≥` on the empty-queue admit → G3 times out;
     - drop the cancel branch → G4 times out.
4. **Wire H1** (runtime `emit` becomes async; `emit_outcome_events` and `emit_turn_checkpoint` become `async fn`).
   - *Tests:* existing runtime and agent suites stay green. The fake-provider test (`lib.rs:7349`) sees identical `stdout`. A stalled-H1 test proves a `TurnCheckpoint` persisted after backpressure is identical to one persisted without it.
   - *Mutation:* revert one emit site to fire-and-forget `try_send` → the G1-style end-to-end test fails.
5. **Wire H2** (`main.rs:6984`, `with_event_sink`, and the direct `ModelRerouted`/fake-delta senders).
   - *Tests:* hold the `AgentReplayHistory` lock from a test while a fake-tool turn streams. H2 peak is ≤ budget. After release, a real-socket SSE client (the M §5 harness) receives every `tool_call_started`/`tool_call_finished`, and its concatenated text equals `stdout`. `TurnFinished` is last.
   - *Mutation:* make H2 unbounded again → the bound assertion fails.
6. **Metrics and docs.** Add the four counters to `/metrics` and the metrics contract test. Update the M buffer-inventory row ("Runtime → daemon per-turn queue"), `crates/ocean-runtime/AGENTS.md`, `crates/ocean-daemon/AGENTS.md`, `docs/ARCHITECTURE.md`, and close the ROADMAP line.

## 7. Open questions for the operator

1. **Backpressure or drop, for deltas.** This proposal backpressures everything and drops nothing, and coalescing makes the delta budget practically unreachable. The alternative is to drop deltas past the budget and add a wire-level `delta_gap` marker. That is a wire change, and clients would render holes until they rebaseline. **Recommended: backpressure.** Yes/no?
2. **Voice raw output.** Voice turns are the only live path that carries up to 2 MiB per tool. Should voice get spill on for the *live event only* (the transcript is unchanged)? That is part of the artifact-backed large-results ROADMAP item. It would drop the worst-case event from 2 MiB to ~16 KB, and the 8 MiB budget could then shrink to 1 MiB. Out of scope here unless you say otherwise.
3. **Surface delta handling.** OK to require a one-line confirmation in `ocean-surface` that deltas are concatenated and not counted, before slice 5 lands?
4. **Live image cap.** Images have no live size cap in any profile. Should the live event take the transcript's 256 KiB image cap (a behavior change, separate slice), or should oversize images keep relying on the empty-queue admit rule?

## 8. Slices 1–2 implementation record (2026-09-26)

Branch `perf/turn-channel-slices-1-2`, from `origin/main` at `c8fad6ce`. Both slices are **landed, pending review**. Neither adds a bound, backpressure, coalescing, a channel type, or a metric; those are slices 3–6 and still wait for the operator's answers in §7.

### Slice 1: the H1 save-stall measurement

**Method.** `checkpoint_save_stall_at_steady_state` is `#[ignore]`d, following the measurements doc's convention for heavier or timing-only sweeps. It never runs in the normal suite and prints its table only when asked:

```text
cargo test --release -p ocean-agent checkpoint_save_stall -- --ignored --nocapture --test-threads=1
```

1. Build a steady-state transcript with the existing `simulate` (4 tool rounds per turn, 32 KiB tool results through the runtime's cap, `compact_history` at every turn start, the 200-message cap), then apply `compact_history` once more, as `run_prompt` does at turn start.
2. The file saw-tooths with the turn count (measurements doc §6), so a size is a (context window, turns) pair. The two points reproduce the sizes in §2 exactly: **539,534 B** (300k window, 21 turns) and **1,029,539 B** (600k window, 23 turns). Both are at the cap (199 messages after the orphan-result trim). The test asserts each size to ±1 %.
3. Each of 30 samples starts from that transcript plus one new round (assistant tool call + result). It times exactly what the `TurnCheckpoint` arm does inline: `extend`, `cap_session_history(clone)`, `replace_messages`, and `session::save` (pretty JSON, write, `sync_all`, durable rename). The file is written once beforehand, so no sample is a create.

**Result** (release, APFS, Mac16,10, three consecutive runs):

| File | p50 | p90 | max |
|---|---|---|---|
| 539,534 B | 9.0–9.8 ms | 9.9–11.9 ms | 11.0–13.9 ms |
| 1,029,539 B | 10.0–12.8 ms | 10.8–14.4 ms | 11.7–18.9 ms |

These agree with the scratch figures in §2. The H1 consumer stalls for about 10 ms per checkpoint at both sizes, so the save is dominated by the fsync, not by the bytes.

**Not done in this slice.** §6 slice 1 also asks for a runtime-level harness that runs `FAKE_TOOL_MODEL` against a consumer stalled for a controlled time and records peak queued bytes and items per hop. It is not built, so the "Runtime → daemon per-turn queue" row in the measurements doc stays "Not measured". Slice 2's acceptance line ("peak H2 bytes drop by the size of the history") depends on that harness and is therefore not yet measured either. Slice 2's effect is proven structurally instead: the history-sized events no longer enter H2 at all.

### Slice 2: stop forwarding what the bridge discards

**Change.** `run_prompt`'s H1 receive loop (`crates/ocean-agent/src/lib.rs`) now:

- updates `stdout`, `stderr`, and `streamed_output` by reference;
- persists `TurnCheckpoint` locally by moving its delta, as before;
- drops `AgentStart`, `AgentEnd`, `TurnStart`, `TurnEnd`, `AssistantMessage`, and `UserMessage` without forwarding them;
- **moves** every other event into the event sink (H2). The old `sink.send(ev.clone())` is gone.

A future runtime variant is forwarded by default, so the bridge's exhaustive `match` still forces a relay-or-document decision. The bridge (`crates/ocean-daemon/src/main.rs`) is unchanged except for a comment: its named no-relay arm is now unreachable and kept for exhaustiveness. The direct `ModelRerouted` and fake-provider `TextDelta` sends are unchanged.

**Consumer audit.** Every holder of H2, and every other reader of runtime `AgentEvent`s, at `c8fad6ce`:

| Consumer | Reads H2? | What it reads | Relies on a dropped variant? |
|---|---|---|---|
| Daemon turn bridge (`ocean-daemon/src/main.rs`, `with_event_sink(event_tx)`) | yes, the only production sink | `TextDelta`, `ThinkingDelta`, `ModelRerouted`, `ProviderRetrying`, `ToolExecutionStart/End` (also to the lifecycle dispatcher), `PermissionDenied`, `Render`, `Unmount`, `BrowserActivity`, `SurfacePatch`, `SlackCanvas`, plus a debug-only `session_id` assert on every event | no. All seven are in its `=> {}` arm. |
| `ocean-agent` unit test `fake_provider_streams_assistant_text_delta_on_event_sink` | yes (test) | `TextDelta` from the `fake-ok` path, which does not use H1 | no |
| `ocean-cli` | no | depends on `ocean-agent` only for `agentdir` and `config_dir_from_env`. Its output comes from the daemon or `stdout`. | no |
| `ocean-tui`, `ocean-acp`, `ocean-surface`, MCP, Observatory, extension services | no | wire `AgentTurnEvent`s over SSE or the bus, produced by the bridge. None depends on `ocean-agent`, and none sets an event sink. | no. Their input is pinned by the golden test below. |
| `ocean-runtime` tests, daemon `run_agent_with_history` tests | no | their own runtime channels (H1-shaped), not `ocean-agent`'s sink | no |
| Transcript and checkpoints | no | `ocean-agent` persists `TurnCheckpoint` and the final `run.messages` itself, before and after this change | no |

**Tests.**

- `ocean-daemon` `scripted_turn_bridge_visible_event_stream_matches_golden`: a `fake-tool` turn through `agent_turn`, the real runtime, H1, and the bridge. The session's agent-bus events must be exactly `session_created, turn_started, tool_call_started, tool_call_finished, assistant_text_delta, turn_finished`, with the tool name `write`, the delta text `done`, and status `completed`.
- `ocean-agent` `event_sink_carries_only_bridge_relayed_events_for_a_scripted_turn`: the same scripted turn at the `ocean-agent` layer. It first checks that `stdout` (`"\ndone\n"`), `stderr`, and the persisted transcript (4 messages) are unchanged, and that the bridge-relayed subsequence is `ToolExecutionStart, ToolExecutionEnd, TextDelta`. Then it checks that none of the seven dropped kinds reaches the sink.

**Mutation checks** (sources restored and `touch`ed after each; both tests green after restore):

| Mutation | `ocean-agent` sink test | daemon golden test |
|---|---|---|
| M1: restore the pre-slice-2 loop (clone and forward everything) | **fails** (`AgentStart reached the event sink`), after its `stdout`/`stderr`/transcript and relayed-golden asserts pass | passes. The bridge-visible stream is identical before and after. |
| M2: also stop forwarding `TextDelta` | **fails** (relayed sequence changed) | **fails** (`assistant_text_delta` missing) |
| M3: forward `AgentEnd` again | **fails** (`AgentEnd reached the event sink`) | passes (the bridge discards it) |
| M4: forward a cloned `TurnCheckpoint` again | **fails** (`TurnCheckpoint reached the event sink`) | passes (the bridge discards it) |
