# Ocean Daemon Longhouse Topic Projection Extraction Manifest

**Date:** 2026-07-16
**Status:** Published; characterization, extraction, dedicated-target validation, compatibility/MSRV/local CI, independent review, hosted CI, and PR #305 merge `9676b18` passed
**Owner:** Ocean OS
**Source baseline:** fetched and rebased `origin/main` `9177598`
**Rollback point:** characterization commit `c443df9` (rebased from reviewed `c1be830`)

## Purpose

Extract only the daemon's scripted Longhouse demo and read-side topic HTTP projection from `crates/ocean-daemon/src/main.rs` into one private binary module, `crates/ocean-daemon/src/longhouse_topics.rs`, without changing behavior.

The final-28% handoff tentatively grouped demo, real convene, and topic reads as the first governance wave. Fresh mapping at `5b9e23a` disproves that full boundary: `longhouse_convene` directly owns ready-model filtering, the asynchronous `ocean_longhouse::convene` call, durable firekeeper-title grant/bind, raw-token response delivery, and convergence logging. Moving it now would either place title authority in the read-side wave or require a new interface. Neither is authorized.

This manifest therefore applies the handoff stop rule. The first move is narrowed to the cohesive `AppState::{longhouse,agent_events}` topic-observability boundary: one scripted producer plus list/detail reads over the same existing projection handle. Real convene, federation parsing, model selection, title mint/bind, claim/revoke/recall/breach/board control, and route composition remain in `main.rs` for the separately reviewed second governance manifest.

The result remains a private module of the `ocean-daemon` binary. It is not a daemon library, public API, service layer, substate, topic store, event bus, governance engine, capability provider, or extension runtime.

## Current upstream and reconciliation rule

This manifest originated from fetched `origin/main` `5b9e23a`, was rebased immediately before extraction onto `e8f3322`, was rebased before initial publication onto `a517f6c`, and was reconciled after PR creation through `ad391f3` and then fetched `origin/main` `9177598`. The last published Phase 2C extraction is Longhouse turn preparation at merge `9095d5a`, with publication follow-up `b21006b`.

The source advanced substantially after `b21006b`:

- `0aa12db` made correlation-aware sequential evidence the default and added `convergence_basis` to the daemon convene response/log;
- `767127b`, `72e6b0e`, `e22953a`, `d705b9f`, and `9754573` evolved the evidence field, exact decay trajectory, uncertainty-driven planner, active review integration, and production-path characterization;
- `7bd0cf2`, `2f2f8b2`, and `236f1d7` added proposal consolidation, structured `ANSWER` validity filtering, and production compliance visibility;
- `cdb7c17`, `822e755`, and `b74b3ff` changed persistent-room named-agent binding and durable room SSE lifecycle adjacent to, but outside, this boundary;
- after the handoff's `4712fdb` baseline, `ca4fe32` changed only the Realtime voice credential seam in `main.rs`; `04c683f` and `5b9e23a` changed the separate voice model default contract.

The three proposed functions are unchanged across `b21006b..5b9e23a`. Reconciliation of the 24 later commits through `e8f3322` found only room-federation/startup/test wiring adjacent to the mapped seams. A second clean rebase over 15 Observatory/session-config commits through `a517f6c` changed adjacent `AppState`, event-bus, startup, and router composition but not the owner, route mounts, shared Longhouse handle, or any moved definition. Later clean rebases over session-config RPC hardening through `ad391f3` and active-request session-summary work through `9177598` again changed adjacent parent code without changing the extracted owner or definitions. All three definitions remain byte-identical to reviewed rollback source; the focused characterization, convene, router, and SSE groups, all 491 daemon tests, daemon Clippy, formatting, docs, and diff checks passed after the latest rebase. The real convene body changed and its `ocean-longhouse` owner changed heavily before the original manifest, which is another reason not to hide it inside this first move. This extraction must not restore any earlier Longhouse algorithm or convergence path.

Before characterization, authorization, extraction, completion documentation, and publication commits:

1. fetch and rebase onto current `origin/main` in this isolated worktree;
2. reread root, crates, daemon, and docs `AGENTS.md` contracts;
3. inspect every upstream diff touching `main.rs`, `longhouse_topics.rs` once present, `ocean-longhouse::{registry,convene,evidence,planner,quorum}`, route contracts, agent-event publication, or relevant tests/docs;
4. rerun affected characterization whenever overlap exists;
5. reconcile overlapping work rather than restoring this manifest's snapshot.

## Fresh boundary map and narrowing decision

At baseline `5b9e23a`:

- `longhouse_demo` is `main.rs:2282-2480` (199 lines). It clones only `AppState::agent_events` and `AppState::longhouse`, returns an immediate HTTP acknowledgement, and owns a detached scripted event task.
- `longhouse_topics` is `main.rs:4406-4412` (7 lines). It clones all topic snapshots from `AppState::longhouse` and recovers a poisoned read lock.
- `longhouse_topic` is `main.rs:4417-4449` (33 lines). It trims/parses one UUID, recovers a poisoned read lock, and maps present/unknown/invalid results to exact HTTP envelopes.
- `longhouse_convene` is `main.rs:2523-2669` (147 lines), preceded by `LonghouseConveneRequest` and `parse_federation`. It depends on `AppState::{agent_events,longhouse,titles}`, live provider readiness, `ocean_longhouse::convene`, title persistence and secret delivery, and tracing. It is not part of this boundary.
- `longhouse_routes()` is `main.rs:2233` and remains the canonical route composition seam.
- the shared `LonghouseRegistryHandle` is allocated once in startup, cloned into `AgentRuntime::with_extensions`, and stored on `AppState`; the extracted module must receive that same state and must not allocate another registry.

The proposed owner is `longhouse_topics.rs`, not the broader tentative `longhouse_governance.rs`, because it owns only topic observability plus the scripted topic producer. The name must not imply ownership of real convene, quorum policy, titles, escrow, or board control.

## Exact proposed production boundary

Move the following existing definitions together, including their attached implementation comments, from `main.rs` into private `longhouse_topics.rs`:

1. `longhouse_demo`;
2. `longhouse_topics`;
3. `longhouse_topic`.

Do not move intervening definitions. Do not include `longhouse_routes()`, `LonghouseConveneRequest`, `parse_federation`, `longhouse_convene`, any title/control handler, `AppState`, `LonghouseRegistryHandle`, startup assembly, event-bus code, or tests.

No helper extraction or deduplication is authorized. In particular, do not invent a shared ingest-and-publish helper between demo, real convene, and board posting merely to make this module look cleaner.

## Inbound dependencies

The moved bodies may depend only on existing dependencies they already use:

- `axum::{extract::{Path, State}, http::StatusCode, Json}`;
- `serde_json::json!` and `serde_json::Value`;
- `uuid::Uuid`;
- `ocean_agent_sdk::{AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark, MarkKind, ProposalTally}`;
- `tokio::spawn` and `tokio::time::{sleep, Duration}`;
- parent-private `AppState`, including its existing `agent_events` and `longhouse` fields.

No new dependency, feature, trait, state wrapper, registry, queue, cancellation primitive, task manager, clock abstraction, error abstraction, or test-only production seam is authorized.

## Outbound callers and visibility

After extraction:

- parent `main.rs::longhouse_routes()` imports and mounts `longhouse_demo`, `longhouse_topics`, and `longhouse_topic` at the same methods and paths;
- the parent inline test module continues to use the real `longhouse_routes()` and existing `AppState` fixtures;
- no other production caller changes.

The module remains private (`mod longhouse_topics;`). Only the three parent-mounted handlers may become `pub(super)`. Nothing becomes `pub`, `pub(crate)`, library-exported, or externally stable.

## Frozen state, lock, task, and event lifecycle

### Shared authority

- Startup constructs exactly one `LonghouseRegistryHandle` for both runtime extension capability and daemon HTTP projection. Extraction must not create, default, replace, or lazily initialize another registry.
- `ocean-longhouse::LonghouseRegistry` continues to own projection folding, topic serialization, the 256-snapshot cap, closed-topic retention, live-topic non-eviction, and newest-deadline/id ordering. The daemon adapter neither reimplements nor changes those algorithms.
- The event bus remains the live feed. The registry remains the in-memory refresh projection. Neither becomes durable persistence through this move.

### Demo acknowledgement and detached task

- `POST /v1/longhouse/demo` clones the existing bus and registry, mints fresh topic/board IDs, starts one ordinary detached `tokio::spawn`, and immediately returns `200` without joining, cancelling, registering, or supervising that task.
- The exact response remains `{ "ok": true, "topic_id": <uuid>, "streaming_on": "/v1/agent/events" }` with no extra keys.
- The spawned task keeps its existing scripted values, relationships, and delay sequence. It emits 17 events in this order: `TopicConvened`, `Convened`, two proposal `MarkPosted`s, one evidence `MarkPosted`, three endorse `MarkPosted`/`QuorumUpdated` pairs, one inhibit `MarkPosted`, `RoleGranted`, final `QuorumUpdated`, `Converged`, `TopicClosed`, and `RunHealth`.
- Existing sleeps remain 600, 700, 500, 600, 500, three times 450, 500, 600, and 400 milliseconds in their current positions. No clock injection, acceleration, batching, or spawn restructuring is part of the move.
- Every demo event attempts to fold into the shared registry before bus publication. A healthy lock is released before `bus.emit`; no guard crosses a sleep or any await.
- Demo projection currently treats a poisoned registry differently from the read routes: `if let Ok` skips that event's projection but still publishes it live. Extraction must preserve that fail-open live-delivery behavior rather than silently switching to poison recovery.
- The detached task has no shutdown token, JoinHandle registry, panic mapping, retry, or completion event beyond its scripted sequence. This checkpoint does not add one.

### Topic reads

- `GET /v1/longhouse/topics` returns `200 { "ok": true, "topics": [...] }`, including the owner-provided ordering and complete `TopicSnapshot` serialization.
- Both read handlers recover a poisoned registry lock with `into_inner()` and clone snapshots before building the JSON response.
- `GET /v1/longhouse/topics/{topic_id}` trims the path string before UUID parsing.
- Invalid UUID input returns exact `400 { "ok": false, "error": "invalid topic id '<original>'; expected a UUID" }`; the error retains the original extractor string, not the trimmed value.
- A valid unknown UUID returns exact `404 { "ok": false, "error": "no longhouse topic with id '<canonical-uuid>'" }`.
- A known UUID returns exact `200 { "ok": true, "topic": <snapshot> }`.
- No read lock crosses await, event publication, spawn, or JSON response delivery.

### HTTP, route, and SSE ownership

- Parent composition retains exact routes: `POST /v1/longhouse/demo`, `GET /v1/longhouse/topics`, and `GET /v1/longhouse/topics/{topic_id}`.
- Axum's default 404/405 behavior, implicit HEAD handling, static/dynamic precedence, banner/operator-guide parity, tracing/CORS layer order, and all other Longhouse methods remain unchanged.
- The module emits `AgentTurnEvent::Extension` through the existing `AgentEventBus` only by the current `LonghouseEvent::into_turn_event` path. It does not own SSE scoping, global opt-in, replay, lag signaling, keepalive, shutdown, framing, or `TurnCheckpoint` filtering.
- Demo events remain sessionless council-wide events subject to the existing global-opt-in SSE filter outside this module.

## Characterization required before extraction

Keep tests in the parent `main.rs` module because they depend on `AppState`, real router composition, event-bus receivers, and shared state fixtures. Add only the missing daemon characterization:

1. **Real-router method and envelope matrix**
   - exact demo `200` status and three-key response, including parseable returned UUID and fixed stream path;
   - exact GET/PUT `405` plus `Allow: POST` for demo;
   - exact topic-list/detail method behavior through `longhouse_routes()` and unregistered sibling `404` behavior;
   - no request-body or content-type requirement added to demo.
2. **Demo task and event/projection order**
   - subscribe before invoking the real handler, consume the complete 17-event sequence under a bounded test timeout, and freeze event variants, fixed strings/models/roles/federation/tallies, dynamic-ID relationships, and final projection state;
   - after every received topic-scoped event, prove the same event's state is already visible in the shared registry, establishing fold-before-publication;
   - prove acknowledgement does not await the full scripted task;
   - extraction-aware source assertions freeze the existing spawn, sleeps, and no-guard-across-await structure without adding a clock seam.
3. **List/detail envelopes and ordering**
   - empty and populated list exact key sets;
   - owner-provided newest-deadline ordering and deterministic UUID tie-break as observed over HTTP, without duplicating the 256-cap corpus;
   - exact known detail, canonical unknown-UUID `404`, malformed `400`, and whitespace-trimmed valid UUID behavior;
   - complete `TopicSnapshot` field/key shape for the seeded state.
4. **Poison behavior**
   - list and detail recover and return the existing data from a deliberately poisoned projection lock;
   - demo still publishes live when the projection lock is poisoned and does not fabricate a projected topic;
   - no panic or lock guard crosses an await.
5. **Shared-handle and authority source boundary**
   - startup still has one Longhouse registry allocation cloned into runtime extensions and `AppState`;
   - `longhouse_routes()` remains in `main.rs` and mounts imports from the private owner;
   - the proposed owner defines only the three authorized functions and contains no real convene, model/provider, title/escrow/revoker/recall/board, permission, runtime-turn, persistence, room, call, librarian, or SSE authority.

Do not duplicate `ocean-longhouse` registry cap/eviction/folding unit tests, sequential-evidence corpus, planner logic, quorum math, proposal consolidation, structured-answer filtering, or title cryptography in the daemon. Daemon tests freeze only HTTP adaptation, shared-handle usage, lock/error behavior, scripted task lifecycle, and publication ordering.

## Explicit exclusions

This checkpoint does not move or change:

- `longhouse_routes()`, `app_router()`, startup assembly, `AppState`, `LonghouseRegistryHandle`, banner routes, middleware, or operator-guide route ownership;
- `LonghouseConveneRequest`, `parse_federation`, or `longhouse_convene`;
- ready-model filtering, default convene models, provider resolution, `ocean_longhouse::convene`, sequential evidence, planner actions, proposal consolidation, structured `ANSWER` parsing/filtering, convergence basis, abort semantics, or council logging;
- title grant/bind, raw-token response delivery, `SqliteTitleRegistry`, claim, revoke, recall, breach, board mutation, `Revoker`, or escrow;
- the runtime extension `longhouse__convene` / `longhouse__board_read` capability surface or extension-owned subagent orchestration;
- Longhouse preparation/inspect/workflow adapters, turn preparation/presentation, librarian query/fetch, compatibility subagent spec, or their caches/loaders;
- persistent rooms, named-agent binding, room SSE, calls, LiveKit, ordinary agent turns, sessions, permissions, persistence, event buses, SSE, or shutdown;
- route names, response cleanup, status changes, new telemetry, new dependencies, or opportunistic correctness/security fixes.

The real convene handler and its coupled title mint/bind step must be re-manifested with the second security-sensitive governance wave. If that manifest cannot move convene plus title control mechanically without a trait, substate, public API, or changed authority boundary, it must leave convene in composition.

## Deferred security and availability dispositions

This adjacent Longhouse manifest records, but does not freeze as desirable or alter, the program's existing deferred findings:

1. cached librarian skill paths can be retargeted through symlinks before fetch; librarian movement remains blocked on warm-cache and cold-index outside-root tests plus an approved canonical-root/revalidation policy;
2. delegated Longhouse loaders can log root/file paths; redaction belongs to a separate `ocean-longhouse` security change;
3. advisory names/descriptions are rendered into model context unsanitized; escaping or prompt-injection hardening is separate;
4. timed-out Longhouse preparation drops an uncancelled blocking JoinHandle and can amplify queued work behind the process-wide cache lock; cancellation, single-flight, lock restructuring, or work caps are separate behavior changes.

This boundary handles no title token. The second governance manifest must independently prove that raw title tokens remain absent from events, logs, SQLite, snapshots, debug output, and non-converged responses.

## Extraction procedure

1. Commit and obtain fresh boundary/security review of this manifest.
2. Fetch/rebase and reconcile any overlapping upstream changes.
3. Add and commit the pre-move characterization only; run focused and full daemon/Longhouse tests in a dedicated target.
4. Obtain fresh characterization review and record that commit as the rollback point.
5. Update the manifest, mission, code-health plan, daemon owner contract validation index, and root ledger to authorize only the characterized move; commit authorization separately.
6. Add private `mod longhouse_topics;` and minimal parent imports.
7. Move only the three authorized definitions and attached comments; change only required `pub(super)` visibility and rustfmt wrapping.
8. Mechanically compare all moved bodies against the rollback commit.
9. Run the full validation matrix, then obtain separate correctness/mechanical and security/architecture/lifecycle reviews.
10. Record completion evidence, commit, fetch/rebase, rerun affected checks, push, open a PR, wait for hosted macOS/Ubuntu/MSRV/cargo-deny, merge, and publish the merge in a separate docs-only follow-up.

Live daemon deployment, LaunchAgent supervision, process kills, restarts, and binary installation remain with the concurrent operator workstream and are not performed from this refactor worktree.

## Characterization result

Commit `c443df9` (rebased from independently reviewed `c1be830`) adds four extraction-aware daemon characterization tests without moving or changing a production handler:

- `longhouse_topic_projection_http_envelopes_methods_and_order_are_exact` freezes the empty and populated list responses, complete seeded detail JSON, newest-deadline/UUID tie-break order, trimmed known UUID, original-text malformed UUID errors, canonical unknown UUID errors, exact 404/405/`Allow` behavior, and the body/content-type-free immediate demo acknowledgement;
- `longhouse_topic_projection_demo_sequence_and_fold_before_publish_are_exact` drives the real router and bus through all 17 events under the existing delay budget, freezes every event kind/content/topic relationship, generated member/mark/proposal handle independence, exact two-entry interim/final tallies, final projection, actual public `AgentTurnEvent` scope/wire non-disclosure, and absence of an eighteenth event;
- `longhouse_topic_projection_poison_policy_is_exact` proves list/detail recover existing state while a poisoned demo projection skips mutation and still publishes live;
- `longhouse_topic_projection_source_preserves_shared_handle_and_authority_boundary` selects `main.rs` before extraction or `longhouse_topics.rs` afterward, proves the registry lock scope ends before publication and every await, freezes exact delays and poison expressions, rejects owner authority markers/items/extra functions, requires one detached spawn, keeps the single startup registry shared with runtime extensions and `AppState`, keeps all three route mounts in composition, and leaves real convene/title ownership in `main.rs`.

The existing route-source parity parser now ends the `longhouse_routes()` section at the still-parent-owned `LonghouseConveneRequest` comment, so it remains valid before and after the authorized demo move. This is a test-only parser adjustment, not a route or production change.

Focused tests, all 372 daemon tests serialized, 168 Longhouse tests plus one host-dependent ignore and one doc test, denied-warning daemon all-target Clippy, formatting, docs, and diff checks passed in dedicated target `/tmp/ocean-target-longhouse-topics-char`. Existing convene alias/model/response, global-opt-in SSE, router-contract, preparation, and turn-preparation groups also passed.

Two independent review rounds first found and then verified fixes for missing empty/full HTTP snapshots, incomplete dynamic-ID/tally coverage, lock-scope proof, whole-owner authority scanning, and lossy token/extra-event checks. Final correctness and security/architecture/lifecycle re-reviews both reported PASS with no unresolved medium-or-higher issue. One non-blocking minor notes that exotic visibility-qualified future item forms are not exhaustively parsed; the authorized move adds no such item, and extraction review must still compare the complete three-function owner mechanically against `c443df9`.

Extraction is authorized only from rebased rollback point `c443df9` and only for the three manifested definitions.

## Extraction result

Commit `0199d57` moves the exact characterized 252-line definition/comment boundary into the 273-line private `crates/ocean-daemon/src/longhouse_topics.rs` owner. `main.rs` changes only the private module/import wiring, moves demo-only SDK types behind `#[cfg(test)]` for unchanged parent characterization, and removes the three definitions. All three route mounts, `AppState`, startup/shared-handle assembly, HTTP/SSE composition, real convene/request/federation parsing/model readiness, and title/control handlers remain in `main.rs`.

Brace-aware comparison against rollback `c443df9` found all three moved definitions and complete attached comments byte-identical after removing only the required `pub(super)` visibility:

- `longhouse_demo`: SHA-256 `fab051ce140bf27465c827c4d08b1908f2dce1343cbff08b0c866e17ded5403a`;
- `longhouse_topics`: SHA-256 `df63548821bf14d4df0d68f053560d217911a0a1ca19bfc0f133850a07029aa1`;
- `longhouse_topic`: SHA-256 `89c70aca569c80f93e183077b4d19be23088761a17b2f665406046e47e9c7142`.

Focused characterization/convene/router/SSE/preparation groups passed. Before the first publication rebase, all 456 daemon tests passed serialized; 168 Longhouse tests and one doc test passed with one host-dependent ignore. Workspace test compilation, `livekit-tap` and `deepgram-stt` checks, denied-warning daemon all-target Clippy, compatibility, pinned Rust 1.88 MSRV, canonical local CI, formatting, docs, and diff checks passed in the dedicated extraction targets. After rebasing onto `a517f6c`, the focused characterization/convene/router/SSE groups, all 483 daemon tests serialized, denied-warning daemon all-target Clippy, formatting, docs, and diff checks passed. After reconciling the opened PR onto `ad391f3`, mechanical comparison, the focused groups, all 488 daemon tests, Clippy, formatting, docs, and diff checks passed. After the latest rebase onto `9177598`, mechanical comparison, the focused groups, all 491 daemon tests serialized, denied-warning daemon all-target Clippy, formatting, docs, and diff checks passed again.

Fresh independent correctness/mechanical and security/architecture/lifecycle reviews both reported PASS with no unresolved medium-or-higher issue. They confirmed exact three-item ownership and visibility, one unchanged detached 17-event task, lock-end-before-publication/await, poison asymmetry, exact HTTP responses, one composition-owned registry shared with runtime extensions and `AppState`, and no title token, model, permission, persistence, route, SSE, runtime, call, registry-construction, or broader governance authority in the new owner. The prior non-blocking source-inventory parser limitation did not affect review because complete definitions and whole-owner contents were compared directly.

Live daemon deployment/supervision remained owned by the concurrent operator workstream and was not performed. Hosted CI run `29615843464` passed the default-parallel macOS and Ubuntu repository/feature/release gates, pinned Rust 1.88 MSRV, and cargo-deny. PR #305 merged the exact reviewed head `ba1a599` over base `9177598` as merge commit `9676b18`.

## Validation matrix

Use a dedicated `CARGO_TARGET_DIR` and serialize environment/state-sensitive daemon tests.

Focused characterization and route groups:

```bash
cargo test -p ocean-daemon longhouse_topic_projection_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon council_wide_extension_event_is_global_opt_in_only -- --nocapture
cargo test -p ocean-daemon convene_rejects_aliases_missing_from_live_ready_registry -- --nocapture --test-threads=1
cargo test -p ocean-daemon council_convene_is_a_live_alias_of_longhouse_convene -- --nocapture --test-threads=1
cargo test -p ocean-daemon convene_route_response_includes_converged_flag -- --nocapture --test-threads=1
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon longhouse_preparation_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_turn_preparation_ -- --nocapture --test-threads=1
```

Completion gates:

```bash
cargo test -p ocean-daemon -- --test-threads=1
cargo test -p ocean-longhouse -- --test-threads=1
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Default-parallel hosted macOS/Ubuntu repository gates, pinned Rust 1.88 MSRV, and cargo-deny passed in CI run `29615843464` before PR #305 merged as `9676b18`.

## Review requirements

Before characterization, a fresh boundary/security reviewer must confirm:

- narrowing real convene out is required by its direct title/model/orchestration coupling;
- demo plus list/detail form a cohesive existing-handle topic-observability boundary;
- the exact three-symbol inventory, dependencies, visibility, and exclusions are complete;
- the proposed tests freeze acknowledgement, event/projection order, poison asymmetry, shared-handle identity, and HTTP envelopes without introducing production seams;
- current sequential-evidence/planner/structured-answer/convergence semantics remain outside and untouched;
- no token, title, model, permission, persistence, turn, room, call, or SSE authority enters the module.

Before extraction acceptance, separate fresh reviewers must:

- mechanically compare every moved body and attached comment against the rollback commit;
- inspect every import and visibility change;
- verify the exact scripted sequence, delays, IDs, fold-before-publish order, poison behavior, and topic HTTP envelopes;
- verify startup still shares one registry with runtime extensions and parent state;
- verify route composition and all excluded governance/control paths remain in `main.rs`;
- report any unresolved medium-or-higher correctness, security, lifecycle, or architecture issue.

## Manifest review result

A fresh read-only boundary/security review against `5b9e23a` reported PASS with no blocker, major, minor, or unresolved medium-or-higher issue. It independently confirmed the three-function scope, title-coupled convene exclusion, exact 17-event/sleep sequence, fold-before-publication and poison asymmetry, single shared registry, parent-owned HTTP/SSE composition, characterization matrix, rollback, and moving-baseline rule. It also verified the three candidate bodies are byte-identical from `b21006b` through `5b9e23a`.

Review validation passed `cargo xtask docs-check`, `git diff --check`, all five `router_contract` tests, and `council_wide_extension_event_is_global_opt_in_only` in a dedicated target. This initial review authorized characterization only; accepted characterization and extraction authorization are recorded above at final rebased rollback `c443df9` (originally reviewed as `c1be830`).

## Stop rules

Stop and request a design decision if:

- the three-function move requires a shared service/emitter trait, new state wrapper, public visibility, or another registry;
- characterization shows demo and topic reads do not share the mapped lifecycle or authority boundary;
- preserving the detached task, fold-before-publish order, poison asymmetry, or HTTP envelopes requires executable changes;
- concurrent work touches the candidate functions, startup shared-handle assembly, event bus, route table, or projection owner without successful reconciliation;
- any reviewer finds unresolved title-token, lock/await, task-leak, event-order, or route-contract risk.

Do not expand this manifest to real convene merely to keep the tentative handoff name or to maximize moved lines.

## Rollback

Rollback is one revert of the mechanical extraction commit after `c443df9`: restore the three definitions and comments to `main.rs`, remove the private module/imports, and leave the characterization tests in place. No schema, wire version, persistence, title, event format, or external API migration is part of this checkpoint.
