# Ocean Daemon Router-Parity Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; focused/full validation and independent review passed
**Owner:** Ocean OS
**Rollback point:** `92a7690e77dac1a7024c50c8bce54d3af6c41d5c`

## Purpose

Establish a reusable internal Axum router seam and executable method/path, fallback, and middleware parity checks before moving any daemon leaf concern out of `src/main.rs`.

This checkpoint is behavior-neutral. It changes neither HTTP contracts nor handler ownership. It exists so later leaf extractions can prove that route registration and middleware semantics did not drift.

## Baseline

At the rollback point:

- `crates/ocean-daemon/src/main.rs` is 19,977 lines.
- `main()` constructs the production router inline.
- The live router registers 72 method/path pairs: 32 GET, 37 POST, 1 PATCH, and 2 DELETE.
- The root banner advertises only 68 of them. It omits the already-live read-only routes `GET /v1/agents`, `GET /v1/agents/{name}`, `GET /v1/memory`, and `GET /v1/lsp`.
- The operator quick reference advertises only 59 of the 72 live pairs. In addition to the four banner omissions, it omits the existing session-message append, voice secret/STT/TTS, filesystem, browser, and workflow-preparation routes.
- This discovery drift predates the extraction. A bounded discovery-contract correction must land before the behavior-neutral router move; it changes no mounted route or handler behavior.
- Durable-room routes are registered through `room_routes()` and Longhouse routes through `longhouse_routes()`; all other routes are registered directly in `main()`.
- The router has no custom fallback. Axum's default behavior is part of the contract: unknown path is `404 Not Found`, and a known path with the wrong method is `405 Method Not Allowed`.
- Middleware is applied inner-to-outer as CORS followed by `TraceLayer::new_for_http()`; equivalently, requests enter HTTP tracing before CORS and route dispatch.
- CORS trusts loopback, Ocean extension, Tauri, and configured exact origins; advertises GET, POST, PATCH, DELETE, and OPTIONS; and allows Content-Type and Authorization headers.
- The Track-0 room projection GET routes remain unmounted. Persistent-room and LiveKit-token routes remain mounted.

The canonical advertised route list is `banner_routes()` in `src/main.rs`. The operator quick reference is `docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`.

## Exact extraction scope

### Symbols changed

- Extract the inline `Router::new()...layer(...)` construction from `main()` into one private reusable function, `app_router`.
- Extract CORS layer construction into one private helper so production and parity tests use the same policy.
- Keep `banner_routes`, `room_routes`, `longhouse_routes`, every handler, and `AppState` at their current visibility.
- Add test-only route materialization and request probes against the real assembled router.

### Files changed

- `crates/ocean-daemon/src/main.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md` only where the checked route baseline exposes missing active routes or needs a parity command
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md` to record the completed Phase 2C router checkpoint
- this manifest
- root `events.md`

No new crate, public library, service trait, domain substate, or public import path is introduced.

## Inbound dependencies

`app_router` depends on:

- all existing daemon handlers in `main.rs` and `browser_stream`;
- `AppState` as the Axum state type;
- `room_routes()` and `longhouse_routes()`;
- Axum `Router` plus `get`/`post` method routers;
- the existing CORS and HTTP trace layers.

The CORS helper depends on the existing origin parser/predicate and method/header policy.

## Outbound consumers

- `main()` builds the production service through `app_router` before `with_state`.
- Router parity tests build the same service with a deterministic fake runtime and in-memory stores.
- `root()` continues to return the route banner consumed by operators and clients.

## Frozen invariants

The checkpoint must preserve:

1. all 72 mounted method/path registrations, with the banner and operator quick reference corrected to advertise the full live table;
2. room and Longhouse merge behavior;
3. Axum default 404/405 fallback behavior;
4. CORS origin policy, methods, allowed headers, and preflight behavior;
5. middleware order: outer HTTP tracing, inner CORS, then route dispatch;
6. absence of retired Track-0 room projection routes;
7. `GET /health` and `GET /ready` paths and payload behavior;
8. all handler bodies, state ownership, permissions, cwd resolution, SSE ordering/replay, and persistence behavior.

## Explicit exclusions

This checkpoint does **not**:

- move any route handler or domain type to another module;
- rename or regroup any route;
- add generated route metadata;
- add a custom fallback or normalization layer;
- change middleware, CORS, tracing, request bodies, responses, status codes, or auth/permission behavior;
- split `AppState`;
- introduce a daemon library;
- redesign route documentation.

Any need for one of those changes stops this checkpoint and moves the decision to Phase 3.

## Characterization and validation

Required executable checks:

- The corrected banner and operator quick reference must each contain all 72 live method/path pairs and no retired route.
- Every banner method/path must resolve through the real assembled router without `404` or `405` for its advertised method. Dynamic path fields use inert probe values; non-GET requests use an intentionally unsupported content type so body extractors reject before side effects.
- Unknown path remains 404.
- Known path with an unregistered method remains 405.
- Trusted-origin CORS preflight for PATCH and DELETE succeeds and advertises the expected origin/method contract.
- Untrusted origins receive no allow-origin response header.
- Existing retired-room route tests remain green.
- Banner duplicate, format, and retained/retired route tests remain green.

Commands:

```bash
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon cors_ -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Before commit, run an independent correctness review focused on Axum method/path resolution, fallback behavior, middleware order, and accidental handler execution in probes.

## Result

The prerequisite discovery correction and behavior-neutral seam are kept as separate commits. The corrected contract contains 72 explicit method/path pairs. Five focused `router_contract` tests establish bidirectional source/banner/operator-guide parity, full-router reachability, 404/405 and trailing-slash fallback behavior, CORS headers and preflights, implicit representative HEAD behavior, group merge reachability, and the existing static/dynamic room-pattern precedence. No handler, mounted path, middleware, state shape, or public Rust API moved in this checkpoint.

## Rollback

Revert the bounded router-parity commit. Because this checkpoint does not move handlers or alter serialized contracts, rollback restores inline router construction without data migration or compatibility handling.
