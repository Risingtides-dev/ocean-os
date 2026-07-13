# Ocean Event Payload and Retention Characterization

**Date:** 2026-07-12
**Baseline commit:** `4cf87475` (rebased parent before the event-retention fix)
**Plan checkpoint:** Phase 0B-1 / Phase 1B-1
**Status:** Complete — risk reproduced, minimal replay-byte fix independently reviewed

## Question

Does the runtime→daemon→SSE event path have a checked policy for payload ownership, maximum inline bytes, retention lifetime, overflow, and durable evidence under finite tool-output pressure?

## Environment and method

- macOS 26.3.1 (25D2128), arm64 Mac mini, Darwin 25.3.0.
- Rust `1.97.0 (2d8144b78 2026-07-07)`.
- Three independent read-only source lanes mapped runtime variants, daemon retention/replay, and built-in/MCP/plugin/browser output bounds.
- Two finite tests exercise the actual queue and replay seams with 1 MiB payloads, fixed counts, single-threaded execution, a 30-second child timeout, and a 256 MiB maximum-RSS acceptance ceiling.

## Checked lifecycle policy — observed behavior

| Stage | Ownership / clone points | Current maximum | Retention / overflow | Durable evidence |
|---|---|---|---|---|
| Runtime producer | `AgentEvent` owns strings, JSON, content, messages, patches, or ops; producers clone from provider/tool/session state. | No generic event-byte cap. | Moved into a per-turn Tokio unbounded MPSC queue; closed receiver drops because send errors are ignored. | None at this stage. |
| Tool completion | `apply_outcome` clones full `content` into `ToolExecutionEnd`; arbitrary `details` moves into the event. | Full live content/details are uncapped generically. | Queue retains every event until the daemon bridge drains it. | Separate transcript copy caps text blocks at 32 KiB and images at 256 KiB; details are not persisted there. |
| Daemon bridge | Sequentially converts selected runtime events to `AgentTurnEvent`; output/details are flattened into `ToolResult`. | No generic bridge cap. | Bridge work can delay queue drain; runtime structural/message variants are intentionally filtered. | Final session transcript/result persists, not exact SSE chronology. |
| Agent broadcast | Envelope is cloned into replay history and moved into Tokio broadcast. | Capacity 1024 **events**, no byte limit. | Producer never blocks. Slow subscribers receive `Lagged`; scoped rail emits an error and records a lag occurrence. | In-memory only. |
| Replay history | `subscribe*` clones envelopes into a connection-local replay vector; SSE serialization allocates JSON strings. | Baseline: 2048 events only. Fixed policy: 2048 events **and 32 MiB serialized event payload**. | Oldest events evict until both limits hold; a single oversized event stays live but is not replay-retained. Unknown/aged-out `Last-Event-ID` gets no replay. | Lost on daemon restart. |
| SSE client | Replay vector chains into live broadcast stream; per-connection JSON and replay-id set are allocated. | Inherits event sizes. | Disconnect drops receiver; 3-second keepalive; slow client cannot backpressure producer. | Client-owned only. |
| Session store | Root agent lock spans load→run→save; messages persist atomically. | Tool transcript caps above; session history has its own message/context bounds. | Survives restart and SSE loss. | Final messages/tool transcript, but not exact deltas, thinking/tool chunks, SSE ids, or event order. |

The daemon request registry is **not** an additional leak: source verification found a 15-minute GC sweep, one-hour terminal TTL, and 10,000-entry hard cap. An initial scout suspicion was rejected.

## Exhaustive runtime `AgentEvent` inventory

| Variant | Payload beyond session id | Inline bound | Daemon disposition / evidence |
|---|---|---|---|
| `AgentStart` | none | small | filtered; daemon emits canonical turn start |
| `AgentEnd` | full `Vec<Message>` clone | no event cap | filtered; transcript separately persists |
| `TurnStart` | none | small | filtered |
| `TurnEnd` | none | small | filtered |
| `AssistantMessage` | full `Message` | no event cap | filtered; visible deltas/final transcript cover it |
| `UserMessage` | full `Message` | no event cap | filtered; user message persists |
| `TextDelta` | provider/fallback string | no local event cap | bridged to assistant delta; final text persists, chronology does not |
| `ThinkingDelta` | provider string | no local event cap | bridged; not persisted as durable public output |
| `ToolExecutionStart` | call id/name and arbitrary args JSON | no generic cap | bridged to `ToolCallStarted`; call args also exist in transcript tool-use content |
| `ToolExecutionEnd` | full `Vec<Content>` and arbitrary details JSON | **no live cap** | bridged to `ToolCallFinished`; capped content persists, details do not |
| `PermissionDenied` | tool name/reason strings | no generic cap | bridged as failed tool lifecycle; policy text is small/controlled |
| `ModelRerouted` | requested/effective/reason | no generic cap | bridged; small controlled strings |
| `Render` | id/kind/arbitrary props JSON | no generic cap | bridged to component render; replay only |
| `Unmount` | component id | small | bridged |
| `BrowserActivity` | bool | fixed | bridged |
| `SurfacePatch` | canvas id and patch vector | no generic cap | bridged; room projection may persist applied state, exact SSE event is volatile |
| `SlackCanvas` | typed operation | no generic cap | bridged with pending/fulfilled result; exact SSE chronology is volatile |

Adjacent SDK-only events also matter: `ToolCallChunk.chunk` has no observed runtime producer/cap, and `Extension.payload` is arbitrary JSON emitted directly onto the daemon bus.

## Upstream tool-output bounds

| Source | Observed bound before live event |
|---|---|
| Universal artifact spill | When enabled and healthy, each text block over 24,000 bytes becomes a ≤16,000-byte preview plus artifact reference. Non-text, details, side effects, store failure, and explicit `artifact://` reads bypass this protection. |
| Bash | 2 MiB stdout + 2 MiB stderr, drained to completion with cap markers. |
| Read | 8 MiB file bytes; 2,000 default lines; 2,000 chars per line. Artifact reads can re-enter with the full stored string. |
| Web fetch | 2 MiB response body, 30-second total timeout. |
| MCP | 2 MiB aggregate text; 16 MiB transport message; images are not generically byte-capped after mapping. |
| Plugin | 16 MiB transport message; no generic result/details cap. |
| LSP | 64 MiB inbound frame ceiling; common formatted results cap at 50 rows. |
| Browser / render / patches | No generic local screenshot, response-body, props, or patch-vector event-byte cap found. |
| `ls` / `grep` / glob / offshore | `ls` 1,000 entries; grep skips files >4 MiB and clips lines; offshore events 256 KiB; glob/match limits depend on requested maxima. |

## Finite characterization results

### Runtime unbounded queue and transcript split

`agent_loop::tests::runtime_event_queue_retains_full_tool_payload_until_drained` applies eight outcomes containing 1 MiB text + 64 KiB details each. It asserts:

- all eight full events remain queued before drain;
- every live event preserves all text/details;
- every transcript copy is capped to ~32 KiB with a marker;
- deterministic drain empties the queue.

Isolated child: **PASS**, 30-second timeout, 256 MiB ceiling, maximum RSS **19,415,040 bytes (18.5 MiB)**.

### Slow/disconnected subscriber and replay

`bus::tests::agent_bus_large_payloads_lag_slow_receiver_and_replay_after_disconnect` sends eight 1 MiB tool completions through a capacity-2 subscriber, asserts `Lagged(6)`, disconnects it, and emits a ninth event. The baseline retained all nine; the fixed test uses a three-event-sized byte ceiling and asserts only the newest three remain replayable.

Baseline isolated child: **PASS**, 30-second timeout, 256 MiB ceiling, maximum RSS **28,147,712 bytes (26.8 MiB)**. The same isolated child after byte eviction peaked at **18,071,552 bytes (17.2 MiB)**. Post-fix focused tests also prove an event larger than the replay ceiling remains live while replay history/byte count stay empty.

## Disposition

**Phase 0B-1 result: RED (bounded test passes, product risk proven).** The implementation behaves as coded, but event-count limits do not bound bytes. At the observed baseline, 2,048 replay entries × 1 MiB can retain roughly 2 GiB before per-connection replay cloning/serialization; a valid uncapped/4-MiB-class tool result makes the upper bound worse. The per-turn unbounded queue is transient but also has no formal byte/backpressure ceiling.

Smallest Phase 1B fix:

1. Add a byte ceiling to the daemon replay ring while retaining the 2,048-event ceiling and live delivery.
2. Evict oldest replay entries until both limits hold; a single event larger than the replay-byte ceiling remains live but is not replay-retained.
3. Keep the existing explicit subscriber-lag behavior.
4. Record the runtime unbounded queue and generic live-event cap as residual risks; do not introduce artifact-backed large-result architecture in this checkpoint.

This fixes the demonstrated long-lived byte-retention defect without changing runtime tool output, live SSE payloads, public event shapes, transcript persistence, or introducing a new artifact protocol.

## Phase 1B fix result

- Added `AGENT_EVENT_REPLAY_MAX_BYTES = 32 MiB` alongside the existing 2,048-event ceiling.
- Counted serialized `AgentTurnEvent` payload bytes with a non-allocating writer during emission.
- Stored aggregate replay bytes in the same mutex-protected state as the deque across cloned bus handles; poisoned-lock recovery recomputes the aggregate from retained envelopes before the next eviction decision.
- Preserved full live broadcast. An individually oversized event is excluded from replay, not from live delivery. The current `AgentTurnEvent` graph serializes totally; a defensive serialization-error fallback follows the same live/no-replay disposition.
- Added focused tests for counter accuracy, cloned-handle emission, poison recovery, slow-subscriber lag plus byte eviction, disconnected replay, and oversized-live/no-replay behavior.

Residual risk: the runtime→daemon per-turn MPSC queue is still unbounded and generic live event variants still lack one universal byte cap. The finite eight-event test passed well below its RSS ceiling, and the bridge normally drains without awaited I/O; changing that channel or public live payload semantics is a separate follow-up, not bundled into this retention fix.

## Final validation

- Runtime queue characterization: 1 focused test passed; full `ocean-runtime` suite **113 passed**.
- Daemon replay policy: 4 focused bus tests passed; full `ocean-daemon` suite **294 passed**.
- `cargo check --workspace --tests`, strict affected-crate Clippy, format, diff, and docs-check passed.
- Full `cargo xtask ci` passed before the poison-state refinement; the final full gate was rerun at closeout.
- Fresh security review verified the 17-variant inventory, dual count/byte limits, cloned-handle behavior, poison-safe accounting, live/replay seam, registry correction, and residual risks; follow-up verdict: **PASS — no remaining blockers**.
