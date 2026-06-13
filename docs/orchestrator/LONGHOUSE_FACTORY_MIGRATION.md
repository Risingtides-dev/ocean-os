# Ocean OS Factory Migration Plan — Ocean-Native Longhouse Workflows

This plan makes Ocean OS own its software-factory workflows in-repo. Longhouse provides advisory prep/workflow context; the daemon retains all execution authority.

## Current reality

- Ocean OS already has the Longhouse prep hook:
  - `crates/ocean-longhouse/src/prepare.rs` indexes repo-local `./skills/**`.
  - `crates/ocean-daemon/src/main.rs` injects matching skill/SOP/workflow briefs before turns.
  - `prepare.rs` currently says SOP/workflow sources are not real yet, so skills are the practical staging mechanism today.
- This repo now owns the factory docs:
  - `docs/orchestrator/FACTORY_GOAL.md`
  - `docs/orchestrator/workflows/factory-tick.md`
  - `docs/orchestrator/workflows/refinement-wave.md`
  - `docs/orchestrator/workflows/pr-review-merge.md`
- This repo has a local Longhouse skill:
  - `skills/ocean-os-software-factory/skill.yaml`
  - `skills/ocean-os-software-factory/SKILL.md`

`../ocean-orchestrator` is no longer part of the normal operating path. Treat it as archived background only, not as a runtime dependency or source of authority.

## Target shape

Ocean OS owns its own software-factory protocols:

1. **Longhouse prep** surfaces the right factory skill/SOP before relevant turns.
2. **Ocean agent** executes the tick through normal tools and daemon permission gates.
3. **Linear + GitHub** remain the ledgers of record.
4. **Legacy JS workflows** become reference material only, then are ported into Ocean-native workflow specs when `POST /v1/workflows/prepare` exists.
5. **External coding tools** become optional fallback capacity, not the orchestration source of truth.

## Migration stages

### Stage 1 — Pin the factory doctrine in-repo

Status: started.

- Keep `docs/orchestrator/FACTORY_GOAL.md` as the standing policy.
- Keep `docs/orchestrator/FACTORY_LOOP.md` as the per-tick checklist.
- Add/update `skills/ocean-os-software-factory/**` so Longhouse prep injects the doctrine when a turn mentions workflows, orchestration, PR waves, Linear, or replacing external orchestration with Ocean OS.
- Reconcile these docs against live Ocean OS behavior and remove stale tool- or host-specific language where Ocean has a native equivalent.

### Stage 2 — Maintain declarative Ocean workflow specs

Durable specs now live under `docs/orchestrator/workflows/`:

- `factory-tick.md`
  - phases: RECONCILE, UNBLOCK, ADVANCE, REFILL, REPORT.
  - inputs: repo set, Linear team/project, GitHub org, live daemon URL, concurrency budget.
  - outputs: status report + ledger changes.
- `refinement-wave.md`
  - phases: research divisions, synthesize findings, ticket queue.
  - inputs: repo paths, focus domains, severity rubric.
  - outputs: findings queue, then Linear tickets after read-back.
- `pr-review-merge.md`
  - phases: CI check, review check, fix, re-request, merge, cleanup.

Until a first-class workflow engine exists, these specs are prompts/checklists that the Ocean agent follows directly.

### Stage 3 — Implement Longhouse workflow source loading

Today `TurnPrep.workflows` is a stable but empty field. Add a real loader for workflow briefs:

- Extend `crates/ocean-longhouse/src/prepare.rs` to scan repo-local workflow docs.
- Return compact `WorkflowBrief { name, description, source_path }` entries from `SkillIndex::prepare` or a sibling index.
- Preserve the existing invariants: read-only, fail-open, cached, time-bounded, advisory only.
- Add daemon prompt-rendering tests proving workflow briefs appear in the prep block.

### Stage 4 — Add structured workflow preparation API

Implement the documented future route:

- `POST /v1/workflows/prepare`

Expected role:

- rank candidate workflow specs for a turn;
- return compact phase/checklist context;
- optionally recommend subagent specs;
- never execute local side effects.

This complements, not replaces, the daemon's normal `/v1/agent/turns` path.

### Stage 5 — Ocean-native dispatch/review loop

Once workflow prep exists, run a factory tick entirely from Ocean:

1. Ask Ocean to run `docs/orchestrator/FACTORY_LOOP.md`.
2. Longhouse injects the factory skill + workflow brief.
3. Ocean uses GitHub/Linear/FS/shell/browser tools normally.
4. Ocean creates tickets before branches.
5. Ocean dispatches isolated worktrees or bounded subagent specs.
6. Ocean reports the tick and updates ledgers.

## Immediate next tickets worth creating

Use Linear to mint real IDs before branch/PR work.

1. **Longhouse workflow briefs loader**
   - File scope: `crates/ocean-longhouse/src/prepare.rs` + tests.
   - Outcome: repo-local workflow docs can appear in `TurnPrep.workflows`.
2. **Factory workflow specs directory**
   - File scope: docs/spec files only.
   - Outcome: legacy `*.workflow.js` protocols are represented as Ocean-native docs.
3. **Workflow prepare endpoint**
   - File scope: `crates/ocean-daemon/src/main.rs`, `crates/ocean-longhouse`.
   - Outcome: documented `POST /v1/workflows/prepare` exists and is read-only/fail-open.
4. **Subagent dispatch bridge design**
   - File scope: docs/design only first.
   - Outcome: clear boundary for Longhouse spec assembly vs daemon-executed local actions.

## Operating rule

Do not move authority into Longhouse. Longhouse stages context and recommends workflows. Ocean daemon executes actions, enforces gates, owns sessions, and preserves the local-first trust boundary.
