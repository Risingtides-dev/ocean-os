# Ocean Daemon Longhouse Turn Preparation Extraction Manifest

**Date:** 2026-07-15  
**Status:** Proposed; pre-move characterization and independent review required  
**Owner:** Ocean OS  
**Rollback point:** Pending; set to the accepted characterization commit before any production move

## Purpose

Extract only the daemon's state-free Longhouse turn-preparation and model-facing presentation helpers from `crates/ocean-daemon/src/main.rs` into one private binary module, `crates/ocean-daemon/src/longhouse_turn_preparation.rs`, without changing behavior.

This checkpoint is the second deliberately narrow Longhouse wave. The published `longhouse_preparation.rs` module continues to own only prepare/inspect/workflow HTTP adaptation. This module will own the fresh per-turn environment gate, deterministic advisory rendering/application, and cached read-only preparation under the existing blocking-task deadline. Parent composition keeps all three call sites and their exact orchestration order.

The result remains a private module of the `ocean-daemon` binary. It is not a daemon library, public API, service layer, turn service, substate, capability provider, or extension runtime.

## Current upstream and reconciliation rule

This manifest starts from fetched `origin/main` merge `c21f45a`, which includes:

- published Longhouse preparation adapters through PR #296 and publication PR #297;
- the standalone, unwired `ocean-minimizer` M1 from PR #298.

PR #298 adds a separate workspace crate and touches no daemon or Longhouse production seam. It changes workspace/index validation and is therefore part of the completion-gate baseline, but it does not alter this extraction boundary.

Before characterization, authorization, extraction, completion documentation, and publication commits:

1. fetch and rebase onto current `origin/main`;
2. reread root, crates, daemon, and docs `AGENTS.md` contracts;
3. inspect every upstream diff touching `crates/ocean-daemon/src/main.rs`, `crates/ocean-daemon/src/longhouse_preparation.rs`, `crates/ocean-longhouse`, turn prompt composition, caller cwd, permissions, events, or tests;
4. rerun affected characterization whenever those seams changed;
5. reconcile overlapping work rather than restoring this manifest's starting snapshot.

## Exact approved production boundary

Move the following existing definitions together, with their attached implementation comments, from `main.rs` into private `longhouse_turn_preparation.rs`:

1. `longhouse_prepare_enabled`;
2. `render_longhouse_prep`;
3. `apply_longhouse_prep`;
4. `LONGHOUSE_PREP_DEADLINE`;
5. `longhouse_prep_for_turn`.

At baseline `c21f45a`, the environment gate is near `main.rs:576`; the deterministic renderer/application pair is near `main.rs:6480`; and the deadline plus asynchronous preparation helper are near `main.rs:6691`.

These five items are one advisory pipeline: resolve the fresh opt-out gate, prepare a compact cached `TurnPrep`, render it as model-facing text, and prepend it without mutating the task. The non-contiguous source placement does not authorize moving intervening route, guidance, browser-context, SSE, or request code.

## Inbound dependencies

The extracted module may depend only on existing dependencies already used by the moved bodies:

- `std::env`;
- `std::time::Duration`;
- `ocean_longhouse::{SkillRoots, TurnBrief, TurnPrep, cached_index_for}` or the existing equivalent qualified paths;
- `tokio::task::spawn_blocking`;
- `tokio::time::timeout`;
- `tracing::warn!`.

No new dependency, feature, configuration snapshot, trait, state wrapper, cache, cancellation primitive, task manager, error abstraction, or test injection seam is authorized.

## Outbound callers and exact visibility

Parent `main.rs` retains exactly three production turn call sites:

1. the ordinary `/v1/prompt` path prepares from `req.prompt` and `req.cwd`, then rewrites `req.prompt` before `build_prompt_control` and runtime dispatch;
2. the asynchronous create-request path performs the same preparation inside the already-spawned, permit-owning task, preserving immediate HTTP acknowledgement timing;
3. `agent_turn` ranks the original operator `prompt`, applies the brief to the already-composed `guided_prompt`, and only then layers browser context.

The sibling HTTP adapter `longhouse_preparation.rs` retains its diagnostic-only `consult_enabled` projection and must import `longhouse_prepare_enabled` from the new module. It does not become a turn caller.

The module remains private (`mod longhouse_turn_preparation;`). Required visibility is limited to `pub(super)`:

- `longhouse_prepare_enabled` for the sibling HTTP adapter and retained parent tests;
- `apply_longhouse_prep` and `longhouse_prep_for_turn` for parent composition;
- `render_longhouse_prep` and `LONGHOUSE_PREP_DEADLINE` only because focused characterization tests remain in the parent test module and reuse parent fixtures/locks.

Nothing becomes `pub`, `pub(crate)`, re-exported from a library, or externally stable.

## Characterization required before extraction

Pre-move tests must freeze the behavior that a mechanical move could otherwise obscure:

1. **Exact advisory presentation**
   - complete header text, Unicode em dash, bullet syntax, and newline layout;
   - deterministic skill → SOP → workflow order;
   - trimmed descriptions, omission of a blank description suffix, and current unsanitized embedded content behavior;
   - no source path, full skill/workflow body, cwd, session, or client metadata in the rendered block.
2. **Byte-preserving application**
   - `None` and empty prep return prompts byte-for-byte, including empty/whitespace/newline-rich prompts;
   - non-empty prep is exactly `{rendered block}\n\n{original prompt}` without trimming or rewriting the task.
3. **Fresh environment truth table**
   - unset is enabled;
   - only trimmed, ASCII-case-insensitive `0`, `false`, `no`, and `off` disable;
   - empty, recognized-on, and unknown values remain enabled;
   - environment mutation remains serialized and restored.
4. **Cwd, cache, blocking, and fail-open structure**
   - disabled and whitespace-empty prompt return before any blocking work;
   - exactly empty cwd selects `SkillRoots::default`; non-empty cwd, including whitespace, uses `SkillRoots::for_cwd` without process-cwd fallback or new canonicalization;
   - cache lookup and ranking remain together inside one `spawn_blocking` closure;
   - timeout wraps the join handle at exactly 250 ms;
   - timeout drops the join handle and leaves the read-only blocking task running, while timeout, join failure, empty result, and irrelevant result all return `None`;
   - the helper-owned timeout/join warnings remain fixed-field and exclude prompt, cwd, path, session, and selected content; delegated `ocean-longhouse` loader logs currently can include root/file paths and are recorded separately below.
5. **Call-site orchestration**
   - exactly three production preparation/application pairs remain in `main.rs`;
   - ordinary prompt preparation remains before prompt-control/runtime dispatch;
   - asynchronous create-request preparation remains inside the spawned task after permit transfer so HTTP acknowledgement is not delayed;
   - `agent_turn` ranks the original prompt, applies to guided prompt, and precedes browser-context layering;
   - no await, permit, request registration, cancellation, event, persistence, model, or permission boundary moves.
6. **Boundary rejection**
   - the proposed owner contains no Axum route/extractor, `AppState`, runtime/model/provider/capability/permission handle, event/SSE publisher, session persistence, governance, call/LiveKit, librarian fetch/query/spec, or ordinary `tokio::spawn` authority.

Existing tests already cover relevant planted-skill selection, default-on and explicit opt-out behavior, empty/irrelevant fail-open results, broad rendering/application shape, guidance layering, and a synthetic timeout shape. New characterization should add exact golden coverage and extraction-aware structural/call-site assertions rather than inventing test-only production abstractions. Actual `spawn_blocking` panic and deterministic cold-filesystem overrun remain impractical to drive without redesign; exact source/body comparison and structural assertions are the accepted behavior-neutral guard for those branches.

Do not duplicate `ocean-longhouse` ranking, root-discovery, cache, TTL, parser, scorer, tie-break, or fixture tests in the daemon.

## Frozen behavior and ordering

### Gate and selection

- `OCEAN_LONGHOUSE_PREPARE` is read fresh for every helper invocation.
- The gate defaults on and uses only the current explicit opt-out spellings.
- Disabled and whitespace-empty prompt paths return `None` without loading or ranking.
- The already-resolved caller cwd string is passed unchanged; the helper never calls `current_dir` and never substitutes daemon launch cwd.
- `SkillRoots`, cached-index policy, ranking, relevance, caps, and `TurnPrep` construction remain owned by `ocean-longhouse`.

### Blocking and deadline

- Filesystem/cache/ranking work remains wholly inside one `spawn_blocking` task.
- The entire join is bounded by `Duration::from_millis(250)`.
- Timeout does not abort or await the blocking task; its ignored read-only result may warm the shared cache for a later turn.
- Each helper invocation fails open to no injected brief after the existing bound, but a timed-out cold/stale load can continue while holding the process-wide Longhouse cache mutex; later turns can enqueue additional blocking tasks behind it. This existing process-availability risk is retained and deferred, not described as harmless or corrected by this extraction.
- Every error/empty/slow branch returns no brief without introducing a turn error, retry, audit, permission decision, persistence side effect, or helper-owned metric/event.
- No new event, metric, retry, audit, permission decision, or persistence side effect is added.

### Presentation

- The advisory header remains exact and explicitly says recommendations are not granted capabilities and normal permission gates remain authoritative.
- Skills, SOPs, and workflows retain their current order and `- name — description` presentation.
- Names/descriptions remain rendered exactly as today; sanitization or prompt-injection hardening would be a separate security change.
- `None`/empty prep preserves the input bytes; a non-empty brief is prepended with exactly one blank line.

### Parent orchestration

- `/v1/prompt`, asynchronous request creation, and `agent_turn` retain their exact await/call positions.
- The async request acknowledgement remains independent of preparation latency because preparation stays inside its spawned task.
- Agent-turn preparation continues to rank raw operator text while presentation wraps the already-guided prompt before browser context.
- Route behavior, HTTP/SSE envelopes, event order, request registration/cancellation, persisted transcript/model input, prompt-control policy, permission authority, and runtime invocation remain composition-owned and unchanged.

## Explicit exclusions

Do not move or alter:

- `longhouse_routes()` or the published prepare/inspect/workflow HTTP adapter bodies;
- librarian `skills_query`, `skills_fetch`, compatibility `subagent_spec`, or their deferred symlink-retarget security disposition;
- Longhouse governance, convene, titles, escrow, recall/revocation, federation, boards, or compatibility subagent metadata;
- any `ocean-longhouse` root, loader, cache, TTL, parser, scorer, ranking, relevance, cap, tie-break, or data-model algorithm;
- the three parent call sites, their await positions, turn permits, request/cancel registration, caller-cwd resolution, prompt guidance/browser layering, runtime invocation, events/SSE, persistence, advisor execution, or model selection;
- calls, LiveKit, rooms, `AppState`, startup, middleware, banners, route documentation, or shutdown;
- `ops/`, `deploy/`, daemon installation/restart, LaunchAgent state, deployed binaries, or any action owned by the concurrent operator deployment workstream;
- environment semantics, the 250 ms deadline, warning severity/fields, current unsanitized model-facing text, or fail-open behavior.

This checkpoint does not resolve the cached librarian-path symlink-retarget issue and does not freeze it as acceptable behavior. That remains a separate security disposition before any librarian extraction. Live daemon deployment and supervision remain operator-workstream responsibilities and are not performed by this extraction or its publication follow-up.

## Known retained security and availability risks

This move must describe, but must not opportunistically redesign, two existing behaviors:

1. `longhouse_prep_for_turn` owns only fixed-field timeout and join-failure warnings, which do not include prompt, cwd, path, session, or selected content. The delegated `ocean-longhouse` loaders can independently log absent roots and malformed file paths. This extraction neither adds nor removes those loader logs; any path-redaction policy change belongs to a separate `ocean-longhouse` security checkpoint.
2. Dropping the timed-out `spawn_blocking` join handle does not cancel its task. `cached_index_for` currently holds the process-wide cache mutex across a cold/stale filesystem load, so a stalled load may retain the mutex while later turns enqueue more blocking tasks, each returning from its own 250 ms wait but leaving additional queued/running work. This is an existing resource-amplification/process-availability risk. The extraction preserves the lock/load/detach behavior exactly; cancellation, single-flight admission, moving the load outside the cache lock, or bounding detached work requires a separately approved behavior change.

The pre-move characterization commit must correct the current source comment's unqualified statement that the detached task is “harmless.” The replacement comment may say that the ignored task is read-only and cannot grant authority, while explicitly acknowledging that it remains uncancelled and can affect process availability. This comment correction changes no runtime behavior and becomes part of the accepted characterization baseline before the mechanical move.

## Extraction procedure

1. Commit and review this manifest.
2. Add and commit pre-move characterization on the current baseline, including the behavior-neutral detached-task comment correction required above.
3. Fetch/rebase, inspect overlapping upstream diffs, rerun characterization, and record the accepted characterization commit as rollback point.
4. Obtain explicit extraction authorization after characterization review.
5. Add private `mod longhouse_turn_preparation;` and the minimal parent imports.
6. Move only the five approved definitions and attached comments; adjust only required `pub(super)` visibility and sibling import paths.
7. Do not edit the three parent call-site bodies or their relative order.
8. Mechanically compare moved bodies and the 250 ms constant against the characterization commit.
9. Run focused, full, feature, compatibility, MSRV, canonical CI, documentation, formatting, and diff gates using dedicated target directories.
10. Obtain fresh correctness and security/architecture reviews.
11. Update the nearest owning `AGENTS.md`, living mission, code-health plan, this manifest, and root `events.md`.
12. Commit, fetch/rebase, revalidate affected seams, push, open a PR, wait for hosted macOS/Ubuntu/MSRV/cargo-deny CI, merge, and record publication in a separate docs follow-up.

## Validation matrix

Use dedicated `CARGO_TARGET_DIR` paths and serialize environment/cache-mutating tests.

Focused daemon tests:

```bash
cargo test -p ocean-daemon render_longhouse_prep -- --nocapture --test-threads=1
cargo test -p ocean-daemon apply_longhouse_prep -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_prepare_enabled -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_prep_for_turn -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_prep_is_time_bounded -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_turn_preparation_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_preparation_ -- --nocapture --test-threads=1
cargo test -p ocean-daemon router_contract -- --nocapture
```

Package, workspace, and supported features:

```bash
cargo test -p ocean-daemon -- --test-threads=1
cargo test -p ocean-longhouse -- --test-threads=1
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
```

Repository gates:

```bash
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Hosted CI must pass the default-parallel macOS and Ubuntu repository gates, pinned Rust 1.88 MSRV lane, and cargo-deny lane before merge.

## Rollback

Before the move, set the rollback point to the accepted characterization commit. Rollback is one revert of the extraction commit: restore the five definitions and comments to their characterized positions in `main.rs`, remove the private module/imports, and restore the sibling adapter import. No wire, persistence, schema, cache, or migration rollback is required because none may change.

## Review requirement

Fresh independent review must confirm:

- the five-item boundary is exact and cohesive;
- the three call sites and their timing/order are unchanged;
- gate/cwd/cache/blocking/deadline/fail-open/presentation behavior is unchanged;
- helper-owned warnings remain fixed-field and content/path-safe, while the existing delegated loader path logs are neither misattributed nor changed;
- the retained uncancelled-task/cache-lock resource-amplification risk is documented and the lock/load/detach mechanics remain exact;
- no permission, capability, runtime, event, persistence, HTTP, governance, librarian, call, or broader turn authority moved;
- no new public API, service abstraction, state split, dependency, or redesign was introduced;
- the librarian symlink-retarget finding remains explicitly separate.
