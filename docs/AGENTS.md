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
- Active docs must not require `docs/.agentarchive/`. Archive material is loaded only when the operator explicitly asks for historical or forensic context.
- A document under `specs/` or `superpowers/` is not current architecture merely because it exists. Preserve its status and verify implementation before using it as a work order.
- Rooms means durable `/v1/rooms/persistent/*` collaboration plus the independent LiveKit token route; Track-0 projection-room material is historical.
- Longhouse coordination does not bypass daemon execution, cwd, or permission authority.

## Work Guidance

- Prefer concise present-tense contracts over dated overlays and append-only handoffs.
- Name source anchors and executable validation for behavioral claims.
- Put open product work in `../ROADMAP.md`; daemon-refactor progress stays in `DAEMON_REFACTOR_MISSION.md`; chronology belongs in `../events.md`.
- Avoid copied route, model, crate, or package inventories when a typed source or executable manifest exists.
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
