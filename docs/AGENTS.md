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
- `specs/2026-07-17-ocean-observatory-architecture.md` governs the Ocean Observatory direction; `specs/2026-07-17-observatory-gate0-decisions.md` records operator-accepted Gate 0 decisions and the 90s-game visual-parity ruling. The operator accepted `specs/2026-07-17-observatory-gate1-implementation-manifest.md` on 2026-07-17, authorizing tasks 2–9 under its strict dependency order and stop conditions; tasks 2–8 are landed and the Task 9 independent review is retained at `specs/2026-07-20-observatory-gate1-task9-independent-review.md`. Its gating repairs G1–G5 (snapshot point-in-time projection, §7.3 replay wire shape, scheduled retention + restart-safe cursor seeding, §7.4-compliant 401s) must land and pass delta review before the production Surface renderer consumes the contract.
- `specs/2026-07-19-ocean-webkit-browser-program.md` governs the browser-engine program: the Chromium backend is quarantined behind the default-off `legacy-chromium` feature (supervised daemon interim-enabled via `ops/install-ocean-daemon.sh`), and its replacement is a custom WebKit build with earned Chrome DevTools parity produced outside the Cargo graph. Acceptance gates and security invariants are fixed there; partial-fidelity network capture must not be represented as parity.
- `ocean-daemon` owns HTTP/SSE composition, the effective hashline/artifact per-turn profile gates, and local execution authority; `ocean-agent` owns product sessions/history, and `ocean-runtime` owns tool execution and permission gates. LSP/memory remain globally registered; unwired stream rules and rich context are not profile capabilities. `ocean-minimizer`, `ocean-walker`, and `ocean-search` exist only as standalone M1 libraries, so minimization and walker-backed typed search are not live profile capabilities. The search crate is a trusted-root path engine, not runtime path authorization.
- `ocean-longhouse` owns deterministic exact-token advisory preparation, its explained-ranking projection, SOP/workflow coordination, and quorum/council behavior without bypassing daemon authority. Inspection remains read-only and path-redacted at the daemon boundary. Local typed memory belongs to `ocean-memory`; shared knowledge belongs to Ocean Bedrock.
- Subagent definitions, dispatch, lifecycle, and orchestration policy belong to extensions, not core crates. Existing core subagent-shaped APIs and metadata are compatibility surfaces pending a separately approved migration. `specs/2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md` is the operator-accepted (2026-07-21) Phase 6 design ratification for that extension-owned orchestration — Stage A is authorized; Stages B–E each require their own implementation manifest: Ocean Crew owns the durable graph engine behind extension-provided `longhouse__delegate_undertow` / `longhouse__delegate_offshore` facade tools (lanes: Undertow local, Offshore remote), with real role capability profiles, a member acceptance ledger, and budget-ladder semantics intersected by host authority; the generic host seams and absorbed R5 engine remain unimplemented.
- Active docs must not require `docs/.agentarchive/`. Archive material is loaded only when the operator explicitly asks for historical or forensic context.
- Active client documentation treats the shared Leptos Surface UI and its
  Tauri host as the desktop implementation; removed desktop implementations
  are not design, migration, or prompt inputs.
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
