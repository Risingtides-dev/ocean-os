# Ocean Daemon Metrics Primitives Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; focused/full/feature validation and independent review passed
**Owner:** Ocean OS
**Rollback point:** `2819e8c`

## Purpose

Move the daemon's cohesive in-process turn-metrics model, Prometheus renderer, and in-flight RAII guard out of `src/main.rs` into one private binary module. Keep the state-extracting `GET /metrics` handler in `main.rs` so this mechanical move does not redesign `AppState` or introduce a substate.

The router-parity foundation and first CORS leaf extraction are complete at the rollback point.

## Exact symbols moved intact

From `crates/ocean-daemon/src/main.rs` to `crates/ocean-daemon/src/metrics.rs`:

- `TURN_LATENCY_BUCKETS_MS`
- `TurnMetrics` and its `Default` derivation
- `TurnMetrics::record_turn`
- `TurnMetrics::render_prometheus`
- `InFlightGuard`
- `InFlightGuard::enter`
- `Drop for InFlightGuard`
- test-only helpers `metric_value` and `labelled_value`
- four focused primitive tests:
  - `metrics_render_empty_is_valid_prometheus`
  - `metrics_record_turn_buckets_and_counts`
  - `metrics_in_flight_guard_up_then_down`
  - `metrics_in_flight_never_underflows`

The `metrics` Axum handler and endpoint/cross-counter integration tests remain in `main.rs` because they read several independently-owned `AppState` atomics and prove the daemon composition boundary.

## Files changed

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/metrics.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on `std` atomics, formatting, and `Arc`.

- `TurnMetrics` is `pub(super)` because `AppState`, startup composition, the turn hot path, and the thin HTTP handler own/use it.
- `record_turn` and `render_prometheus` are `pub(super)` for those parent callers.
- `InFlightGuard` and `enter` are `pub(super)` for the turn hot path.
- Metrics fields, histogram bounds, and the guard's internals remain private.
- `metric_value` and `labelled_value` are visible to the parent only under `cfg(test)` because retained endpoint/cross-counter tests parse the rendered exposition.

No symbol becomes public outside the binary crate.

## Frozen invariants

- Every counter/gauge remains an `AtomicU64` using relaxed ordering.
- Histogram bounds and inclusive cumulative-bucket semantics remain byte-identical.
- `+Inf` remains total successful plus failed turns.
- `_sum` remains milliseconds converted to seconds and `_count` remains total turns.
- All Prometheus metric names, labels, HELP/TYPE lines, ordering, content, and newline behavior remain unchanged.
- Externally-owned persistence, GC, and SSE-lag values remain supplied verbatim by the parent handler.
- `InFlightGuard::enter` increments exactly once; `Drop` remains saturating and cancellation/panic safe.
- The `GET /metrics` path, status, content type, full-router middleware, state reads, and response body remain unchanged.
- Route, fallback, CORS, permissions, cwd, session, event, and persistence behavior remain unchanged.

## Explicit exclusions

This move does not:

- add a metrics crate or dependency;
- rename metrics or alter bucket boundaries;
- move the HTTP handler or split `AppState`;
- add labels, endpoints, authentication, caching, or scrape configuration;
- change atomic ordering or synchronization;
- redesign the turn hot path;
- move GC/SSE/persistence ownership into the metrics module;
- clean up unrelated code.

Any semantic or wire-format change stops this extraction and requires a separate design change.

## Validation

```bash
cargo test -p ocean-daemon metrics::tests:: -- --nocapture
cargo test -p ocean-daemon metrics_ -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare all moved production/test bodies against rollback point `2819e8c`, verify private fields and minimal `pub(super)` visibility, confirm the endpoint handler and call sites are unchanged, and confirm focused/full/feature gates pass.

## Result

The metrics primitives, renderer, in-flight guard, test parsing helpers, and four focused unit tests moved into private `metrics.rs`. The state-extracting HTTP handler and endpoint/cross-counter integration tests remain in `main.rs`; route registration, content type, parent-owned counter reads, turn call sites, and rendered text are unchanged.

## Rollback

Revert the bounded metrics extraction commit. There is no data migration, wire-version handling, or compatibility cleanup.
