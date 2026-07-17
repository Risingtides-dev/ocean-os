# Ocean Observatory Architecture

**Date:** 2026-07-17
**Type:** cross-repository product and runtime architecture proposal
**Status:** Proposed direction; implementation is not yet approved
**Owners:** Ocean OS (observation authority), Ocean Surface (product experience), orchestration extensions (subagent graph semantics)
**Concept name:** Ocean Observatory; Ocean Floor is its topology view

## 1. Decision sought

Build Ocean Observatory as a truthful operator view of root agents, extension-owned
subagents, their relationships, and their current work. The downloaded Claude
Design `Agent Floor` export is disposable concept evidence only. Its implementation,
mock event model, pixel office, fixed rooms, inferred state, and cross-session
controls are not an implementation foundation.

This proposal does not authorize implementation. Gate 0 requires explicit acceptance
of the unresolved ownership and security decisions in section 12.

## 2. Product intent

Observatory answers, in order:

1. What needs operator attention now?
2. Which root and child executions are active, and what are they doing?
3. How are executions related?
4. What happened, including gaps and restarts?
5. What can the current operator safely do through existing authority?

The default experience is an operational instrument, not a virtual office or
screensaver. Ocean Floor uses bathymetric containment and state-driven current:

- project shelves contain workspace basins;
- workspace basins contain stable execution stations;
- explicit parent/child relationships render as directed tethers;
- lifecycle, concurrent activity, and attention remain independent channels;
- motion occurs only when a recorded fact changes state;
- the normal transcript remains the authority for session content.

Observatory is a primary mode reached through existing secondary navigation. It does
not replace chat or add permanent header chrome.

## 3. Authority boundary

### Ocean OS

Owns:

- host-observed execution, request, turn, tool, permission, and terminal facts;
- validation and transport of extension-attested topology;
- safe metadata projection, authorization, redaction, replay, and retention;
- snapshot and live-stream consistency;
- existing permission and cancellation enforcement.

Ocean OS is the observation authority, not the orchestration authority. It must not
add a core scheduler, `spawn_worker`, named-subagent runtime, join/retry policy,
worker budgets, or result aggregation.

### Orchestration extensions

Own:

- subagent definitions and roles;
- dispatch, spawn, join, retry, recursion, and budget policy;
- parent/child graph semantics;
- attestation of graph metadata to the host.

Extension attestation does not widen tool, cwd, model, secret, permission, or process
grants. Every local child turn remains host-executed and permission-gated.

### Ocean Surface

Owns:

- the shared Leptos/WASM Observatory product mode;
- normalized client state and deterministic reduction;
- scene rendering, semantic inspector/list, replay controls, and responsive behavior;
- links into existing session-scoped transcript and control paths.

Surface must not consume the raw global agent firehose as a product transcript,
infer topology, retain secrets, or acquire runtime authority.

## 4. Non-goals

- Reskinning the fixed pixel office as an aquarium.
- Inferring children from a tool named `subagent`, titles, cwd, timing, or prose.
- Showing raw thinking, prompts, tool arguments/output, environment, paths, or errors.
- Approving arbitrary cross-session permissions from observer state.
- Adding one stream, component tree, animation loop, or worker per actor.
- Forking browser, extension, Tauri, and mobile implementations.
- Treating event silence as idle or completion.

## 5. Truth and identity model

Every displayed claim carries one provenance:

- `host_observed` — recorded by daemon-owned request/runtime/permission adapters;
- `extension_attested` — graph metadata reported by an activated extension;
- `derived` — folded presentation state with source cursor references.

A node is `live` only while a current host execution or supervised extension lease
supports that claim. Extension attestation alone is `reported`, not `live`.

Required stable identifiers:

| Identifier | Meaning |
| --- | --- |
| `observatory_id` | Persistent local observation authority |
| `daemon_instance_id` | One daemon boot; distinguishes restarts |
| `cursor` | Daemon-allocated monotonic durable order, encoded as a decimal string |
| `event_id` | Idempotency and audit identity |
| `execution_id` | One root or child attempt; canonical topology node |
| `root_execution_id` | Root of the execution graph |
| `parent_execution_id` | Immutable immediate parent, null only for roots |
| `edge_id` | Immutable daemon-validated relation identity |
| `producer_id` | Activated extension or daemon producer identity |
| `session_id`, `turn_id`, `request_id` | Existing host correlations, not topology identities |
| `tool_call_id`, `permission_id` | Activity correlations, never graph node identities |

Retries receive new execution IDs. Parent/root relationships are immutable. Reject
unknown or cross-authority parents, cycles, excess depth, and duplicate idempotency
keys.

## 6. Versioned metadata event envelope

Illustrative v1 envelope:

```json
{
  "schema_version": 1,
  "cursor": "1842",
  "event_id": "uuid",
  "observatory_id": "uuid",
  "daemon_instance_id": "uuid",
  "occurred_at": "2026-07-17T18:02:31.123Z",
  "recorded_at": "2026-07-17T18:02:31.125Z",
  "kind": "execution.phase_changed",
  "truth": "host_observed",
  "producer": { "kind": "daemon", "id": "ocean-daemon" },
  "topology": {
    "execution_id": "uuid",
    "root_execution_id": "uuid",
    "parent_execution_id": null,
    "edge_id": null,
    "session_id": "uuid",
    "turn_id": "uuid",
    "request_id": "uuid"
  },
  "correlation": {
    "tool_call_id": null,
    "permission_id": null
  },
  "visibility": "metadata",
  "payload": {}
}
```

V1 kinds should cover:

- daemon start/stop and stream gaps;
- execution admission, binding, phase changes, heartbeat, and finish;
- tool start/finish using safe name/classification, duration, outcome, and byte counts;
- permission waiting/resolved using fixed reason/outcome codes;
- model reroute using safe aliases and fixed reason codes;
- rejected topology attestations.

Payloads are typed Rust enums. Unknown additive kinds remain ignorable, while an
unsupported major schema is rejected visibly.

## 7. Extension admission and host binding

Topology cannot be inferred after the fact. Add a generic extension-host seam:

1. An activated extension requests admission for a root or child execution with a
   parent ID, safe labels, producer identity, lease, and idempotency key.
2. The daemon validates the relationship and mints execution/root/edge IDs plus a
   short-lived one-time binding token.
3. The extension submits an ordinary daemon turn with an additive observation
   binding.
4. The daemon verifies and consumes the token before request registration.
5. The binding is removed before prompt/provider serialization.

Admission records orchestration outcomes but never starts work or grants capability.
Binding and permission decision tokens are never persisted, logged, streamed, or
placed in URLs or browser storage.

## 8. Read-only Observatory API

All routes require an authenticated observer principal and return
`Cache-Control: no-store`.

### Snapshot

`GET /v1/observatory/snapshot`

Returns a transactionally consistent projection at `watermark_cursor` with nodes,
edges, pending-attention summaries, daemon instance, earliest available cursor, and
capabilities. Default detail is metadata-only and excludes inactive historical bulk.

### Live tail

`GET /v1/observatory/events`

- resumes from `Last-Event-ID` or `?after=<cursor>`;
- sends only facts after the snapshot watermark;
- catches broadcast lag up from durable storage;
- returns an explicit reset/gap response for expired, malformed, or future cursors;
- never silently attaches live when history completeness is unknown.

### Replay

`GET /v1/observatory/replay`

Provides ascending bounded JSON pages with `after`, optional `through`, filters,
`next_after`, `has_more`, and `complete`. Replay crossing retention returns an
explicit gap/410 response. The Surface owns playback timing and event stepping.

Snapshot, stream, and replay are read-only. Existing session, permission, and
cancellation routes remain separate authorities.

## 9. Authorization, redaction, and retention

CORS and the Surface proxy's shared Basic auth are insufficient authorization.
Introduce scoped principals:

- `observatory:summary` — topology, coarse phases, safe aliases, metrics;
- `observatory:content` — separately approved bounded content projection;
- extension producer scope — admit/renew/read only that producer's graph;
- no implied control scope.

V1 defaults to metadata only. The Observatory pipeline cannot represent prompts,
thinking text, assistant deltas, absolute paths, tool args/output, permission args,
headers, environment, raw extension payloads, raw errors, or any decision/binding
secret. Redaction occurs before durable append and broadcast.

Durable replay should use a separate owner-only store with an append-only event log
and current node/edge projections updated atomically. SQLite/WAL is the leading
choice, but the owning package is a Gate 0 decision. Do not add Observatory tables to
room storage or session JSON by convenience.

Proposed starting retention, subject to approval:

- metadata events: seven days and one GiB maximum;
- nonterminal nodes/edges: never prune;
- terminal projections: thirty days;
- restart: previous nonterminal host executions become `interrupted`;
- expired extension leases become `disconnected`, never `completed`.

## 10. Ocean Surface architecture

Observatory lives in the shared `crates/ocean-surface-ui` bundle:

```text
src/observatory/
  mod.rs        mode boundary and stage
  domain.rs     normalized IDs, events, state, filters
  reducer.rs    pure deterministic fold and bounded eviction
  adapter.rs    snapshot/live/replay transport adapter
  layout.rs     stable topology layout and semantic zoom
  scene.rs      retained visual renderer and lifecycle
  inspector.rs  semantic DOM detail, trace, and actions
```

The scene owns pixels only. The DOM owns labels, hierarchy, selection, controls,
attention, focus, transcript links, and the complete list alternative. Canvas is
`aria-hidden`; a canvas-only control surface is forbidden.

The client reducer must:

- scope state by observatory/daemon instance and cursor;
- reject duplicates and stale generations;
- keep lifecycle, concurrent activities, and attention independent;
- freeze and label stale state on gaps/disconnects;
- cap nodes, activities, pause buffers, and detail rings deterministically;
- request a fresh snapshot after overflow or reset;
- produce identical semantic state from live fold and replay at the same cursor.

Mount through the existing command registry and header overflow as a primary stage.
Keep `app.rs` integration minimal. Browser/PWA, extension, Tauri, and future mobile
consume the same module; no shell gains runtime authority.

## 11. Interaction and motion grammar

The default hierarchy is:

1. conditional attention shelf;
2. bathymetric topology/list view;
3. one selected execution inspector;
4. replay rail only while replay is active.

Truthful motion examples:

- execution admitted: one bounded materialization;
- thinking observed: low-amplitude state glow;
- tool running: one activity port per in-flight call;
- output delta: coalesced luminance/material update;
- permission wait: warning halo and attention row;
- successful finish: one transition to static completed state;
- error: one transition to static failure state;
- confirmed parent/child activity: one directed flow packet;
- telemetry gap: freeze all operational motion and label state incomplete.

Never animate pacing, typing avatars, coffee breaks, smoke, ambient creatures,
fake progress, or topology edges without a recorded relationship. Reduced motion
removes continuous animation without removing information.

Compact/coarse-pointer mode is DOM-first with project/workspace groups, execution
rows, attention, inspector, and replay controls. It does not shrink a pannable desktop
map into a phone viewport.

## 12. Gate 0 decisions requiring acceptance

1. **Persistence owner:** dedicated `ocean-observatory` crate (recommended) or an
   explicitly approved existing owner.
2. **Credential distribution:** how first-party web, extension, Tauri, and CLI
   obtain scoped observer authority without embedding a bearer secret.
3. **Initial scope:** whole daemon (product goal), project, or active-session-only
   calibration slice. A shipping whole-daemon view requires the new API.
4. **Entity vocabulary:** root/child executions as canonical nodes, with sessions,
   turns, tools, and permissions as attached facts (recommended).
5. **Content retention:** none by default (recommended); any content projection is a
   separate privacy/encryption/export decision.
6. **Remote children:** omit in v1 or show as `extension_attested/reported`, never
   host-live without a binding.
7. **Retention defaults:** accept or revise the proposed time/size bounds.
8. **Control:** read-only Observatory in v1 (recommended); session-scoped actions
   remain in existing authoritative surfaces.

## 13. Delivery gates

### Gate 1 — authority and protocol

- accepted threat model and scoped observer principal;
- typed metadata schema that cannot serialize forbidden fields;
- durable monotonic cursor, daemon instance, snapshot watermark, and explicit gaps;
- validated extension admission/binding with no capability widening;
- end-to-end proxy resume support, including `Last-Event-ID` or equivalent cursor;
- shared fixtures and drift tests across Ocean OS and Ocean Surface.

### Gate 2 — reducer and product skeleton

- pure replay-equivalent reducer;
- attention shelf, semantic list, inspector, empty/degraded states;
- no runtime mutation from replay or observer-only state;
- compact, keyboard, screen-reader, reduced-motion, and forced-color behavior.

### Gate 3 — renderer selection

Benchmark virtualized DOM/SVG, Canvas 2D, and WebGL against the actual design. Choose
from measurements, not ambition. One renderer, one reducer, one stream, and one
animation loop per Observatory instance.

Initial acceptance targets:

- 500 visible and 2,000 tracked executions with deterministic aggregation above
  the visible cap;
- at most ten coalesced delta batches per second per observer;
- p95 reducer/layout at or below 8 ms per batch;
- p95 desktop frame at or below 16.7 ms;
- zero continuous animation work while hidden, idle, paused, or reduced-motion;
- no monotonic memory growth in a 60-minute churn soak;
- 100 mount/unmount cycles leave zero streams, RAFs, timers, observers, workers,
  or renderer resources;
- zero forbidden fields across redaction/property fixtures.

### Gate 4 — integration and rollout

- feature flag and server-side kill switch;
- overload degradation: lower update rate, aggregate-only, then static snapshot;
- Observatory failure cannot block agent execution, permissions, transcript SSE,
  daemon shutdown, or Surface startup;
- runbook for disable, credential revocation, reset, overload, and suspected exposure;
- independent security, protocol, accessibility, and performance review.

## 14. Architecture choices rejected

- Product use of `/v1/agent/events?all=1`.
- Mirroring raw runtime, transcript, permission, or extension payloads.
- Client-side authorization after receiving a global feed.
- Core-owned subagent scheduling or lifecycle policy.
- Parent/state inference from names, prompts, tools, paths, proximity, or timeout.
- Random UUID event IDs as the only recovery position.
- Joining independent streams by client timestamps.
- Query-string credentials or control tokens.
- Unbounded observability queues.
- One connection, component tree, or animation loop per actor.
- Canvas-only semantics or controls.
- Desktop-first delivery with compact/reduced-motion postponed.

## 15. Required validation before implementation acceptance

- topology invariants, retries, restart interruption, lease expiry, and provenance;
- snapshot-plus-tail equivalence, duplicate/order/gap/restart/proxy recovery;
- authentication, cross-principal isolation, redaction, and non-control observer tests;
- load tests with 2,000 executions, ten observers, slow clients, and unrelated raw
  tool traffic;
- Surface replay/live equivalence, reducer caps, deterministic layout, lifecycle
  cleanup, compact behavior, keyboard/screen-reader operation, and renderer fallback;
- repository gates from each owning `AGENTS.md`, including docs checks and fresh
  architecture/security review.

## 16. Immediate next checkpoint

Do not begin the animated renderer first. The next bounded checkpoint is Gate 0:
review and accept the eight decisions in section 12, then write the exact Gate 1
schema/auth/persistence implementation manifest. Visual calibration may proceed only
against synthetic, metadata-safe fixtures and must remain disposable until the daemon
contract is accepted.
