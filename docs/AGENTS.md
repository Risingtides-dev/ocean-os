# docs/ — Ocean documentation contract

## Purpose

This child contract governs current architecture, operations, cross-repository routing, subsystem references, plans, and retained evidence under `docs/`.

## Ownership

- **Scope:** `docs/`
- **Parent contract:** `../AGENTS.md` — read it first
- **Documentation index:** `README.md`
- **Cross-repository policy:** `OCEAN_DOCUMENTATION_CONTRACT.md`
- **Active daemon-refactor mission:** `DAEMON_REFACTOR_MISSION.md`

## Local Contracts

- Start from source, manifests, tests, workflows, and deployment scripts. Do not promote an old plan or handoff into current truth.
- `README.md` classifies current contracts, references, plans, and history.
- `OCEAN_PROJECT_MAP.md` is the canonical full four-repository map. Sibling repositories keep local boundary summaries rather than copied implementation inventories.
- `ARCHITECTURE.md` describes implemented composition and state authority; package inventory belongs only in `../crates/AGENTS.md`.
- `OPERATIONS.md` is the concise runbook. The extended runtime operator guide is reference material and remains subordinate to source and current scripts.
- `DAEMON_REFACTOR_MISSION.md` is the living authority for the active behavior-neutral daemon extraction program. Its completed extraction manifests are retained evidence; do not mistake an individual completed checkpoint for completion of the mission.
- `specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md` governs the approved extension architecture and staged migration. Phase 0 is accepted; the Phase 1 schema/tool-lane checkpoint is implemented but not accepted, with state separation and inspect/doctor still pending.
- `ocean-daemon` owns HTTP/SSE composition, the effective hashline/artifact per-turn profile gates, and local execution authority; `ocean-agent` owns product sessions/history, and `ocean-runtime` owns tool execution and permission gates. LSP/memory remain globally registered; unwired stream rules, rich context, and minimization are not profile capabilities.
- `ocean-longhouse` owns deterministic exact-token advisory preparation, its explained-ranking projection, SOP/workflow coordination, and quorum/council behavior without bypassing daemon authority. Inspection remains read-only and path-redacted at the daemon boundary. Local typed memory belongs to `ocean-memory`; shared knowledge belongs to Ocean Bedrock.
- Subagent definitions, dispatch, lifecycle, and orchestration policy belong to extensions, not core crates. Existing core subagent-shaped APIs and metadata are compatibility surfaces pending a separately approved migration.
- Active docs must not require `docs/.agentarchive/`. Archive material is loaded only when the operator explicitly asks for historical or forensic context.
- A document under `specs/` or `superpowers/` is not current architecture merely because it exists. Preserve its status and verify implementation before using it as a work order.
- Rooms means durable `/v1/rooms/persistent/*` collaboration plus the independent LiveKit token route; Track-0 projection-room material is historical.

## Work Guidance

- Prefer concise present-tense contracts over dated overlays and append-only handoffs.
- Name source anchors and executable validation for behavioral claims.
- Put open product work in `../ROADMAP.md`; daemon-refactor progress stays in `DAEMON_REFACTOR_MISSION.md`; chronology belongs in `../events.md`.
- Avoid copied route, model, crate, or package inventories when a typed source or executable manifest exists.
- Do not port Pi TypeScript line-by-line; document idiomatic Rust concepts and boundaries.
- Mark a known mismatch honestly instead of documenting intended behavior as shipped.

## Verification

```bash
cargo xtask docs-check
git diff --check
```

`docs-check` validates active repository-local Markdown targets, archive boundaries, workspace/index parity, and non-default-member rationale. It does not validate heading fragments or prove behavior. Run the owning crate's checks when documentation changes with implementation. Route and CORS claims additionally require the focused daemon contract tests named by `crates/ocean-daemon/AGENTS.md`.

## Child devlog Index

No nested `AGENTS.md` boundaries currently exist below `docs/`.

## Historical archive

`docs/.agentarchive/` stores opt-in forensic history: stale handoffs, completed or superseded plans, and context that could redirect a cold agent away from current intent. Do not link active contracts to archive material as required reading.
