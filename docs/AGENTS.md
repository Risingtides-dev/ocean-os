# docs/ — Ocean Documentation Child Doc

## Purpose

This child doc governs the `docs/` subtree: architecture notes, operator guides, handoffs, plans, and durable project documentation.

## Ownership

- **Scope:** `docs/`
- **Parent contract:** `../AGENTS.md` — read it first
- **Primary responsibilities:** current documentation, architecture references, operator workflows, planning artifacts

## Local Contracts

- Keep durable docs current with the implementation they describe.
- Cross-repo routing and ownership map lives in `OCEAN_PROJECT_MAP.md`; keep it mirrored with sibling Ocean repos when connection contracts change.
- Longhouse is the hive: the local-first agentic operations hub where agents go before they act. See `LONGHOUSE.md`.
- `ocean-daemon` owns local sessions, streaming, filesystem/tools, permission gates, and execution authority.
- `ocean-longhouse` owns SOPs, routines/workflows, tools/MCP discovery, skills, memory/knowledge, subagent specs, and quorum/council coordination.
- Do not port Pi TypeScript line-by-line; document idiomatic Rust concepts and boundaries.
- Active docs must not require material from `.agentarchive` to explain current behavior. Historical evidence may be retained there as opt-in operator context, but current contracts point to active sources.
- Active code-health/agent-readiness plan: `specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`.
- Living daemon-refactor mission, progress, and target: `DAEMON_REFACTOR_MISSION.md`.

## Work Guidance

- Prefer concise operational docs over historical notes.
- Link to source crates when documenting implementation behavior.
- Keep daemon/client protocol documentation stable and simple.
- Document rooms as durable `/v1/rooms/persistent/*` collaboration plus the independent LiveKit token route; the former Track-0 projection API is historical only.
- Update the root `AGENTS.md` child index if durable doc boundaries change.

## Verification

- For docs-only edits, run `cargo xtask docs-check`; it validates repo-local Markdown file targets, not heading fragments. Manually verify headings, commands, and code-behavior claims that static checks cannot prove.
- If documentation changes describe code behavior, run the relevant crate or workspace verification from the root contract.

## Child devlog Index

No child boundaries defined within `docs/` at this time.

## .agentarchive
Location:

~/dev/ocean-os/docs/.agentarchive

This is where stale context documents are stored for later use to analyze things and get forensic analysis of projects, but it's not something that ever gets loaded into active context. Agents do not read from this directory unless expressly requested by an operator. 

Rules on when to transfer docs into .agentarchive --- sweeping contradictions, patterns of builds that are outdated, finished or could somehow redirect an agent away from understanding the current intent of the operator --- actively suggesting to move docs to this folder is best practice
