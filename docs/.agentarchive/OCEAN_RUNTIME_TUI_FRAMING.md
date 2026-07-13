# Ocean Runtime + TUI Framing

Ocean is a Rust-native Pi-style coding-agent harness/runtime. `ocean-tui` is the active steering cockpit for that harness and the Rust-native Tides Mesh **MeshFloor**. It is not a passive daemon dashboard that merely asks for updates.

The goal is not to build a second terminal runtime. The goal is to make the cockpit strong enough to steer the daemon honestly: route work, show floor state, compose prompts, inspect sessions, watch events, request/cancel work, and approve/deny permissions while the daemon remains the runtime authority.

## Boundary

```text
ocean-tui ── client protocol ── ocean-daemon ── ocean-agent / tools / providers
```

The TUI actively renders and controls the operator surface: it gathers input, sends requests, displays events, shows floor state, drives cancel/retry/approval actions, and exposes the Tides Mesh MeshFloor. It must not own the agent loop, provider calls, session storage authority, or tool execution authority.

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
- MeshFloor / Tides Mesh cockpit view
- board, events, inbox, and agents panels
- composer/input editing
- event/transcript rendering
- session picker
- command history
- request status display
- approval UI for daemon permission requests
- cancel/retry controls
- floor visibility for Glyph, KNOX, Charlotte, Orchestrator, BRICK, PIXEL, Henry, and Rev workspaces

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

## Current cockpit / MeshFloor layout

The main TUI context should promote the live Tides Mesh floor, not bury it in side notes. The current Rust-native MeshFloor target is documented in:

- [`docs/OCEAN_TUI_TIDES_MESH_PARITY.md`](OCEAN_TUI_TIDES_MESH_PARITY.md) — active no-feature-drop parity contract.
- [`docs/OCEAN_TUI_MOCKUPS.md`](OCEAN_TUI_MOCKUPS.md) — active cockpit layout and interaction reference.

MeshFloor summary:

```text
┌ Glyph / audit ─┬ KNOX review ─┬ Charlotte research ─┐
├ Orchestrator / board-events-inbox-agents cockpit ───┤
├ PIXEL UI / TUI lane ───────┬ BRICK runtime lane ────┤
└ adjacent context: WritersRoom/Henry, Rev review, ops ┘
```

The daemon-steering cockpit remains part of this same surface:

```text
┌ Ocean TUI ─ status: daemon ok / model / session / request ┐
│ Transcript / MeshFloor / Sessions / Requests              │
├ Events / tools / permissions / Tides Mesh activity ───────┤
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
