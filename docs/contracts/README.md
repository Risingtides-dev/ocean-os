# Cross-repo wire contracts

Artifacts in this directory are published by `ocean-os` for sibling repos to
vendor. Each one is held equal to the code that serves it by a test in this
repo, so a change to the wire either updates the artifact in the same commit or
turns that test red.

| Artifact | Consumer | Held equal by |
|---|---|---|
| `room-wire.json` | ocean-surface (Rooms) | `room_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `session-wire.json` | every first-party surface (sessions, agent events) | `session_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `component-wire.json` | surfaces that render agent components | `component_wire_contract_matches_the_runtime` in `crates/ocean-runtime/src/tools/component.rs` |
| `voice-wire.json` | ocean-surface (web and Tauri voice), Ocean Buddy, TUI dictation | `voice_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |

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
| `ocean-tui` component projection | `component-wire.json` | every kind `component_lines` and the pinned-height table branch on is published, and both keep a fallback arm | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-tui/src/shell/components/chat.rs` |
| `ocean-acp` event bridge | `session-wire.json` | `event_to_update` names exactly the published event types, with no wildcard | `event_to_update_covers_the_published_session_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` component markdown | `component-wire.json` | every kind `render_component_markdown` special-cases is published, and a fallback arm renders the rest | `component_kinds_are_inside_the_published_component_wire` in `crates/ocean-acp/src/convert.rs` |
| `ocean-acp` session create | `session-wire.json` | the create body it sends and the response it decodes (it relies on `cwd`) | `session_create_is_inside_the_published_session_wire` in `crates/ocean-acp/src/daemon.rs` |
| `ocean-mcp` room tools | `room-wire.json` | the `/snapshot` keys `ocean_room_read` reads, the message kinds `render_row` branches on, and the participant kinds it sends and reads | `room_literals_are_inside_the_published_room_wire` in `crates/ocean-mcp/src/bin/ocean_mcp.rs` |
| Ocean Buddy (Swift) | `voice-wire.json` | the client-secret route, body keys, `purpose` and error key; the response keys `BuddyRealtimeSecret` decodes; the tool names `fulfill` dispatches on (equal to the tools a conversation mint can hand it); the handoff route, body keys, role, kind and `ok` acknowledgement | the four `buddy_*` tests in `crates/ocean-daemon/tests/buddy_voice_contract.rs` |

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
open" answer (DoD 1.10). It is the daemon half of Rooms DoD 5.8; the surface
half vendors the file and checks its decoders against it. Both repositories
are public, so a consumer's CI can fetch the current artifact directly without
a cross-repo credential.

`session-wire.json` covers the session half: the `type` tag and the full set
of agent-event type names on `/v1/agent/events`, and the request fields and
response keys of `POST /v1/agent/sessions`. Event names are derived from the
`AgentTurnEvent` source, and its tagging attribute is pinned so that
derivation stays valid.

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
client type, and the handoff `kind` values. Every value is derived from the
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
