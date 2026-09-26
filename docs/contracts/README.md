# Cross-repo wire contracts

Artifacts in this directory are published by `ocean-os` for sibling repos to
vendor. Each one is held equal to the code that serves it by a test in this
repo, so a change to the wire either updates the artifact in the same commit or
turns that test red.

| Artifact | Consumer | Held equal by |
|---|---|---|
| `room-wire.json` | ocean-surface (Rooms), the `ocean-mcp` room tools | `room_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `session-wire.json` | every first-party surface (sessions, agent events) | `session_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
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
| `ocean-tui` component projection | `component-wire.json` | every kind `component_lines` and the pinned-height table branch on is published, and both keep a fallback arm | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-tui/src/shell/components/chat.rs` |
| `ocean-acp` event bridge | `session-wire.json` | `event_to_update` names exactly the published event types, with no wildcard | `event_to_update_covers_the_published_session_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` component markdown | `component-wire.json` | every kind `render_component_markdown` special-cases is published, and a fallback arm renders the rest | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` session create | `session-wire.json` | the create body it sends, its `acp-zed` client type, and the response it decodes (it relies on `cwd`) | `session_create_is_inside_the_published_session_wire` in `crates/ocean-acp/src/daemon.rs` |
| `ocean-mcp` room tools | `room-wire.json` | the `/snapshot` keys `ocean_room_read` reads, the row fields and message kinds `render_row` reads, the participant kinds it sends and reads, and the list, post, inspect and resources keys the other room tools read | `room_literals_are_inside_the_published_room_wire` in `crates/ocean-mcp/src/bin/ocean_mcp.rs` |
| Ocean Buddy (Swift) | `voice-wire.json` | the client-secret route, body keys, `purpose` and error key; the response keys `BuddyRealtimeSecret` decodes; the tool names `fulfill` dispatches on (equal to the tools a conversation mint can hand it); the `note` argument `write_handoff` reads; the handoff route, body keys, role, kind and `ok` acknowledgement; the event ingress route, the event, state, attachment, response and card names it encodes and decodes, and its mock capture's mime type | the six `buddy_*` tests in `crates/ocean-daemon/tests/buddy_voice_contract.rs` |

The Rust workspace cannot compile Swift, so the Buddy tests read the Swift
source the way `voice_wire_contract_matches_the_daemon` reads the daemon's
handlers, and a literal the scan cannot find fails the test. `ocean-cli` uses
none of these contracts: its routes (`/v1/prompt`, `/v1/sessions`,
`/v1/events`, `/v1/permissions`, `/v1/extensions`) are not published by any of
them. ocean-surface still has to vendor the artifacts; #225 covers
`room-wire.json` there.

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
a resource, from the projections that build them (a fresh room has none). It is the daemon half of Rooms DoD 5.8; the surface
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
session with an unlisted value.

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

Other daemon routes the in-repo consumers still call are not published yet:
the TUI's turn, compact, permission, model, memory, LSP and Observatory
routes; `ocean-mcp`'s health, identity, session list and prompt routes and
its post and join request bodies; and `ocean-cli`'s routes listed above.
