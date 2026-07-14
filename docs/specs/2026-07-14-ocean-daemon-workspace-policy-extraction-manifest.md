# Ocean Daemon Workspace Policy Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; focused/full/feature validation and independent review passed
**Owner:** Ocean OS
**Rollback point:** `9261d50`

## Purpose

Move the daemon's pure ordinary agent-turn/session-read cwd policy out of `src/main.rs` into one private binary module. Keep runtime cwd/project resolution, session lookup and persistence, HTTP query extraction and response mapping, startup repository-cwd enforcement, room/call fallbacks, and all orchestration in `main.rs`.

This checkpoint deliberately preserves the daemon's asymmetric current contract: turns may rebind a resumed session to the caller's newly requested cwd, while a workspace-scoped session-detail read rejects a different bound workspace. It does not claim to centralize every cwd use in the daemon.

## Exact symbols to move intact

From `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/workspace_policy.rs`:

- `CwdBindingError`
- `CwdBindingError::message`
- `cwd_has_traversal`
- `resolve_bound_cwd`
- `session_detail_scope_check`

Move these nine focused tests beside the policy:

- `new_session_runs_in_requested_cwd`
- `resumed_turn_in_same_workspace_rebinds_to_requested_cwd`
- `bound_cwd_still_rejects_traversal_on_any_turn`
- `resumed_turn_rebinds_when_workspace_changes`
- `path_traversal_cwd_is_rejected_for_new_session`
- `path_traversal_cwd_is_rejected_for_resumed_session`
- `cwd_has_traversal_detects_parent_components_only`
- `session_detail_rejects_cross_workspace_read`
- `session_detail_allows_matching_or_unscoped_read`

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/workspace_policy.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new production module depends only on `std::path::{Component, Path}` and owned strings.

- `CwdBindingError` is `pub(super)` because parent composition receives it from the two policy calls.
- `CwdBindingError::message`, `resolve_bound_cwd`, and `session_detail_scope_check` are `pub(super)` for the unchanged parent callers.
- Error variants and fields remain private to the module; parent composition does not inspect them.
- `cwd_has_traversal` remains private.

No symbol becomes public outside the daemon binary.

Inbound callers remain:

- `agent_turn` resolves the caller/project cwd through `AgentRuntime`, then calls `resolve_bound_cwd`; it continues mapping any rejection to the same log and HTTP 400 `AgentTurnResponse` shape.
- `agent_session` obtains requested and persisted workspace strings in composition, then calls `session_detail_scope_check`; it continues mapping a mismatch to the same log and HTTP 400 response.

## Composition anchors that must stay unchanged

- The inline startup repository-cwd guard remains immediately after `startup::validate_startup_config()` and before runtime, database, router, or listener construction. Its exact lowercase override grammar, `current_dir` failure behavior, Git subprocess arguments, failure-open behavior, and bail text remain unchanged.
- `SessionListQuery`, `SessionDetailQuery`, `workspace_filter`, and `requested_workspace` remain HTTP/query composition.
- `session_workspace_binding` remains composition-owned because it reads persisted session detail through `AgentRuntime` and retains its one-sided cwd/root fallback.
- `AgentRuntime::resolve_cwd_for_turn`, session creation/rebind/persistence, and strict resume behavior remain owned by `ocean-agent`.
- Room, call/voice, Longhouse, LSP, project, and filesystem cwd behavior remain outside this leaf.

## Frozen invariants

- Traversal detection remains lexical `Path::components()` matching only `Component::ParentDir`; it does not canonicalize, require existence, resolve symlinks, normalize case, or actually enforce the error text's absolute-path claim.
- `../b` and embedded `/../` components remain rejected; a literal component such as `..b` remains accepted.
- A new turn runs in exactly the requested cwd.
- A resumed turn runs in the caller's newly requested cwd, including another subdirectory or another workspace; it is not pinned to its old cwd and is not rejected for crossing workspaces.
- The compatibility parameters `_requested_workspace_root` and `_session_binding` remain present and ignored.
- Session-detail reads reject only when both requested and persisted workspace roots exist and differ as raw strings.
- Matching, unscoped, and legacy-unbound session-detail reads remain allowed.
- Error variants, messages, log context, statuses, and response bodies remain unchanged.
- Unknown resumed sessions continue to reach the downstream runtime's canonical not-found handling.
- Caller cwd remains authoritative for ordinary agent turns; session persistence remains owned by `ocean-agent`.

## Explicit exclusions

This move does not:

- change or centralize the startup repository-cwd guard;
- move persisted-session lookup or change its defensive one-sided binding fallback;
- move query structs, handlers, routes, state, runtime invocation, or persistence;
- change room/process-cwd or call/voice fallback behavior;
- canonicalize or clean paths, enforce absoluteness/existence, or alter symlink handling;
- restore old same-workspace pinning or reject cross-workspace turn rebinding;
- change workspace query precedence or truthy grammar;
- replace the Git subprocess, add startup override spellings, or change startup ordering;
- introduce a daemon library, public API, dependency, trait, substate, wire change, or opportunistic cleanup.

Any behavior, error-text, HTTP-shape, startup, persistence, or broader domain change stops this extraction and requires a separate decision.

## Validation

```bash
cargo test -p ocean-daemon workspace_policy::tests:: -- --nocapture
cargo test -p ocean-daemon session_detail -- --nocapture
cargo test -p ocean-agent bind_workspace -- --nocapture
cargo test -p ocean-agent
cargo test -p ocean-runtime
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare every moved production/test body against rollback point `9261d50`, verify minimal visibility, confirm all composition anchors above are unchanged, and confirm focused/full/feature gates pass with no unresolved medium-or-higher issue.

## Result

A private `workspace_policy.rs` now owns only the pure traversal, caller-cwd pass-through, and session-detail scope policy plus its nine existing tests. Every production and test body is unchanged from rollback point `9261d50` except for minimal `pub(super)` visibility. Composition continues to acquire cwd/workspace data, enforce startup placement, read persisted sessions, map HTTP responses, and orchestrate runtime behavior. Focused policy/session/rebinding tests, full agent/runtime/daemon suites, router contracts, workspace-test compilation, both supported daemon feature checks, formatting, documentation, and diff checks passed; independent review found no unresolved medium-or-higher issue.

## Rollback

Revert the bounded workspace-policy extraction commit. There is no data migration, wire-version handling, or compatibility cleanup.
