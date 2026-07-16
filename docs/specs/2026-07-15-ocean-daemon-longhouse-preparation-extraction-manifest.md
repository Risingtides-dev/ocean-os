# Ocean Daemon Longhouse Preparation Extraction Manifest

**Date:** 2026-07-15  
**Status:** Ready for publication; characterization, extraction, dedicated-target validation, compatibility/MSRV/local CI, and independent review passed; hosted CI and merge pending
**Owner:** Ocean OS  
**Rollback point:** characterization commit `66ba5db`

## Purpose

Extract only the daemon's state-free, read-only Longhouse prepare/inspect/workflow HTTP adapters from `crates/ocean-daemon/src/main.rs` into one private binary module, `crates/ocean-daemon/src/longhouse_preparation.rs`, without changing behavior.

This first Longhouse checkpoint is intentionally narrower than the adjacent librarian and compatibility adapters. Skill query/fetch and subagent-spec remain in `main.rs`; a fresh review identified a cached indexed-path symlink-retarget risk in skill fetch that requires a separate security disposition before any future librarian extraction. This checkpoint neither freezes nor changes that behavior.

The result remains a private module of the `ocean-daemon` binary. It is not a daemon library, service layer, substate, public API, or extension runtime.

## Current upstream and reconciliation rule

This manifest started from fetched `origin/main` merge `dc44343`, which includes:

- persistent-room extraction PR #293 and publication PR #294;
- PR #292's exact-token Longhouse relevance/scoring correction and fixture;
- PRs #290/#291's advisor fix and bounded Longhouse inspection route.

Before the characterization commit, the branch rebased onto `1413424` (PR #295), which reconciled effective harness-profile gates in daemon/agent/runtime code but did not change Longhouse preparation, routes, roots/cache, response projection, or tests. The complete focused and serialized daemon test sets were rerun after that reconciliation.

PR #292 changed `ocean-longhouse` scoring, explained-match fields, Longhouse inspect wire evidence, and related daemon tests. Those changes are the baseline. This checkpoint must not restore the earlier substring scorer, remove `exact_name_phrase`, or alter index/cache/root behavior.

Before characterization, extraction, documentation, and publication commits:

1. fetch and rebase onto current `origin/main`;
2. reread root, crates, daemon, and docs `AGENTS.md` contracts;
3. inspect every upstream diff touching `crates/ocean-daemon/src/main.rs`, `crates/ocean-longhouse`, route contracts, Longhouse docs, or test fixtures;
4. rerun affected characterization whenever those seams changed.

## Exact approved production boundary

Move the following existing definitions together, with their attached implementation comments, from `main.rs` into private `longhouse_preparation.rs`:

1. `LonghousePrepareRequest`;
2. `LonghouseInspectSkillBrief` and its `From<SkillBrief>` implementation;
3. `LonghouseInspectWorkflowBrief` and its `From<WorkflowBrief>` implementation;
4. `LonghouseInspectSopBrief` and its `From<SopBrief>` implementation;
5. `LonghouseInspectSkillMatch` and its `From<ExplainedSkillMatch>` implementation;
6. `LonghouseInspectWorkflowMatch` and its `From<ExplainedWorkflowMatch>` implementation;
7. `LonghouseInspectPrep` and its `From<TurnPrep>` implementation;
8. `longhouse_prepare`;
9. `longhouse_inspect`;
10. `workflows_prepare`.

At baseline `dc44343`, this is the contiguous implementation region beginning with `LonghousePrepareRequest` near `main.rs:2688` and ending after `workflows_prepare` near `main.rs:3018`.

Do not include `longhouse_routes()`. Parent composition continues to mount the three existing handlers at their exact methods and paths.

## Inbound dependencies

The extracted module may depend only on existing dependencies already used by the moved bodies:

- `axum::Json`;
- serde derives and `serde_json::json!`/`serde_json::Value`;
- `ocean_longhouse` preparation, inspection, skill, workflow, and explained-match types/functions;
- `tokio::task::spawn_blocking`;
- `tracing::warn!`;
- parent-private `longhouse_prepare_enabled()` for the inspect diagnostic flag.

No new dependency, feature, trait, state wrapper, cache, task manager, or error abstraction is authorized.

## Outbound callers

After extraction:

- `main.rs::longhouse_routes()` imports and mounts `longhouse_prepare`, `longhouse_inspect`, and `workflows_prepare` exactly as before;
- existing parent characterization tests continue to call those handlers and construct `LonghousePrepareRequest`;
- no other production caller changes;
- turn-time `longhouse_prep_for_turn()` remains a separate parent-owned path using `ocean_longhouse` directly.

## Visibility

The module remains private (`mod longhouse_preparation;`).

Only symbols required by the parent router/tests may become `pub(super)`:

- `longhouse_prepare`, `longhouse_inspect`, and `workflows_prepare`;
- `LonghousePrepareRequest`, including fields directly initialized by existing parent tests.

Inspect wire-projection DTOs remain module-private. No `pub`, `pub(crate)`, public re-export, library target, or external API is permitted.

## Frozen behavior

### Route and extractor contract

Parent composition retains these exact mounted routes:

- `POST /v1/longhouse/prepare`;
- `POST /v1/longhouse/inspect`;
- `POST /v1/workflows/prepare`.

Preserve Axum's default JSON extractor behavior, including content-type requirements, `422` rejection status/text, malformed-JSON behavior, unknown-field tolerance, and method handling. `prompt` remains required; `session_id`, `cwd`, `client_type`, and `top_n` remain optional with their existing serde defaults. No custom rejection mapper or request validation may be introduced.

### Root, cache, and ranking contract

Preserve exactly:

- non-empty `cwd` uses `SkillRoots::for_cwd`; absent/empty `cwd` uses `SkillRoots::default`;
- all three handlers use `cached_index_for`;
- prepare/workflows use `prepare` or `prepare_top_n` according to `top_n`;
- inspect uses `inspect` or `inspect_top_n` according to `top_n`;
- all ranking, candidate counts, tie-breaks, exact-token matching, exact-name phrase evidence, caps, and serialization remain owned by `ocean-longhouse`;
- no source path from outside selected roots appears in inspect output.

### Blocking and failure contract

All three handlers continue to perform filesystem/index work through `spawn_blocking`; no filesystem walk moves onto the async executor.

Preserve current JoinError behavior:

- prepare: warn and return empty `TurnPrep` plus `skills_indexed: 0`;
- inspect: warn and return default empty inspection;
- workflows: warn and return `workflows: []`.

No retry, timeout, cancellation wrapper, metric, or error-shape cleanup is authorized. JoinError branches need not gain a test-only panic seam; exact source/body comparison must verify them during extraction review.

### Wire contract

Preserve exact top-level and stable nested JSON shapes:

- prepare: `ok`, `advisory`, `skills_indexed`, and `prep`, where prep serializes `skills`, `sops`, and `workflows`;
- inspect: `ok`, `advisory`, `consult_enabled`, `skills_indexed`, `skill_candidates`, `workflows_indexed`, `workflow_candidates`, `selected_skills`, `selected_workflows`, and path-redacted `prep`;
- each selected match: `brief`, `score`, `matched_prompt_terms`, and `exact_name_phrase`;
- selected skill brief: `name`, `description`, `source`;
- selected workflow brief: `name`, `description`;
- workflows: `ok`, `advisory`, and `workflows`, retaining `WorkflowBrief` serialization.

Inspect remains path-redacted and must not echo raw prompt, session id, cwd, source paths, skill/workflow bodies, or non-contributing terms. Its selected evidence retains `matched_prompt_terms` and additive `exact_name_phrase` for both skills and workflows.

`consult_enabled` remains a diagnostic projection of parent-owned `longhouse_prepare_enabled()` and does not alter automatic preparation.

### Read-only and authority contract

These handlers remain state-free and advisory. They do not receive `State<AppState>`, publish an event, spawn a turn, invoke a model, grant a capability, mutate a session, bypass a permission gate, fetch a full skill body, or perform Longhouse governance.

## Explicit exclusions

This checkpoint does not move or change:

- `longhouse_routes()`, `app_router()`, banner routes, middleware, or operator-guide route ownership;
- `longhouse_prepare_enabled`, `render_longhouse_prep`, `apply_longhouse_prep`, `longhouse_prep_for_turn`, or any turn call site;
- `skills_query`, `skills_fetch`, their request/outcome types, or indexed-path file reading;
- `subagent_spec`, its request type, or any subagent compatibility surface;
- `AppState` or startup composition;
- `longhouse_demo`, federation parsing, convene, topics, titles, claim/revoke/recall/breach/board handlers, registry handles, escrow, or event publication;
- `ocean-longhouse` loaders, cache implementation, scorer, relevance floor, tie-breaks, exact-token behavior, fixtures, or spec assembler;
- provider/model selection, session state, cwd authority for real turns, permission policy, tools, capabilities, runtime execution, SSE, calls, rooms, or persistence;
- route renames, protocol redesign, response cleanup, new telemetry, new dependencies, or opportunistic fixes.

Longhouse librarian/fetch, compatibility-spec, turn-time preparation, and governance each require separate disposition/manifests. Calls remain a later domain wave.

## Characterization required before extraction

Keep all existing tests in `main.rs`. Add only daemon characterization that freezes adapter behavior not already exact:

1. **Real-router extractor matrix**
   - for each of the three routes, exact missing-prompt `422` status/body;
   - exact missing-content-type and malformed-JSON status/body;
   - unknown-field tolerance through the real router;
   - exact `405` behavior for GET/PUT while POST remains registered.
2. **Exact response-envelope matrix**
   - exact prepare top-level and `prep` key sets;
   - all ten inspect top-level fields plus exact selected-skill, selected-workflow, brief, and prep key sets;
   - exact workflows top-level and workflow-brief key sets;
   - `top_n: 0` and optional-field defaults without host-library-dependent membership assumptions.
3. **Read-only/blocking source boundary**
   - all three handlers retain `spawn_blocking` around index work;
   - all retain `cached_index_for` and exact JoinError fallbacks;
   - no `State<AppState>`, event emit, ordinary `tokio::spawn`, runtime/model/capability/permission call enters the boundary.
4. **PR #292 inspection evidence/privacy**
   - both skill and workflow selected evidence retain `matched_prompt_terms` and `exact_name_phrase`;
   - top-N cap and cwd confinement remain exact;
   - unique prompt/session/client/cwd/body/non-contributing sentinels are absent from complete response bytes, except explicitly expected contributing terms.

Do not duplicate `ocean-longhouse`'s exact-token corpus, scorer, cache TTL, loader, workflow parser, or assembler unit tests in the daemon. Daemon tests freeze only HTTP adaptation, redaction, blocking/read-only ownership, and route integration.

## Deferred security finding outside this boundary

A manifest reviewer identified that cached skill-fetch entries are read later by stored pathname. If an indexed skill file is replaced by a symlink before fetch, the current read may follow the retargeted path. Because skill query/fetch is excluded here, this checkpoint must not characterize that disclosure as compatible behavior or bundle a fix.

Before any future skill-librarian extraction, use a separate security disposition with at least:

- a Unix test for a cached indexed path retargeted to an outside readable secret;
- a cold-index test for a symlinked skill targeting outside selected roots;
- an approved decision on canonical-root revalidation and TOCTOU behavior.

## Characterization result

Commit `66ba5db` adds three daemon-only characterization tests without moving or changing a production body:

- `longhouse_preparation_http_extractors_methods_and_defaults_are_exact` drives all three real routes and freezes exact missing-prompt `422`, missing-content-type `415`, malformed-JSON `400`, GET/PUT `405` plus `Allow: POST`, unknown-field tolerance, all-optionals-omitted parsing, `top_n: 0`, and empty/default envelope shapes;
- `longhouse_preparation_http_envelopes_and_privacy_are_exact` freezes exact non-empty prepare/inspect/workflow key sets, both PR #292 explained-match shapes and contributing terms, exact-name phrase flags, top-N cap, cwd confinement, and complete-response non-disclosure of prompt-only/session/client/cwd/body/source-path sentinels;
- `longhouse_preparation_source_preserves_blocking_read_only_boundary` automatically reads the current owner (`main.rs` before extraction or `longhouse_preparation.rs` after it), proves each cache lookup remains inside its `spawn_blocking` closure before the await/fallback boundary, freezes the three fail-open expressions, and rejects state/event/ordinary-spawn/runtime/permission/turn authority markers.

The router tests serialize `fake_convene_state` environment mutation through `AUTO_CONVENE_ENV_LOCK` and restore `OCEAN_CONFIG_DIR`, `OCEAN_MODEL`, and `OCEAN_YOLO` panic-safely. They avoid host-library membership assumptions where optional cwd/top-N values are omitted and do not duplicate the `ocean-longhouse` exact-token corpus or cache/parser tests.

After rebasing onto PR #295, all focused preparation/inspect/workflow/router tests and all 348 daemon tests passed serialized in dedicated target `/tmp/ocean-target-longhouse-prep`. A fresh characterization re-review reported PASS with no unresolved medium-or-higher issue. `cargo fmt --all -- --check` and `git diff --check` passed.

## Extraction result

Commit `15f8cfb` moved the exact characterized 334-line implementation boundary into the 349-line private `src/longhouse_preparation.rs` module. The additional lines are the owner header/imports, required `pub(super)` visibility for three parent-mounted handlers plus the parent-test request DTO/fields, and rustfmt's multiline handler signatures. A whitespace/comment/visibility-normalized comparison against characterization commit `66ba5db` found the moved definitions and bodies identical; the only executable-token normalization required was rustfmt's trailing parameter commas.

`main.rs` retains `longhouse_routes()`, all route/banner/middleware composition, `longhouse_prepare_enabled`, turn-time render/apply/fail-open preparation and call sites, skill query/fetch, compatibility subagent spec, Longhouse demo/convene/topics/title/escrow/recall/board state, `AppState`, and all parent characterization tests. The extraction-aware source test now reads the module automatically and proves all three cache lookups remain inside their original `spawn_blocking` closures before the unchanged JoinError fallbacks. The module accepts no daemon state and owns no event, runtime, model, capability, permission, session, turn, dispatch, or governance authority.

Two fresh mechanical and security/architecture extraction reviews reported PASS with no unresolved medium-or-higher issue. The disclosed skill-fetch symlink-retarget risk remains byte-for-byte outside this module and requires its separate security disposition before any librarian extraction.

Post-extraction validation passed:

- focused preparation, inspect, workflow, librarian/spec non-regression, and five router-contract groups;
- all 348 daemon tests serialized;
- 118 `ocean-longhouse` tests plus one host-dependent ignored test and its doc test;
- `cargo check --workspace --tests`;
- `livekit-tap` and `deepgram-stt` daemon feature checks;
- `cargo xtask ci --compatibility`;
- pinned Rust 1.88 `cargo xtask ci --msrv`;
- canonical `cargo xtask ci`, including workspace tests, denied-warning all-target Clippy, format, docs/index validation, and dependency policy;
- `cargo fmt --all -- --check`, `cargo xtask docs-check`, and `git diff --check`.

Hosted default-parallel macOS/Ubuntu CI, pinned Rust 1.88, cargo-deny, and merge remain pending. Live daemon deployment/supervision remains owned by the concurrent operator workstream and is not performed by this checkpoint.

## Validation

Use a dedicated `CARGO_TARGET_DIR`. Run environment/cache-mutating tests serialized locally.

Focused commands:

```bash
cargo test -p ocean-daemon longhouse_prepare -- --nocapture --test-threads=1
cargo test -p ocean-daemon longhouse_inspect -- --nocapture --test-threads=1
cargo test -p ocean-daemon workflows_prepare -- --nocapture --test-threads=1
cargo test -p ocean-daemon router_contract -- --nocapture
```

Completion gates:

```bash
cargo test -p ocean-daemon -- --test-threads=1
cargo test -p ocean-longhouse
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

Default-parallel hosted macOS/Ubuntu CI, pinned Rust 1.88, and cargo-deny remain required before merge.

## Review gates

Before characterization acceptance, a fresh reviewer must verify:

- the narrowed exact boundary and exclusions;
- extractor/method/envelope coverage;
- PR #292 exact-token inspection evidence without duplicated algorithm tests;
- path/body/raw-input/non-contributing-term redaction;
- blocking/read-only ownership;
- skill fetch and subagent compatibility remain untouched.

Before extraction acceptance, a separate fresh reviewer must:

- compare every moved body against the characterization commit;
- inspect every visibility/import change;
- verify `longhouse_routes()`, librarian/spec, turn-time, and governance paths remain in composition;
- verify all three index lanes retain `spawn_blocking`, `cached_index_for`, and exact failure mapping;
- verify no state/event/runtime/model/permission authority entered the module;
- report any unresolved medium-or-higher issue.

## Rollback

Rollback reverts only the mechanical extraction commit after `66ba5db`, restoring the characterized bodies to `main.rs`. No schema, wire version, persistent data, cache format, or external API migration is part of this checkpoint.
