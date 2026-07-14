# Ocean Daemon Refactor: Mission, Progress, and Target

**Status:** Active, green, and shipping in bounded checkpoints
**Updated:** 2026-07-14
**Published implementation:** `36ca285`
**Scope:** `crates/ocean-daemon`

## Mission

Turn `ocean-daemon` from a large shared compilation unit into a composition-focused service that a human or cold agent can navigate, change, review, and verify without rediscovering the entire runtime.

The daemon remains the same product throughout this work. This is not a rewrite. HTTP/SSE contracts, route behavior, session ownership, caller cwd, permission gates, persistence, event ordering, and shutdown behavior stay stable while cohesive concerns move into private modules.

The practical north star is:

> A fresh contributor can identify one daemon concern, open its owning module, understand its invariants, make a bounded change, and run the correct focused test without reading twenty thousand lines of unrelated code.

## Target

The desired end state is a binary-only daemon whose `main.rs` is primarily composition:

- startup validation and dependency construction;
- `AppState` assembly until a separately approved state redesign exists;
- the canonical router and middleware seam;
- listener, shutdown, and top-level task orchestration;
- explicit imports from cohesive private modules.

The code-health plan prefers `main.rs` below 500 lines. That is a navigation target, not permission to hide coupling or force abstractions. Production modules should generally remain below 1,500 lines unless their cohesion and invariants justify otherwise.

No extraction may introduce a public daemon library, service-trait architecture, domain substates, route redesign, wire change, or opportunistic cleanup merely to make a move compile. Those are Phase 3 decisions.

## Non-negotiable invariants

- `GET /health` remains the liveness path.
- All 72 explicit method/path pairs remain synchronized with `GET /` discovery and the operator guide.
- Axum's default 404/405 behavior and global HTTP-tracing/CORS order remain stable.
- Turns execute in the caller's cwd; daemon process cwd is never the turn fallback.
- Runtime permission gates remain authoritative.
- Session persistence remains owned by `ocean-agent`.
- Agent SSE replay remains bounded by 2,048 events and 32 MiB, with full live delivery and visible lag signals.
- `TurnCheckpoint` remains internal and does not leak onto SSE.
- Persistent-room and LiveKit-token contracts remain; retired Track-0 projection routes stay retired.
- Every mechanical move has an extraction manifest, focused tests, full gates, independent review, and a rollback commit.

## Progress

### Foundation — complete

- Canonical workspace ownership and validation indexes
- Executable documentation and CI parity checks
- Rust 1.88 compatibility and supported-feature gates
- Event replay byte ceiling
- Shell descendant cancellation guarantees
- Bounded browser single-flight startup
- Agent-loop performance baseline and strict-lint inventory

### Daemon Phase 2C

| Checkpoint | State | Evidence |
|---|---|---|
| Route and middleware characterization | Complete | Reusable `app_router`; five full-router contract tests; exact 72-route source/banner/guide parity |
| Discovery drift correction | Complete | Four missing banner routes and thirteen missing operator-guide entries corrected separately from extraction |
| CORS policy leaf | Complete | Private `src/cors.rs`; policy and seven tests moved intact |
| Turn-metrics primitives | Complete | Private `src/metrics.rs`; counters, histogram renderer, in-flight guard, and four focused tests moved intact; HTTP handler stays in composition |
| Core↔SDK event adapters | Complete | Private `src/event_adapter.rs`; exhaustive legacy-mirror and SDK SSE-name adapters plus three focused tests; publication, provenance, runtime relay, filtering, replay, and framing stay in composition |
| Workspace/cwd policy | Complete | Private `src/workspace_policy.rs`; traversal, caller pass-through, scoped-read policy, and nine tests moved intact; startup guard, lookup, queries, HTTP mapping, runtime rebinding, room/call fallbacks, and persistence stay in composition |
| Catalog and settings | Next | Model routing/readiness and YOLO precedence remain unchanged |
| Projects and filesystem | Queued | Preserve HOME sandboxing, canonicalization, response shapes, limits, and project/session behavior |
| Canvas bridge | Queued, higher coupling | Store, runtime registry, SSE re-emit, TTL, cap, and GC must move together or remain together |
| Persistent rooms, Longhouse, calls, registries | Later domain waves | One reviewed domain at a time |
| Agent-turn/SSE orchestration | Last | Highest-risk authority path; moves only after leaf and domain boundaries are proven |

At this checkpoint, `main.rs` is approximately 19.6k lines, with CORS, metrics, pure event adapters, and ordinary turn/session-read workspace policy now independently owned. The route characterization and adapter characterization added substantial test coverage before and during line reduction, so raw line count alone understates progress. The important movement is ownership: behavior now has a checked boundary before it leaves the monolith.

## Course from here

1. Extract catalog/settings leaves without changing provider resolution or YOLO precedence.
2. Separate filesystem and project concerns only after their security and response contracts are fully indexed.
3. Move canvas and stateful domains only with explicit lifecycle/GC manifests.
4. Move turn/SSE orchestration last.
5. Request separate Phase 3 approval before splitting `AppState`, generating route metadata, creating a daemon library, or redesigning internal service boundaries.

## Completion standard for every wave

A wave is complete only when:

- its extraction manifest names exact symbols, dependencies, visibility, invariants, exclusions, rollback, and reviewer;
- focused tests pass;
- `cargo test -p ocean-daemon` passes;
- workspace test compilation passes;
- `livekit-tap` and `deepgram-stt` compile;
- route-contract tests remain green;
- formatting and documentation checks pass;
- an independent reviewer finds no unresolved medium-or-higher issue;
- the change is committed, synchronized with upstream, pushed, and—when runtime code changed—deployed from clean `main` with `/health` proving the revision.

## Canonical references

- [Code health and agent readiness plan](specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md)
- [Router-parity extraction manifest](specs/2026-07-14-ocean-daemon-router-parity-extraction-manifest.md)
- [CORS extraction manifest](specs/2026-07-14-ocean-daemon-cors-extraction-manifest.md)
- [Metrics extraction manifest](specs/2026-07-14-ocean-daemon-metrics-extraction-manifest.md)
- [Event-adapters extraction manifest](specs/2026-07-14-ocean-daemon-event-adapters-extraction-manifest.md)
- [Workspace-policy extraction manifest](specs/2026-07-14-ocean-daemon-workspace-policy-extraction-manifest.md)
- [Daemon local contract](../crates/ocean-daemon/AGENTS.md)
- [Runtime operator guide](OCEAN_RUNTIME_OPERATOR_GUIDE.md)
