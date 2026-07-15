# Ocean Daemon Request Control Extraction Manifest

**Date:** 2026-07-15
**Status:** Published via PR #286; characterization, intact extraction, dedicated-target validation, independent review, hosted CI, and merge passed; live deployment is owned by the concurrent operator workstream
**Owner:** Ocean OS
**Rollback point:** `4f8b6dd`

## Purpose

Characterize and then move the daemon's in-memory request/permission control records and bounded synchronous lifecycle mutations into one private binary module without changing request identities, status/timestamp transitions, cancellation-token linkage, task-handle attachment, permission-waiter consumption, decision messages, secret handling, snapshots, GC observability, shutdown drain behavior, HTTP responses, event ordering, or runtime permission authority.

This is a storage/mechanics boundary, not a permission redesign. Operator YOLO policy remains unchanged and currently bypasses ordinary tool prompts when enabled. The runtime still decides which tools require permission; parent daemon composition still constructs `DaemonPermissionPolicy`, emits permission events, validates decision tokens, maps HTTP statuses, coordinates cancellation, schedules GC, drains shutdown tasks, and runs turns.

## Characterization and seams before extraction

Add two behavior-neutral seams in `main.rs`:

1. `requests_snapshot(&RequestRegistry) -> Vec<RequestStatus>` mechanically lifts the existing `GET /v1/requests` map/sort/reverse body; the handler keeps the exact response envelope.
2. Change `register_running_request` from `state: &AppState` to `requests: &RequestRegistry`, replacing only `state.requests.write()` with `requests.write()` and updating the four production callers to pass `&state.requests`. This removes an unnecessary composition dependency without changing call position or ordering.

Add focused tests:

- `request_snapshots_sort_newest_first_and_exclude_controls`
- `permission_snapshots_sort_newest_first_and_exclude_secrets`
- `register_running_request_preserves_identity_token_and_exact_initial_state`
- `register_running_request_duplicate_id_replaces_control`
- `attach_request_handle_unknown_id_detaches_task_without_registry_entry`
- `cancel_permission_waiter_mismatch_consumes_without_signalling`
- `permission_result_variants_preserve_exact_messages_and_live_reset`
- `control_terminal_helpers_preserve_timestamp_and_sender_semantics`

Keep and rely on existing tests for terminal-state preservation, cancelling→cancelled completion, permission result behavior, waiter denial release, active-turn projection, token binding/secrecy, GC TTL/caps, handle attachment, and bounded shutdown draining.

The mismatch and missing-ID tests deliberately freeze existing behavior rather than fixing it: a mismatched waiter is removed before ownership is checked, and dropping an unattached `JoinHandle` detaches rather than aborts its task.

## Exact symbols to move intact

After characterization passes, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/request_control.rs`:

- `RequestRegistry`
- `PermissionRegistry`
- `RequestControl`
- `PermissionWaiter`
- `impl RequestControl::{is_terminal, terminal_at}`
- `impl PermissionWaiter::{is_terminal, terminal_at}`
- `requests_snapshot`
- `pending_permissions_snapshot`
- `register_running_request`
- `attach_request_handle`
- `cancel_permission_waiter`
- `update_request_permission_result`
- `update_request_finished`

Move attached symbol documentation with the symbols. The only permitted extraction changes are `pub(super)` visibility and module-local imports/formatting.

## Visibility contract

The module remains private to the daemon binary. Minimal parent visibility is explicit because retained composition and tests still enforce security/event behavior:

- both registry aliases are `pub(super)`;
- both control structs are `pub(super)`;
- `RequestControl.status`, `.cancel`, `.handle`, and `.decision_token` are `pub(super)`;
- `PermissionWaiter.status`, `.sender`, and `.decision_token` are `pub(super)`;
- terminal helpers and all moved functions are `pub(super)`.

No item becomes public outside `ocean-daemon`. This checkpoint improves navigation but intentionally does not claim field encapsulation; a later redesign would require separate Phase 3 approval.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/request_control.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies

The new module depends only on existing crates/types:

- `std::{collections::HashMap, sync::Arc}`;
- `chrono::{DateTime, Utc}`;
- `ocean_core::{PermissionId, PermissionStatus, PromptRequest, RequestId, RequestState, RequestStatus, SessionId}`;
- `ocean_runtime::PermissionDecision`;
- `tokio::{sync::{oneshot, RwLock}, task::JoinHandle}`;
- `tokio_util::sync::CancellationToken`.

It does not depend on `AppState`, Axum, event buses, Slack Canvas, provider/runtime construction, persistence, metrics, or router code.

## Frozen request-control invariants

### Registration and snapshots

- A supplied request ID is preserved; an absent ID is generated once and written back into `PromptRequest`.
- Registration creates a fresh cancellation token and stores a clone linked to the returned token.
- Initial status preserves request/session IDs, caller-supplied state/message, `permission_id: None`, equal non-null `started_at`/`updated_at`, and `finished_at: None`.
- The request's private decision token is copied verbatim into `RequestControl` and never appears in request/permission snapshots or SSE/wire types.
- Inserting a duplicate request ID remains last-write-wins, replacing the entire prior control including cancellation token, handle, message, and secret.
- Request snapshots clone statuses only and sort descending by optional `started_at` using the current stable sort/reverse sequence.
- Permission snapshots clone statuses only, include both pending and already-consumed in-map waiters, and sort descending by `created_at`.

### Handle and waiter lifecycle

- Handle attachment acquires only the request write lock and sets `Some(handle)` when the ID exists.
- Missing-ID attachment drops the `JoinHandle`; Tokio task execution remains detached rather than aborted, and no registry entry is created.
- Waiter cancellation removes by permission ID before checking request ownership.
- A matching waiter consumes its sender and sends the exact `Deny` reason `request cancelled while waiting for permission`.
- A mismatched waiter remains removed, its sender is dropped without a decision, and it is not reinserted.
- Missing and already-consumed waiters remain no-ops.

### Permission and finish transitions

- Permission results do nothing for missing, terminal, or `Cancelling` requests.
- Any other live state is reset to `Running`, clears `permission_id`, updates `updated_at`, and uses the exact allow/allow-session/deny message grammar.
- Completion returns `None` for a missing request.
- `Cancelling` or `Cancelled` completion remains `Cancelled`, merges a newly returned session ID over the existing one, writes the exact ignored-output message, updates both timestamps, and takes the stored handle.
- Already terminal non-cancelled status remains unchanged except its handle is taken.
- Ordinary completion/error merges session ID, applies the requested terminal state/message, updates both timestamps, takes the handle, and returns the requested state.

### Terminal helper semantics

- Request terminality delegates only to `RequestState::is_terminal`.
- Request terminal timestamp precedence remains `finished_at`, then `updated_at`, then `started_at`, then `Utc::now` fallback.
- Permission terminality depends only on `sender.is_none()`; old pending senders are not terminal.
- Permission terminal timestamp remains `status.created_at`.

## Composition anchors and exclusions

This move does not:

- move/change `AppState`, `DaemonPermissionPolicy`, `PermissionPolicy::check`, `build_prompt_control`, permission hashing/deduplication, decision-token verification, HTTP handlers, status mapping, event buses, SSE, runtime invocation, or turn orchestration;
- move/change `gc_registries`, generic `evict_overflow`, GC constants/scheduler/panic accounting, Canvas GC, `record_gc_failure`, `drain_request_tasks`, shutdown supervision, `active_turn_for_session`, session enrichment, or `record_prompt_result`;
- change cancellation handler lock ordering, decision verification-before-removal, request/permission lock separation, or any await boundary;
- change route/method/banner/operator-guide contracts, JSON shapes, token secrecy, YOLO policy, permission authority, persistence, or runtime tool behavior;
- fix existing mismatched-waiter consumption, missing-handle detachment, live-entry cap eviction, fast-task attachment race, or multi-active-turn selection;
- introduce a public API, daemon library, service trait, substate, dependency, generated routing, or opportunistic redesign.

Any status transition, timestamp, secret, snapshot order, sender/handle ownership, lock/await, event, HTTP, permission, GC, shutdown, or runtime behavior change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon request_snapshot -- --nocapture
cargo test -p ocean-daemon permission_snapshot -- --nocapture
cargo test -p ocean-daemon register_running_request -- --nocapture
cargo test -p ocean-daemon attach_request_handle -- --nocapture
cargo test -p ocean-daemon cancel_permission_waiter -- --nocapture
cargo test -p ocean-daemon permission_result -- --nocapture
cargo test -p ocean-daemon control_terminal_helpers -- --nocapture
cargo test -p ocean-daemon finish_ -- --nocapture
cargo test -p ocean-daemon decision_ -- --nocapture
cargo test -p ocean-daemon gc_ -- --nocapture
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon request_snapshot -- --nocapture
cargo test -p ocean-daemon permission_snapshot -- --nocapture
cargo test -p ocean-daemon register_running_request -- --nocapture
cargo test -p ocean-daemon attach_request_handle -- --nocapture
cargo test -p ocean-daemon cancel_permission_waiter -- --nocapture
cargo test -p ocean-daemon permission_result -- --nocapture
cargo test -p ocean-daemon control_terminal_helpers -- --nocapture
cargo test -p ocean-daemon finish_ -- --nocapture
cargo test -p ocean-daemon decision_ -- --nocapture
cargo test -p ocean-daemon gc_ -- --nocapture
cargo test -p ocean-runtime permission -- --nocapture
cargo test -p ocean-runtime
cargo test -p ocean-agent
cargo test -p ocean-daemon router_contract -- --nocapture
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

The serial full-daemon command is recorded because upstream baseline `2c326bd`/`4f8b6dd` has an inherited process-global `OCEAN_CONFIG_DIR` race between permission fixtures and the YOLO precedence test; default-parallel GitHub CI remains the acceptance gate.

A fresh security/concurrency reviewer must compare every moved definition against the characterization commit, inspect parent field access and visibility, verify secret exclusion plus lock/await ordering, and report any unresolved medium-or-higher issue.

## Characterization result

Characterization commit `133a18b` introduced the two behavior-neutral seams and direct coverage for status-only snapshots, secret exclusion, caller-supplied/generated identity, linked cancellation tokens, exact initial fields, live duplicate replacement without cancellation/abort, missing-ID task detachment, matching/mismatched waiter consumption, exact permission messages and timestamp advancement, terminal timestamp/sender rules, all finish branches, session merging, exact cancellation text, both final timestamps, and handle consumption. Existing permission policy, decision-token, active-turn, GC, and bounded shutdown tests remained authoritative.

All 329 daemon tests passed serialized with the focused request, permission, finish, decision, and GC groups green. Formatting, documentation, and diff checks passed in the dedicated target directory. A fresh security/concurrency review found no unresolved medium-or-higher characterization issue after its first review drove stronger live-task, timestamp, finish-path, and exact-message coverage.

## Result

Extraction commit `87c3599` moved the two registry aliases, two control records, terminal helpers, status-only snapshots, registration/handle mechanics, waiter cancellation, and permission/finish transitions into private `request_control.rs`. Automated reviewer comparison found each executable body identical to characterization commit `133a18b` after normalizing only `pub(super)`, module imports, and rustfmt's multiline signature. The module is 238 lines and has no visibility outside the daemon binary.

`AppState`, cancellation and decision handlers, verifier-before-removal ordering, `DaemonPermissionPolicy`, event emission, HTTP mapping, active-turn projection, GC constants/scheduling/failure accounting, shutdown task draining, and turn execution remain in `main.rs`. Registry locks are still released before waiter signaling or task-handle awaits. Decision tokens still live only in private controls/waiters and are absent from status snapshots and SSE payloads.

Focused request/permission/finish/decision/GC checks, two focused runtime-permission tests, all 122 runtime unit tests plus integrations, all 155 agent tests, five router contracts, all 329 daemon tests serialized, workspace-test compilation, both supported daemon feature checks, formatting, documentation, and diff checks passed in the dedicated target directory. Feature builds are warning-free. Fresh parity/security review found no unresolved medium-or-higher issue. PR #286 passed default-parallel hosted CI and merged as `ee3860a`. Live daemon deployment/supervision is owned by the concurrent operator workstream and is not changed by this documentation closeout.

## Rollback

Revert the bounded extraction after the characterization point; if reverting the two seams too, inline the request snapshot body and restore `register_running_request(&AppState, ...)` plus its four callers. There is no data migration, wire-version handling, or compatibility cleanup.
