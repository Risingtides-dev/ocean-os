# Ocean Daemon CORS Leaf Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; focused/full/feature validation and independent review passed
**Owner:** Ocean OS
**Rollback point:** `2bc5eb9`

## Purpose

Move the daemon's cohesive CORS policy out of `src/main.rs` into one private binary module without changing policy, middleware placement, route behavior, or public API.

The router-parity foundation required by Phase 2C is complete at the rollback point. This move is the first behavior-neutral daemon leaf extraction.

## Exact symbols moved intact

From `crates/ocean-daemon/src/main.rs` to `crates/ocean-daemon/src/cors.rs`:

- `cors_allowed_methods`
- `parse_allowed_origins`
- `is_trusted_origin`
- `is_loopback_origin`
- `cors_layer`
- the test-only `origin` helper
- the seven focused CORS policy tests:
  - `cors_allows_localhost_on_any_port`
  - `cors_allows_chrome_extension_origin`
  - `cors_allows_tauri_shell_origin`
  - `cors_allows_configured_extra_origins`
  - `cors_rejects_untrusted_public_origins`
  - `parse_allowed_origins_trims_and_drops_empties`
  - `cors_allow_methods_include_patch_and_delete`

## Files changed

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/cors.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- `axum::http::{header, HeaderValue, Method}`
- `tower_http::cors::{AllowOrigin, CorsLayer}`

`cors_layer` and `parse_allowed_origins` are `pub(super)` because daemon composition calls them. The origin predicate and method-list helpers remain private to the module. No symbol becomes public outside the binary crate.

Inbound callers remain:

- `main()` parses `OCEAN_ALLOWED_ORIGINS`, logs configured normalized origins, and passes them to the CORS layer builder.
- `app_router` receives the completed `CorsLayer` and keeps applying it inside `TraceLayer::new_for_http()`.
- router-contract tests construct the same layer through the module function.

## Frozen invariants

- Trusted loopback HTTP/HTTPS origins on any port remain accepted.
- `chrome-extension://*`, `tauri://localhost`, and `https://tauri.localhost` remain accepted.
- Operator origins remain comma-separated, trimmed, empty-filtered, and normalized by one trailing slash removal.
- Arbitrary public origins remain rejected.
- Allowed methods remain GET, POST, PATCH, DELETE, and OPTIONS.
- Allowed headers remain Content-Type and Authorization.
- CORS remains global to the full router, inside HTTP tracing, including default 404/405 responses.
- All 72 explicit route declarations, default fallback behavior, implicit HEAD behavior, room precedence, handler bodies, state, permissions, cwd, and SSE behavior remain unchanged.

## Explicit exclusions

This move does not:

- alter any origin or CORS policy;
- add credentials, wildcard origins, exposed headers, max-age, or private-network support;
- move router construction or route handlers;
- introduce a service trait, substate, public library, or new dependency;
- rename symbols beyond the narrow module-qualified import required by the move;
- clean up unrelated code.

Any policy or wire change stops this extraction and requires a separate design change.

## Validation

```bash
cargo test -p ocean-daemon cors::tests:: -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare the moved function/test bodies against rollback point `2bc5eb9`, verify visibility and imports, and confirm the production `app_router` layer order and focused/full gates remain unchanged.

## Result

The policy and all seven focused tests moved into private `cors.rs`; only `cors_layer` and `parse_allowed_origins` are visible to the parent composition module. The router continues to receive the same concrete `CorsLayer` at the same point and the route-contract middleware matrix remains unchanged.

## Rollback

Revert the bounded CORS extraction commit. There is no data migration, wire-version handling, or compatibility cleanup.
