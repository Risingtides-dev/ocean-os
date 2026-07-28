# Ocean Daemon Longhouse Governance-Control Extraction Manifest

**Date:** 2026-07-19
**Status:** Published; exact 13-definition governance-control boundary extracted and independently re-reviewed on current main
**Owner:** Ocean OS
**Source baseline:** current `origin/main` `7a51f5bb` before reconciliation
**Rollback points:** original characterization import-contract commit `0e7a4dc`; current-base pre-extraction replay `299de7bd`

## Purpose

Extract only the daemon's existing Longhouse claim, manual revoke, recall, policy-breach, and board-mutation HTTP adapters from `crates/ocean-daemon/src/main.rs` into one private binary module, `crates/ocean-daemon/src/longhouse_governance_control.rs`, without changing behavior, wire contracts, authority, persistence, lifecycle, or security posture.

This checkpoint is narrower than the tentative coupled real-convene/title-control wave. Fresh mapping found that the five mutating control adapters already form a cohesive boundary over existing `AppState` handles and owner-crate algorithms, while real convene still combines provider readiness, awaited council orchestration, projection/event delivery, durable title grant/bind, and one-time raw-token response delivery. Deterministic successful real-convene characterization is not available without an unauthorized production seam. The stop rule therefore keeps real convene in `main.rs` and requires a later manifest.

The result, if authorized, remains a private module of the `ocean-daemon` binary. It is not a daemon library, service interface, state subobject, routing authority, or new trust boundary.

## Current source and moving-baseline rule

This manifest started from fetched `origin/main` `afffd1d` and was reconciled before characterization through fetched `origin/main` `729542b`. The published topic-projection implementation is PR #305 merge `9676b18`, with publication follow-up PR #306 merge `4ed957a`.

The candidate definitions remain executable-token-identical from `4ed957a` through `729542b`; only the separately reviewed truth-in-comments checkpoints changed their attached comments. Intervening daemon changes add and harden the separate session-compaction and voice-planner seams; later provider, Crew, and documentation work does not change Longhouse control definitions, `AppState` Longhouse/title fields, startup authority assembly, route mounts, or `ocean-longhouse` algorithms. The compact route, banner, tests, and all other upstream behavior remain current and must not be restored from an older snapshot.

Before manifest review, characterization, authorization, extraction, completion documentation, and publication:

1. fetch `origin/main` and record the exact object ID;
2. rebase this isolated branch;
3. inspect every intervening diff touching `crates/ocean-daemon/src/main.rs`, the candidate owner, `crates/ocean-longhouse/**`, Longhouse routes/tests, `AppState`, startup assembly, title storage, Revoker/recall authority, event publication, and affected contracts;
4. rerun focused characterization on adjacent overlap and the full security-sensitive matrix on semantic overlap;
5. recompute the exact symbol inventory and mechanically compare every moved definition/comment boundary against the accepted rollback commit;
6. stop rather than restore stale behavior when overlap cannot be reconciled exactly.

Concurrent uncommitted work in other worktrees is not source authority and must not be inspected, overwritten, or merged implicitly. Reconcile only committed refs.

## Fresh boundary decision

Fresh source, characterization, upstream, and oracle reviews selected the smallest cohesive boundary:

- move only the five control adapters in this checkpoint;
- leave real convene in composition;
- leave the already-published scripted demo and topic list/detail adapters in `longhouse_topics.rs`;
- do not introduce an interface merely to make real convene testable or movable.

No operator ruling is needed to manifest or characterize this behavior-neutral move. Separate explicit rulings remain required before changing authentication/trust policy, recall voter identity, title grant/bind atomicity, poison behavior, recall persistence/retention, or any response/event contract.

## Exact candidate inventory

Move the following existing definitions together with the truthful attached authority, status-mapping, lock, lifecycle, and security comments established by the separate pre-characterization comment-accuracy checkpoint below:

1. `with_titles<T>`;
2. `LonghouseClaimRequest`;
3. `longhouse_claim`;
4. `LonghouseRevokeRequest`;
5. `longhouse_revoke`;
6. `POLICY_BREACH_STRIKE_THRESHOLD`;
7. `LonghouseRecallRequest`;
8. `longhouse_recall`;
9. `LonghouseBreachRequest`;
10. `longhouse_breach`;
11. `LonghouseBoardPostRequest`;
12. `parse_board_mark_kind`;
13. `longhouse_board_post`.

At baseline these definitions and attached comments occupy the control block in `main.rs` beginning with persisted-escrow authority immediately before `with_titles` and ending with `longhouse_board_post`. Line numbers are advisory and must be re-resolved immediately before movement.

The five handlers mounted by `longhouse_routes()` and `with_titles` may become `pub(super)`. Parent-owned characterization and fixtures already call `with_titles`; its parent-private visibility preserves those callers without rewriting tests or exposing it outside the binary module tree. Request types may receive only the minimum `pub(super)` visibility required for the handler signatures and denied-warning compilation; their fields remain private. The breach threshold and board-kind parser remain module-private. Nothing becomes `pub`, `pub(crate)`, library-exported, or externally stable.

## Required owner dependencies

The private owner may import only existing dependencies required by the moved bodies:

- parent-private `AppState` and existing recall-registry operations;
- Axum `State`, `Json`, and response/status types;
- `ocean_longhouse` claim/revoke/recall/breach/title result types and owner algorithms;
- SDK `AgentRole` for live-firekeeper lookup plus `LonghouseEvent`, `Mark`, and `MarkKind` for board projection/publication;
- `serde::Deserialize`, `serde_json::{json, Value}`, and `uuid::Uuid`;
- existing logging macros used by the bodies.

No new crate dependency, handle, wrapper, trait, test-only production seam, second registry, key accessor, async lock facade, or failure-injection API is permitted.

## Parent-owned authority that must not move

The following remain in `main.rs`:

- `AppState` and every Longhouse/title/revoker/recall field;
- `LonghouseRegistryHandle`, `TitleRegistryHandle`, and `RevokerHandle` aliases;
- startup title DB path resolution/open/migration;
- construction of the single Longhouse registry shared with runtime extensions and `AppState`;
- construction and secret ownership of the daemon-held `Revoker`;
- construction of the memory-only recall tally handle;
- `longhouse_routes()` and canonical router/middleware composition;
- `LonghouseConveneRequest`, `parse_federation`, and real `longhouse_convene`;
- ready-model filtering, provider-backed council execution, title grant/bind, and raw-token delivery;
- `titles_db_path`;
- AgentEventBus/SSE filtering, replay, framing, lag, keepalive, and shutdown;
- parent fixtures and extraction-aware characterization tests;
- all `ocean-longhouse` cryptography, SQLite schema, title, escrow, recall, breach, quorum, and convergence algorithms.

The module must not own permissions, turns, sessions, rooms, calls, librarian/spec compatibility, subagent dispatch, provider routing, task spawning, or deployment.

## Existing behavior to preserve

### Title lock adapter

`with_titles` recovers a poisoned standard mutex and performs one synchronous title-registry operation while holding the existing guard. This checkpoint does not convert SQLite work to `spawn_blocking`, an async mutex, or a transaction facade. No moved handler currently awaits; no title or recall guard may cross publication, response delivery, or any future await.

### Claim

- Parse `title_id`, `agent_id`, and `decision` UUIDs in the current order before registry access.
- Blank tokens map to absence.
- Identity/token verification precedes bound-decision comparison.
- Success ratifies and closes the title and releases the existing stakes.
- Unknown title, wrong firekeeper, wrong/blank token, revoked title, and released title remain deliberately indistinguishable 403 responses.
- Unbound and wrong-decision claims remain 409 with their current exact envelopes; no forged caller gains decision disclosure.

### Manual revoke

- UUID parsing and existing default-reason behavior precede mutation.
- The immutable daemon-held Revoker and its existing server key remain composition-owned state.
- Live success, unknown title, non-live title, unreachable unauthorized, and storage failures retain exact 200/404/409/403/500 mappings and bodies.
- No Revoker key enters requests, responses, events, logs, or the new module's public surface.

### Recall

- Parse topic, firekeeper, and voter UUIDs in the current order.
- Validate a live title before opening a tally.
- First-cast threshold ownership, zero clamp in the owner, distinct UUID counting, duplicate idempotence, and carried latching remain exact.
- Pending responses do not touch title state.
- Carried execution uses the daemon-held Revoker authority.
- Remove only the named tally and only after successful revocation; every failed or non-live execution retains it.
- Tallies remain memory-only, unbounded, and reset on daemon-state reconstruction.

### Breach

- Parse the title UUID and retain the current default-detail behavior.
- A fresh threshold-three ledger object per request delegates strike durability to SQLite.
- Strikes one and two warn; strike three revokes; an unknown title returns 404. After closure, the current owner reports zero strikes rather than `NotLive`, so the route returns 200 with `revoked: false, strikes: 0`; preserve that observable wire behavior. Unauthorized/storage mappings remain exact but are unreachable without a forbidden injection seam.
- Existing persisted strike/revocation behavior remains restart-safe.

### Board

- Validate topic and author UUIDs and nonempty summary before mutation.
- Only explicit `evidence` maps to `MarkKind::Evidence`; every other supplied or omitted kind maps to non-quorum `Note`.
- Healthy behavior checks topic existence, folds the mark into the shared projection, releases the guard, then publishes the existing public event.
- Existing poison asymmetry remains: the recovered existence read can succeed while the second mutation lock fails, after which the handler can still return success and publish without updating projection.
- No guard crosses bus publication or await.

## Existing security/lifecycle risks preserved, not endorsed

This extraction must describe current behavior honestly without broadening, hiding, or fixing it:

- recall voter identity is a caller-supplied UUID, not cryptographic voter authentication;
- recall, breach, board, and manual revoke rely on the daemon's existing local-route trust posture; CORS is not authentication;
- reachable control routes invoke daemon-held Revoker authority without exposing its key;
- recall tallies are memory-only, unbounded, and retained after unsuccessful carried execution;
- SQLite operations synchronously block an async handler thread under the existing standard mutex;
- authenticated wrong-decision claims disclose the bound decision while forged identities are collapsed;
- board poison can produce live publication/success without projection mutation;
- a breach report against an already closed title returns 200 with zero strikes rather than the handler's mapped `NotLive` 409 branch;
- comments that imply a durable board or cryptographically credentialed/unforgeable HTTP caller overstate current behavior and are not promoted into the module contract;
- composition-owned convene retains grant-success/bind-failure token delivery, grant-failure convergence mapping, ready-model ordering, and poison behavior.

Changing any item above requires a separately approved security/behavior change. Raw title-token disclosure, broadened visibility, a second authority object, or new lock-across-await is not deferrable and is a hard stop.

## Pre-characterization comment-accuracy checkpoint

Before characterization is accepted, make and independently review one behavior-neutral comment-only checkpoint in `main.rs`. It must not alter executable tokens. Correct the attached control comments so the later exact mechanical move does not promote claims contradicted by the routes:

- manual revoke must say the HTTP route relies on the current local-route trust posture and that the daemon, not the caller, owns the cryptographic Revoker key; merely naming a title through an authorized/trusted route is sufficient for the daemon to attempt revocation;
- recall must say `voter_id` is caller-supplied, distinctness is UUID-based rather than credential authentication, omitted/zero threshold clamps to one, and a first distinct caller can carry a threshold-one tally;
- breach must say each accepted request exercises daemon-held Revoker authority, three accepted live-title reports can reach hard revocation, and a later report against the closed title currently returns 200 with zero strikes rather than 409; it must not call the report detected, authenticated, or unforgeable;
- board must call `LonghouseRegistry` an in-memory read-side projection, not a durable board/record/mirror, and must describe current poison divergence honestly.

The corrected comments become part of the accepted characterization rollback source and are then included in exact body/comment comparison. This separate checkpoint may clarify documentation only; it may not add authentication, alter a response, change mutation order, or fix poison behavior.

Comment-only checkpoints `6d636c2` and `e0eccce` satisfy this requirement. Fresh correctness and security/trust-boundary reviews verified that every changed Rust line was a comment and that the final wording matches caller admission, daemon-held Revoker authority, threshold-one recall, three-report breach, post-close breach 200/zero-strike behavior, in-memory board projection, mutex blocking, and poison divergence.

## Characterization required before authorization

Keep characterization inline in `main.rs` because it requires real router composition, `AppState`, SQLite fixtures, event buses, poison fixtures, and parent authority scans. Prefer a bounded set of table-driven extraction-aware tests.

### 1. Exact HTTP status/envelope/method and unauthenticated-authority matrix

Freeze malformed/missing/wrong-type bodies, UUID validation precedence, POST behavior, Axum 404/405/`Allow` behavior, complete JSON key sets, and exact bodies for all five routes. Explicitly prove the current router attaches no authorization, observer, permission-decision, or principal extractor to revoke/recall/breach/board; a request without those credentials can reach mutation under the existing local-route trust posture. Freeze that omitted/zero/one recall threshold lets the first caller-supplied distinct voter carry, and that three accepted breach submissions can reach hard revocation. These assertions document current behavior and do not approve it.

### 2. Claim lifecycle and non-disclosure

Freeze success; unknown/wrong-agent/blank-token/wrong-token/revoked/released indistinguishable 403; unbound/wrong-decision 409; identity-first decision disclosure; and one-shot ratification. The composition-owned successful convene response remains the sole permitted raw-token delivery. Prove token absence from every control response/error, event, SSE/replay/snapshot projection, captured tracing/log output, and persisted plaintext/DB bytes after reopen. Add a whole-owner source assertion that the request token is never formatted or passed to logging, debug, error, event, projection, or persistence sinks. Preserve the existing token-bearing request type's derived `Debug` only as an unused latent surface; source-assert the derive and its lack of every sink, and defer any derive removal to a separate security-hardening change.

### 3. Revoke lifecycle and persistence

Freeze omitted/blank reason, live success, unknown 404, non-live 409, exact bodies, later claim rejection, and persisted revoked status after reopen. Unreachable unauthorized/storage branches must not motivate a production injection seam.

### 4. Recall lifecycle and memory boundary

Freeze malformed/unknown/non-live no-tally behavior; first threshold; zero clamp; distinct/duplicate votes; pending/carried envelopes; successful-only cleanup; failed/non-live execution retention where reachable without a production seam; and memory-only reset after state reconstruction.

### 5. Breach lifecycle and persistence

Add the missing daemon route matrix: malformed/default detail, unknown 404, strikes one/two, strike-three revocation, the exact post-revoke 200 `revoked:false,strikes:0` envelope, and persisted strike/revocation continuation after reopen.

### 6. Board fold/publication/poison behavior

Freeze ID/summary/kind validation; only explicit evidence; unknown topic; exact response/event; healthy projection-before-publication with no quorum/status/firekeeper mutation; and current poisoned-lock live-publication/projection divergence.

### 7. Lock and source authority

Freeze title and recall poison recovery where reachable; prove no guard crosses publication or await; require exactly the authorized 13 definitions in the selected owner; reject provider, token-mint, Revoker-key construction, startup, route, SSE, task, permission, room, call, and extra-registry authority; and prove startup still shares one Longhouse registry with runtime extensions and `AppState`.

Owner-crate escrow/recall/breach tests remain authoritative for cryptographic constant-time verification, verifier-only persistence, DB reopen, key authorization, strike algorithms, and unreachable error branches. Daemon characterization must not duplicate algorithms or add executable seams only for testing.

## Characterization result

Commit `f1971c0` adds seven parent-owned, extraction-aware tests and their private test helpers without changing a production handler:

- `longhouse_governance_control_http_methods_and_parse_precedence_are_exact` freezes all five route methods, `Allow`, empty/malformed/non-object/missing-body extractor responses, UUID/summary precedence, and complete error envelopes;
- `longhouse_governance_control_claim_lifecycle_and_non_disclosure_are_exact` freezes identity-first claim behavior, uniform forged/unknown/revoked/released responses, unbound/wrong-decision disclosure, one-shot release, all-token response/event/projection/captured-log/reopened-DB-byte non-disclosure, and the unused latent `Debug` derive's no-sink source boundary;
- `longhouse_governance_control_unauthenticated_mutation_lifecycle_is_exact` freezes no-credential manual revoke, omitted/blank reason, unknown/non-live/reopen behavior, omitted/zero/one threshold recall, successful cleanup plus retained non-live tally, three-report breach revocation, the exact post-close 200/zero-strike response, unknown/blank breach behavior, and later claim refusal;
- `longhouse_governance_control_persistence_and_memory_reset_are_exact` freezes persisted breach strikes/revocation across reopen while recall tallies reset with reconstructed state;
- `longhouse_governance_control_board_fold_publish_and_poison_are_exact` freezes validation, complete responses, exact mark/event fields, non-quorum kind mapping, healthy fold-before-publication, and poisoned publication/success without mutation;
- `longhouse_governance_control_title_poison_recovery_is_exact` freezes parent title-lock poison recovery through the real claim route;
- `longhouse_governance_control_source_authority_boundary_is_exact` freezes exact owner and import inventories, exact per-item visibility and private request fields, token-source sinks, lock-end-before-publication, five parent route mounts, one startup registry shared with runtime extensions and `AppState`, composition-owned title/Revoker/recall construction, and real-convene/title-path exclusions.

Focused governance/control, recall, claim, revoke, board, convene, topic-projection, and router tests passed. All 500 daemon tests passed serialized. All 168 executed Longhouse tests plus one doc test passed with one host-dependent fixture ignored. Denied-warning daemon all-target Clippy, formatting, docs, and diff checks passed in dedicated target `/tmp/ocean-target-longhouse-governance-control`.

Fresh correctness/test-adequacy and security/architecture/lifecycle review rounds found and drove closure of incomplete route matrices, token-log evidence, persistence, private-field compatibility, exact import/visibility inventory, and post-close breach truth. Final reviews reported PASS with no unresolved medium-or-higher issue.

Test-only correction `0e7a4dc` updates the extraction-aware parent import expectation so `with_titles` remains a separate `#[cfg(test)]` import instead of creating a release-build unused import; production behavior and the seven tests are unchanged.

Extraction was authorized only from rollback point `0e7a4dc`, only for the exact 13 definitions/comments listed above, and only with the manifested imports and visibility. Real convene remains excluded.

## Publication result

Current-main replay commit `f64cda59` moves the exact boundary into private `crates/ocean-daemon/src/longhouse_governance_control.rs`. Mechanical comparison against original rollback `0e7a4dc`, current-base replay `299de7bd`, and the extraction parent is exact after removing only the authorized `pub(super)` visibility modifiers. `AppState`, route composition, startup authority assembly, Revoker/recall construction, real convene, durable title grant/bind, provider execution, and raw-token delivery remain in `main.rs`.

The reconciled checkpoint passed all 592 daemon tests serialized, the seven focused governance-control tests, denied-warning daemon all-target Clippy, formatting, docs, and diff checks. Fresh independent correctness/security review reported CLEAR with no blocker, high, or medium finding. Preserved risks remain the pre-existing unauthenticated local mutation posture, caller-supplied recall voter UUIDs, synchronous SQLite mutex work, memory-only/unbounded abandoned recalls, latent request `Debug`, post-close breach 200/zero-strike behavior, and board poison publication/projection divergence.

## Validation matrix

Use a dedicated target and serialize environment/state-sensitive tests.

```bash
export CARGO_TARGET_DIR=/tmp/ocean-target-longhouse-governance-control
cargo test -p ocean-daemon longhouse_governance_control_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon recall_registry -- --nocapture --test-threads=1
cargo test -p ocean-daemon recall_route -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_topic_projection_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon convene_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon -- --test-threads=1
cargo test -p ocean-longhouse -- --test-threads=1
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo clippy -p ocean-daemon --all-targets -- -D warnings
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Characterization acceptance requires fresh correctness and security/architecture/lifecycle reviews with no unresolved medium-or-higher issue. Extraction acceptance additionally requires complete mechanical body/comment comparison against the accepted rollback commit.

## Stop rules

Stop before extraction and request a concrete design decision if:

- any candidate body, wire response, route mount, title/recall authority, startup handle identity, or owner-crate algorithm changes upstream and cannot be reconciled exactly;
- preserving behavior requires a trait, service, `AppState` substate, public API, second registry/Revoker, new dependency, generated route, or test-only production seam;
- a raw title token could enter logs, events, SQLite plaintext, snapshots/replay, debug output, control responses, or the new owner's reusable surface;
- a lock would cross publication or await, or successful-only recall cleanup/order would change;
- board projection/publication order or poison behavior changes;
- characterization exposes a required schema, wire, trust-policy, authentication, persistence, concurrency, or lifecycle redesign;
- real convene enters this checkpoint merely to maximize moved lines;
- comments or contracts describe recall voters as credentialed/authenticated, manual control as an operator-authenticated request, breach reports as detected/authenticated, recall as unforgeable, or the in-memory board as durable without a separately approved implementation change;
- any fresh reviewer reports an unresolved medium-or-higher correctness, security, architecture, or lifecycle issue.

Do not weaken the characterization threshold to force a move. If an important branch cannot be characterized without a new production seam, leave that symbol in `main.rs` and narrow again.

## Rollback

Before production extraction, rollback is deletion of this manifest and its progress references. After authorization, the accepted characterization commit becomes the rollback point. After movement, rollback is one revert of the mechanical extraction commit: restore the exact 13 definitions/comments to `main.rs`, remove the private module/import wiring, and retain characterization. No route, schema, wire, credential, database, event, or external API migration is part of this checkpoint.
