# Ocean OS architecture

> Last validated against source: 2026-06-05.

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
  Scheduler binary for Ocean daemon routines: prompt-injection hooks now,
  courier jobs later. Generates and runs schedulers that drive the daemon URL.

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

## Built, pending daemon integration

These crates and types **exist and are tested** in the workspace, but the daemon
does not yet construct or register them — so the *library exists* while the
*feature does not yet work* live. This section is operator honesty: it names the
gap so nobody assumes a capability is on just because the crate compiles.

```text
ocean-store
  SQLite-backed durable Room store (SqliteRoomStore). Mirrors the in-memory
  RoomRegistry API method-for-method behind a `dyn RoomStore` trait, with its
  own unit tests (including a reopen-survives-restart test).
  STATUS: NOT wired. The daemon still constructs the in-memory
  `ocean_agent::RoomRegistry` (crates/ocean-daemon/src/main.rs) and does not
  even depend on `ocean-store`. The crate's own top-level docs say so: "the
  daemon is not wired to use this store — that is a separate follow-up ticket."
  OPERATOR IMPACT: persistent rooms, their rosters, and their transcripts live
  in process memory and are LOST on every daemon restart. Durable rooms only
  take effect once the daemon swaps its `Mutex<RoomRegistry>` for the SQLite
  store (the method names/signatures already match, so it is a field-type swap).

ocean-plugin
  Subprocess plugin runtime + a `PluginProvider` that implements the runtime's
  `CapabilityProvider` seam (the same seam `ocean-mcp` uses), exposing plugin
  tools to the agent as `plugin__<plugin>__<tool>` (OCEAN-95).
  STATUS: NOT wired. The daemon's capability registry is built by
  `build_capability_registry` (in ocean-agent), which registers only
  BuiltinProvider, BrowserProvider, and any configured McpProviders. No
  `PluginProvider` is constructed, and neither the daemon nor ocean-agent
  depends on `ocean-plugin`.
  OPERATOR IMPACT: installed plugins contribute ZERO tools to a turn. The
  plugin transport runs in isolation; nothing in the live agent path loads,
  lists, or calls it yet.

Content::Image (multimodal content — built, partial wiring)
  The protocol type `Content::Image { data, mime_type }` exists and is PRODUCED
  today — the browser `perceive` tool captures screenshots as image content,
  and the Anthropic provider serializes it correctly, so screenshots reach
  Claude models end-to-end.
  STATUS: cross-provider wiring incomplete. The OpenAI provider's user-message
  encoder filters content to text only (`filter_map(as_text)`) and Gemini has
  no Image arm; the daemon's transcript flattener also ignores Image (text-only
  view).
  OPERATOR IMPACT: vision/browser turns are first-class on Anthropic models but
  silently text-only on OpenAI/Gemini — a screenshot taken mid-turn never
  reaches a non-Anthropic model. The type is defined; the cross-provider feature
  is not complete.

ocean-acp permission forwarding (wired, not yet functional — subscribe-order race)
  The per-turn ACP permission bridge (`spawn_permission_bridge`) is fully built:
  it watches the daemon control stream (`/v1/events`), forwards each
  `PermissionRequest` scoped to our turn to the editor as
  `session/request_permission`, and POSTs the decision back to
  `POST /v1/permissions/{id}/decision`.
  GATING IS REAL: `AgentTurnRequest` has NO `yolo` field — the daemon decides the
  mode per turn via `yolo_enabled()`, which reads the `OCEAN_YOLO` env and
  defaults to GATED (OCEAN-51 / #54). So ACP turns DO gate by default: a mutating
  tool call blocks inside the daemon's `runtime.prompt(...)` and raises a
  `PermissionRequest` on the control stream. That part works.
  STATUS: the bridge delivery is broken by an ordering race (not the gating).
  `run_turn` (in `crates/ocean-acp/src/main.rs`) awaits `submit_turn(...)` BEFORE
  calling `spawn_permission_bridge`, but the daemon's `POST /v1/agent/turns`
  handler only returns its HTTP response AFTER `runtime.prompt(...)` completes —
  and a gated prompt blocks inside that call waiting for the permission decision.
  So the daemon emits the `PermissionRequest` while the ACP side is still awaiting
  `submit_turn`; the bridge subscribes to the control stream only afterward and
  there is no replay, so it never sees the request. (Note the typed agent stream
  IS subscribed before submit at the top of `run_turn` — it is specifically the
  bridge's separate control-stream subscription that arrives too late.)
  OPERATOR IMPACT: in Zed today, a gated Ocean tool call hangs — no editor-side
  approval prompt is delivered, because the request fired before the bridge was
  listening. Fix tracked as OCEAN-146 (subscribe to the control stream before /
  concurrently with `submit_turn`, on the ACP side).
```

The same "library exists vs feature works" gap also covers two items tracked in
their own docs:

- **Room auto-convene** — the trigger policy is stored and evaluated
  (`evaluate_trigger_policy`), and `room_post_message` emits a `room_trigger`
  notice + audit line on a matching `@mention`, but it does NOT yet queue an
  agent turn for the mentioned participant, so no agent actually wakes up. See
  `docs/OCEAN_ROOMS_COLLABORATION_MODEL.md` § "Mentions and triggers".
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

