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
