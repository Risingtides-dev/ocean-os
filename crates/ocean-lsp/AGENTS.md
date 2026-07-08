# ocean-lsp — Code Intelligence

## Purpose

This crate owns the `lsp` agent tool: one action-dispatched tool (status,
diagnostics, definition, references, hover, symbols, rename, reload) backed by
the workspace's own language servers, registered through the same
`CapabilityProvider` seam as MCP.

## Ownership

- **Scope:** `crates/ocean-lsp/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** LSP client lifecycle, server auto-discovery, the `lsp` tool surface, diagnostics dedupe

## Local Contracts

- Dependency direction: `ocean-lsp` depends UP into `ocean-runtime`; `ocean-runtime` must never depend back on this crate.
- A server auto-enables only when BOTH its root marker is present in the workspace AND its binary is on `$PATH` (`servers.rs`). Adding a language is a new `ServerDef` entry — never a code change elsewhere (mechanism over integration long-tail).
- Clients are shared process-wide per `(server, workspace-root)` — never spawn one language server per session.
- Positions are addressed by `file + line + symbol substring`, never by character column; an unmatched symbol is an ERROR ("re-read the file"), never a guess.
- A fresh server must be settled via `wait_quiescent` before its first real query — rust-analyzer answers `null` mid-indexing. Trust `experimental/serverStatus quiescent:true` when offered; otherwise require the `$/progress` active count to hold at zero for the settle window.
- Server→client requests MUST be answered (`workspace/configuration` gets one null per asked item) — rust-analyzer stalls its startup pipeline on an unanswered `window/workDoneProgress/create`.
- The diagnostics ledger is session-scoped: dedupe must never bleed across sessions.
- `rename` with `apply:true` refuses overlapping edits and file create/rename/delete resource ops loudly rather than half-applying a workspace edit.

## Work Guidance

- The in-repo `fake_lsp` binary (mirroring ocean-mcp's `fake_server`) is the deterministic test double; keep it minimal and synchronous.
- The real-server smoke test (`real_rust_analyzer_smoke`) is `#[ignore]`d; run it with `cargo test -p ocean-lsp -- --ignored` when touching client/handshake/quiescence logic.

## Verification

- `cargo test -p ocean-lsp`
- `cargo test -p ocean-lsp -- --ignored` (needs rust-analyzer on PATH)
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-lsp/` at this time.
