# Ocean Daemon Filesystem Extraction Manifest

**Date:** 2026-07-14
**Status:** Characterization complete; extraction, full validation, and review pending
**Owner:** Ocean OS
**Rollback point:** `d0a4cab`

## Purpose

Characterize and then move the daemon's home-sandboxed directory-listing and capped-file-read HTTP leaf into one private binary module without changing its filesystem security boundary, response contracts, synchronous execution, or router composition. Leave project CRUD and persistence as a separate later checkpoint while preserving its two shared path-helper calls.

This is the lower-coupling half of the mission's projects/filesystem wave. Project handlers depend on runtime state, persistence, session association, git subprocess enrichment, and mutation responses; they do not move with this security-focused filesystem leaf.

## Characterization before extraction

Add three direct-handler tests in `main.rs` before ownership moves:

- `fs_endpoints_reject_symlink_escape_outside_home`
- `fs_dirs_preserves_home_boundary_and_error_contracts`
- `fs_file_errors_preserve_uniform_envelope`

They freeze canonicalization-before-containment for symlink escapes, the null-parent `$HOME` boundary, endpoint-specific missing/outside status codes, exact directory error shapes, and every key/default in the uniform file-error envelope. They remain in parent composition with the existing filesystem tests after extraction.

Existing parent tests continue to pin:

- tilde expansion and separator-bounded containment;
- text reads, binary sniffing, exact cap/truncation behavior, and true size;
- missing-file and direct outside-home rejection;
- sorted directory/file entries, hidden-directory omission, dotfile inclusion, git fields, and optional `files` omission.

## Exact symbols to move intact

After characterization is green, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/filesystem.rs`:

- `expand_tilde`
- `path_is_under`
- `try_canonicalize`
- `FsResolveError` and its implementation
- `resolve_under_home`
- `FsDirsQuery`
- `fs_dirs`
- `FsFileQuery`
- `FS_FILE_CAP`
- `FS_FILE_BINARY_SNIFF`
- `fs_file`
- `read_capped`
- `fs_file_error_body`

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/filesystem.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- `axum::{extract::Query, http::StatusCode, Json}`;
- `serde_json::json`;
- standard environment, filesystem, I/O, and path APIs;
- existing `ocean_agent::git_head_info`;
- parent-private `query_flag_truthy`, which stays in composition because SSE query parsing also owns it.

Minimal parent visibility:

- `expand_tilde` and `try_canonicalize` are `pub(super)` for the unchanged `project_create` path;
- `fs_dirs` and `fs_file` are `pub(super)` for unchanged parent router registration;
- `path_is_under`, `FsDirsQuery`, `FsFileQuery`, and `FS_FILE_CAP` are `pub(super)` only under the visibility needed by retained parent tests; query fields are parent-visible because those tests construct them directly;
- `FsResolveError`, `resolve_under_home`, `FS_FILE_BINARY_SNIFF`, `read_capped`, and `fs_file_error_body` remain private to the leaf.

No symbol becomes public outside the daemon binary. No state, lock, or dependency is introduced.

## Frozen security and behavior invariants

- Both `$HOME` and the requested target are canonicalized before the containment check, so symlinks inside home cannot escape the sandbox.
- Containment accepts only exact home or a separator-bounded descendant; sibling prefixes such as `/home/user2` remain rejected for `/home/user`.
- Leading `~` expansion remains limited to `~` and `~/`; unset `HOME` leaves the literal input unchanged at that helper layer.
- Outside-home targets remain 403 for both endpoints, with the raw requested path in the error.
- Missing targets remain 400 for `fs_dirs` and 404 for `fs_file`; unset/unresolvable `$HOME` remains 500.
- `GET /v1/fs/dirs` keeps canonical `path`/`home`, null `parent` at home, hidden-directory omission, optional regular files including dotfiles, exact query truthiness, alphabetical ordering, and per-directory git fields.
- Without a truthy `files` query, the `files` key remains absent rather than an empty array.
- `GET /v1/fs/file` keeps stat-before-read size, cap-plus-one truncation detection, the 512 KiB content cap, first-8-KiB NUL sniff, lossy UTF-8 conversion, and no `ok` field.
- File success/error envelopes keep exactly `path`, `content`, `truncated`, `binary`, `size`, and `error`, with the same defaults and status mapping.
- Synchronous filesystem calls remain inside the async handlers. Introducing `spawn_blocking`, descriptor traversal, or other TOCTOU hardening is a separate behavior/concurrency design decision.
- Routes, method/path discovery, middleware, fallback behavior, and operator-guide entries remain unchanged.

## Composition anchors and exclusions

This move does not:

- move or change project request/query types, CRUD handlers, runtime project persistence, git enrichment subprocesses, worktree parsing, session association, or project tests;
- move or change `query_flag_truthy`, its grammar, SSE callers, or tests;
- change router registrations, banner entries, operator-guide entries, middleware, or fallback behavior;
- change status codes, JSON keys/defaults, error wording, caps, sorting, hidden-file policy, symlink policy, or async boundaries;
- touch `AppState`, sessions, cwd rebinding, permission policy, model/YOLO settings, rooms, calls, Longhouse, or SSE;
- introduce a daemon library, public API, service trait, substate, new dependency, path abstraction, or opportunistic cleanup.

Any sandbox, symlink, path-resolution, status, response, file-content, query, ordering, git-field, or concurrency change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon fs_ -- --nocapture
cargo test -p ocean-daemon project_create_ -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon fs_ -- --nocapture
cargo test -p ocean-daemon project_create_ -- --nocapture
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

An independent security-focused reviewer must compare every moved definition against the characterization commit, verify the symlink and status/envelope tests plus both retained project-helper call sites, and confirm no unresolved medium-or-higher issue.

## Characterization result

Three direct-handler tests now freeze symlink-escape rejection for both endpoints, the null-parent home boundary, distinct missing/outside status mappings, exact directory error bodies, and every key/default in the uniform file-error envelope. All nine focused filesystem tests, all three project-helper callers, all five router contracts, all 297 daemon tests, formatting, documentation, and diff checks pass at the characterization point.

## Planned result

A private `filesystem.rs` owns the two home-sandboxed HTTP handlers and their complete path/read policy. Parent composition retains router registration, the shared SSE query parser, project CRUD, and all characterization/integration tests.

## Rollback

Revert the bounded extraction commit after the characterization point. There is no data migration, wire-version handling, or compatibility cleanup.
