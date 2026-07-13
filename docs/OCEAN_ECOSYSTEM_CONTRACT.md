# Ocean Ecosystem Contract

Status: active contract for runtime and first-party surfaces.

## Terms

- `Project`: repo/product identity. A project is the durable named thing Ocean recognizes, configures, and caretakes.
- `Workspace`: one concrete checkout/worktree/local directory on disk. A workspace has a path and may belong to a project.
- `Session`: one agent/human work thread rooted in exactly one workspace.
- `Surface`: a UI/client attached to a session, such as GPUI, the Chrome extension, web, TUI, ACP, CLI, or voice.
- `Canvas`: a tldraw/CRDT document visible from a surface. Concurrent
  operator+agent edits converge via the per-component version-vector merge
  (OCEAN-258); see `docs/OCEAN_CANVAS_CONVERGENT_MERGE.md`.
- `LiveKit room`: the real-time audio/video/data collaboration container.
- `Longhouse`: the reasoning/federation plane.

## Invariants

1. A product surface chooses or creates a session before it submits a turn.
2. A session stores its workspace root when created; turns inherit cwd from the session.
3. Product surfaces subscribe with `GET /v1/agent/events?session_id=<id>`.
4. Product surfaces submit turns with `session_id=<id>`.
5. SSE events never change a surface's active session, and never cross between
   sessions. A subscriber on `?session_id=<id>` receives only that session's
   events. Only user attach/select or explicit session creation can change a
   surface's active session.
   - **Extension-event exception.** `AgentTurnEvent::Extension` events (e.g.
     Longhouse council events) carry an optional `scope`. A council spans many
     agents/sessions, so it often has no single owning session. Delivery rules:
     - `scope = Some(session_id)` — treated exactly like any session-bearing
       event: delivered only to that session's subscribers (and `?all=1`).
     - `scope = None` (council-wide / global-by-design) — delivered **only** to
       subscribers who opt into the global stream via `?all=1`. It is **never**
       delivered to a session-scoped subscriber. This keeps the no-crossing
       guarantee intact: a council never leaks into an unrelated session's
       transcript; it reaches the deck (which subscribes globally) by design.
   - **Verified against `crates/ocean-daemon/src/main.rs:3453` (`should_emit_agent_event`)
     on 2026-06-06.** The live SSE filter matches this contract exactly:
     `(Some(want), Some(sid)) => sid == want` (a scoped subscriber sees only its
     own session), `(Some(_), None) => false` (a council-wide `scope = None`
     event is never delivered to a scoped subscriber), and `(None, _) => all`
     (any session-bearing or council-wide event requires the explicit `?all=1`
     firehose opt-in). `Extension` scope routing is implemented by
     `AgentTurnEvent::session_id()` returning `*scope` for `Extension` events
     (`crates/ocean-agent-sdk/src/lib.rs:487`).
6. The global `/v1/agent/events` stream (no `session_id`, opt in with `?all=1`)
   is for debug, the Longhouse deck, and legacy clients only. Session-bearing
   events and council-wide extension events are delivered there only when
   `?all=1` is set.
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

`POST /v1/agent/sessions` is **live** in the daemon today. It explicitly
allocates and persists a session before the first turn instead of relying on the
implicit create-on-turn path. Shape:

```text
POST /v1/agent/sessions
  { "workspace_root": "<path>",       # required; resolved to git toplevel if inside a repo
    "project_id": "<uuid>",           # optional; falls back to the project's workspace_root
    "client_type": "surface-gpui" }   # optional render/communication medium

-> { "session_id": "<id>",
     "cwd": "<resolved working dir>",
     "client_type": "surface-gpui" }
```

The returned `session_id` is then carried on `GET /v1/agent/events?session_id=<id>`
and on every `POST /v1/agent/turns`. The session owns its `cwd`; turns inherit it
and do not need to resend a workspace root.

Two surfaces intentionally sharing one session both subscribe to the same `session_id`.
Two surfaces on different sessions cannot receive each other's session-bearing events.

### Turn request shape

The full `AgentTurnRequest` accepted by `POST /v1/agent/turns` (source of truth:
`crates/ocean-agent-sdk/src/lib.rs`, `struct AgentTurnRequest`):

```text
POST /v1/agent/turns
  { "prompt": "<operator instruction>",   # required
    "cwd": "<working directory>",         # required (may be empty when project_id is set)
    "session_id": "<id>",                 # optional; a new session is created if omitted
    "client_type": "surface-gpui",        # optional render/communication medium
    "project_id": "<uuid>",               # optional; with empty cwd the daemon binds the
                                          #   turn to the project's workspace_root
    "guidance": ["focus on tests"],       # optional list of guidance hints
    "thinking_level": "high",             # optional ThinkingLevel; per-turn reasoning-effort
                                          #   override, applied to this turn only (does not
                                          #   mutate the runtime's global thinking_level)
    "model_id": "claude-opus-4-7" }       # optional; per-turn / per-session model override
                                          #   (OCEAN-36). Drives this turn only, leaving the
                                          #   runtime's global model selection untouched, so
                                          #   independent client windows can each pin a model
                                          #   without racing through POST /v1/model.
```

Every field except `prompt` and `cwd` is optional. `thinking_level` and
`model_id` are per-turn overrides: they never mutate global runtime state, which
is what lets two ACP/surface windows run different models or reasoning efforts
against the same daemon concurrently. (`model_id` is OCEAN-36; source of truth
`crates/ocean-agent-sdk/src/lib.rs`, `AgentTurnRequest::model_id`.)

### Events of note

Beyond turn lifecycle deltas (`TurnStarted`, `AssistantTextDelta`,
`ToolCall*`, `TurnFinished`), two event families have surface-facing contracts:

- **`BrowserActivity { session_id, active }`** — emitted so the Chrome
  extension side panel can auto-focus while Ocean drives the browser and release
  afterward. Per OCEAN-77 the emission rule is contract-honest: every browser
  tool that performs a **live browser action** (a CDP round-trip to the running
  Chrome) emits `BrowserActivity { active: true }`. This includes the
  read-only-but-live tools `browser_list_tabs` (enumerates tabs over CDP) and
  `browser_response_body` (issues `Network.getResponseBody`). Two tools are
  **exempt** because they read a purely in-memory buffer with no CDP round-trip
  (the live action that populated the buffer already flagged activity):
  `browser_captured_requests` (netcap snapshot) and `browser_downloads`
  (download-tracking snapshot). Source of truth:
  `crates/ocean-runtime/src/tools/browser/mod.rs` (module-level contract comment
  + `active_result`). `BrowserActivity` is session-bearing and obeys Invariant 5.
- **`Extension { extension, payload, scope }`** — the catch-all for council /
  extension events; its `scope`-based delivery is the Invariant 5 exception
  documented above.

## Persistent Rooms (OCEAN-65)

The **persistent `Room`** is Ocean's durable collaboration entity. A `Room`
owns a free-form id (`RoomKey`), a name, a participant roster, created/updated
timestamps, an append-only transcript, and an optional trigger policy. Its
lifecycle is namespaced under `/v1/rooms/persistent`. Source of truth:
`crates/ocean-core/src/lib.rs` (`Room`, `RoomKey`, `RoomParticipant`,
`RoomMessage`, `RoomTriggerPolicy`, `RoomTriggerEvent`, `evaluate_trigger_policy`)
and `crates/ocean-agent` (`RoomRegistry` store, `RoomStoreError`).

Routes (all in `crates/ocean-daemon/src/main.rs`, typed `{ ok, error }` bodies,
400 on a bad key, 404 on an unknown room):

```text
POST /v1/rooms/persistent                                  # create a room
GET  /v1/rooms/persistent                                  # list rooms
GET  /v1/rooms/persistent/{key}                            # fetch one room
POST /v1/rooms/persistent/{key}/participants               # join (add participant)
DEL  /v1/rooms/persistent/{key}/participants/{participant_id}  # leave
POST /v1/rooms/persistent/{key}/messages                   # append a transcript entry
GET  /v1/rooms/persistent/{key}/transcript                 # read transcript (after_seq tail)
GET  /v1/rooms/persistent/{key}/snapshot                   # hydrate durable room state
GET  /v1/rooms/persistent/{key}/events                     # tail durable room events
POST /v1/rooms/{room_id}/livekit-token                     # mint voice/video join token
```

**Transcript** is a flat, append-only event log of `RoomMessage` entries, each
carrying author attribution (`author_id`, `author_kind`), a `kind`
(`Message` / `ParticipantJoined` / `ParticipantLeft` / `System`), a body, and a
store-assigned monotonic `seq` so clients can request `after_seq` tails.

**Trigger-policy evaluation** is the pure, I/O-free
`evaluate_trigger_policy(policy, event) -> TriggerDecision` in `ocean-core`. The
optional `RoomTriggerPolicy` gates each `RoomTriggerEvent` variant one-for-one:
`on_mention` ↔ `Mention`, `on_thread_reply` ↔ `ThreadReply`,
`on_component_event` ↔ `ComponentEvent`, `on_schedule` (cron) ↔ `Schedule`. An
absent policy (`None`) never convenes. A positive `TriggerDecision`
(`should_convene: true`, optional `target_participant`, human-readable `reason`)
is the seed for auto-convene; the daemon evaluates it at the transcript/
component-event wiring point.

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
