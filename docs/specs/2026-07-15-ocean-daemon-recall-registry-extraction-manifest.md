# Ocean Daemon Recall Registry Extraction Manifest

**Date:** 2026-07-15
**Status:** Published; PR #287 merged as `3e051c1` after characterization, extraction, dedicated-target validation, independent review, hosted CI, and Rust 1.88 MSRV passed
**Owner:** Ocean OS
**Rollback point:** `aea7044` (characterization commit)

## Purpose

Characterize and then move the daemon's in-memory quorum-of-recall tally storage and bounded synchronous mutations into one private binary module without changing title lookup, distinct-voter counting, first-threshold ownership, zero-threshold clamping, poison recovery, carried-tally cleanup, persisted title/revoker authority, HTTP responses, route behavior, or existing unbounded process-lifetime retention of abandoned tallies.

This is a storage-mechanics extraction, not a Longhouse governance redesign. `ocean-longhouse::RecallVote` remains the tally/threshold/outcome authority. Parent daemon composition continues to resolve the live persisted title, validate request UUIDs, map HTTP responses, decide when a carried outcome may execute, present the daemon-held `RevokerKey`, mutate the persisted title registry, and remove a tally only after successful revocation.

## Current-upstream reconciliation

This checkpoint starts from fetched `origin/main` at `827b65b`, after request-control PR #286 merged as `ee3860a`. Commit `827b65b` changed daemon permission modes and their prompt, room-turn, call-runner, agent-turn, route, and test call sites. It did not change `RecallRegistryHandle`, `with_recalls`, `longhouse_recall`, or the recall/title/revoker lifecycle. The extraction must remain based on `827b65b`; it must not restore pre-permission-mode code from `ee3860a`.

Before publication, fetch and rebase onto the then-current `origin/main`, reread the root/crates/daemon/docs contracts, inspect every upstream daemon diff since `827b65b`, and rerun characterization if any recall, title, Revoker, Longhouse route, `AppState`, or test-fixture seam changed.

## Characterization seams before extraction

Introduce four behavior-neutral storage seams in `main.rs`:

1. `new_recall_registry() -> RecallRegistryHandle` mechanically centralizes the existing `Arc::new(Mutex::new(HashMap::new()))` construction used by production and daemon test states.
2. Change `with_recalls` from `(&AppState, closure)` to `(&RecallRegistryHandle, closure)`, replacing only `state.recalls.lock()` with `recalls.lock()`.
3. `cast_recall_vote(&RecallRegistryHandle, title_id, voter_id, threshold) -> RecallOutcome` mechanically lifts the existing entry-or-insert plus `RecallVote::cast` body.
4. `remove_recall_tally(&RecallRegistryHandle, title_id)` mechanically lifts the successful-revocation `HashMap::remove` call.

Update only the existing production and test call sites to pass `&state.recalls` or call the constructor. Do not change call ordering.

Add focused characterization for:

- first-cast threshold ownership and later lower-threshold rejection;
- one-credential-per-voter idempotence;
- zero threshold clamping to one by `RecallVote`;
- carried-outcome latching delegated unchanged to `RecallVote`;
- named-tally removal without disturbing another title;
- poisoned-mutex recovery for construction-independent cast/removal;
- malformed UUID `400` and exact body without opening a tally;
- unknown/non-live title `404` and exact body without opening a tally;
- pending route response fields and first-threshold retention;
- successful carry, persisted title revocation, exact success fields, and spent-tally removal.

## Exact symbols to move after characterization

Move from `crates/ocean-daemon/src/main.rs` to new private `crates/ocean-daemon/src/recall_registry.rs`:

- `RecallRegistryHandle` and its attached lifecycle documentation;
- `new_recall_registry`;
- `with_recalls`;
- `cast_recall_vote`;
- `remove_recall_tally`.

Move no request DTO, Axum handler, title helper, Revoker capability, route, or `AppState` field. The only permitted extraction changes are `pub(super)` visibility, module-local imports, and rustfmt formatting.

## Visibility and dependencies

The module remains private to the daemon binary. Minimal parent visibility:

- `RecallRegistryHandle`, `new_recall_registry`, `cast_recall_vote`, and `remove_recall_tally` are `pub(super)` because `AppState`, startup, the retained handler, and parent characterization tests use them.
- `with_recalls` stays module-private after extraction.

The new module depends only on existing types:

- `std::{collections::HashMap, sync::{Arc, Mutex}}`;
- `uuid::Uuid`;
- `ocean_longhouse::{RecallOutcome, RecallVote}`.

It does not depend on Axum, `AppState`, title persistence, `Revoker`, event buses, runtime/provider construction, rooms, requests/permissions, GC scheduling, shutdown, or SSE.

## Frozen invariants

### Identity, threshold, and vote semantics

- The registry key is exactly the live persisted firekeeper `title_id`, never the topic, public firekeeper, or voter ID.
- The first cast creates exactly one `RecallVote::new(title_id, threshold)` entry; later request thresholds are ignored while that tally exists.
- A raw threshold of `0` still reaches `RecallVote::new`, whose existing owner clamps it to `1`.
- Counting, duplicate-voter idempotence, carried latching, and returned `RecallOutcome` remain delegated verbatim to `ocean-longhouse::RecallVote::cast`.
- Distinct titles retain independent tallies.

### Locking and lifecycle

- The registry remains `Arc<std::sync::Mutex<HashMap<Uuid, RecallVote>>>`.
- A poisoned mutex remains recovered with `PoisonError::into_inner`; poison does not become a panic, `500`, or registry reset.
- Every storage helper is synchronous. No registry guard survives the helper return or crosses title lookup, Revoker execution, response construction, logging, event work, or any `.await`.
- Malformed coordinates and missing/non-live titles open no tally because request validation and live-title lookup remain before the first cast.
- Pending outcomes retain the tally.
- A carried tally is removed only in the existing successful-revocation branch.
- `NotLive`, authorization, registry, and any future execution failures retain the tally exactly as today.
- Successful removal deletes only the named title's tally.
- Abandoned/pending tallies remain memory-only, non-persistent, and unbounded by TTL/cap/GC/shutdown. Fixing that behavior is explicitly outside this checkpoint.

### Authority and security

- `SqliteTitleRegistry` remains the durable live-title and revocation authority.
- The daemon-held `Revoker` remains on `AppState`; its key/secret never enters the new registry or any wire/event/log surface.
- `recall_to_revocation` still receives a carried outcome only after parent handler policy decides to execute.
- No raw title token, Revoker key, or other secret is stored in the recall registry.

## Composition anchors and exclusions

This move does not:

- move/change `AppState`, `LonghouseRecallRequest`, UUID parsing, `longhouse_recall`, `with_titles`, persisted title lookup, `recall_to_revocation`, the `Revoker`, its key, status/body mapping, tracing, routes, banner, or operator guide;
- move/change `LonghouseRegistryHandle`, `TitleRegistryHandle`, `RevokerHandle`, rooms, calls, request/permission control, generic registry GC, shutdown, events, SSE, or turn orchestration;
- add tally persistence, TTL/cap/GC, cleanup on failed execution, threshold validation, voter authentication, or retry behavior;
- introduce a public API, daemon library, service trait, substate, dependency, generated routing, renamed wire field, or opportunistic cleanup.

Any threshold, identity, outcome, poison, lock lifetime, title/revocation, cleanup, HTTP, route, event, or security behavior change stops the extraction and requires a separate decision.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/recall_registry.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon recall_registry -- --nocapture
cargo test -p ocean-daemon recall_route -- --nocapture
cargo test -p ocean-daemon claim_route -- --nocapture
cargo test -p ocean-daemon revoke_route -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon recall_registry -- --nocapture
cargo test -p ocean-daemon recall_route -- --nocapture
cargo test -p ocean-daemon claim_route -- --nocapture
cargo test -p ocean-daemon revoke_route -- --nocapture
cargo test -p ocean-longhouse recall -- --nocapture
cargo test -p ocean-longhouse escrow -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Use a dedicated `CARGO_TARGET_DIR` for this worktree. The inherited process-global daemon environment fixtures remain serialized locally; default-parallel hosted CI is still an acceptance gate.

A fresh security/concurrency reviewer must compare every moved body against the characterization commit, verify the first-threshold and remove-only-on-success call ordering, confirm poison recovery and guard lifetime, inspect visibility and secret exclusion, and report any unresolved medium-or-higher issue.

## Characterization result

The characterization checkpoint introduced the four planned storage seams and six focused tests without changing route, title, or Revoker behavior. Direct registry coverage freezes first-threshold ownership, duplicate-voter idempotence, zero-threshold clamping in `RecallVote`, carried latching, named-only removal, and poisoned-mutex recovery. HTTP coverage freezes malformed-coordinate `400`, missing/non-live-title `404`, no tally creation before live-title resolution, exact pending and carried response bodies, durable title revocation, and successful spent-tally removal.

The focused recall-registry, recall-route, claim-route, revoke-route, and five router-contract groups passed. All 338 daemon tests passed serialized in the dedicated target directory; formatting, documentation, and diff checks passed. A fresh security/concurrency review compared the production seams with `827b65b` and found no unresolved medium-or-higher issue. It recorded one low, non-blocking residual: deterministic injection of a post-carry `recall_to_revocation` execution failure is absent, while the unchanged retained handler branches visibly remove only in `Ok(revocation)` and keep the tally on all failure arms.

## Result

Extraction commit `df09399` moved the private recall handle, constructor, poison-recovering lock helper, first-cast tally construction/cast, and named removal into the 52-line private `recall_registry.rs`. Automated normalized comparison against characterization commit `aea7044` found the moved production bodies mechanically identical after only module imports, type qualification, and required `pub(super)` visibility. Parent tests replaced direct access to the now-module-private lock helper with a test-only tally-ID snapshot; production call ordering is unchanged.

`AppState`, UUID parsing, live-title lookup, persisted title storage, daemon-held Revoker/key, `recall_to_revocation`, HTTP response mapping, routes, and logging remain in `main.rs`. The route still casts only after live-title resolution, returns pending without cleanup, executes carried outcomes through the persisted title registry, and removes the named tally only inside `Ok(revocation)`. Error arms retain the tally. The new module has no Axum, `AppState`, persistence, Revoker, secret, async, room, request, event, SSE, GC, or shutdown dependency.

Focused recall-registry, recall-route, claim-route, revoke-route, Longhouse recall, Longhouse escrow, and five router-contract groups passed. All 338 daemon tests passed serialized. Workspace test compilation, `livekit-tap`, `deepgram-stt`, formatting, documentation, and diff checks passed in dedicated target `/tmp/ocean-target-recall-registry`. Both fresh security/concurrency reviews found no unresolved medium-or-higher issue; the extraction review reported normalized mechanical equality. The inherited low residual remains explicit: no deterministic route injection forces a post-carry revocation execution error, while unchanged branch placement visibly retains the tally on every failure arm.

Hosted Ubuntu, macOS, Rust 1.88 MSRV, and cargo-deny lanes passed. The Rust 1.88 lane exposed an unrelated upstream TUI test incompatibility; commit `dff5ea2` changed the assertion to compare `cwd.as_path()` with `Path::new("/repo")`, after which the complete MSRV gate passed. PR #287 merged as `3e051c1`. Live daemon deployment/supervision belongs to the concurrent operator workstream and was not performed by this checkpoint.

## Rollback

Revert the bounded extraction after the characterization commit. If reverting the seams too, inline the original `Arc<Mutex<HashMap<...>>>` constructors, restore `with_recalls(&AppState, ...)`, inline entry-or-insert plus cast in `longhouse_recall`, and inline successful-branch removal. There is no data migration, wire-version handling, persisted recall state, or compatibility cleanup.
