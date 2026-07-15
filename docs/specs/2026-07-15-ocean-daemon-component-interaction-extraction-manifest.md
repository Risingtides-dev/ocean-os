# Ocean Daemon Component Interaction Extraction Manifest

**Date:** 2026-07-15
**Status:** Complete; characterization, intact extraction, full validation, and independent review passed
**Owner:** Ocean OS
**Rollback point:** `bed9979`

## Purpose

Characterize and then move the daemon's single component-interaction HTTP adapter into a private binary module without changing request validation, session/component key scoping, event payload delivery, one-shot consumption, registry poison handling, response status/body shapes, route mounting, runtime ownership, or tool wait behavior.

This is a leaf compatibility adapter over the runtime-owned `COMPONENT_WAIT_REGISTRY`. It is not a new component service or daemon-owned registry: `ocean-runtime` continues to own wait registration, timeout, and ordinary post-await cleanup, while the daemon fulfills a pending `(session_id, component_id)` slot by removing its sender and sending the `POST /v1/component/event` payload. Existing agent-loop cancellation can drop an in-flight wait future before its ordinary cleanup runs; this checkpoint preserves rather than repairs that behavior.

## Characterization before extraction

Add five direct-handler tests in `main.rs`:

- `component_event_rejects_missing_or_non_string_ids`
- `component_event_unknown_waiter_preserves_scoped_not_found_envelope`
- `component_event_delivers_explicit_and_default_payload_once`
- `component_event_dropped_receiver_is_gone_and_consumed`
- `component_event_poisoned_registry_preserves_internal_error`

The tests serialize access to the process-global runtime registry, use unique keys, and clean up every inserted or poisoned slot. They freeze all current branches:

- absent or non-string `session_id` returns the exact 400 missing-session envelope;
- absent or non-string `component_id` returns the exact 400 missing-component envelope;
- missing `event` defaults to `{}`;
- an unknown session/component pair returns the exact scoped 404 envelope;
- successful delivery returns the exact 200 envelope, sends the payload verbatim, and consumes the slot before send;
- a dropped receiver returns the exact 410 envelope and leaves no slot to retry;
- a poisoned runtime-registry mutex returns the current 500 error envelope rather than recovering or panicking.

No production seam or behavior change is required before the move.

## Exact symbol to move intact

Move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/component_interaction.rs`:

- `component_event`

Move the handler's documentation with it. The only permitted code change is minimal `pub(super)` visibility plus rustfmt/import adaptation.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/component_interaction.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- `axum::{http::StatusCode, Json}`;
- `serde_json::{json, Value}`;
- runtime-owned `ocean_runtime::tools::component::COMPONENT_WAIT_REGISTRY`.

`component_event` is `pub(super)` only because the parent router and retained parent tests call it. No item becomes public outside the daemon binary, no `AppState` or service abstraction is introduced, and the runtime registry remains the sole wait-state authority.

After extraction, `main.rs` imports only `component_interaction::component_event`; router registration and route/banner/operator-guide parity remain parent-owned.

## Frozen validation, delivery, and lifecycle invariants

- Validation order remains `session_id`, then `component_id`, then event extraction and registry access.
- IDs must remain JSON strings; empty strings remain accepted exactly as they are today.
- Missing `event` remains an empty JSON object; present JSON is delivered verbatim regardless of shape.
- Keys remain `(String, String)` in `(session_id, component_id)` order with exact, case-sensitive matching.
- The pending sender is removed under the registry mutex before the one-shot send.
- The registry mutex is released before sending to the waiting tool.
- Successful delivery remains exactly `200 {"status":"delivered"}`.
- A missing slot remains exactly 404 and echoes both requested identifiers.
- A dropped receiver remains exactly `410 {"status":"nobody waiting"}` and is not reinserted.
- A poisoned mutex remains a 500 with the existing formatted `registry lock: ...` error; extraction must not silently recover poison.
- The handler remains state-free and performs no permission check, persistence, event-bus emission, logging, retry, timeout, or cancellation.
- Runtime `ComponentWaitTool` remains permission-gated, session-bound by `SessionContext`, and authoritative for registration, timeout, and ordinary post-await cleanup; daemon fulfillment remains authoritative for remove-and-send delivery. Existing cancellation-drop behavior remains unchanged.

## Composition anchors and exclusions

This move does not:

- change `POST /v1/component/event`, request JSON, statuses, bodies, middleware, banner text, or operator documentation;
- move `AppState`, router construction, runtime/tool registration, SSE component render/unmount relays, permission policy, or turn orchestration;
- modify `ComponentWaitRegistry`, `ComponentWaitTool`, its global static, timeout behavior, session injection, cancellation-drop behavior, possible stale sender lifetime, or runtime tests;
- add authentication, permissions, persistence, TTL/GC, retries, logging, a public API, daemon library, service trait, substate, or dependency;
- repair or generalize the direct global-registry compatibility seam.

Any validation-order, key, remove/send ordering, mutex-poison, response, permission, runtime wait, route, or middleware change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon component_event_ -- --nocapture
cargo test -p ocean-runtime component -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon component_event_ -- --nocapture
cargo test -p ocean-runtime component -- --nocapture
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

A fresh reviewer must compare the moved handler against the characterization commit, verify all response and one-shot lifecycle branches, confirm runtime ownership remains unchanged, and report any unresolved medium-or-higher issue.

## Characterization result

Five direct-handler tests now freeze validation order and exact 400/404/410/500/200 envelopes, explicit and default payload delivery, scoped key matching, one-shot consumption, dropped receivers, and mutex-poison behavior. Sixteen runtime component unit tests plus the concurrent-wait lifecycle integration, all five router contracts, all 310 daemon tests, formatting, documentation, and diff checks pass. Independent review corrected the cancellation-cleanup ownership description and then found no unresolved medium-or-higher issue.

## Result

Private `component_interaction.rs` now owns the exact 72-line handler and documentation from characterization commit `fbb7c2d`; the only changes are `pub(super)` visibility and module-local imports/formatting. Parent composition retains route mounting, banner/operator-guide parity, runtime construction, and all five characterization tests. Runtime wait registration, permission/session binding, timeout, ordinary post-await cleanup, and existing cancellation-drop behavior remain untouched. Five handler tests, 16 focused runtime component unit tests plus lifecycle integration, the full 122-test runtime suite and integrations, 154 agent tests, all five router contracts, all 310 daemon tests, workspace-test compilation, both supported-feature checks, formatting, documentation, and diff checks passed. Fresh review verified exact characterization parity and found no unresolved medium-or-higher issue.

## Rollback

Revert the bounded extraction commit to restore the handler to `main.rs`; retain characterization tests unless their shared-global cleanup is defective. There is no data migration, wire-version handling, or compatibility cleanup.
