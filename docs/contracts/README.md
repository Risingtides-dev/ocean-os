# Cross-repo wire contracts

Artifacts in this directory are published by `ocean-os` for sibling repos to
vendor. Each one is held equal to the code that serves it by a test in this
repo, so a change to the wire either updates the artifact in the same commit or
turns that test red.

| Artifact | Consumer | Held equal by |
|---|---|---|
| `room-wire.json` | ocean-surface (Rooms), the `ocean-mcp` room tools | `room_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `session-wire.json` | every first-party surface (sessions, agent events, turns, prompts, permissions, models and the other daemon-control routes) | `session_wire_contract_matches_the_daemon` and `session_wire_consumer_routes_match_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `observatory-wire.json` | the TUI's live Observatory graph | `observatory_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `component-wire.json` | surfaces that render agent components | `component_wire_contract_matches_the_runtime` in `crates/ocean-runtime/src/tools/component.rs` |
| `voice-wire.json` | ocean-surface (web and Tauri voice), Ocean Buddy (voice and event ingress), TUI dictation | `voice_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |

## In-repo consumers

The tests above hold each artifact equal to the daemon. The tests below hold
each in-repo consumer inside the artifact, so a consumer that drifts from the
published wire fails CI on its own side. A decoder must know every published
variant it claims to handle and must not rely on anything unpublished, so most
checks are subset checks; where a consumer dispatches on a closed set, the
check is equality.

| Consumer | Contract | What is pinned | Test |
|---|---|---|---|
| `ocean-tui` dictation | `voice-wire.json` | `POST /v1/voice/stt`, the `audio/wav` body type, the `text` response key and the `error` key | `stt_client_is_inside_the_published_voice_wire` in `crates/ocean-tui/src/shell/client.rs` |
| `ocean-tui` session stream | `session-wire.json` | its decoder knows every published event type; every event variant it branches on is published; its session-create body and decoded response stay inside the published keys | `session_client_is_inside_the_published_session_wire` in `crates/ocean-tui/src/shell/client.rs` |
| `ocean-tui` stream recovery, sync and config | `session-wire.json` | the `/v1/agent/events` route and its `error` reset frame, the `ocean.session_changed` extension, the `tui` client type, `/v1/sessions/{id}/sync` and the sync keys it decodes, the config routes, PATCH body, decoded keys, `error` key and busy answer | `session_stream_sync_and_config_are_inside_the_published_session_wire` in `crates/ocean-tui/src/shell/client.rs` |
| `ocean-tui` daemon routes | `session-wire.json` | the session-create, health, turn, compact, cancel, permission-decision, permission-settings, models, memory, LSP and `/v1/events` routes; the turn body and acknowledgement, its busy answer, and a deliberate branch for every published turn status; the `session_id` and `replay` stream query fields; the compact keys and the pre-commit refusal statuses it trusts; the cancel keys; the decision body and decision values; the settings bodies and modes; the model, memory and LSP keys and the `cwd` query; and on `/v1/events`, a decoder that knows every published event type, a published type for every event it branches on, and the envelope fields it decodes | `daemon_routes_are_inside_the_published_session_wire` in `crates/ocean-tui/src/shell/client.rs` |
| `ocean-tui` Observatory graph | `observatory-wire.json` | the snapshot and events routes, the `detail`, `after` and `scope` query fields and the `summary` value it asks for, the resume header, the Bearer scheme and the `401` it answers by dropping its environment token, the `reset` and `error` frames it rebaselines on, the typed `StreamGap` gap envelope it decodes and rebaselines on by name, the snapshot and envelope fields, and the payload kinds it branches on | `observatory_client_is_inside_the_published_observatory_wire` in `crates/ocean-tui/src/shell/client.rs` |
| `ocean-tui` component projection | `component-wire.json` | every kind `component_lines` and the pinned-height table branch on is published, and both keep a fallback arm | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-tui/src/shell/components/chat.rs` |
| `ocean-acp` event bridge | `session-wire.json` | `event_to_update` names exactly the published event types, with no wildcard | `event_to_update_covers_the_published_session_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` component markdown | `component-wire.json` | every kind `render_component_markdown` special-cases is published, and a fallback arm renders the rest | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` session create | `session-wire.json` | the create body it sends, its `acp-zed` client type, and the response it decodes (it relies on `cwd`) | `session_create_is_inside_the_published_session_wire` in `crates/ocean-acp/src/daemon.rs` |
| `ocean-acp` daemon routes | `session-wire.json` | the models, model-set, session detail, session list, turn, `/v1/events`, permission-decision and cancel routes; the `all` stream query and the `cwd` and `cursor` list query; the model-set, turn and decision bodies; the model, session detail, session list and turn keys it decodes; and on `/v1/events`, every published event type decodes, every event `main.rs` branches on is published, and the envelope fields stay inside the published ones | `daemon_routes_are_inside_the_published_session_wire` in `crates/ocean-acp/src/daemon.rs` |
| `ocean-mcp` room tools | `room-wire.json` | the room routes it calls, the `/snapshot` query it sends and the keys `ocean_room_read` reads, the row fields and message kinds `render_row` reads, the participant kinds it sends and reads, the post and join bodies it sends, and the list, post, inspect and resources keys the other room tools read | `room_literals_are_inside_the_published_room_wire` in `crates/ocean-mcp/src/bin/ocean_mcp.rs` |
| `ocean-mcp` daemon tools and `doctor` | `session-wire.json` | the health, identity, session list, prompt and agents routes, the prompt body it sends, and the health, identity, session list and prompt keys it reads | `session_literals_are_inside_the_published_session_wire` in `crates/ocean-mcp/src/bin/ocean_mcp.rs` |
| `ocean-cli` (outside `ocean extension`) | `session-wire.json` | the health, prompt, `/v1/events`, permission-decision and legacy session list and detail routes; the prompt and decision bodies; the `cwd` list query; the health, prompt and session keys it decodes; and on `/v1/events`, every event the permission bridge branches on and the envelope fields it decodes | `daemon_routes_are_inside_the_published_session_wire` in `crates/ocean-cli/src/main.rs` |
| Ocean Buddy (Swift) | `voice-wire.json` | the client-secret route, body keys, `purpose` and error key; the response keys `BuddyRealtimeSecret` decodes; the tool names `fulfill` dispatches on (equal to the tools a conversation mint can hand it); the `note` argument `write_handoff` reads; the handoff route, body keys, role, kind and `ok` acknowledgement; the event ingress route, the event, state, attachment, response and card names it encodes and decodes, and its mock capture's mime type | the six `buddy_*` tests in `crates/ocean-daemon/tests/buddy_voice_contract.rs` |

The Rust workspace cannot compile Swift, so the Buddy tests read the Swift
source the way `voice_wire_contract_matches_the_daemon` reads the daemon's
handlers, and a literal the scan cannot find fails the test. The TUI, ACP and
CLI tests read their wire types' field names through the same serde name probe
the daemon tests use, or from a written value where a `flatten` hides them.
ocean-surface still has to vendor the artifacts; #225 covers `room-wire.json`
there.

## What each artifact covers

`room-wire.json` covers what a Rooms client branches on: the `/events` SSE
event names, the access-state, message-kind and participant-kind vocabularies,
the top-level keys of `/snapshot` and `/transcript`, and the one "room not
open" answer (DoD 1.10). It also covers what a room client reads off the
other room routes: the transcript row (`RoomMessage`), participant, room and
access-projection fields from the serde derives; the keys of the room list,
of a local post (`201`) and a queued federated post (`202`), of `/inspect`
and its `execution` block, and of `/resources`, from real answers; and the
keys of an inspect agent entry, its `execution` block, a credential slot and
a resource, from the projections that build them (a fresh room has none). It
also covers the routes a room client calls for those facts (`routes`, each held
to its registered handler), the `/snapshot` paging query, and the bodies a
client sends to post a message and to join, from the types those handlers
extract. The post body type refuses unknown fields, so a client that sends one
more field is refused. It is the daemon half of Rooms DoD 5.8; the surface
half vendors the file and checks its decoders against it. Both repositories
are public, so a consumer's CI can fetch the current artifact directly without
a cross-repo credential.

`session-wire.json` covers the session half: the `type` tag and the full set
of agent-event type names on `/v1/agent/events`, and the request fields and
response keys of `POST /v1/agent/sessions`. Event names are derived from the
`AgentTurnEvent` source, and its tagging attribute is pinned so that
derivation stays valid. It also covers the stream's one literal frame name,
`event: error`, which carries an `AgentReplayGap` (its fields and codes) and
tells a client to reset and resync; the `ocean.session_changed` extension, a
scoped `extension` event with an empty payload that tells a synchronized
client to call `/sync`; `GET /v1/sessions/{id}/sync` (statuses, response,
snapshot and fence fields); and the session config routes (statuses, the
`model`-only PATCH body, response keys, `error` key, and the `409` busy
answer, whose text the TUI matches to wait and retry). `client_type` is an
open set: the daemon accepts and echoes any string, and the published `known`
values are the ones `HarnessProfile::from_client_type` maps (anything else
runs with the CLI profile). The test proves both by source and by creating a
session with an unlisted value. Every live Ocean Surface host is in `known`:
the browser/PWA sends `surface-web`, the Chrome extension `surface-extension`,
the Tauri desktop `surface-tauri` (all three the Web profile), and voice turns
from any of them `leo-voice` (Voice).
The create body's optional `title` is a
display-title hint. The daemon adopts it the way it adopts a first-turn title:
whitespace squashed, truncated to the switcher length, a blank hint ignored,
and the first title written wins, so the first turn does not relabel it. The
create response does not echo it; read it back from the session list or
detail `title`.

`session-wire.json` also covers every other daemon route an in-repo consumer
calls: `POST /v1/agent/sessions` itself (`session_create_route`), `GET
/health`, `GET /v1/identity` (and its `source` values), `POST /v1/agent/turns`
(its statuses, request and acknowledgement fields, and the `409` busy text the
TUI matches), the `/v1/agent/events` query fields, `GET /v1/agent/sessions`
and `GET /v1/agent/sessions/{id}`, the legacy `GET /v1/sessions` and `GET
/v1/sessions/{id}` that `ocean-cli` reads, `POST /v1/sessions/{id}/compact`
(whose `sync` and `fence` are the published `session_sync` snapshot and fence
shapes), `POST /v1/requests/{id}/cancel`, `POST /v1/permissions/{id}/decision`
(its body and decision values), `GET`/`POST /v1/settings/permissions` (bodies
and modes), `GET /v1/models` and `POST /v1/model`, `GET /v1/memory`, `GET
/v1/lsp`, `GET /v1/agents`, the legacy `GET /v1/events` stream (its `type` tag,
event types and envelope fields) and `POST /v1/prompt`. Each section publishes
the route, the statuses its handler can answer, the request and query fields,
and the full response shape, so a consumer's subset check has the whole shape
to check against. `session_wire_consumer_routes_match_the_daemon` rebuilds
every section from the code and compares it whole: routes and handlers from
the router, statuses from the handler's `StatusCode::*` (a handler that answers
a bare `Json` is `200`), fields from the serde derives through the name probe
(or a fully populated value where a `flatten` or an input `alias` would hide
or add names), variant names of tagged enums from the error their derive gives
an unknown tag, literal-built answers from the handler's own `json!` keys, and
the rest from real answers. It also checks that each handler extracts and
answers the type the section is derived from.

`observatory-wire.json` covers the Observatory read API. It is a contract of its
own because the API is a surface of its own: it has its own auth (an observer token, sent by the TUI as
`Bearer`), its own error body (`error`, `message`, `http_status`), its own SSE
frames and cursor resume, and wire types from the `ocean-observatory` crate
under the Gate 1 manifest, so folding it into `session-wire.json` would mix
two authorities. It covers `GET /v1/observatory/snapshot` (statuses including
the auth `401` and the store's `503`, the `at` and `detail` query fields, the
accepted `detail` values, and the `ObservatorySnapshot` fields) and `GET
/v1/observatory/events` (statuses, the `after` and `scope` query fields, the
accepted `scope` values, the `last-event-id` resume header, the `error`,
`message` and `reset` frame names, the envelope fields, `EventKind` names, the
adjacently tagged `EventPayload` tag and variant names, and `gap_kind` /
`gap_payload_kind`, the kind and payload variant of the gap signal the tail
sends when the durable log skips). That gap signal is an ordinary
`EventEnvelope` on the `message` frame (manifest §2.1 Property 4 and §7.2):
`kind` is `stream_gap`, the payload is `StreamGap { from_cursor, to_cursor,
reason }`, `truth` is `derived` (published as `gap_truth`), the topology ids
are empty, `cursor` is the first missing cursor, and there is no SSE `id:`.
The gap is never written to the durable log, so its `recorded_at` (like
`occurred_at`) is the time the tail built the frame, not the manifest §1.2
durable-write time. A consumer must not treat a gap envelope as a durable
record: it does not appear in `/replay`, and its cursor and `event_id` are
not stored. The daemon test builds it
from the daemon's own `gap_envelope` and proves it decodes as an envelope. The
TUI test proves the TUI decodes it and rebaselines on it by name. Daemons
before this change sent an untyped `"kind":"stream.gap"` object instead; the
TUI still rebaselines on any data frame it cannot decode, as a backstop. The
payload variant names are PascalCase on the wire because `EventPayload` has
no `rename_all`; the contract publishes them as they are.

`component-wire.json` lists the component kinds the runtime's component tools
accept, in `VALID_KINDS` order, and its test also requires a section per kind
in `docs/AGENT_RENDER_PROTOCOL.md`, so the protocol doc cannot fall behind the
code again. It was one kind short (`dashboard`) when this landed.

`voice-wire.json` covers the voice routes: `POST /v1/agent/voice`, the
realtime client-secret mint, `/v1/voice/stt` and `/v1/voice/tts`, and the
`POST /v1/agent/sessions/{id}/messages` handoff append that the realtime
agent's `write_handoff` tool uses. For each route it pins the request fields,
the response keys, and the status codes that route's own handler can answer.
Error bodies carry `error`. It also pins the realtime `purpose` values, the
default model, the tool names a surface dispatches on in each mint mode, the
WAV content types STT accepts, the default TTS voice, the `leo-voice`
client type, the handoff `kind` values, and the arguments each realtime tool
declares (`write_handoff` takes `note`; `render_component` declares none and
takes any object, so it has no entry). It also covers Ocean Buddy's event
ingress, `POST /v1/ocean-buddy/events`: its statuses, the `BuddyEvent`,
state, attachment, response and card names, and the one state
(`attached`), target and mime type the mock slice accepts, plus the keys of
its `422` rejection. That route is not a voice route, but its only consumer
is Buddy, which already takes its voice and handoff wire from this artifact;
one mock route does not warrant a contract of its own. It is published as the
narrow first slice it is: a real capture is refused today. Every value is derived from the
code. Routes come from the `app_router` registrations. Field and variant names
come from the serde derives, through a probe deserializer, so a `rename`
changes them. Response keys and tool names come from the pure builders the
handlers call, and the rest comes from the handler source. The test also
checks every key of the artifact, so the file cannot hold a fact that nothing
checks. Voice routes answer a prose `error` string rather than a machine
`code`, so no error codes are pinned. `POST /v1/agent/voice` shares its
response with `POST /v1/agent/turns` and hands every non-rejection outcome to
it. The LiveKit call tap has no route of its own. It runs inside the daemon
under the `livekit-tap` feature. `POST /v1/rooms/{room_id}/livekit-token` and
`/v1/calls/*` are room and call media routes, so this artifact does not cover
them.

These additions kept every artifact at `version: 1`. Nothing here asks for a
bump on an additive change, and every consumer reads by key.

## Fields Surface sends that the daemon drops

`AgentTurnRequest` does not deny unknown fields, so a field a client sends
and the daemon does not declare is dropped without an error. One such field
is open:

- Turn `canvas`. When the operator's canvas holds at least one placed
  component, Surface (`dispatch_prompt` in
  `ocean-surface-ui/src/daemon.rs`, since ocean-surface a9a6acd, 2026-07-11)
  adds `canvas` to `POST /v1/agent/turns`: a `CanvasContext` with an optional
  `active_canvas_id` and one entry per non-empty canvas (`canvas_id`, up to
  128 `components` each with `id`, `kind`, optional `title`/`body`/`value`/
  `status` text capped at 280 characters, and a `rect` `[x, y, w, h]` in canvas
  units; `edges` with `id`, `from`, `to`, `kind`, optional `label`; and an
  `omitted_components` count). The field has never been declared on the daemon
  side: no ocean-os commit has added it and no spec defines how the daemon
  consumes it. Surface's own doc comment says that consumption "lives with the
  ocean-os team (inject as turn context, never as the prompt itself)", and
  `docs/OCEAN_CANVAS_CONVERGENT_MERGE.md` assumes a next-turn canvas context.
  Today the snapshot is dropped and the model never sees the canvas. This is
  an operator decision, not a wire repair. Either the daemon accepts `canvas`
  (a typed, bounded field on `AgentTurnRequest`, sanitized the way
  `client_context.browser` is, rendered into turn context separately from the
  prompt and kept out of the persisted display title, then published in
  `agent_turn.request_fields`), or Surface stops sending it until that exists.
  Until one of those lands, Surface's pin lists it as known-unpublished.

## Not published, on purpose

- Error prose. Consumers key on status codes and the published error key.
  The one exception is the session-config busy text, which the TUI matches,
  so it is published as `session_config.busy`.
- The inner shape of `permission_mode` in the session config answer. The
  daemon reports global permission state there for forward compatibility and
  the route is read-only for it in v1, so only the key is published.
- The rows of a sync `snapshot.transcript`. The TUI decodes them with the
  shared `ocean_core` type, so in-repo drift is a compile error; publish them
  when a sibling repo decodes them.
- The inner fields of the legacy `GET /v1/sessions/{id}` `session` object
  (`SessionDetail`). `ocean-cli` decodes it with the shared `ocean_core` type
  and prints it whole, for the same reason as the transcript rows.
- The SSE frame names on the legacy `/v1/events` stream. They come from an
  internal naming helper, not the serde tag, and no consumer reads them: all
  three decode the `data:` envelope and ignore the frame name.
- The Observatory error codes (`invalid_cursor`, `cursor_expired` and the
  rest), and `GET /v1/observatory/replay`. The TUI keys on status and frame
  name and never reads a code, and no in-repo client calls replay; publish
  them when ocean-surface's Observatory reducer vendors this file.
- The inner shapes of the Observatory snapshot's nodes, edges and attention
  items, the envelope's topology, correlation and producer objects, and each
  payload's data fields. The TUI decodes them with the shared
  `ocean_observatory` types.
- `ocean-cli`'s `ocean extension` routes (`/v1/extensions/*`). Their contract
  is the Stage A implementation manifest's §15 surface: the pre-commit and
  committed envelopes, the `202` and exit-code rules, and the operator
  credential. The A3b route tests and the A5 end-to-end gate hold the CLI and
  the routes to it. Stage A is not accepted and five operator rulings are
  still open, one of them the mutation credential class. A copy here would be
  a second authority over a surface that can still change under those
  rulings. Publish them when Stage A is accepted.

Every other daemon route an in-repo consumer calls is now published, and the
consumer tests in the table above hold each call site inside its contract.
