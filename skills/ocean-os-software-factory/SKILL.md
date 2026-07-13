# Ocean OS Software Factory

## Purpose

Use this skill when the operator wants to keep Ocean OS development moving through Ocean itself instead of Claude Code: workflow ticks, PR triage, Linear reconciliation, refinement waves, or migration of the old `../ocean-orchestrator` protocol into Longhouse/Ocean-native routines.

The job is to run a verified software factory, not just write code. The loop is:

`RECONCILE -> UNBLOCK -> ADVANCE -> REFILL -> REPORT`

## Source materials

Read these first when running the factory:

1. `docs/orchestrator/FACTORY_GOAL.md` — standing goal, rails, circuit breakers.
2. `docs/orchestrator/workflows/factory-tick.md` — Ocean-native tick protocol.
3. `docs/orchestrator/workflows/refinement-wave.md` — Ocean-native discovery protocol.
4. `docs/orchestrator/workflows/pr-review-merge.md` — Ocean-native PR gate.
5. `docs/LONGHOUSE.md` — Longhouse boundary: advisory coordination only; daemon owns execution and gates.

Do not depend on `../ocean-orchestrator` for normal operation. It is legacy reference material only.

## Non-negotiables

- Orchestration prerequisite: worker dispatch/fanout requires an installed extension. Without one, run this skill serially/manual through normal daemon tools; core daemon/runtime/Longhouse do not spawn workers.
- Model routing: the extension orchestrator routes, it does not read the world. Push status
  sweeps / CI & log reads / ledger read-back / discovery scans to **cheap-tier**
  workers and consume their compact findings; **mid-tier** runs implementation
  lanes; reserve the **expensive (opus) tier** for judgment — merges on flagged PRs,
  architecture/security calls, synthesis. Every dispatch carries an explicit `Tier:`.
  Full tier table in `FACTORY_GOAL.md`.
- Linear ticket first. Read back the real ID before stamping branch/commit/PR.
- Do not derive ticket IDs from arithmetic or memory.
- One worktree per implementation lane.
- Open PRs before new work.
- Codex/review findings are real; never blind-merge flagged PRs.
- Never restart/kill the live daemon on `:4780` unless the operator explicitly authorizes it.
- For daemon-touching work, verify with the full workspace build/test path required by the ticket.
- End every tick with a short report: merged PRs, tickets moved, blockers, next first action.

## Ocean-native migration direction

The old `../ocean-orchestrator/*.workflow.js` files are legacy reference material only. The Ocean-native source of truth is now:

- repo-local Longhouse skills/SOP briefs under `./skills/**`, so the daemon prep hook injects them automatically;
- durable workflow docs under `docs/orchestrator/workflows/**`;
- `POST /v1/workflows/prepare` and other advisory preparation for installed extensions;
- extension-owned worker dispatch/orchestration over generic daemon execution seams;
- normal Ocean daemon tools for all local actions.

Longhouse can recommend, rank, convene, and assemble advisory briefs. It must not own general subagent dispatch, bypass daemon permission gates, or mutate local state directly.
