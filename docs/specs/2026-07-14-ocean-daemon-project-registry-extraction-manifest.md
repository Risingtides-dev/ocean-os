# Ocean Daemon Project Registry Extraction Manifest

**Date:** 2026-07-14
**Status:** Characterization complete; extraction, full validation, and review pending
**Owner:** Ocean OS
**Rollback point:** `0fce6cc`

## Purpose

Characterize and then move the daemon's project-registry HTTP adapters into one private binary module without changing runtime persistence authority, pagination, git/worktree enrichment, workspace/session association, directory creation/canonicalization, response shapes, or timestamps. Keep router composition, turn cwd resolution, session ownership, and the separately extracted filesystem sandbox unchanged.

This is the state-backed half of the mission's projects/filesystem wave. The home-sandboxed directory/file endpoints already live in `filesystem.rs`; project creation intentionally remains outside that HOME sandbox and only reuses its tilde-expansion and canonicalization primitives.

## Characterization before extraction

Add five direct-handler tests in `main.rs` before ownership moves:

- `projects_list_preserves_git_enrichment_and_failure_fallbacks`
- `project_get_preserves_workspace_session_association_and_response_contracts`
- `project_patch_preserves_partial_fields_identity_and_timestamps`
- `project_delete_preserves_sessions_and_response_contracts`
- `project_create_preserves_payload_timestamps_and_error_contracts`

They freeze enriched list fields and failure fallbacks, exact project→workspace-session association, GET/PATCH/DELETE success/not-found/error envelopes, PATCH identity/root/created/config preservation, DELETE session retention, create payload/timestamps/tilde handling, and create failure envelopes. They remain in parent composition after extraction.

Existing parent tests continue to pin:

- bounded newest-first pagination and cursor termination;
- create-directory/canonical-path behavior, empty-root pass-through, and preservation of existing contents;
- worktree porcelain parsing, branch-prefix stripping, empty input, and final-entry flush.

## Exact symbols to move intact

After characterization is green, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/project_registry.rs`:

- `CreateProjectRequest`
- `PatchProjectRequest`
- `ProjectsListQuery`
- `projects_list`
- `project_create`
- `enriched_project_json`
- `parse_worktree_list`
- `project_get`
- `project_patch`
- `project_delete`

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/project_registry.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- parent-private `AppState`;
- sibling `filesystem::{expand_tilde, try_canonicalize}`;
- `axum::{extract::{Path, Query, State}, http::StatusCode, Json}`;
- `chrono::Utc`;
- existing `ocean_core::{Project, ProjectConfig, ProjectId, ProjectResponse}`;
- `serde_json::json`;
- existing Tokio command/timeout APIs;
- existing UUID and `ocean_agent::{git_head_info, WorktreeInfo}` paths.

Minimal parent visibility:

- the five route handlers are `pub(super)` for unchanged parent router registration;
- all three request/query types and fields are `pub(super)` because retained parent tests construct them;
- `parse_worktree_list` is `pub(super)` for retained parent parser tests;
- `enriched_project_json` remains private because list ownership moves with it;
- `AppState` remains private to the binary parent; no field visibility changes;
- `Project` and `ProjectConfig` become test-only parent imports after production ownership moves.

No symbol becomes public outside the daemon binary. No dependency, lock, state abstraction, or service layer is introduced.

## Frozen persistence and behavior invariants

- `ocean-agent` remains the sole owner of `projects.json` load/upsert/delete, atomic writes, newest-first ordering, cursor pagination, timestamp stamping, workspace reverse lookup, and session persistence.
- `GET /v1/projects` remains sequentially enriched in page order and returns exactly `{ok, projects, next_cursor, has_more}` on success.
- Git branch remains a pure filesystem HEAD read. Dirty status remains `git -C <root> status --porcelain` with the same 1.5-second timeout. Worktrees remain `git worktree list --porcelain`, with the exact project root excluded.
- Non-repo or enrichment failure remains `git_branch:null`, `git_dirty:null`, and/or `worktrees:[]` according to the same current conditions; enrichment never fails the list route.
- List persistence errors remain 500 with `{ok:false, projects:[], error, next_cursor:null, has_more:false}`.
- Project create keeps empty workspace roots unchanged; otherwise it expands leading tilde, runs synchronous `create_dir_all`, canonicalizes, then persists.
- Create keeps one wall-clock millisecond value for both initial `created_ms` and the runtime-stamped `updated_ms`, returns 201 on success, 400 for mkdir/canonicalization failure, and 500 for persistence failure with the same `ProjectResponse` fields/error wording.
- `GET /v1/projects/{id}` remains un-enriched and lists sessions from exactly the project's `workspace_root`; session-list failure still degrades to `[]` rather than failing the route.
- PATCH can change only optional name/config; it preserves id, workspace root, created timestamp, and omitted fields, while runtime stamps `updated_ms` from the handler's single current time.
- DELETE removes only the project index entry. Sessions and workspace buckets remain untouched.
- GET/PATCH/DELETE unknown ids and persistence errors keep their exact statuses, JSON shapes, and `unknown project {id}` wording.
- Router paths/methods, discovery/banner/operator-guide parity, middleware, `AppState`, turn cwd/project resolution, and session ownership remain unchanged.

## Composition anchors and exclusions

This move does not:

- move or change runtime project/session APIs, project file layout, pagination caps/order/cursors, atomic persistence, or session data;
- move turn `project_id`/cwd resolution, owning-project projection, room/call project behavior, or any non-project handler;
- move or change `filesystem.rs` sandbox handlers/policy; project creation remains intentionally outside the HOME sandbox;
- change git commands, timeout, worktree parsing/filtering, enrichment order, failure fallback, status codes, response keys, error strings, timestamp source, or synchronous filesystem behavior;
- move router registrations, banner/operator-guide entries, middleware, fallback behavior, tests, or shared fixtures;
- introduce a daemon library, public API, project service, trait, substate, generated routing, new dependency, or opportunistic cleanup.

Any persistence, pagination, enrichment, workspace/session association, cwd, create-path, timestamp, response, status, or routing change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon project_ -- --nocapture
cargo test -p ocean-daemon projects_list_ -- --nocapture
cargo test -p ocean-daemon parse_worktree_list_ -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon project_ -- --nocapture
cargo test -p ocean-daemon projects_list_ -- --nocapture
cargo test -p ocean-daemon parse_worktree_list_ -- --nocapture
cargo test -p ocean-agent
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare every moved definition against the characterization commit, verify all persistence/enrichment/session/cwd anchors above, and confirm no unresolved medium-or-higher issue.

## Characterization result

Five direct-handler tests now freeze list enrichment and persistence fallbacks, exact project/workspace session association and fail-open session listing, partial PATCH identity/timestamp behavior, DELETE session retention, create payload/tilde/timestamp behavior, and success/not-found/persistence/path-error response contracts. Existing pagination, create-path, and worktree-parser tests remain green. Focused project tests, all five router contracts, all 302 daemon tests, formatting, documentation, and diff checks pass at the characterization point.

## Planned result

A private `project_registry.rs` owns only project HTTP request/query types, CRUD/list adapters, response-time git enrichment, and worktree parsing. Parent composition retains router registration, turn/session/cwd integration, state assembly, and all characterization/integration tests.

## Rollback

Revert the bounded extraction commit after the characterization point. There is no data migration, wire-version handling, or compatibility cleanup.
