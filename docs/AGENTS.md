# docs/ — Ocean Documentation Child Doc

## Purpose

This child doc governs the `docs/` subtree: architecture notes, operator guides, handoffs, plans, and durable project documentation.

## Ownership

- **Scope:** `docs/`
- **Parent contract:** `../AGENTS.md` — read it first
- **Primary responsibilities:** current documentation, architecture references, operator workflows, planning artifacts

## Local Contracts

- Keep durable docs current with the implementation they describe.
- Longhouse is the hive: the local-first agentic operations hub where agents go before they act. See `LONGHOUSE.md`.
- `ocean-daemon` owns local sessions, streaming, filesystem/tools, permission gates, and execution authority.
- `ocean-longhouse` owns SOPs, routines/workflows, tools/MCP discovery, skills, memory/knowledge, subagent specs, and quorum/council coordination.
- Do not port Pi TypeScript line-by-line; document idiomatic Rust concepts and boundaries.

## Work Guidance

- Prefer concise operational docs over historical notes.
- Link to source crates when documenting implementation behavior.
- Keep daemon/client protocol documentation stable and simple.
- Update the root `AGENTS.md` child index if durable doc boundaries change.

## Verification

- For docs-only edits, manually verify links and commands named in the edited doc.
- If documentation changes describe code behavior, run the relevant crate or workspace verification from the root contract.

## Child devlog Index

No child boundaries defined within `docs/` at this time.
