# Ocean Daemon Phase 2C — Final 28% Handoff

**Date:** 2026-07-16  
**Status:** Active execution handoff  
**Published Phase 2C checkpoint:** merge `b21006b` (PR #300)  
**Current source baseline at handoff:** `origin/main` `4712fdb`
**Scope:** Remaining behavior-neutral extraction work in `crates/ocean-daemon`

## Purpose

This is the cold-start handoff for finishing the estimated final 28% of daemon Phase 2C. The estimate is complexity-weighted, not a raw line-count calculation. Most leaf adapters and mechanical registries are published; the remaining work contains the most coupled state, security, lifecycle, feature-gated call, and turn/SSE seams.

The required order is:

1. Longhouse governance, in two separately manifested waves;
2. call HTTP/webhook/token adaptation;
3. call runtime/persistence bridge;
4. a fresh inventory of any genuinely unowned registry/control-plane mechanics;
5. agent-turn/SSE orchestration last, split only where characterization proves a cohesive private boundary.

Do not treat this document as authorization to redesign. Each wave still needs its own extraction manifest, committed characterization, authorization checkpoint, exact mechanical move, full validation, independent review, PR, hosted CI, merge, and publication follow-up.

## Current published baseline

The worktree used by the prior program was `/tmp/ocean-daemon-phase2c-next`. Merge `b21006b` is the last published Phase 2C extraction checkpoint, not the current tip. At final handoff review, the clean current source baseline is `origin/main` `4712fdb`; always fetch again because it may advance.

Latest Longhouse checkpoints:

- state-free preparation HTTP adapters: PR #296, merge `29d65f8`;
- their publication follow-up: PR #297, merge `4fdebed`;
- turn preparation/presentation: PR #299, merge `9095d5a`;
- its publication follow-up: PR #300, merge `b21006b`.

The turn-preparation characterization rollback point is `f6e8efe`; extraction is `4eea76b`. The exact 228-line implementation boundary now lives in the 246-line private `src/longhouse_turn_preparation.rs` module. All three call sites remain in `main.rs`.

Other published stateful checkpoints that must not be reopened casually:

- request/permission control: PR #286, `ee3860a`;
- recall tally registry: PR #287, `3e051c1`;
- persistent rooms: PR #293, `92e03bf`;
- persistent-room publication: PR #294, `dc44343`.

Use `docs/DAEMON_REFACTOR_MISSION.md` for the complete checkpoint table and each linked manifest for exact evidence.

### Upstream reconciliation after `b21006b`

The source advanced substantially before this handoff closed. Changes through `4712fdb` include correlation-aware sequential evidence, uncertainty-driven review planning, active convergence integration, proposal consolidation, structured `ANSWER` validity filtering, Longhouse compliance/leak fixes, named-agent persistent-room binding, and durable persistent-room SSE tailing/disconnect cleanup. They changed `ocean-longhouse::{convene,quorum,evidence,planner}`, daemon `main.rs`, and `persistent_rooms.rs`.

These changes are current behavior and supersede old scout assumptions. The next governance manifest must begin from a fresh symbol/behavior map at current `origin/main`; it must preserve sequential-evidence/planner/structured-answer semantics and must not restore the pre-`b21006b` convergence path. Persistent-room SSE and named-agent behavior are published adjacent contracts, not governance extraction scope.

## Non-negotiable program rules

Before every manifest, characterization, authorization, extraction, completion-doc, and publication checkpoint:

1. fetch `origin`;
2. rebase or restart from current `origin/main` in an isolated worktree;
3. read root `AGENTS.md`, `crates/AGENTS.md`, `crates/ocean-daemon/AGENTS.md`, and `docs/AGENTS.md` completely;
4. inspect every upstream diff touching the candidate seam, its owner crate, tests, routes, or docs;
5. rerun affected characterization when overlap exists;
6. reconcile concurrent changes rather than restoring an older snapshot.

Every extraction remains binary-private. Do not introduce:

- a daemon library;
- public daemon APIs;
- `AppState` substates;
- service traits or `dyn` conversions;
- generated routing;
- new dependencies;
- protocol renames or response cleanup;
- opportunistic correctness or security fixes;
- extension-owned named-subagent dispatch, scheduling, or lifecycle in core.

Use one writer for the active worktree. Use fresh read-only reviewers around that writer. Keep tests in `main.rs` when they depend on parent fixtures, state factories, environment locks, or real router composition.

Use a dedicated `CARGO_TARGET_DIR` for every wave. Shared targets previously produced stale-artifact failures. Remove only the wave's own target when evidence no longer needs it.

Live daemon deployment, LaunchAgent supervision, process kills, restarts, and binary installation remain with the concurrent operator workstream. Do not deploy from this refactor workstream.

## Standard wave protocol

Use this sequence for every remaining boundary.

### 1. Manifest

The manifest must name:

- exact symbols and source owner;
- inbound dependencies;
- outbound callers;
- required `pub(super)` visibility;
- state/lock/task/persistence lifecycle;
- HTTP/SSE/event/prompt ordering;
- feature-gated branches;
- exact exclusions;
- characterization gaps;
- validation matrix;
- rollback procedure;
- moving-baseline reconciliation rule;
- reviewer requirements.

Commit and review the manifest before production movement.

### 2. Characterize before moving

Prefer real router/handler tests, exact JSON/status/header assertions, event ordering, state snapshots, and extraction-aware source assertions. Do not add production abstractions only to make a mechanical extraction testable.

Commit characterization independently. Run focused tests and the full daemon suite in a dedicated target. Obtain fresh review. Record the accepted characterization commit as the rollback point.

### 3. Authorize

Update the manifest, mission, code-health plan, nearest `AGENTS.md` validation index, and root `events.md`. State exactly what has been frozen and what remains outside. Commit this checkpoint before moving production code.

### 4. Extract mechanically

Move only the authorized bodies and attached comments. Required visibility changes are `pub(super)` only. Parent route composition, `AppState`, startup assembly, and orchestration stay unless the manifest explicitly characterized them.

Mechanically compare moved definitions against the rollback commit. Normalize only required visibility and rustfmt wrapping. Do not accept unexplained body differences.

### 5. Validate and review

Minimum completion matrix:

```bash
cargo test -p ocean-daemon -- --test-threads=1
cargo test -p ocean-longhouse -- --test-threads=1   # Longhouse waves
cargo test -p ocean-call                            # call waves
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo xtask ci --compatibility
# Ensure the Rust 1.88 toolchain bin directory is first in PATH locally:
cargo xtask ci --msrv
cargo xtask ci
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Run focused commands named by the manifest first. Require at least one correctness/mechanical review and one security/architecture/lifecycle review for governance, calls, or turn/SSE changes.

### 6. Publish

Update ownership docs and completion evidence, commit, fetch/rebase, rerun affected checks, push, open a PR, wait for hosted macOS/Ubuntu/MSRV/cargo-deny, merge, then make a separate docs-only publication PR recording merge commit and hosted run.

Keep `events.md` append-only and schema-valid: use a branch/ref in `worktree:`, an enumerated `type:`, and an enumerated `area:`.

## Wave 1 — Longhouse topic/convene governance

### Goal

Extract the read-side topic projection plus demo/convene HTTP adaptation into one private module, tentatively `longhouse_governance.rs`, only if fresh mapping confirms cohesion. Do not combine title escrow, recall, breach, or board mutation in the first governance move.

### Candidate symbols

Re-resolve current line numbers with `rg`; old scout line numbers predate several extractions. Candidate names include:

- `longhouse_demo`;
- `LonghouseConveneRequest`;
- `parse_federation`;
- `longhouse_convene`;
- `longhouse_topics`;
- `longhouse_topic`;
- the narrow lock/projection helpers used only by these handlers.

Keep `longhouse_routes()` in composition for the first move.

### Critical invariants to characterize

- The same `LonghouseRegistry` handle is shared by daemon projection and runtime extension capability; never create a second registry.
- Convene model filtering uses only current ready/resolvable models and preserves exact fallback/error envelopes.
- Topic projection is folded before the corresponding bus publication where current code does so.
- Federation parsing, defaults, malformed input, limits, convergence flags, response key sets, and council alias behavior remain exact.
- Title mint/bind behavior invoked by convene must remain in its current owner if it cannot be excluded cleanly. If convene cannot move without title authority, narrow the first wave further or explicitly include the coupled title step in the second governance manifest; do not hide cross-module mutation.
- No mutex guard crosses `.await` or event publication.
- No subagent dispatch or execution authority enters the module.

### Tests to add before moving

- exact malformed/empty/default convene HTTP matrix;
- ready-model filtering and fail-soft behavior;
- topic projection versus event ordering;
- poison recovery/error mapping for the projection lock;
- list/detail unknown-topic and ordering envelopes;
- council alias parity;
- exact absence of title token from persistence, logs, and events.

### Stop rule

If convene cannot be separated from title mint/bind without a new interface, do not invent a service trait. Manifest the existing coupled boundary honestly or leave convene in composition and move only topic read projection.

## Wave 2 — Longhouse title/escrow/recall/breach/board governance

### Goal

Extract the security-sensitive daemon adapter over `SqliteTitleRegistry`, daemon-held `Revoker`, the already-extracted recall tally registry, and board projection. Tentative owner: `longhouse_governance_control.rs`. This is a separate manifest and review from Wave 1.

### Candidate symbols

Re-resolve, then evaluate together:

- title lock helpers and title DB-path adapter if still in `main.rs`;
- claim request/handler;
- revoke request/handler;
- recall request/handler and recall-to-revocation mapping;
- breach request/handler;
- board mark request/handler;
- `parse_board_mark_kind`;
- narrow helpers used only by these operations.

Keep startup DB opening, `AppState::{titles,revoker,recalls,longhouse}`, route mounting, and secret construction in composition unless a manifest proves a smaller exact move.

### Security invariants

- Only a verifier is persisted; raw title tokens exist only where currently returned and never enter logs, events, SQLite, snapshots, or debug output.
- Claim identity/token verification precedes decision comparison and preserves constant-time owner behavior from `ocean-longhouse`.
- Revoker authority remains daemon-held and non-serializable. Decide and execute remain separate.
- Recall counts distinct credentialed voters, preserves the first threshold, clamps zero in the owner engine, and removes a tally only after successful revocation.
- Pending, unknown, revoked, released, wrong-token, wrong-decision, `NotLive`, and execution-failure responses remain exact.
- Board mutation remains topic-scoped and follows current projection/event ordering.
- No lock crosses SQLite work, Revoker execution, event publication, JSON creation, or await.

### Characterization priorities

- complete exact status/body matrix for claim/revoke/recall/breach/board;
- token non-disclosure across response variants, logs, events, and DB dump;
- restart persistence and revoked-title rejection;
- recall threshold/distinct voter/successful-only cleanup;
- breach strike thresholds and graduated versus hard revocation;
- board mark parsing, unknown topic, mutation, and ordering;
- poison recovery for every moved lock adapter.

### Exclusions

Do not change escrow cryptography, SQLite schema, quorum/recall algorithms, title lifecycle states, route paths, or extension ownership. Any security hardening beyond exact current behavior requires a separate approved change.

## Wave 3 — Call HTTP/webhook/token adapters

### Goal

Move the state-light call request/webhook/media-token HTTP boundary first, leaving call runtime/session/persistence orchestration in `main.rs`. Tentative owner: `call_http.rs`.

### Candidate symbols

- `call_demo`;
- `PlaceCallRequest` and `call_place`;
- `webhook_action_to_event` and `call_webhook`;
- `resolve_publish_grant`;
- `call_room_token_allowed`;
- `room_livekit_token`.

Keep route mounting in `app_router`/`room_routes`. The LiveKit token route remains an independent media contract; do not merge it with durable-room APIs.

### Critical invariants

- Webhook authenticity verification happens before mapping, event emission, room mutation, or spawn.
- Invalid authentication has exact fail-closed status/body and zero side effects.
- Duplicate webhook behavior and call-room create/close semantics remain exact.
- Place-call validation precedes credential checks in the current order; missing-credential responses name the same environment fields.
- `call:` token minting requires a known open persistent room. Closed/unknown call rooms fail exactly; non-call room IDs retain current passthrough behavior.
- Publish permission is secret-gated and defaults fail-closed. Header grammar and comparison remain exact.
- No telephony/media/session task lifecycle moves in this wave.

### Required tests

- webhook auth failure and success side-effect footprints;
- every webhook action mapping and unknown action behavior;
- duplicate delivery behavior;
- place-call malformed number, blank number, missing credentials, and accepted envelope order;
- token blank room, unknown/open/closed call room, non-call room, missing credentials, publish denied/granted;
- static/dynamic room-route precedence and 404/405 behavior.

## Wave 4 — Call runtime and persistence bridge

### Goal

Move the cohesive call lifecycle bridge only after Wave 3. Tentative owner: `call_runtime_bridge.rs`. If the manifest cannot prove one cohesive lifecycle boundary, leave it in composition rather than introducing traits or substates.

### Candidate symbols

- call persistence job/retry mechanics still in `main.rs`;
- `BusSink` and its `ocean_call::EventSink` implementation;
- `DaemonTurnRunner` and `ocean_call::TurnRunner` implementation;
- `spawn_call_session`;
- `state_emit_call_ended`;
- `call_voice_muted`;
- directly coupled helper types/constants.

### Lifecycle invariants

- The existing shared persistent-room handle remains the only call transcript authority.
- Successful hot-path order stays persistence before live SSE emission where currently implemented.
- Retry only the existing transient DB class, with identical attempt count/backoff/drop accounting. Live delivery remains independent of persistence failure.
- Every call-session exit emits at most/exactly the currently characterized end event; no duplicate close or missing end path.
- Call rooms retain `call:<uuid>` identity and closed-room audit replay.
- `DaemonTurnRunner` remains `HarnessProfile::Voice`, `yolo: false`, and `PromptControl::without_tools()` regardless of global permission posture.
- Caller cwd/project/session fallback behavior remains exact; do not broaden the legacy neutral-cwd exception.
- Muted mode, cancellation, task ownership, JoinHandle behavior, and shutdown semantics remain exact.
- `livekit-tap` and `deepgram-stt` branches compile and preserve behavior.

### Required characterization

- `BusSink` persist/retry/drop/event ordering for every call event class;
- transient versus non-transient persistence errors;
- final transcript segments/summaries and room close behavior;
- every `spawn_call_session` exit, cancellation, panic/error, and end-event footprint feasible without new production seams;
- muted mode;
- voice runner no-tools/permission posture;
- shared-room poison recovery and no-lock-across-await;
- feature-gated compile and focused tests.

### Stop rule

Do not create a daemon `CallService`, async trait, queue redesign, or durable retry scheduler. If exact lifecycle movement requires redesign, keep the bridge in `main.rs` and proceed to a smaller proven helper boundary.

## Wave 5 — Remaining registries/control plane inventory

Recall, request control, persistent rooms, component waits, Canvas fulfillment, and event-bus storage already have owners. Do not assume another registry wave exists merely because the mission table says “remaining registries.” Perform fresh symbol/state inventory after governance and calls.

A candidate qualifies only if it has:

- a distinct stored record or handle;
- cohesive synchronous lifecycle mechanics;
- bounded callers;
- no need to move policy, route, event, or orchestration authority;
- meaningful navigation value after extraction.

Do not extract per-turn permission dedupe, immutable startup state, or one-off local maps merely to reduce line count. If no qualifying registry remains, document the inventory result and close this item without code movement.

## Wave 6 — Agent-turn and SSE orchestration, last

### First action: write a new decomposition manifest

Do not attempt one giant move. Re-scout current `main.rs` after all preceding merges and map:

- ordinary `/v1/prompt` and asynchronous request creation;
- `/v1/agent/turns` orchestration;
- request registration, cancellation, permission policy, prompt-control construction, runtime invocation, persistence/result recording, and advisor scheduling;
- runtime-to-agent-event bridge;
- legacy mirror publication;
- SSE filtering, replay, lag signaling, keepalive, shutdown, and framing;
- active-turn/read-side projections.

### Preferred order inside the last domain

Only if characterization proves these boundaries:

1. pure event-scope/filter/framing helpers;
2. SSE replay selection and stream termination adapter, leaving bus ownership in `bus.rs`;
3. prompt-control/request registration composition helpers that do not own policy;
4. ordinary prompt and create-request shared mechanics without moving their different acknowledgement timing;
5. agent-turn orchestration last.

Do not force this order if current code coupling disproves it.

### Frozen contracts

- exact HTTP methods, paths, statuses, JSON, Axum 404/405, CORS/tracing order;
- caller cwd resolution and resumed-session rebinding;
- request IDs, decision-token verification, duplicate-ID behavior, handle attachment, waiter consumption quirks, GC, cancellation, and shutdown drain;
- permission posture and inert request `yolo`;
- user-message, `TurnStarted`, runtime deltas, checkpoints, final result, advisor, and finish ordering;
- `TurnCheckpoint` filtering from SSE;
- session scoping, explicit global opt-in, full replay, Last-Event-ID behavior, 2,048-event/32 MiB bounds, oversized-live-only behavior, lag signal, keepalive, and shutdown termination;
- create-request immediate acknowledgement versus ordinary prompt blocking response;
- agent-turn prompt-layer order, model/profile selection, tools/capabilities, and persistence;
- no public service architecture or new session model.

### Characterization before any move

Build an exact event timeline for success, provider error, permission wait/allow/deny, cancellation, saturation, runtime panic/error, SSE reconnect/replay, subscriber lag, and graceful shutdown. Freeze both state snapshots and emitted wire sequence. Use extraction-aware source assertions only for call positions that cannot be driven deterministically without redesign.

This is the highest-risk portion of Phase 2C. A safe decision to leave a tightly coupled orchestrator in `main.rs` is preferable to hiding it behind an artificial module. The mission target is composition-focused code, not a mandatory line threshold.

## Deferred security dispositions

These are not behavior-neutral extraction work:

1. **Librarian symlink retarget/TOCTOU:** cached indexed skill paths can be replaced by symlinks before fetch. Before moving `skills_query`, `skills_fetch`, or compatibility `subagent_spec`, require warm-cache retarget and cold-index outside-root symlink tests plus an approved canonical-root/revalidation policy.
2. **Longhouse loader path logs:** delegated loaders can log root/file paths. Any redaction policy is an `ocean-longhouse` security change.
3. **Unsanitized advisory text:** names/descriptions are rendered into model context as currently stored. Escaping or prompt-injection hardening is separate.
4. **Timed-out blocking work amplification:** timed-out Longhouse preparation drops its JoinHandle but cannot cancel the blocking task; a cold/stale load can hold the global cache mutex while later tasks queue. Single-flight, lock restructuring, cancellation, or work caps are separate behavior changes.

Document these in every adjacent manifest so they are neither lost nor accidentally frozen as desirable behavior.

## Known behavior that must remain frozen

- request duplicate IDs are last-write-wins;
- mismatched permission waiters are consumed before ownership validation;
- attaching a JoinHandle to an unknown request detaches rather than aborts;
- recall abandoned tallies remain memory-only and unbounded;
- persistent-room successful post order remains persisted author row → global event → best-effort audit attempt → spawn;
- a concurrent room post may receive a sequence between trigger and audit because the initial lock is dropped before event/audit;
- caller-submitted/resumed turns never fall back to daemon cwd;
- legacy internal auto-convene for an unbound persistent room retains its neutral daemon-cwd compatibility exception;
- production permission posture remains operator-controlled; clients cannot self-enable it;
- Slack transport/API behavior remains extension-owned.

Do not “clean up” these quirks inside extraction commits.

## Practical restart checklist for the next session

```bash
cd /path/to/ocean-os
git fetch origin
git worktree add /tmp/ocean-daemon-phase2c-governance -b pi/daemon-longhouse-governance-YYYYMMDD origin/main
cd /tmp/ocean-daemon-phase2c-governance

git status --short
git log -3 --oneline --decorate
```

Then:

1. read the four applicable `AGENTS.md` files;
2. read `docs/DAEMON_REFACTOR_MISSION.md`, this handoff, the two published Longhouse manifests, and relevant `ocean-longhouse` owner code/tests;
3. run `rg` for current governance symbols and create a fresh line/symbol map;
4. inspect `git log -p b21006b..origin/main -- crates/ocean-daemon/src/main.rs crates/ocean-longhouse crates/ocean-daemon/src/persistent_rooms.rs`, then inspect any newer delta since `4712fdb`;
5. propose the first governance manifest only;
6. obtain boundary/security review before characterization;
7. continue through the standard wave protocol without asking for repeated approval unless a real design decision or blocker appears.

## Completion definition for Phase 2C

Phase 2C is complete when:

- every moved domain has a published manifest and accurate owner contract;
- governance and calls have private owners or a documented reviewed reason to remain composition-owned;
- remaining registry inventory is closed;
- safe SSE/turn helpers are extracted while irreducible orchestration remains explicit;
- `main.rs` is composition-focused even if it remains above the aspirational 500-line target;
- full local and hosted gates pass;
- fresh reviewers acknowledge all logic/security/protocol/architecture changes;
- deployment status is handed to the operator workstream;
- the tree is clean and synchronized with `origin/main`.

The final 28% should be judged by reduced rediscovery and preserved authority boundaries, not by maximizing lines moved.
