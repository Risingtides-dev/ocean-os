# Workflow Port Scout — Ocean-native workflow specs

**Model used:** GPT-4.1 (ACP/Ocean turn)

## Files and commands inspected

- `docs/orchestrator/FACTORY_GOAL.md`
- `docs/orchestrator/FACTORY_LOOP.md`
- `docs/orchestrator/LONGHOUSE_FACTORY_MIGRATION.md`
- `../ocean-orchestrator/ocean-loop.workflow.js`
- `../ocean-orchestrator/refinement-wave.workflow.js`
- `../ocean-orchestrator/ORCHESTRATOR_HANDOFF.md`
- `../ocean-orchestrator/PROJECT_STATE.md`
- `grep` across `docs/orchestrator` and `../ocean-orchestrator` for `prepare`, `workflow`, `Longhouse`, `workflow spec`

## Findings

- The legacy workflow JS files are **analysis prompts**, not execution engines. They encode the old factory loop as structured text with phase labels and research prompts, but they do not define a portable spec format yet.
- `ocean-loop.workflow.js` maps cleanly to a first Ocean-native **factory tick** spec:
  - phases: `RECONCILE`, `UNBLOCK`, `ADVANCE`, `REFILL`, `REPORT`
  - inputs: repo set, Linear team/project, GitHub org, daemon URL, concurrency budget
  - outputs: short status report + ledger changes
- `refinement-wave.workflow.js` maps cleanly to a second spec family for **research/refinement waves**:
  - division-based parallel research
  - synthesize findings to per-division scratchpads
  - output findings queue, then ticket creation after read-back
- Current migration docs already name the target shape in `docs/orchestrator/LONGHOUSE_FACTORY_MIGRATION.md`, including the future `POST /v1/workflows/prepare` path and the need for compact `WorkflowBrief { name, description, source_path }` entries.
- `FACTORY_LOOP.md` already establishes the operational contract for one tick; it can serve as the canonical behavior doc behind the spec file.
- `ORCHESTRATOR_HANDOFF.md` and `PROJECT_STATE.md` show that the live factory context still expects the old loop semantics, so the first Ocean-native specs should preserve those semantics and stay advisory/read-only.

## Recommended first spec files to create

1. `docs/orchestrator/workflows/factory-tick.workflow.md`
   - Canonical Ocean-native version of `ocean-loop.workflow.js`
   - Keep phase checklist and inputs/outputs explicit
   - Use this as the prep brief for normal factory ticks

2. `docs/orchestrator/workflows/refinement-wave.workflow.md`
   - Canonical Ocean-native version of `refinement-wave.workflow.js`
   - Encode division-based research, synthesis, and ticket-ready outputs
   - Keep it read-only/advisory until workflow prepare exists

3. Optional follow-on: `docs/orchestrator/workflows/pr-review.workflow.md`
   - Pull the merge/review/cleanup path out of the tick loop
   - Useful once workflow preparation starts ranking multiple candidate briefs

## Small implementation tickets needed

- **Longhouse workflow briefs loader**
  - Scope: `crates/ocean-longhouse/src/prepare.rs` + tests
  - Goal: surface repo-local workflow docs in `TurnPrep.workflows`
- **Workflow spec directory + canonical docs**
  - Scope: docs only
  - Goal: create the first stable Ocean-native workflow briefs above
- **Workflow prepare endpoint**
  - Scope: `crates/ocean-daemon/src/main.rs`, `crates/ocean-longhouse`
  - Goal: documented `POST /v1/workflows/prepare` that ranks briefs and returns compact context, read-only/fail-open
- **Extension-owned worker dispatch bridge design**
  - Scope: docs only first
  - Goal: define extension-owned orchestration over Longhouse advisory context and generic daemon permission/execution seams before any automation work

## Blockers

- No hard blocker for creating the spec docs.
- The real blocker for operational use is the missing workflow loader / prepare endpoint; until those land, the specs remain manual prompts/checklists.

## Recommended next action

Create `docs/orchestrator/workflows/factory-tick.workflow.md` and `docs/orchestrator/workflows/refinement-wave.workflow.md` first, then ticket the loader work so Longhouse can actually surface them in prep.
