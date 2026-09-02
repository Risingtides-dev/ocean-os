# Ocean Ecosystem Contract

Status: active contract for runtime and first-party surfaces.

## Terms

- `Project`: repo/product identity. A project is the durable named thing Ocean recognizes, configures, and caretakes.
- `Workspace`: one concrete checkout/worktree/local directory on disk. A workspace has a path and may belong to a project.
- `Session`: one daemon-owned agent/human work thread with a persisted workspace binding that an explicit later request may rebind under current workspace-resolution rules.
- `Surface`: a UI/client attached to a session, such as the Tauri desktop app, Chrome extension, web/PWA, TUI, ACP, CLI, or voice.
- `Canvas`: a tldraw/CRDT document visible from a surface. Concurrent
  operator+agent edits converge via the per-component version-vector merge
  (OCEAN-258); see `docs/OCEAN_CANVAS_CONVERGENT_MERGE.md`.
- `LiveKit room`: the real-time audio/video/data collaboration container.
- `Longhouse`: the reasoning/federation plane.

## Invariants

1. A product surface chooses or creates a session before it submits a turn.
2. A session stores its workspace root, but every turn request still carries the required `cwd` field. A caller may send an explicit cwd, or an empty cwd together with `project_id`; an empty cwd without a project is rejected. A valid new binding may intentionally rebind and persist the resumed session.
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
POST /v1/agent/turns { session_id, prompt, cwd, project_id?, client_type }
```

`POST /v1/agent/sessions` is **live** in the daemon today. It explicitly
allocates and persists a session before the first turn instead of relying on the
implicit create-on-turn path. Shape:

```text
POST /v1/agent/sessions
  { "workspace_root": "<path>",       # required; resolved to git toplevel if inside a repo
    "project_id": "<uuid>",           # optional; falls back to the project's workspace_root
    "client_type": "surface-tauri" }  # optional render/communication medium

-> { "session_id": "<id>",
     "cwd": "<resolved working dir>",
     "client_type": "surface-tauri" }
```

The returned `session_id` is then carried on `GET /v1/agent/events?session_id=<id>` and on every `POST /v1/agent/turns`. Resuming does not make `cwd` optional: each turn must provide it, or provide an empty value with `project_id` so the daemon can resolve the project workspace. A different valid binding intentionally rebinds the persisted session.

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
    "client_type": "surface-tauri",       # optional render/communication medium
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
and `crates/ocean-store` (`RoomStore` trait for lifecycle, inherent access-projection and outbox APIs, `RoomStoreError`).

Routes (all in `crates/ocean-daemon/src/main.rs`; JSON routes use typed `{ ok, error }` bodies with 400 on a bad key and 404 on an unknown room; SSE is streaming; retry returns strict status codes):

```text
POST /v1/rooms/persistent                                  # create a room
GET  /v1/rooms/persistent                                  # list rooms
GET  /v1/rooms/persistent/{key}                            # fetch one room (includes access projection)
POST /v1/rooms/persistent/{key}/participants               # join (add participant)
DEL  /v1/rooms/persistent/{key}/participants/{participant_id}  # leave
POST /v1/rooms/persistent/{key}/messages                   # append a transcript entry
GET  /v1/rooms/persistent/{key}/transcript                 # read transcript (after_seq tail)
GET  /v1/rooms/persistent/{key}/snapshot                   # hydrate durable room state (includes access + closed + agent_owners; before_seq for the tail)
GET  /v1/rooms/persistent/{key}/events                     # merged SSE tail (access + messages)
POST /v1/rooms/persistent/{key}/outbox/retry               # retry a failed outbox item
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
`on_component_event` ↔ `ComponentEvent`, `on_schedule` (cron) ↔ `Schedule`,
`on_build_failure` ↔ `BuildFailed`, and `on_ci_failure` ↔ `CiFailure`. The two
workspace flags are independent rather than one widened flag. Only build
failure has an accepted core dispatch path: `ci_checked` is a transcript marker
and CI dispatch remains extension-owned. `ComponentEvent`, `Schedule`, and
`CiFailure` therefore have no core source that can convene an agent, so room
write routes refuse values that would turn those three on rather than store
configuration that silently never acts. An absent policy (`None`) never
convenes. A positive `TriggerDecision`
(`should_convene: true`, optional `target_participant`, human-readable `reason`)
is the seed for auto-convene; the daemon evaluates it at the transcript/
component-event wiring point.

**Access projection** is a typed, store-owned `RoomAccessProjection` (a struct, not a tagged variant) returned on room detail (`GET /v1/rooms/persistent/{key}`) and snapshot (`GET /v1/rooms/persistent/{key}/snapshot`). Access states are exact `local`, `connecting`, `live`, `recovering`, and `revoked`. `members` and `outbox` are skip-when-empty struct fields, not variant-confined fields. `self_member_id` is a skip-when-empty room-level field naming the daemon's own authenticated member id, derived at read time from the private credential row; absent on local rooms and pre-field daemons. Outbox stays separate and never enters the confirmed transcript before Bedrock confirmation. Rooms without an access row (including the frozen soft-closed fixture) default exact `local`.

**Transcript window** on the snapshot is one bounded page, and `before_seq` chooses which END of the log it comes from. Without it the page runs forward from the start (`after_seq` optional, `next_seq` the cursor to replay) — which for a room with 12,000 messages means hydration opens at message #1 and the tail is reachable only by transferring the whole log. With it the page is the newest `limit` rows whose `seq` is strictly less than the cursor, still ascending so no renderer changes, and `prev_seq` is the OLDEST row returned, replayed as the next `before_seq` to page further back. `has_more` always means "more rows exist in the direction this page was paging" — newer for a forward read, older for a backward one — and `last_seq` stays the newest row on the page, which is what a tail-opened client replays as `after_seq` on `/events`. A `before_seq` above every stored `seq` is how a client opens at the tail before it knows the last seq; `before_seq=0` is a terminal empty page, since nothing precedes the first message. `after_seq` and `before_seq` together are a typed 400 (`conflicting_transcript_cursors`) rather than a precedence rule. Backward paging is `/snapshot` only — `/transcript` stays forward-only — and it answers identically for a soft-closed room AT ANY LENGTH, so a frozen call room and a live one hydrate to the same screen. That parity is a property of where the closed read gets its rows: it pages the stored transcript, not the frozen record's own copy, which is bounded to the oldest 1000 and would answer the newest page of the first thousand while calling it the tail — with `prev_seq`, `has_more` and `last_seq` all looking correct beside it. The parameter and `prev_seq` are additive; a pre-field daemon ignores the first and omits the second.

**Closed marker** is a plain `closed` boolean on the snapshot body only (`GET /v1/rooms/persistent/{key}/snapshot`), deliberately not a field on `Room` — that struct is serialized by room create, detail, PATCH, and list as well as the federation payloads, so a field there would be a wire change across all of them to serve one route. It is true exactly when the open-room read missed and the soft-closed audit view answered — the room is frozen and replayable, not live — and it is derived from that same read, so it cannot disagree with the transcript beside it. `GET /v1/rooms/persistent/{key}` still 404s on a closed room and the `/events` SSE tail still refuses one, so a client that hydrates through `/snapshot` has no other signal: on `closed: true` it must present an audit view rather than open a tail nothing will ever feed and a composer whose every send 404s. The field is additive and absent from pre-field daemons, where a missing key reads as open.

**Agent ownership** is an `agent_owners` array on BOTH room detail (`GET /v1/rooms/persistent/{key}`) and the snapshot (`GET /v1/rooms/persistent/{key}/snapshot`), one shape from one projection: `[{ "agent_id": "researcher", "owner_id": "alice", "owner_present": true }]`, ordered by roster position and empty for a room with no owned agents. It is the local half of "a worker persists alongside their agents" — which WORKER owns which Agent participant, recorded when the agent joins with an `owner_id` naming a Human already on the roster. `owner_present` is a separate field rather than a filter because the binding OUTLIVES the worker: anyone may remove a participant, so an owner can leave while the ownership really did happen and the agent really is unclaimed now — dropping the row would deny the first, and reporting the row alone would assert a live claim the room cannot prove. It rides the snapshot for the same reason `closed` does: hydration goes through that route, so a field only room detail serves is a field no client can reach. A soft-closed room reports it unchanged: closing retains the roster and the ownership rows, and the snapshot IS the audit view, so a frozen room still says who owned what and whether they were still present when it froze — the one place the two routes diverge, because room detail 404s on a closed room. Additive and absent from pre-field daemons, where a missing key reads as no recorded ownership.

**Pending outbox** is durable per-room storage of locally-authored federated events awaiting Bedrock confirmation. `POST /v1/rooms/persistent/{key}/outbox/retry` accepts `{ "client_event_id": "<id>" }` and returns 202 on successful durable requeue, 403 revoked, 404 for an unknown room or item, 409 pending/local, 400 for malformed/non-object body, or sanitized store 500 on internal error. Outbox durability lives in `ocean-store`; the retry route still owns only HTTP validation, store lookup, and wake publication — it performs no network call itself.

**Federation bridge (S2 P2-B/P2-C).** `ocean-daemon::room_federation` owns one AppState-held supervisor with one cancellable task tree per credentialed room. It subscribes to Bedrock's header-bearer room SSE from the store cursor, commits a safe roster before first ingest, maps authenticated message rows through the store's atomic `ingest_confirmed_event`, and publishes transcript/access hints only after commit. A duplicate publishes nothing; non-message rows advance only the access cursor. The sender periodically scans the durable Pending outbox and posts exact one-at-a-time producer tuples; a `201` is only remote-persistence acknowledgement and leaves transcript/outbox unchanged until ordered SSE confirms the full tuple. Transport/429/5xx preserve Pending; row/schema conflicts fail that row; auth/scope failures revoke the room with Pending cleanup before Revoked persists. No store guard crosses network I/O. Missing/invalid `OCEAN_FEDERATION_URL` moves credentialed rooms to Recovering.

P2-C splits Local and federated intent authority at credential installation. Local messages retain immediate 201 append/trigger behavior; federated human posts ignore browser identity, and bound-agent replies use the opaque bound member id. Both allocate only a durable Pending outbox row, return/use 202, and await ordered SSE confirmation before transcript append or trigger claim. Mention ids resolve only against the current safe member projection. Confirmed claims require positive current User roster evidence and local mention policy; dispatch then revalidates a current safe locally-owned Agent roster member plus its private binding, while fallback-Human and Agent authors claim nothing. The claim journal commits before an unbounded nonblocking daemon dispatch channel, giving at-most-once local execution without P2-B-era backfill. Federated agent replies re-enter the outbox and local-only audit/failure rows are not appended.

The daemon-only control plane adds owner invite creation, idempotent `{code, redemption_id, token}` recovery plus bodyless self-join, and ordered safe-agent registration. `OCEAN_FEDERATION_OWNER_TOKEN` is used only to register a Local room and is never surface-submitted. Every pending redemption survives restart; existing rooms start first, then a concurrency-four worker attempts every listed triple once. Registration keys are domain-separated SHA-256 over stable instance/room/agent identity and cross only in the daemon-to-Bedrock agent batch alongside the allowlisted public description/model/skill-count/subagent-name fields. Bearers cross only in daemon-authenticated Bedrock requests. Neither bearer nor registration key enters a surface request, projection, transcript, log, or error; local paths/tools/provider credentials and permission posture never cross Bedrock.

**Merged SSE** (`GET /v1/rooms/persistent/{key}/events`) delivers both `room_access` and `room_message` frames on a single stream. The initial frame is a full `room_access` projection (no `event.id`) reflecting the committed access state; `room_message` replay follows via `Last-Event-ID` (numeric or 400, wins over `after_seq`). Post-commit, the tail sends access-update frames on a dedicated `RoomAccessWakeBus` and unchanged id-bearing message frames on `RoomWakeBus` — separate buses so heavy transcript traffic never back-pressures access subscribers. Both bus wake hints are payload-free; relevant and lagged hints reread from SQLite. Message gap recovery pages ascending; access dedup compares the full projection. Unknown/closed rooms return 404; `call:` rooms return the typed unsupported rejection.

## Implementation Anchors

Runtime side:

- `crates/ocean-agent-sdk/src/lib.rs`: `AgentSessionCreateRequest`,
  `AgentSessionCreateResponse`, `AgentTurnEvent::session_id`.
- `crates/ocean-agent/src/lib.rs`: session creation and workspace binding.
- `crates/ocean-daemon/src/main.rs`: `POST /v1/agent/sessions`, surface turn
  guard, and scoped SSE filtering.

Surface side:

- `../ocean-surface/crates/ocean-surface-ui/src/daemon.rs`: shared
  web/extension/Tauri daemon client, scoped event URL, and
  create-session-before-turn flow.
- `../ocean-surface/crates/ocean-tauri/src/lib.rs`: native shell composition
  around the shared Surface UI.
- `../ocean-surface/crates/ocean-surface-proxy/src/main.rs`: forwards
  `POST /v1/agent/sessions` and preserves `?session_id=...` on SSE.

## Legacy Notes

Older docs may discuss TUI rooms, tmux layouts, or room guidance. Treat those as
TUI/runtime reference material unless they explicitly point back to this
contract. Archived client implementation plans are not current architecture.
