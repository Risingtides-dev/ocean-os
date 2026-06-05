# Ocean OS architecture

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

Planned crates (extensibility phase — not yet in the workspace):

```text
ocean-store
  SQLite session/event store. (Today session/event persistence lives inside
  ocean-agent; ocean-store would extract it into a dedicated crate.)

ocean-plugin
  WASM/subprocess plugin runtime.
```

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

