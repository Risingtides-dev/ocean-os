# Ocean OS documentation

This index separates implemented current state from reference material,
proposals, and history. If two documents conflict, source plus the current
contract listed here wins; do not reconcile by averaging old prose.

Documentation policy: [`OCEAN_DOCUMENTATION_CONTRACT.md`](OCEAN_DOCUMENTATION_CONTRACT.md).

## Start here

| Question | Current authority |
| --- | --- |
| What does Ocean OS own? | [`../README.md`](../README.md) and [`OCEAN_PROJECT_MAP.md`](OCEAN_PROJECT_MAP.md) |
| How is the runtime assembled? | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| How do I build, run, verify, deploy, or recover it? | [`OPERATIONS.md`](OPERATIONS.md) |
| Which package owns a change and what is its narrow test? | [`../crates/AGENTS.md`](../crates/AGENTS.md) |
| What is still open? | [`../ROADMAP.md`](../ROADMAP.md) |
| What changed over time? | [`../events.md`](../events.md) |

## Current contracts

- [`OCEAN_PROJECT_MAP.md`](OCEAN_PROJECT_MAP.md) — canonical four-repository
  ownership and connection map.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — implemented runtime layers, state
  ownership, request flow, and client boundary.
- [`OPERATIONS.md`](OPERATIONS.md) — concise local and supervised operating path.
- [`OCEAN_WORKSPACE_BINDING.md`](OCEAN_WORKSPACE_BINDING.md) — cwd/project/session binding invariants; verify source symbols rather than relying on line numbers.
- [`DAEMON_REFACTOR_MISSION.md`](DAEMON_REFACTOR_MISSION.md) — active behavior-neutral daemon extraction mission, progress, and target.

## Package and subsystem references

- [`OCEAN_RUNTIME_OPERATOR_GUIDE.md`](OCEAN_RUNTIME_OPERATOR_GUIDE.md) — extended API/configuration/troubleshooting reference; the concise `OPERATIONS.md`, executable route parity, and source override stale examples.
- [`AGENT_RENDER_PROTOCOL.md`](AGENT_RENDER_PROTOCOL.md) — render-protocol design and implementation reference; verify interaction behavior against daemon handlers and SDK types.
- [`OCEAN_BROWSER_CONTROL_PLANE.md`](OCEAN_BROWSER_CONTROL_PLANE.md) and [`OCEAN_BROWSER_CONTROL_SURFACE.md`](OCEAN_BROWSER_CONTROL_SURFACE.md) — browser planning and presentation references; current tools and typed context live in source.
- [`OCEAN_CALL_SETUP.md`](OCEAN_CALL_SETUP.md) — optional call/voice setup reference; external account, phone, and end-to-end status require live revalidation.
- [`LONGHOUSE.md`](LONGHOUSE.md) and [`LONGHOUSE_ORCHESTRATION.md`](LONGHOUSE_ORCHESTRATION.md) — current Longhouse subsystem overview, entry-path behavior, orchestration boundaries, and explicitly labeled target work.
- [`OCEAN_NATIVE_INTERNALS_MAP.md`](OCEAN_NATIVE_INTERNALS_MAP.md) — focused
  `ocean-agent` internal seams; verify planned sections before acting.
- [`../crates/ocean-search/README.md`](../crates/ocean-search/README.md) — standalone,
  bounded typed-search M1 contract and its explicit trusted-root/non-confinement boundary.
- [`PLUGINS.md`](PLUGINS.md) — subprocess plugin contract.
- [`OCEAN_ECOSYSTEM_CONTRACT.md`](OCEAN_ECOSYSTEM_CONTRACT.md) — detailed
  session/surface ecosystem invariants.
- [`examples/agents/README.md`](examples/agents/README.md) — folder-as-agent
  examples.
- Factory/orchestration operating references moved to the private
  `risingtides-agents` repository (`docs/orchestrator/`) on 2026-07-19;
  workflow behavior remains subordinate to runtime permissions and source truth.

## Plans and retained evidence

`specs/` and `superpowers/` contain design proposals, completed implementation
plans, extraction manifests, benchmarks, and characterization reports. Their
presence does not mean an item is current or unimplemented. Read the status in
the document and verify source before using one as a work order.

Active plans:

- [`specs/2026-07-16-ocean-minimizer-command-capture-runtime-integration-design.md`](specs/2026-07-16-ocean-minimizer-command-capture-runtime-integration-design.md) — reviewed design for conservative minimization of explicitly tokenized Bash argv in active-run provider requests; live events and durable session history stay raw, while exact recovery artifacts remain pinned for the projection lifetime. Implementation is a separate characterization-first checkpoint.
- [`specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md) — approved extension architecture, ownership boundaries, accepted Phases 0–1 evidence, and staged migration gates; Phase 1 includes strict installed/trusted/enabled state and static no-execution inspect/doctor reads, while lifecycle/service and package-management mutations remain pending.
- [`specs/2026-07-17-ocean-observatory-architecture.md`](specs/2026-07-17-ocean-observatory-architecture.md) — cross-repository architecture for authenticated, metadata-safe root/subagent topology, replay, and the Ocean Floor product mode. Gate 0 is accepted in [`specs/2026-07-17-observatory-gate0-decisions.md`](specs/2026-07-17-observatory-gate0-decisions.md); the operator accepted the [`Gate 1 implementation manifest`](specs/2026-07-17-observatory-gate1-implementation-manifest.md) on 2026-07-17, authorizing its strictly ordered code tasks. Tasks 2–8 are landed; the Task 9 independent review is retained at [`specs/2026-07-20-observatory-gate1-task9-independent-review.md`](specs/2026-07-20-observatory-gate1-task9-independent-review.md) with gating repairs G1–G5 that precede production Surface renderer work.
- [`specs/2026-07-19-ocean-webkit-browser-program.md`](specs/2026-07-19-ocean-webkit-browser-program.md) — ratified browser-engine program: custom WebKit with earned Chrome DevTools parity, fixed acceptance gates and security invariants, out-of-Cargo build model, and the interim `legacy-chromium` quarantine contract.
- [`specs/2026-07-16-ocean-daemon-phase2c-final-28-percent-handoff.md`](specs/2026-07-16-ocean-daemon-phase2c-final-28-percent-handoff.md) — cold-start execution order, invariants, characterization priorities, stop rules, and validation guidance for the complexity-weighted final 28% of daemon Phase 2C.
- [`specs/2026-07-16-ocean-daemon-longhouse-topic-projection-extraction-manifest.md`](specs/2026-07-16-ocean-daemon-longhouse-topic-projection-extraction-manifest.md) — active first governance checkpoint, narrowed by fresh source mapping to the scripted demo and topic list/detail adapters; real convene/title authority remains a separate security wave.
- [`specs/2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md`](specs/2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md) — operator-accepted Phase 6 design ratification for extension-owned durable orchestration: the Ocean Crew task-graph extension, six generic host seams, absorbed June 2026 R5 durable-workflow engine, Undertow/Offshore facade lanes, member acceptance and budget/attention semantics, staging/grace safety, and the read-only Observatory relationship. Stage A is authorized for extension-host Phases 2–3; Crew Stages B–E each require a separate implementation manifest before code.

Proposals awaiting operator ruling:

- [`specs/2026-07-19-cross-device-approval-and-attention.md`](specs/2026-07-19-cross-device-approval-and-attention.md) — phased design for permission-block notifications, a daemon-wide "Needs you" attention surface, and Web Push background reach, building on the 2026-07-19 `/web` `/desk` `/beam` session-handoff fabric. Phase 1 (notify on block) is accepted and implemented in `ocean-surface-ui`; Phases 2–3 remain proposed.

Active implementation reference:

- [`specs/2026-07-03-omp-port-map.md`](specs/2026-07-03-omp-port-map.md) — source-researched OMP-to-Ocean mechanism map plus the dated implementation audit, including the implemented-but-unwired standalone walker and typed-search M1 crates. It is a prioritized reference, not current architecture; live runtime search adoption remains open through [`../ROADMAP.md`](../ROADMAP.md), and its original core-orchestration placement is superseded by the extension architecture.

Completed extraction manifests are retained evidence, not independent work orders. The broader behavior-neutral daemon refactor remains active under `DAEMON_REFACTOR_MISSION.md`; consult its progress section and the current code-health plan before selecting the next checkpoint.

## Historical archive

`docs/.agentarchive/` is opt-in forensic history. Active documentation must not
depend on it. Do not load archived handoffs or plans into default agent context
unless the operator asks for historical analysis.

## Verification

```bash
cargo xtask docs-check
```

The check validates active repository-local Markdown targets, package/index
parity, non-default-member rationale, and archive boundaries. It does not prove
heading fragments or behavioral claims; inspect referenced source and run the
owning crate's checks when those change.
