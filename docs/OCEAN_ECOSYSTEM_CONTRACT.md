# Ocean Ecosystem Contract

Status: active contract for runtime and first-party surfaces.

## Terms

- `Project`: repo/product identity. A project is the durable named thing Ocean recognizes, configures, and caretakes.
- `Workspace`: one concrete checkout/worktree/local directory on disk. A workspace has a path and may belong to a project.
- `Session`: one agent/human work thread rooted in exactly one workspace.
- `Surface`: a UI/client attached to a session, such as GPUI, the Chrome extension, web, TUI, ACP, CLI, or voice.
- `Canvas`: a tldraw/CRDT document visible from a surface.
- `LiveKit room`: the real-time audio/video/data collaboration container.
- `Longhouse`: the reasoning/federation plane.

## Invariants

1. A product surface chooses or creates a session before it submits a turn.
2. A session stores its workspace root when created; turns inherit cwd from the session.
3. Product surfaces subscribe with `GET /v1/agent/events?session_id=<id>`.
4. Product surfaces submit turns with `session_id=<id>`.
5. SSE events never change a surface's active session. Only user attach/select or explicit session creation can do that.
6. The global `/v1/agent/events` stream is for debug and legacy clients only.
7. `client_type` describes render/communication medium. It is not a session id, workspace id, LiveKit participant id, or canvas id.

## Runtime Flow

```text
Project -> Workspace -> Session -> Turns -> Events
Surface -> Session
```

New first-party surface flow:

```text
choose/create Project or local directory
choose/create Workspace
POST /v1/agent/sessions
GET /v1/agent/events?session_id=<id>
POST /v1/agent/turns { session_id, prompt, client_type }
```

Two surfaces intentionally sharing one session both subscribe to the same `session_id`.
Two surfaces on different sessions cannot receive each other's session-bearing events.

## Implementation Anchors

Runtime side:

- `crates/ocean-agent-sdk/src/lib.rs`: `AgentSessionCreateRequest`,
  `AgentSessionCreateResponse`, `AgentTurnEvent::session_id`.
- `crates/ocean-agent/src/lib.rs`: session creation and workspace binding.
- `crates/ocean-daemon/src/main.rs`: `POST /v1/agent/sessions`, surface turn
  guard, and scoped SSE filtering.

Surface side:

- `../ocean-surface/crates/ocean-gui/src/shell/daemon.rs`: GPUI daemon client
  wire types and scoped event URL.
- `../ocean-surface/crates/ocean-gui/src/shell/view.rs`: create-session-before-turn flow.
- `../ocean-surface/crates/ocean-surface-ui/src/daemon.rs`: web/extension create-session-before-turn flow.
- `../ocean-surface/crates/ocean-surface-proxy/src/main.rs`: forwards
  `POST /v1/agent/sessions` and preserves `?session_id=...` on SSE.

## Legacy Notes

Older docs may discuss TUI rooms, tmux layouts, or room guidance. Treat those as
TUI/runtime reference material unless they explicitly point back to this
contract. They are not the GPUI canvas/LiveKit collaboration spec.
