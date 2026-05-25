# Ocean Runtime + TUI Framing

Ocean TUI is the first steering client for the canonical `ocean-rs` daemon.

The goal is not to build a second terminal runtime. The goal is to force the daemon protocol to become complete, streamable, inspectable, and useful before GUI polish.

## Boundary

```text
ocean-tui ── client protocol ── ocean-daemon ── ocean-agent / tools / providers
```

The TUI may render state, gather input, send requests, display events, and ask for approvals. It must not own the agent loop, provider calls, session storage authority, or tool execution authority.

## Runtime responsibilities

`ocean-rs` owns:

- agent runs
- request IDs
- session IDs
- session persistence
- event emission
- tool execution
- permission requests
- cancellation
- provider configuration
- protocol compatibility for CLI/TUI/GUI

## TUI responsibilities

`ocean-tui` owns:

- terminal layout
- composer/input editing
- event/transcript rendering
- session picker
- command history
- request status display
- approval UI for daemon permission requests
- cancel/retry controls

## Initial daemon protocol target

Existing:

```text
GET  /health
POST /v1/prompt
GET  /v1/sessions
```

Shared types already in `crates/ocean-core`:

- `RequestId` / `SessionId` / `PermissionId`
- `PromptRequest` / `PromptResponse`
- `RequestState` / `RequestStatus`
- `EventEnvelope` / `OceanEvent`
- `CancelRequest`
- `PermissionDecision` / `PermissionDecisionRequest`

Next:

```text
GET  /v1/events                     # SSE stream
POST /v1/requests                   # start prompt/request
POST /v1/requests/:id/cancel        # cancel request
GET  /v1/sessions/:id               # inspect session
POST /v1/permissions/:id/decision   # approve/deny tool request
```

SSE is preferred first because the daemon can keep request/approval commands as ordinary HTTP posts while streaming output and tool events one-way to clients.

Shared event names are snake_case on the wire (`tool_started`, `permission_request`, `request_cancelled`, etc.) even when the docs show a dotted conceptual model.

## First TUI layout

```text
┌ Ocean TUI ─ status: daemon ok / model / session / request ┐
│ Transcript                                                 │
│                                                           │
├ Events / tools / permissions ─────────────────────────────┤
│                                                           │
├ Composer ─────────────────────────────────────────────────┤
│ >                                                         │
└ shortcuts: enter send | ctrl-c cancel | ctrl-s sessions ──┘
```

## First vertical slices

1. Add `crates/ocean-tui` with health display.
2. Add prompt composer calling existing `/v1/prompt`.
3. Add sessions panel calling existing `/v1/sessions`.
4. Add daemon event broadcaster and `/v1/events` SSE endpoint.
5. Stream prompt events through TUI.
6. Add cancellation.
7. Add permission request/decision protocol.

## Success criteria

A developer can run:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
cargo run -p ocean-daemon
cargo run -p ocean-tui
```

Then use the TUI to check daemon health, send a prompt, watch assistant/tool events, inspect sessions, and cancel or approve actions without any GUI process running.
