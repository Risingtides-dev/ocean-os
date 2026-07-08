# crates/ — Rust Workspace Child Doc

## Purpose

This child doc governs the `crates/` workspace directory. It indexes Ocean OS Rust crates and points agents to crate-specific contracts where durable boundaries exist.

## Ownership

- **Scope:** `crates/` and all Rust workspace members beneath it
- **Parent contract:** `../AGENTS.md` — read it first
- **Primary owner:** Rust workspace maintainers and agents editing crate source

## Local Contracts

- Treat each crate as a separate ownership boundary when it has its own `AGENTS.md`.
- Read the target crate's `AGENTS.md` before editing files in that crate.
- Keep workspace dependency and crate-boundary changes reflected in the root crate map when they change durable responsibilities.
- Do not introduce cross-crate coupling without documenting the contract in the affected crate docs.

## Work Guidance

- Prefer small, explicit crate boundaries.
- Run crate-local checks when possible, then workspace checks before merge.
- Session/history changes usually cross `ocean-agent`, `ocean-core`, and `ocean-daemon`; coordinate those edits explicitly.

## Verification

- `cargo check --workspace` is the repo-wide gate.
- For crate-specific logic changes, run the narrowest relevant `cargo test -p <crate>` check before the workspace gate.

## Child devlog Index

- `ocean-agent/` — session/history layer and system prompt loading → `ocean-agent/AGENTS.md`
- `ocean-core/` — shared protocol types → `ocean-core/AGENTS.md`
- `ocean-daemon/` — long-running HTTP daemon and API surface → `ocean-daemon/AGENTS.md`
- `ocean-lsp/` — code intelligence: the `lsp` tool over workspace language servers → `ocean-lsp/AGENTS.md`
- `ocean-protocol/` — multi-provider LLM wire protocol → `ocean-protocol/AGENTS.md`
- `ocean-runtime/` — agent loop and permission-gated tool execution → `ocean-runtime/AGENTS.md`
- `ocean-tui/` — terminal steering cockpit → `ocean-tui/AGENTS.md`
