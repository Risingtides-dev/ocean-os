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
- [`LONGHOUSE.md`](LONGHOUSE.md) and [`LONGHOUSE_ORCHESTRATION.md`](LONGHOUSE_ORCHESTRATION.md) — Longhouse design and implementation references; source determines what has shipped.
- [`OCEAN_NATIVE_INTERNALS_MAP.md`](OCEAN_NATIVE_INTERNALS_MAP.md) — focused
  `ocean-agent` internal seams; verify planned sections before acting.
- [`PLUGINS.md`](PLUGINS.md) — subprocess plugin contract.
- [`OCEAN_ECOSYSTEM_CONTRACT.md`](OCEAN_ECOSYSTEM_CONTRACT.md) — detailed
  session/surface ecosystem invariants.
- [`examples/agents/README.md`](examples/agents/README.md) — folder-as-agent
  examples.
- [`orchestrator/`](orchestrator/) — current factory/orchestration references;
  each workflow remains subordinate to runtime permissions and source truth.

## Plans and retained evidence

`specs/` and `superpowers/` contain design proposals, completed implementation
plans, extraction manifests, benchmarks, and characterization reports. Their
presence does not mean an item is current or unimplemented. Read the status in
the document and verify source before using one as a work order.

Active plans:

- [`specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md) — approved extension architecture, ownership boundaries, accepted Phase 0 evidence, and staged migration gates; the Phase 1 schema/tool-lane checkpoint is implemented but not accepted, while state separation and inspect/doctor remain pending.

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
