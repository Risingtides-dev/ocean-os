# Ocean Daemon Slack Canvas Host Fulfillment Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; aligned host ownership, characterization/GC seam, focused/full/feature validation, and independent review passed
**Owner:** Ocean OS
**Rollback point:** `8024ffa`

## Purpose

Characterize and then move the daemon-owned Slack Canvas **host fulfillment seam** into one private binary module without changing bridge validation, raw-result storage, runtime lookup delivery, fulfilled SSE re-emission, query behavior, TTL/cap eviction, scheduler order, or pending turn-event relay behavior.

The host boundary is intentionally all-or-nothing for its local lifecycle: daemon query store, key/value types, bridge-to-SDK conversion, POST/GET adapters, runtime-registry write, fulfilled SSE re-emit, TTL/cap policy, and the canvas portion of registry GC move together. The initial pending `AgentEvent::SlackCanvas` relay remains in parent turn orchestration because it is part of the runtime-event match, not the fulfillment lifecycle.

## Extension-program alignment

This checkpoint follows the approved split in `2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`:

- the future `ocean-slack` extension owns Socket Mode, Slack credentials/API access, reconnect/intake, replies, files, and real Canvas delivery;
- `ocean-os` temporarily retains the typed Slack Canvas protocol plus session, permission, runtime lookup, scoped-event, and fulfillment-ingress enforcement needed by that extension during parity migration;
- `ocean-agents` retains deployment-specific content-agent identity and policy;
- this extraction adds no Slack transport or API behavior to core and does not compete with the extension implementation;
- retiring or generalizing the typed Slack-specific host seam happens only after extension parity under a separate protocol proposal.

The module is therefore named `slack_canvas_fulfillment.rs`, not `canvas_bridge.rs`: it is a host compatibility/enforcement seam, while the actual Slack bridge belongs to the extension.

## Characterization and GC seam before module extraction

Add three focused tests in `main.rs`:

- `canvas_fulfillment_key_matches_runtime_for_every_op`
- `gc_canvas_fulfillments_honors_injected_cap`
- `gc_canvas_fulfillments_sweeps_daemon_and_runtime_registries_together`

They freeze the daemon/runtime key algorithm for all five op variants, cap injection, local oldest-first eviction, coupled local/runtime TTL sweep on the same injected clock, and exact-TTL retention.

Before moving ownership, make one required behavior-neutral seam in `main.rs`:

1. add an explicit `max_entries` argument to generic `evict_overflow`, with every existing request/permission/canvas caller passing the same current `REGISTRY_MAX_ENTRIES` value;
2. mechanically lift the existing canvas-local store sweep and runtime-registry sweep from `gc_registries` into synchronous `gc_canvas_fulfillments(store, now, max_entries)` at the same source position;
3. call the new helper after permission GC with `REGISTRY_MAX_ENTRIES`, preserving lock and call order;
4. pass the same injected cap to both local and runtime canvas sweeps. This is behavior-identical because `REGISTRY_MAX_ENTRIES` and the runtime canvas cap are both 10,000 at the characterization point; a parity assertion freezes that fact.

The helper exists in parent composition for characterization, then moves intact with the bridge module.

Existing parent tests continue to pin:

- honest pending Slack Canvas relay and session-scoped SSE filtering;
- every stable key string;
- read/list/create bridge conversion, failed-read honesty, and metadata;
- POST→GET real-content delivery, session isolation, runtime-tool readback, fulfilled SSE re-emit, and malformed-body rejection;
- local TTL eviction, fresh retention, hard-cap bounding, and oldest-first eviction.

## Exact symbols to move intact

After the seam and characterization are green, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/slack_canvas_fulfillment.rs`:

- `CanvasFulfillmentStore`
- `CanvasFulfillmentKey`
- `CanvasFulfillment`
- `CANVAS_FULFILLMENT_TTL`
- `gc_canvas_fulfillments`
- `canvas_fulfillment_key_for_op`
- `fulfilled_result_from_bridge`
- `canvas_fulfillment_post`
- `CanvasFulfillmentQuery`
- `canvas_fulfillment_get`

`gc_registries` and generic `evict_overflow` remain in parent composition; only the canvas-specific helper and domain state move.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/slack_canvas_fulfillment.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- parent-private `AppState` and generic `evict_overflow`;
- `axum::{extract::{Query, State}, http::StatusCode, Json}`;
- `chrono::{DateTime, Utc}`;
- `serde_json::{json, Value}`;
- standard `Arc`, `Mutex`, `HashMap`;
- existing SDK session/turn/event and Slack Canvas vocabulary;
- runtime `CANVAS_FULFILLMENT_REGISTRY` and TTL contract;
- tracing through the existing macro call.

Minimal parent visibility:

- store/key aliases, stored fulfillment type/fields, TTL, both pure helpers, both route handlers, query type/fields, and GC helper are `pub(super)` only because `AppState`, parent router/GC composition, and retained parent tests reference them;
- no item becomes public outside the daemon binary;
- `AppState.canvas_fulfillments` remains parent-private and no state split is introduced.

## Frozen delivery and lifecycle invariants

- Bridge `result` JSON is stored verbatim and returned verbatim by GET.
- Store keys remain `(AgentSessionId, canvas key)`, session-scoped and last-write-wins.
- Key formats remain: read/update/append → canvas id, list → `list:{channel_id}`, create → `create:{title}` or `create:`.
- Daemon and runtime key derivation remain identical for every op.
- POST validates non-empty/parseable session, SDK op, and object result before any store write, runtime write, log, or event.
- Session keys written to runtime remain normalized `AgentSessionId::to_string()`.
- Local raw storage remains first, followed by typed conversion/runtime storage, then fulfilled SSE emission, then the same log and HTTP response.
- Fulfilled SSE remains session-scoped, uses a fresh turn id, and carries the original op plus typed fulfilled result.
- Failed/missing read data remains honestly pending; malformed list data defaults exactly as before; mutating results retain current bridged/not-applicable semantics.
- GET remains non-consuming, recovers poisoned mutexes, accepts the `key` alias, and preserves exact 200/400/404 envelopes and reason text.
- GC remains synchronous, after request and permission GC, with no await between local and runtime canvas sweeps.
- Entries strictly older than 30 minutes are evicted; entries exactly at the TTL survive.
- Local and runtime stores receive the same injected `now` and cap on each scheduler tick; oldest entries are removed first on overflow.
- The scheduler interval, failure accounting, spawn placement, and shutdown behavior remain unchanged.
- Pending runtime-event Slack Canvas relay construction, filtering, publication, and ordering remain in parent turn orchestration unchanged.

## Composition anchors and exclusions

This move does not:

- move or change the pending `AgentEvent::SlackCanvas` match arm, runtime relay task, event publication order, SSE filtering/replay/framing, or bus ownership;
- change runtime `SlackCanvasTool`, SDK vocabulary, external Slack bridge behavior, route path/method, or middleware;
- move `AppState`, router registration, GC scheduler, request/permission registries, generic overflow ordering, failure metrics, fixtures, or tests;
- change JSON/status/error/log shapes, validation order, key grammar, storage order, typed conversion, TTL comparison, cap value, mutex recovery, or sync/async boundaries;
- introduce a daemon library, public API, bridge service, trait, substate, new dependency, or opportunistic cleanup.

Any event-order, session-scope, validation, key, storage, runtime lookup, SSE, response, TTL/cap, scheduler, or lock change stops this extraction and requires a separate decision.

## Validation

Characterization/seam gate:

```bash
cargo test -p ocean-daemon fulfillment -- --nocapture
cargo test -p ocean-daemon slack_canvas -- --nocapture
cargo test -p ocean-daemon gc_ -- --nocapture
cargo test -p ocean-runtime slack_canvas -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon fulfillment -- --nocapture
cargo test -p ocean-daemon slack_canvas -- --nocapture
cargo test -p ocean-daemon gc_ -- --nocapture
cargo test -p ocean-runtime slack_canvas -- --nocapture
cargo test -p ocean-runtime
cargo test -p ocean-agent
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

A fresh security-focused reviewer must compare all moved definitions against the characterization commit, inspect the GC seam against the pre-seam rollback point, verify coupled delivery/lifecycle behavior, and confirm no unresolved medium-or-higher issue.

## Characterization result

The generic overflow helper now accepts an explicit cap while all existing production callers pass the same current 10,000 value. The canvas-local and runtime-registry sweep blocks are mechanically grouped into one synchronous main-local helper at the same scheduler position. Three tests freeze all-op daemon/runtime key parity, injected-cap oldest-first eviction, same-clock coupled GC, and exact-TTL retention. Eleven fulfillment tests, two pending-relay tests, ten daemon GC tests, 25 runtime canvas tests, all five router contracts, all 305 daemon tests, formatting, documentation, and diff checks pass from isolated clean main. Independent seam review found no unresolved medium-or-higher issue.

## Result

A private `slack_canvas_fulfillment.rs` now owns the complete host fulfillment lifecycle retained for the future `ocean-slack` extension. All ten moved definitions are unchanged from characterization commit `65ca2ee` except for minimal `pub(super)` visibility and formatting. Parent composition retains state assembly, route mounting, generic registry scheduling, the pending runtime-event relay, and all characterization/integration tests. No Slack transport, API, credential, reconnect, reply, file, or real Canvas-delivery behavior moved into core; those remain assigned to the extension program. Eleven fulfillment tests, two pending-relay tests, ten daemon GC tests, 25 runtime canvas tests, the full 122-test runtime suite plus integrations, 154 agent tests, all five router contracts, all 305 daemon tests, workspace-test compilation, both supported-feature checks, formatting, documentation, and diff checks passed. Fresh security/architecture review found no unresolved medium-or-higher issue.

## Rollback

Revert the bounded extraction after the characterization point; if reverting to the pre-seam state, revert the characterization commit as well. There is no data migration, wire-version handling, or compatibility cleanup.
