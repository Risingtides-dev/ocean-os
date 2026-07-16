# Ocean OS architecture

Status: implemented current-state architecture. Package-level ownership and
narrow validation live in [`../crates/AGENTS.md`](../crates/AGENTS.md); this
document explains composition and state flow without duplicating all 25 package
rows.

## Runtime model

Ocean OS is one local runtime with multiple clients.

```text
TUI / CLI / ACP / Ocean Surface
        |
        | HTTP requests + session-scoped SSE
        v
  ocean-daemon (:4780)
        |
        +-- ocean-agent -------- sessions, prompt assembly, capabilities
        |       |
        |       +-- ocean-runtime ---- agent loop, tools, permissions, halt
        |       +-- ocean-mcp -------- external MCP tools
        |       +-- ocean-plugin ----- subprocess plugin tools
        |       `-- ocean-hooks ------ hook protocol/config substrate
        |
        +-- ocean-providers ---------- model catalog, auth, readiness
        +-- ocean-protocol ----------- provider requests/streams/retry
        +-- ocean-store -------------- durable local rooms
        +-- ocean-memory ------------- local typed memory
        +-- ocean-longhouse ---------- council/quorum/title coordination
        `-- ocean-browser/ocean-call - browser and optional call capabilities
```

The daemon composes these packages in-process. It does not shell out to another
agent runtime. Clients do not call providers or execute tools themselves.

## Authority and state ownership

| State or decision | Owner | Notes |
| --- | --- | --- |
| HTTP routes, middleware, live request registry, event buses, metrics, runtime composition | `ocean-daemon` | Long-running service and only first-party HTTP/SSE authority; cohesive private leaves are being extracted under the active daemon mission |
| Product sessions, transcript/history persistence, workspace rebinding | `ocean-agent` | Shared by every client; no client-local session authority |
| Provider rounds, tool execution, permission waits, cancellation | `ocean-runtime` | Receives capabilities assembled by `ocean-agent` |
| Product session/turn/event wire vocabulary | `ocean-agent-sdk` | Used by daemon and first-party clients |
| Lower-level daemon control/event vocabulary | `ocean-core` | Legacy/control paths and shared leaf types |
| Provider request/stream encoding | `ocean-protocol` | Anthropic, OpenAI/Codex-compatible, and Gemini paths |
| Model aliases, credentials, routing, readiness | `ocean-providers` | Separate from provider wire encoding |
| Durable local rooms/rosters/transcripts | `ocean-store` | SQLite-backed daemon state |
| Agent/operator memory rows | `ocean-memory` | Shared Bedrock knowledge remains external |
| Quorum, councils, titles, escrow, recall/revocation | `ocean-longhouse` | Daemon still owns execution and permissions |
| Product rendering and interaction | clients | TUI or sibling `ocean-surface`; no runtime authority |

## Product turn flow

First-party surfaces create or select a session before steering it:

```text
1. POST /v1/agent/sessions
2. GET  /v1/agent/events?session_id=<id>
3. POST /v1/agent/turns
4. daemon binds session/cwd/model/profile and starts one runtime turn
5. runtime emits text, tool, permission, component, and completion events
6. daemon streams the session-scoped projection to attached clients
7. ocean-agent persists the completed session/history
```

`AgentTurnRequest` is defined in `ocean-agent-sdk`. Its current optional controls
include project binding, named agent selection, model/role, thinking level,
images, permission decision token, client context, and advisor control. Read the
type rather than copying its full field list into client documentation.

The lower-level `/v1/prompt`, `/v1/events`, request, permission, and legacy
session routes remain compatibility/control surfaces. New product clients use
the session-scoped agent API.

## Session and workspace invariants

- A session has one daemon-owned transcript and persistence record.
- A turn binds to a caller cwd/project under daemon policy; the daemon itself
  runs from a neutral cwd so fallback cannot bind to this repository.
- Same-session load → run → save is serialized to prevent transcript corruption.
- Explicit resume and create behavior stay distinct; an unknown explicit session
  must not silently fork.
- Images remain in raw messages while lightweight session projections avoid
  copying encoded image bytes.
- Two clients share a conversation only by attaching to the same session id.

See [`OCEAN_WORKSPACE_BINDING.md`](OCEAN_WORKSPACE_BINDING.md).

## Tools, capabilities, and permissions

`ocean-agent` assembles the capability registry for a turn from built-in
runtime tools plus configured MCP and plugin providers. `ocean-runtime` exposes
schemas to the selected model, executes calls, records results, and applies
permission/cancellation policy.

Mutating tools are gated by default. A client may carry a per-turn decision
token so approvals cannot be replayed by an unrelated localhost observer.
Trusted bypass modes are explicit operator choices; documentation and surfaces
must never imply a silent allow.

Unix shell commands run in a child-owned process group. Halt/timeout/drop kills
the ordinary descendant tree before the direct child is reaped. Deliberately
re-sessioned descendants and non-Unix tree termination are outside that
contract.

## Events and retention

Live runtime events are delivered to the daemon for the active turn. The daemon
maintains a replay buffer for reconnecting clients with two ceilings:

- 2,048 retained events;
- 32 MiB of serialized retained payload.

An oversized event is still delivered live but is not retained for replay. This
bounds replay memory without truncating the active stream. The per-turn
runtime-to-daemon MPSC remains a separate live-queue design concern tracked in
[`../ROADMAP.md`](../ROADMAP.md).

## Profiles and named agents

For ordinary surface turns, `ocean-agent` maps `client_type` to a surface flag
and prefers an on-disk profile at `assistants/<FLAG>/system.md`, falling back to
a compiled seed when no non-empty file resolves. `OCEAN_ASSISTANTS_DIR`
overrides the default profile root. Profile content is owned by the sibling
Ocean Agents repository.

Named folder-as-agent turns use the agent folder selected by
`AgentTurnRequest.agent`; discovery is exposed through `/v1/agents`. Runtime
tool and permission enforcement remains in Ocean OS regardless of profile
source.

Known mismatch: Ocean Surface's Tauri host currently emits `surface-tauri`,
which is not mapped by `ocean-agent::surface_flag`; see the project map and
roadmap.

## Other daemon domains

The daemon also exposes current route families for:

- models, projects, filesystem views, browser stream/input, memory, and LSP;
- persistent rooms and independent LiveKit token minting;
- render-protocol component events;
- Longhouse/council, skills, subagent specs, and workflow preparation;
- optional call placement/webhooks/demo and daemon-owned voice endpoints;
- health, readiness, metrics, cancellation, and permission decisions.

The canonical `app_router` currently registers 75 explicit method/path pairs. Executable parity tests keep router registration, discovery output, and the operator-guide quick reference synchronized while preserving Axum fallback behavior and tracing/CORS middleware order. CORS policy and turn metrics are private daemon leaves; composition remains in `main.rs`. The extended route reference is [`OCEAN_RUNTIME_OPERATOR_GUIDE.md`](OCEAN_RUNTIME_OPERATOR_GUIDE.md), and ongoing extraction boundaries live in [`DAEMON_REFACTOR_MISSION.md`](DAEMON_REFACTOR_MISSION.md).

## Clients

- `ocean-tui` is the active Ratatui coding cockpit. It steers the daemon and can render bounded render-protocol component projections; it is not another runtime. A shell-owned session component tray below Files derives run-local todo state only from successful correlated tool events, clears across turns/sessions, and marks incomplete projections when SSE gaps prevent trustworthy reconstruction.
- `ocean-cli` builds the `ocean-rs` one-shot/control client.
- `ocean-acp` maps editor sessions and permission requests to daemon contracts.
- `ocean-heartbeat` is an external scheduler CLI that calls the daemon; it is
  not in-daemon scheduling authority.
- `ocean-surface` is a sibling repository containing the product web/PWA,
  extension, proxy, and Tauri hosts.

## Process and deployment boundary

The supported operated daemon path on this Mac is a launchd LaunchAgent named
`dev.risingtides.ocean-daemon`. `ops/install-ocean-daemon.sh` requires the `main` branch, builds release code, installs the plist, and starts the daemon with a neutral cwd. Cleanliness and synchronization with `origin/main` are operator preconditions enforced by the commands in `OPERATIONS.md`, not by the installer itself. A release binary copied or built from another branch is not a supported deploy.

See [`OPERATIONS.md`](OPERATIONS.md) for the current commands and
[`../crates/AGENTS.md`](../crates/AGENTS.md) for change-impact validation.

## Deliberate non-claims

Ocean OS does not currently claim:

- sandbox-grade isolation beyond its documented permission/cwd/process controls;
- bounded live per-turn MPSC memory;
- runtime composition of Ocean Agents `_shared`/`_base` profile sources;
- wired production execution of configured lifecycle hooks;
- `surface-tauri` profile mapping;
- shared cloud storage authority (Ocean Bedrock owns that plane).
