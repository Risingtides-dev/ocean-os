# Ocean Daemon Core↔SDK Event Adapters Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; focused/full/feature validation and independent review passed
**Owner:** Ocean OS
**Rollback point:** `af32dbb`

## Purpose

Move the daemon's two pure `AgentTurnEvent` adapter helpers out of `src/main.rs` into one private binary module. Keep all event-bus publication, envelope provenance/session stamping, replay, filtering, serialization, SSE framing, and runtime→SDK relay orchestration in `main.rs`.

The router-parity foundation, CORS leaf, and turn-metrics leaf are complete at the rollback point.

## Exact symbols to move intact

From `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/event_adapter.rs`:

- `agent_to_ocean_event`
- `agent_event_type_name`

Add focused characterization tests beside the moved helpers:

- `legacy_mirror_preserves_supported_payloads`
- `legacy_mirror_filters_agent_only_events`
- `agent_event_type_names_match_sdk_wire_tags`

The tests add no production behavior. `emit_agent`, `agent_events`, `agent_replay_frames`, `should_emit_agent_event`, the runtime `AgentEvent` bridge, and its exhaustive relay-classification test remain in `main.rs` because they publish, route, filter, frame, or orchestrate events rather than purely convert them.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/event_adapter.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new production module depends only on:

- `ocean_agent_sdk::{AgentTurnEvent, AgentTurnStatus}`
- `ocean_core::OceanEvent`

Both moved functions are `pub(super)` because daemon composition calls them. No symbol becomes public outside the binary crate.

Inbound callers remain:

- `emit_agent` calls `agent_to_ocean_event` before composition stamps the legacy `EventEnvelope` with the core session id and `origin: "agent"` and publishes it.
- live and replay agent-SSE framing call `agent_event_type_name` before composition serializes the unchanged SDK event payload.
- retained inline SSE tests may call `agent_event_type_name` while exercising composition behavior.

## Frozen invariants

- The full-fidelity `AgentTurnEvent` is always emitted on `AgentEventBus` before any optional legacy mirror.
- Only assistant deltas, tool start/chunk/finish, turn finish, and session creation mirror to `OceanEvent`; all other SDK variants remain agent-rail-only.
- Legacy tool chunks/finishes retain the placeholder tool name `"tool"` and chunk `is_error: false` behavior.
- Legacy turn completion remains `ok` only for `AgentTurnStatus::Completed`; missing `wall_ms` remains `0`.
- SDK variant-to-SSE event names remain byte-identical to their current snake_case wire tags.
- Legacy envelope `session_id`, `origin`, ordering, and publication stay owned by `emit_agent` in `main.rs`.
- Live/replay filtering, serialization fallback, event ids, keepalive, lag signals, and replay bounds remain unchanged.
- The runtime `AgentEvent`→SDK bridge remains in the turn orchestration path and continues to filter `TurnCheckpoint` and the other documented lifecycle/message variants.
- Route, fallback, CORS, permissions, cwd, session persistence, event ordering, and shutdown behavior remain unchanged.

## Explicit exclusions

This move does not:

- move or redesign either event bus;
- move `emit_agent`, envelope construction, session/provenance stamping, SSE handlers, replay helpers, or runtime relay tasks;
- add, remove, rename, or reorder any wire event;
- change which SDK events mirror to the legacy core rail;
- replace the exhaustive matches with serde reflection, generated metadata, traits, or a registry;
- move session-id, transcript-to-turn, timestamp, tool-output, or runtime-metadata helpers;
- introduce a daemon library, public API, dependency, service trait, substate, or route change;
- clean up unrelated code.

Any semantic, protocol, or orchestration change stops this extraction and requires a separate design change.

## Validation

```bash
cargo test -p ocean-daemon event_adapter::tests:: -- --nocapture
cargo test -p ocean-daemon agent_event -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare both moved production bodies against rollback point `af32dbb`, verify minimal visibility and imports, confirm bus publication/envelope/SSE/runtime-relay code stayed in composition, and confirm focused/full/feature gates pass.

## Result

A private `event_adapter.rs` now owns only the two pure SDK-event conversion/name functions and three focused characterization tests. The production match bodies are unchanged from rollback point `af32dbb` except for minimal `pub(super)` visibility. Composition retains all stateful publication, provenance, filtering, replay, framing, and runtime-relay behavior. Focused, router, full daemon, workspace-test compilation, both supported daemon feature checks, formatting, documentation, and diff checks passed; independent review found no unresolved medium-or-higher issue.

## Rollback

Revert the bounded event-adapter extraction commit. There is no data migration, wire-version handling, or compatibility cleanup.
