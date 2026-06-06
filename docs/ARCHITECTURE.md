# Ocean OS architecture

> Last validated against source: 2026-06-06 (post Epoch 6 / M1: durable rooms,
> plugin tools, cross-provider vision, and room auto-convene have all shipped —
> see "Shipped since the original integration list" below).

Ocean OS is a Rust-native coding-agent runtime and daemon. The daemon owns runtime authority; `ocean-tui` is the active steering cockpit and Rust-native Tides Mesh MeshFloor over that runtime.

## Crates

```text
ocean-core
  Shared protocol types: requests, responses, events, sessions.

ocean-protocol
  Unified multi-provider LLM wire protocol (Anthropic, OpenAI, Google,
  OpenAI-compatible). SSE streaming, retry, cancellation.

ocean-runtime
  Agent loop with permission-gated tool execution. Built-in tools:
  read, write, edit, bash, ls, grep, glob, web_fetch, todo.

ocean-providers
  Ocean-owned provider registry: model routing, credential resolution,
  readiness checks.

ocean-agent
  Native Ocean agent facade. Owns model selection, credential discovery,
  session persistence, daemon-safe permissions, project prompt loading, and
  protocol/event mapping. Wraps ocean-runtime + ocean-protocol in-process.

ocean-agent-sdk
  Typed product vocabulary shared by the daemon and all clients: AgentSession,
  AgentTurn, AgentTurnEvent (the canonical SSE payload), session create/list
  request/response types, and LonghouseEvent. Deliberately separate from
  ocean-core so the product contract is explicit and isolated.

ocean-mcp
  Ocean as an MCP *client*. Connects to external Model Context Protocol servers
  and exposes their tools to the agent through the runtime's CapabilityProvider
  seam — this is how the keys in tools.env (Brave, Slack, Linear, …) become
  agent tools without hardcoded native Rust tools. Depends one-way on
  ocean-runtime; the runtime never depends back on it.

ocean-browser
  Typed async handle to a Chrome instance driven over the DevTools Protocol.
  Launch/attach, navigation, screenshot, hybrid read_page, inspect, Layer-3
  input, live network capture, downloads, and a tab shell that treats tabs as
  first-class addressable objects. Powers the runtime's browser tool suite.

ocean-longhouse
  The real quorum engine and convening flow behind the longhouse deck. A pure,
  daemon-computed QuorumEngine counts credential-weighted, time-decaying,
  cross-inhibiting marks and decides when a proposal crosses quorum — never an
  LLM. The convene flow staffs a council with real LLM workers on cheap models,
  runs a two-round propose → endorse/inhibit protocol, and emits the existing
  LonghouseEvents so the deck renders a live council with zero deck changes.

ocean-acp
  ACP (Agent Client Protocol) bridge. Exposes the Ocean daemon to Zed and other
  ACP editors over stdio, mapping editor sessions onto daemon turns.

ocean-heartbeat
  Standalone scheduler CLI for Ocean daemon routines: prompt-injection hooks
  now, courier jobs later. It is a separate binary (`crates/ocean-heartbeat`,
  src/main.rs) and an HTTP *client* of the daemon — NOT wired into the daemon
  and not depended on by it (nothing in the workspace references
  ocean-heartbeat). Meant to run under launchd/cron, it reads a TOML routine
  (id, cwd, prompt, optional room_id/project_id, durable session file), GET-
  prechecks the daemon's /health, then POSTs the routine prompt to
  /v1/agent/turns and persists the returned session_id so successive runs
  resume the same session. Subcommands: `run` (fire one routine now), `init`
  (write a starter routine TOML), `component` (print a render-protocol stat
  snapshot for PWA/dashboard clients), and `launchd` (print a macOS launchd
  plist for the routine — does not install it). Targets the daemon at
  OCEAN_DAEMON_URL / http://127.0.0.1:4780 by default.

ocean-call
  Daemon-side Twilio/LiveKit call-intelligence pipeline. A real phone number,
  bridged via Twilio SIP into a LiveKit room, that Ocean joins as a server-side
  participant. Over one shared audio stream it runs two lanes: a passive lane
  (server-side room audio tap → streaming STT → rolling summarizer + task
  detection, emitting call OceanEvents onto the daemon SSE rail) and an
  active, wake-word-gated lane ("hey Ocean…" → wake-gate → one ephemeral agent
  turn → TTS speaker back into the call). Components: room_tap, frame
  re-chunker, stt (+ stt_xai), summarizer, task_detector, wake gate,
  sip_bridge, speaker (TTS), and a Twilio/LiveKit webhook. Backs three daemon
  routes (handlers in crates/ocean-daemon/src/main.rs):
  - POST /v1/calls/place  — place a real outbound call: { "to": "<number>" }.
    Normalizes to E.164, and if the SIP/LiveKit env is configured
    (LIVEKIT_URL/_API_KEY/_API_SECRET + OCEAN_CALL_OUTBOUND_TRUNK +
    OCEAN_CALL_CALLER_NUMBER) it mints a call room, emits CallStarted, and dials
    via the LiveKit SIP bridge. Returns 503 naming the unset env when telephony
    is not provisioned; emits CallEnded on a failed dial so no phantom call is
    left "in progress".
  - POST /v1/calls/webhook — LiveKit webhook receiver. Verifies the signature
    and, on a room_started/room_finished for a call_ room, emits
    CallStarted/CallEnded onto the SSE rail — the path that lets an INBOUND call
    reach the pipeline.
  - POST /v1/calls/demo  — scripted, no-telephony demo: runs a canned transcript
    through the passive lane (summarizer + wake gate) and streams call
    OceanEvents onto /v1/events. Proves the pipeline without a real phone line.

ocean-daemon
  Long-running OS service. Owns API surface for OceanTUI/Ocean GUI and
  calls the native runtime in-process. It must not shell out to a second agent
  runtime.

ocean-cli
  Thin terminal client for daemon control and one-shot prompts.

ocean-tui
  Active ratatui steering cockpit and Rust-native Tides Mesh MeshFloor. It
  renders floor state, prompts, sessions, requests, events, approvals, and
  mesh panels while leaving provider calls, tools, sessions, and agent loops
  under daemon authority.

  Main layout/parity references:
  - docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md
  - docs/OCEAN_TUI_TIDES_MESH_PARITY.md
```

## Shipped since the original integration list

This section used to read "Built, pending daemon integration" and listed a set of
crates and types as existing-but-not-wired. **As of Epoch 6 / M1 nearly all of
them are now constructed and live in the daemon path.** They are recorded here so
the history is clear and so the one genuinely remaining gap (a display-only
transcript flattener) is not lost in the noise.

```text
ocean-store — WIRED (durable rooms).
  SQLite-backed durable Room store (`SqliteRoomStore`) behind the `RoomStore`
  trait. The daemon depends on it (`crates/ocean-daemon/Cargo.toml:25
  ocean-store.workspace = true`) and constructs it at startup
  (`crates/ocean-daemon/src/main.rs:565 ocean_store::SqliteRoomStore::open(...)`),
  holding it as `Arc<Mutex<ocean_store::SqliteRoomStore>>` (main.rs:85) instead of
  the old in-memory `RoomRegistry`. Every persistent-room handler routes through
  it via `with_rooms(...)` and `RoomStore` (main.rs:45, 1900) (OCEAN-86 / 107).
  OPERATOR IMPACT: rooms, rosters, and transcripts now PERSIST across daemon
  restarts — they live in the rooms SQLite db, not process memory.

ocean-plugin — WIRED (plugin tools reach the agent).
  Subprocess plugin runtime + a `PluginProvider` implementing the runtime's
  `CapabilityProvider` seam, exposing plugin tools as `plugin__<plugin>__<tool>`
  (OCEAN-95). `ocean-agent` depends on it (`crates/ocean-agent/Cargo.toml:17
  ocean-plugin.workspace = true`). `build_capability_registry` now calls
  `discover_plugin_providers(config_dir)` and registers each returned
  `ocean_plugin::PluginProvider` (`crates/ocean-agent/src/lib.rs:978`,
  constructed at lib.rs:1060). PluginProvider tools report
  `requires_permission == true`, so they gate like any mutating tool.
  OPERATOR IMPACT: installed plugins now contribute their tools to a turn; the
  live agent path discovers, lists, and calls them.

Content::Image (cross-provider vision) — WIRED on the model wire path; one
  display-only flattener remains.
  The protocol type `Content::Image { data, mime_type }` is produced by the
  browser/computer-use tools and now encoded by ALL FOUR providers on the way to
  the model:
    - Anthropic — `crates/ocean-protocol/src/providers/anthropic.rs:158,193`
    - OpenAI    — `crates/ocean-protocol/src/providers/openai.rs:198,286,813,864`
                  (OCEAN-99 user-message vision, OCEAN-131 tool-result images)
    - Gemini    — `crates/ocean-protocol/src/providers/google.rs:131,235,617,654`
                  (OCEAN-99 / OCEAN-132)
    - Codex     — `crates/ocean-protocol/src/providers/codex.rs:66,154,651,688`
                  (OCEAN-133)
  The old "OpenAI text-only / Gemini has no Image arm" claim is OBSOLETE — a
  screenshot taken mid-turn now reaches every provider's model.
  REMAINING GAP (LOW sev, display-only): the daemon's transcript flattener
  `text_from_content` (`crates/ocean-agent/src/lib.rs:1762-1772`) still drops
  `Content::Image` — it keeps only `Text`/`Thinking`. This affects ONLY the
  human-readable transcript returned by `GET /v1/sessions/{id}`, NOT the model
  wire path. So a session-detail view shows an image-bearing turn as text-only,
  even though the model itself received the image. Cosmetic, not a capability gap.

ocean-acp permission forwarding — WIRED and functional (race fixed, OCEAN-146).
  The per-turn ACP permission bridge (`spawn_permission_bridge`) watches the
  daemon control stream, forwards each scoped `PermissionRequest` to the editor as
  `session/request_permission`, and POSTs the decision back. Gating is real: the
  daemon decides the mode per turn (`yolo_enabled()`, default GATED, OCEAN-51).
  The old subscribe-order race is FIXED. `run_turn`
  (`crates/ocean-acp/src/main.rs`) now subscribes the control stream BEFORE
  submitting the turn (main.rs:490-508, then `submit_turn` at ~525), so the
  bridge is listening before the daemon can emit the gated `PermissionRequest`.
  Because the broadcast channel has no replay, this ordering is what makes
  delivery work. The turn's `request_id` is learned from the event stream, not
  from `submit_turn`'s (deadlock-prone) response.
  OPERATOR IMPACT: in Zed today a gated Ocean tool call surfaces an editor-side
  approval prompt — it no longer hangs.

Room auto-convene — WIRED (a resolved mention now wakes the agent).
  `room_post_message` evaluates the stored trigger policy
  (`evaluate_trigger_policy`, `crates/ocean-daemon/src/main.rs:2114`) on an
  `@mention`. When the policy says convene AND the mentioned id resolves to a real
  `Agent` in the roster, the handler emits the `room_trigger` notice (main.rs:2148),
  writes an `auto-convene:` audit line, and — crucially — calls
  `spawn_room_agent_turn(...)` (main.rs:2181, defined at main.rs:2302), which
  spawns an actual agent turn for the mentioned participant (resumes the
  deterministic room+agent session, builds a transcript-tail prompt, runs it).
  Note OCEAN-128: the `room_trigger` event and audit line only fire once an Agent
  is actually resolved, so a mention of a non-agent id no longer claims a convene
  that never happened. (OCEAN-111 / OCEAN-128.)
  OPERATOR IMPACT: mentioning a room agent now actually wakes it; no agent-turn is
  queued for human/bot/system or unknown mentions.
```

One related item is still partial and tracked in its own doc:

- **Longhouse governance (quorum steps 6+)** — the convergence engine (steps
  1–5) is built and tested; the escrow trio (TitleRegistry + Revoker +
  validator escrow) and the unforgeable `claim_outcome` gate are stubbed. See
  `docs/LONGHOUSE.md` § "Built vs unbuilt".

## API model

Clients do not directly run agents. They connect to the daemon. `ocean-tui` is still an active operator cockpit: it steers requests, approvals, cancellation, and MeshFloor visibility through protocol calls instead of becoming a second runtime.

```text
OceanTUI ─┐
OceanGUI ─┼── HTTP/WebSocket/Unix socket ── ocean-daemon ── agent workers
CLI      ─┘
```

## Core protocol target

Shared protocol types live in `crates/ocean-core` and cover:

- `RequestId`, `SessionId`, and `PermissionId`
- `PromptRequest` / `PromptResponse`
- `RequestState` / `RequestStatus`
- `EventEnvelope` / `OceanEvent`
- `PermissionDecision` / `PermissionDecisionRequest`
- `CancelRequest`
- `SessionSummary`

The stream envelope is flat and carries request/session correlation at the top level:

```json
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"session_created"}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"user_message","text":"..."}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"assistant_delta","text":"..."}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"tool_started","tool":"bash","args":{}}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"tool_output","tool":"bash","text":"..."}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"tool_ended","tool":"bash","is_error":false}
{"id":"...","at":"...","session_id":"...","request_id":"...","permission_id":"...","type":"permission_request","tool":"write","reason":"...","args":{}}
{"id":"...","at":"...","session_id":"...","request_id":"...","type":"turn_finished","ok":true,"wall_ms":12}
{"id":"...","at":"...","request_id":"...","type":"cancelled","reason":"user cancelled"}
{"id":"...","at":"...","type":"error","message":"..."}
```

Compatibility notes:

- `GET /health` is unchanged.
- `POST /v1/prompt` still accepts the legacy body; `request_id` is optional and responses may omit it for older callers.
- `GET /v1/sessions` still emits `id`, and `ocean-core` also accepts `session_id` as a deserialize alias.
- New event/session fields are additive; old clients can ignore them.

