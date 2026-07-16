# Ocean Daemon Refactor: Mission, Progress, and Target

**Status:** Active, green, and shipping in bounded checkpoints
**Updated:** 2026-07-16
**Published Phase 2C implementation:** `b21006b`
**Current manifest baseline:** `e8f3322`
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
- Caller-submitted HTTP turns execute in the caller's cwd; daemon process cwd is never their fallback. Internal auto-convene for a legacy persistent room with no workspace binding retains the existing neutral daemon cwd fallback until a separately approved workspace-binding migration; the startup guard still forbids repository cwd.
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
| Model catalog | Complete | Private `src/model_catalog.rs`; get/list/set adapters moved after four characterization tests; canonical routing, ordered readiness, credential discovery, and persistence stay owner-controlled; `/ready` and turn/domain model policy stay in composition |
| YOLO settings | Complete | Private `src/yolo_settings.rs`; env → persisted → safe-off precedence, inert wire flag, exact GET/POST shapes, persistence timing, permission authority, voice fail-fast, and shared test-lock order remain exact |
| Filesystem sandbox | Complete | Private `src/filesystem.rs`; canonical HOME containment, symlink-escape rejection, statuses, response envelopes, caps, binary sniffing, sorting, and git fields preserved |
| Project registry | Complete | Private `src/project_registry.rs`; runtime persistence/pagination/timestamps, session association, git/worktree enrichment, create-path semantics, and CRUD response contracts preserved |
| Slack Canvas host fulfillment | Complete | Private `src/slack_canvas_fulfillment.rs`; host ingress/query store, runtime lookup, fulfilled SSE re-emit, TTL/cap, and coupled GC moved together; external Slack transport remains extension-owned |
| Component interaction fulfillment | Complete | Private `src/component_interaction.rs`; exact HTTP validation, scoped runtime-registry lookup, remove-before-send delivery, and 200/400/404/410/500 envelopes frozen by five direct tests |
| Model roles | Complete | Private `src/model_roles.rs`; once-at-startup fail-open config loading plus exact turn/advisor precedence and verbatim string behavior frozen by focused tests |
| Request/permission control records | Published | Private `src/request_control.rs`; PR #286 merged as `ee3860a` after characterization, extraction, dedicated-target gates, fresh reviews, and hosted CI; policy, token verification, HTTP/events, GC scheduling, and shutdown drain stay in composition; live daemon supervision/deployment is owned by the concurrent operator workstream |
| Recall tally registry | Published | Private `src/recall_registry.rs`; first-threshold ownership, distinct-voter semantics, zero clamp, poison recovery, exact HTTP responses, persisted revocation, and successful-only cleanup were characterized; dedicated-target gates, two fresh reviews, hosted CI, and PR #287 merge `3e051c1` passed |
| Persistent rooms | Published | Private `src/persistent_rooms.rs`; exact HTTP envelopes/defaults, persisted-row/event/audit/spawn ordering, closed-room audit asymmetry/paging, poison recovery, static/dynamic route precedence, call/LiveKit consumers, cwd/session identity, and three-state permission behavior were characterized; dedicated-target gates, two fresh extraction reviews, hosted CI, and PR #293 merge `92e03bf` passed |
| Longhouse preparation adapters | Published | Private `src/longhouse_preparation.rs` owns only state-free prepare/inspect/workflow HTTP shells; exact extractor/method/envelope/privacy, PR #292 evidence, cwd confinement, and blocking/read-only behavior were characterized; the 334-line move, dedicated-target gates, compatibility/MSRV/local CI, two fresh extraction reviews, hosted CI, and PR #296 merge `29d65f8` passed; librarian fetch/spec remain separate after a disclosed symlink-retarget security disposition |
| Longhouse turn preparation/presentation | Published | Private `src/longhouse_turn_preparation.rs` owns only the fresh opt-out gate, cached read-only preparation under the existing blocking deadline, and deterministic advisory rendering/application; exact presentation/no-op, environment, cwd/cache/blocking/deadline/fail-open, helper-warning, module-authority, and all three call-site ordering contracts were characterized at `f6e8efe`; the exact 228-line move, dedicated-target gates, compatibility/MSRV/local CI, two fresh extraction reviews, hosted CI, and PR #299 merge `9095d5a` passed |
| Longhouse topic projection/demo | Extraction authorized | Fresh `5b9e23a` mapping applied the handoff stop rule and narrowed the private `src/longhouse_topics.rs` owner to the scripted demo plus topic list/detail adapters. Characterization `3aedaf1` (rebased from reviewed `c1be830`) freezes exact HTTP/event/projection/poison/shared-handle/task/authority contracts; all 372 daemon and 168 Longhouse tests plus one ignored host fixture and one doc test, daemon Clippy, focused groups, and two final independent reviews passed on the original baseline. Reconciliation onto `e8f3322` left all three bodies byte-identical and reran the focused characterization/convene/router/SSE/preparation matrix. Real convene remains composition-owned because it is directly coupled to ready-model filtering, asynchronous council orchestration, durable title grant/bind, and raw-token response delivery |
| Longhouse convene and title/control governance | Next security wave | Re-manifest the coupled convene/title boundary separately; claim/revoke/recall/breach/board authority must not be hidden behind a new trait, substate, or public API |
| Calls and remaining registries | Later domain waves | One separately manifested domain at a time after both governance waves |
| Agent-turn/SSE orchestration | Last | Highest-risk authority path; moves only after leaf and domain boundaries are proven |

At this checkpoint, `main.rs` is approximately 20.8k lines after persistent-room and both Longhouse characterizations deliberately expanded the parent test suite, while the 886-line room, 334-line Longhouse HTTP-preparation, and 228-line Longhouse turn-preparation implementation boundaries moved into private owners. CORS, metrics, pure event adapters, ordinary turn/session-read workspace policy, model catalog and roles, security-sensitive settings, the home-sandboxed filesystem surface, project-registry adapters, Slack Canvas host fulfillment, component-interaction fulfillment, bounded advisor execution, request/permission control records, recall tally storage, persistent-room lifecycle/orchestration, and Longhouse advisory preparation now have independent private owners. The 238-line request-control module owns storage mechanics and exact bounded transitions only. The 52-line recall-registry module owns memory-only tally construction, casting, poison recovery, and named removal only. The current 2,081-line persistent-room module owns the shared handle/lock adapters, durable-room HTTP lifecycle/paging, named-agent auto-convene path, and durable room SSE tail/disconnect cleanup; `AppState`, startup, router composition, call persistence/retries, and LiveKit authorization remain in `main.rs`. The 349-line Longhouse preparation module owns only state-free prepare/inspect/workflow HTTP adaptation. The 246-line Longhouse turn-preparation module owns only the fresh gate, rendering/application, deadline, and read-only blocking preparation helper; routes, all three call sites, librarian/spec compatibility, governance state, and `ocean-longhouse` algorithms remain outside it. Characterization tests deliberately keep checked contracts visible even when they increase raw line count.

## Course from here

1. Characterize, review, and extract the narrowed [Longhouse topic-projection manifest](specs/2026-07-16-ocean-daemon-longhouse-topic-projection-extraction-manifest.md), then separately manifest the coupled real-convene/title-control boundary before calls and turn/SSE orchestration.
2. Keep filesystem/project policy, permission authority, settings policy, host/extension ownership, and call-site orchestration fixed while domain boundaries move.
3. Treat the deferred librarian security disposition and any remaining state-registry or control-plane boundary as their own checkpoints; do not bundle them with a domain move.
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
- [Model-catalog extraction manifest](specs/2026-07-14-ocean-daemon-model-catalog-extraction-manifest.md)
- [YOLO-settings extraction manifest](specs/2026-07-14-ocean-daemon-yolo-settings-extraction-manifest.md)
- [Filesystem extraction manifest](specs/2026-07-14-ocean-daemon-filesystem-extraction-manifest.md)
- [Project-registry extraction manifest](specs/2026-07-14-ocean-daemon-project-registry-extraction-manifest.md)
- [Slack Canvas host-fulfillment extraction manifest](specs/2026-07-14-ocean-daemon-canvas-bridge-extraction-manifest.md)
- [Component-interaction extraction manifest](specs/2026-07-15-ocean-daemon-component-interaction-extraction-manifest.md)
- [Model-roles extraction manifest](specs/2026-07-15-ocean-daemon-model-roles-extraction-manifest.md)
- [Request-control extraction manifest](specs/2026-07-15-ocean-daemon-request-control-extraction-manifest.md)
- [Recall-registry extraction manifest](specs/2026-07-15-ocean-daemon-recall-registry-extraction-manifest.md)
- [Persistent-rooms extraction manifest](specs/2026-07-15-ocean-daemon-persistent-rooms-extraction-manifest.md)
- [Longhouse-preparation extraction manifest](specs/2026-07-15-ocean-daemon-longhouse-preparation-extraction-manifest.md)
- [Longhouse turn-preparation extraction manifest](specs/2026-07-15-ocean-daemon-longhouse-turn-preparation-extraction-manifest.md)
- [Longhouse topic-projection extraction manifest](specs/2026-07-16-ocean-daemon-longhouse-topic-projection-extraction-manifest.md)
- [Final 28% execution handoff](specs/2026-07-16-ocean-daemon-phase2c-final-28-percent-handoff.md)
- [Daemon local contract](../crates/ocean-daemon/AGENTS.md)
- [Runtime operator guide](OCEAN_RUNTIME_OPERATOR_GUIDE.md)
