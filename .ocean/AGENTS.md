# .ocean/ — Ocean Runtime Child Doc

## Purpose

This child doc governs the `.ocean/` directory only — Ocean runtime artifacts, session data, config, and operational state. It does NOT own the devlog framework. The root devlog contract lives at `../AGENTS.md` (repo root).

## Ownership

- **Scope:** `.ocean/` tree only
- **Parent contract:** `/AGENTS.md` (repo root) — read it first
- **What lives here:** runtime config, session snapshots, worktree state, GPUI assets, operational artifacts

## Local Contracts

- Do not store source code, business logic, or durable architecture decisions here.
- Runtime artifacts in `.ocean/` are volatile — do not treat them as canonical state.
- If you are editing `.ocean/` contents, you are touching runtime config, not contracts. Update this file if `.ocean/`'s structure or purpose changes.

## Work Guidance

- The ocean-agent session loader reads `.ocean/AGENTS.md` as a project instructions source (alongside `AGENTS.md`, `CLAUDE.md`, `.pi/instructions.md`).
- Edits to `.ocean/` do not require a parent AGENTS.md update unless the structure of `.ocean/` itself changes.

## Verification

- `cargo check --workspace` must still pass after any change that touches `.ocean/` loading logic in `crates/ocean-agent/src/lib.rs`.

## Child devlog Index

No child boundaries defined within `.ocean/` at this time.
