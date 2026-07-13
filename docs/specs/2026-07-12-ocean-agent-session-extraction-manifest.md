# ocean-agent Session Extraction Manifest

**Date:** 2026-07-12
**Type:** behavior-neutral structural extraction
**Status:** Complete — independently re-reviewed after upstream sync
**Rollback commit:** `5be4cf6d`

## Source

- Package: `ocean-agent`
- File before extraction: `crates/ocean-agent/src/lib.rs`
- Intact symbol boundary: private `mod session` (pre-extraction lines 2604–3336)
- Root callers retained in `lib.rs`: `session::{list, workspace_root, migrate_legacy_sessions, session_file_gc, ttl_from_env, detail, load_resumable, save, Session}` and test-only session helpers.

## Target

- `crates/ocean-agent/src/session/mod.rs`
- `lib.rs` retains the private `mod session;` declaration at the same module boundary.
- No public re-export or external public path is added, removed, or renamed.

## Dependencies and visibility

- Preserve `use super::*` and `std::io::Write`; the module remains a child of the crate root.
- Preserve all existing `pub`, `pub(crate)`, `pub(super)`, and private visibility exactly.
- Preserve root helpers/types consumed through `super`, including protocol messages, session DTOs, model/session ids, serde, paths, and project/git helpers.

## Critical invariants

- Persisted JSON field names/defaults and old-session deserialization remain byte/schema compatible.
- Atomic save/write/sync/rename order and duplicate-file purge behavior remain unchanged.
- Resume remains strict for unknown/corrupt sessions; duplicate healing remains deterministic.
- Workspace rebinding, workspace bucketing, git metadata, TTL/GC behavior, pagination, transcript projection, image metadata, and raw message retention remain unchanged.
- The per-session load→run→save lock scope in root runtime code is untouched.

## Explicitly not included

- No session schema, API, error, migration, lock, persistence, path, or client behavior change.
- No finer split of session internals.
- No renames, visibility cleanup, test relocation, or opportunistic refactor.
- No performance optimization.

## Validation

1. Verify the extracted module body is copied intact before formatting.
2. Compare symbol/visibility inventory before and after.
3. `cargo fmt --all -- --check`
4. `cargo test -p ocean-agent session`
5. `cargo test -p ocean-agent`
6. `cargo check --workspace`
7. `cargo xtask docs-check`
8. Fresh independent review against rollback commit `5be4cf6d`.

## Validation result

- Pre-format module body copied intact: **30,078 bytes**, SHA-256 `08790831a70c9b77f2bef3c97d4e76d75bc9e4601d27dd2a905c2e2ebedac3b9` before and after extraction.
- Rustfmt-normalized rollback-commit body equals `src/session/mod.rs` exactly: **27,322 bytes**, SHA-256 `13c3769527041ccdf357f258cfca8fce89c07c35e46e40c0afdc447e78f98d59`.
- `cargo test -p ocean-agent session`: **24 passed**.
- `cargo test -p ocean-agent`: **149 passed**.
- `cargo check --workspace`: passed.
- `cargo xtask docs-check`: passed with 25 indexed packages and 93 active Markdown files.
- `cargo fmt --all` and `git diff --check`: passed.
- Full `cargo xtask ci`: passed (docs/index, workspace build/tests, strict all-target Clippy, format, and cargo-deny).
- Fresh reviewer compared the rebased target and root callers/tests with `5be4cf6d`, reran all 24 focused tests, and returned **PASS** with no blockers.
