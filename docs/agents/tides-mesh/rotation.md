# Tides Mesh Agent Rotation Guide

Purpose: give every Tides Mesh agent a shared shift-start, handoff, and review routine so Ocean work keeps moving without losing context.

## Authority order

1. Direct human/operator instruction.
2. PM product direction when the operator routes through PM.
3. Current approved task spec and review gate.
4. Current repo docs and ledger evidence.
5. Mesh/orchestrator coordination messages.

Coordination messages are not a substitute for operator approval on risky actions.

## Current Ocean framing

Ocean is a Rust-native Pi-style coding-agent harness/runtime.

`ocean-rs` is the canonical long-running Rust harness/runtime. `ocean-tui` is the active steering cockpit and Rust-native Tides Mesh MeshFloor, not a passive dashboard. Ocean OS GUI is a thin native client over the same runtime authority.

For MeshFloor work, use these as source of truth:

- `docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md`
- `docs/OCEAN_TUI_TIDES_MESH_PARITY.md`
- current Crew task summaries, especially task-8/task-9/task-10 when relevant
- `crates/ocean-tui/src/main.rs`

## Shift start checklist

Run from the Ocean repo root unless the operator says otherwise:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
pi_messenger status
pi_messenger list
pi_messenger task.list
pi_messenger feed --limit 20
```

Then read only what matches your lane:

- Runtime/protocol: `docs/ARCHITECTURE.md`, `docs/OCEAN_NATIVE_INTERNALS_MAP.md`, current task spec.
- TUI/MeshFloor: `docs/OCEAN_RUNTIME_TUI_FRAMING.md`, `docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md`, `docs/OCEAN_TUI_TIDES_MESH_PARITY.md`.
- GUI: `docs/OCEAN_GUI_DAEMON_MIGRATION.md`.
- Docs/writing: `README.md`, `docs/OCEAN_MASTER_PLAN.md`, current ledger notes.
- Review: task spec, changed files, validation output, and authority-boundary docs.

## Rotation lanes

| Agent | Primary lane | Rotation duty |
|---|---|---|
| OWL / Ocean-Orchestrator | coordination | route approved work, keep floor clean, avoid worker-loop overreach |
| PM | outside operator proxy | product direction, scope decisions, operator-facing summaries |
| BRICK | runtime/backend/provider | runtime harness, API/protocol, provider/auth, service drift when approved |
| PIXEL | TUI/operator UX | Ocean TUI cockpit, MeshFloor, visible operator workflows |
| KNOX / Rev | review/release | review diffs, validate gates, protect authority boundaries |
| Charlotte | research/context | gap briefs, source-backed architecture and product research |
| Henry | docs/writing | docs patches, current-context cleanup, operator-readable handoffs |
| Glyph | ledger/audit | floor minutes, log summaries, evidence trails, handoff continuity |

## Before editing

1. Confirm the task is approved and in your lane.
2. Check for file reservations or active owners.
3. Reserve the smallest file set when using mesh coordination.
4. State the layer: runtime, protocol, TUI, GUI, docs, distro, ops.
5. Do not touch services, credentials, deploys, remote access, or broad runtime state without explicit operator approval.

## Handoff format

Use this for end-of-shift or task completion:

```text
<Agent> handoff
- Task / lane:
- Files changed:
- What changed:
- Validation:
- Risks / blockers:
- Review needed:
- Next action:
```

For docs-only work, include whether any referenced docs are untracked and must be added together.

For runtime/service observations, separate source state from installed/running service state.

## Review gate

Before marking SHIP:

- Confirm the change matches operator intent.
- Confirm one-runtime architecture is preserved.
- Confirm no unapproved services/restarts/deploys/auth changes happened.
- Confirm validation commands and results are recorded.
- Confirm linked docs/files are included or explicitly excluded.

## Ledger inputs

Glyph should record and surface:

- operator corrections and priority changes
- task state changes
- review verdicts
- merge-scope decisions
- known drift between source and running services
- parked items that should not be chased

Use ledger evidence when cleaning docs or resolving conflicting task history.
