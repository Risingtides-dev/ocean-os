# ocean-agent System Prompt Extraction Manifest

**Date:** 2026-07-12
**Type:** behavior-neutral structural extraction
**Status:** Complete — independently re-reviewed after upstream sync
**Rollback commit:** `fa1eab27`

## Source

- Package: `ocean-agent`
- File before extraction: `crates/ocean-agent/src/lib.rs`
- Intact symbol boundary: private `mod system_prompt` (pre-extraction lines 6149–7297 after upstream prompt commit `eba86f04`)
- Callers retained in `lib.rs`: `system_prompt::surface_flag` and `system_prompt::build_system_prompt`

## Target

- `crates/ocean-agent/src/system_prompt.rs`
- `lib.rs` retains the private `mod system_prompt;` declaration.
- No public re-export or public path is added, removed, or renamed.

## Invariants

- Move the complete module body, constants, loaders, surface routing, memory context, project-instruction budget logic, and embedded tests together.
- Preserve prompt literal bytes and surface selection behavior.
- Preserve file/env resolution, ancestor walking, memory lookup, project budget, and fallback behavior.
- Preserve all caller expressions and crate-private function names.

## Explicitly not included

- No prompt wording edits.
- No API, protocol, session, permission, performance, or error-policy change.
- No test redesign or finer module split.
- No opportunistic cleanup inside the moved code.

## Validation

1. Verify the extracted module body is copied intact before formatting.
2. `cargo fmt --all -- --check`
3. `cargo test -p ocean-agent system_prompt`
4. `cargo test -p ocean-agent`
5. `cargo check --workspace`
6. `cargo xtask docs-check`
7. Fresh independent review of the extraction-only diff.

## Validation result

- Pre-format module body copied intact: **59,425 bytes**, SHA-256 `c8d1aa6e35c3bdb160ce010e6675b33dc640fade3314f1fd8572ca8a6e6d66bd` before and after extraction.
- Upstream commit `eba86f04` tightened prompt wording before this extraction was replayed; the move preserves that upstream body and introduces no additional wording change.
- Rustfmt-normalized rollback body equals `src/system_prompt.rs` exactly: SHA-256 `f929c269239707d5d3666195fd1e5a4d0705ea0f662050ae5116390b6c1a92c0`.
- `cargo test -p ocean-agent system_prompt`: **22 passed**.
- `cargo test -p ocean-agent`: **149 passed**.
- `cargo check --workspace`: passed.
- `cargo xtask docs-check`: passed with 25 indexed packages and 92 active Markdown files.
- `cargo fmt --all` and `git diff --check`: passed.
- Fresh reviewer compared the rebased target with `fa1eab27`, reran all 22 focused tests, verified upstream compactness/recall guidance, and returned **PASS** with no blockers.
