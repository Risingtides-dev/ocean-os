# ocean-rs architecture

Ocean-rs is a Rust-native agent runtime for Ocean OS.

## Crates

```text
ocean-core
  Shared protocol types: requests, responses, events, sessions.

ocean-daemon
  Long-running OS service. Owns API surface for OceanTUI/Ocean GUI and
  calls the native runtime in-process. It must not shell out to a second agent
  runtime.

ocean-agent
  Native Ocean agent facade. Owns model selection, DeepSeek key discovery,
  session persistence, daemon-safe permissions, project prompt loading, and
  protocol/event mapping. It currently uses the small pi-agent/pi-ai Rust crates
  as replaceable components, not as an external process.

ocean-cli
  Thin terminal client for daemon control and one-shot prompts.
```

Planned crates:

```text
ocean-agent internals
  Replace remaining pi-agent/pi-ai components with Ocean-owned agent loop,
  provider clients, and tools.

ocean-providers
  DeepSeek/OpenAI-compatible, Anthropic, Gemini, xAI.

ocean-tools
  read/write/edit/bash/grep/find/ls plus Rust-specific tools.

ocean-store
  SQLite session/event store.

See `docs/OCEAN_NATIVE_INTERNALS_MAP.md` for the current Pi-borrowed
`ocean-agent` surfaces and the extraction order that preserves smoke behavior.

ocean-tui
  ratatui client.

ocean-plugin
  WASM/subprocess plugin runtime.
```

## API model

Clients do not directly run agents. They connect to the daemon.

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

