# Cross-repo wire contracts

Artifacts in this directory are published by `ocean-os` for sibling repos to
vendor. Each one is held equal to the code that serves it by a test in this
repo, so a change to the wire either updates the artifact in the same commit or
turns that test red.

| Artifact | Consumer | Held equal by |
|---|---|---|
| `room-wire.json` | ocean-surface (Rooms) | `room_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |
| `session-wire.json` | every first-party surface (sessions, agent events) | `session_wire_contract_matches_the_daemon` in `crates/ocean-daemon/src/main.rs` |

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
