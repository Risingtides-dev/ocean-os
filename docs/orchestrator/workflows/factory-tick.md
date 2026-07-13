---
name: ocean-os-factory-tick
description: Ocean-native factory loop for keeping ocean-os, ocean-surface, and ocean-agents moving through daemon-mediated tools without depending on any external orchestrator.
---

# Workflow: Ocean OS Factory Tick

This is the Ocean-native factory loop for keeping `ocean-os`, `ocean-surface`, and `ocean-agents` moving without depending on an external orchestrator repository. A single Ocean agent can execute serial steps through normal daemon-mediated tools, but every worker dispatch/fanout instruction in this workflow requires an installed orchestration extension. Without that extension, treat dispatch phases as an advisory/manual checklist—core daemon/runtime and Longhouse do not spawn workers. Longhouse may surface the workflow as context; the daemon remains generic permission-gated execution authority.

Throughout this document, **orchestrator** means that installed extension, never a core daemon/runtime/Longhouse subsystem. The extension owns worker definitions, tier routing, worktree lanes, lifecycle, and joins; it calls generic daemon tools and APIs for gated actions.

## Identity

- **Name:** `ocean-os-factory-tick`
- **Owner:** Ocean OS / Longhouse
- **Primary repo:** `~/dev/ocean-os`
- **Sibling repos:** `~/dev/ocean-surface`, `~/dev/ocean-agents`
- **Ledger:** Linear workspace `rising-tides-agents`, team `OCEAN`
- **GitHub org:** `Risingtides-dev`
- **Live daemon:** `http://127.0.0.1:4780` (`GET /health`, not `/v1/health`)

## Standing objective

Maintain a truthful, reviewed, continuously improving Ocean OS system by running this cycle:

`RECONCILE -> UNBLOCK -> ADVANCE -> REFILL -> REPORT`

A successful tick does at least one of:

- lands a clean reviewed PR;
- fixes a blocked/red PR;
- reconciles ledger drift;
- advances a live in-flight ticket;
- creates real Linear tickets from verified findings;
- reports an honest blocker and keeps every unblocked lane moving.

## Model routing (read before every tick)

When the orchestration extension is installed, it **routes** rather than reading the world
itself. Each phase below is tagged with the tier that does its work (see the model
routing rail in `FACTORY_GOAL.md` for the tier table):

- **`[cheap]`** — dispatch a haiku-tier worker, consume its compact finding. The
  orchestrator never pulls raw `git status`, PR JSON, CI logs, or full diffs into
  its own context.
- **`[mid]`** — dispatch a sonnet-tier implementation lane.
- **`[expensive]`** — the orchestrator itself, or an opus-tier worker, makes the
  call. Reserved for judgment, not reads.

A worker that hits something above its tier returns a flag (`escalate: <reason>`)
instead of guessing; the orchestrator then re-routes upward. Downshifting is free.

## Inputs

At the beginning of the tick, read:

1. `docs/orchestrator/FACTORY_GOAL.md`
2. this file
3. current git state for all three repos
4. current open PRs for all three repos
5. current Linear OCEAN ticket states relevant to open PRs / in-flight work
6. daemon health if live-path validation is relevant

Do **not** read or depend on `../ocean-orchestrator` for normal operation. That directory is legacy reference material only.

## Hard rails

### Ledger integrity

Create a Linear ticket first, then read back its real identifier, before stamping that ID on a branch, commit, PR, or status board entry. Never infer ticket IDs from memory, arithmetic, or adjacent ticket numbers.

If Linear writes fail:

1. stop creating new work items;
2. append findings to `docs/orchestrator/queued-findings.md` or another local queue file;
3. report the Linear failure clearly;
4. continue only already-ticketed lanes.

### Review gate

- Feature/logic PRs require green CI and a clean review pass.
- Address all standing P1/P2 review findings before merge.
- Test-only/docs-only PRs may merge on green CI when no material review risk exists.
- Never blind-merge a flagged PR.

### Isolation

Each implementation lane gets its own git worktree. Never run two code agents in the same checkout.

### Live daemon

- Standing authorization: the loop may kill, restart, and redeploy the live daemon from main as part of normal deploy — no per-restart approval.
- Still build/deploy only from up-to-date `main`.
- Prefer restarting when no agent turn is mid-render (check `/metrics ocean_turns_in_flight` or log activity), but dropping an in-flight turn is acceptable, not a blocker.
- Use a specific PID kill, not blind broad sweeps, so you don't take down unrelated processes; re-check supervision (launchd vs hand-launched) before each restart since the box gets reconfigured.
- Verify daemon health with `GET /health` after restart.

### Git hygiene

After each merge:

1. return the main checkout to `main`;
2. pull latest main;
3. delete the merged branch;
4. remove its worktree;
5. mark the ticket Done with the PR linked.

## Phase 1 — RECONCILE  `[cheap]`

Goal: make ledger, git, and PR state describe the same reality.

This is pure read-the-world work — the single biggest token sink if the
orchestrator does it itself. **Dispatch one cheap-tier worker** with the checklist
below and have it return only a compact structured finding; the orchestrator reads
the finding (a few lines), not the raw git/PR/Linear output.

Worker checklist (the worker runs this, not the orchestrator):

1. For each repo, inspect git status and current branch.
2. Fetch latest `main` and PR refs.
3. List open PRs.
4. For every recently merged PR, verify:
   - its ticket exists in Linear;
   - the ticket is Done or should be moved Done;
   - the PR link is attached or commented;
   - the local branch/worktree is cleaned up.
5. For every In Progress ticket, verify:
   - a branch, PR, or active worktree exists;
   - the branch name carries the correct read-back ticket ID;
   - the ticket is not orphaned.

Worker returns a compact finding, e.g.:

```text
repos: ocean-os@main clean | surface@main clean | agents 1 dirty (feat/slack-x)
open_prs: #312 OCEAN-340 (CI green, review clean) | #313 OCEAN-341 (CI red)
drift: OCEAN-338 In Progress but no branch/worktree → orphaned
phantom: none
```

The orchestrator then acts on the drift the worker reports — `[expensive]`, since any
ledger move is a judgment call:

6. If an In Progress ticket has no real lane, move it back to Todo with a comment, or respawn the lane if the work is still valid.
7. If any branch/commit/PR references a non-resolving ticket ID, stop new ticket creation and report the phantom-ID failure mode.

Output of this phase: the worker's compact reconciliation finding, carried into the final report.

## Phase 2 — UNBLOCK

Goal: open PRs first. Never start fresh work while mergeable reviewed PRs sit idle.

Triage cheap, fix mid, decide expensive. The orchestrator does not read CI logs or
diffs itself.

1. **`[cheap]`** Dispatch a worker to triage every open PR and return a compact
   per-PR status line: ticket ID + resolves?, CI green/red (+ first failing job if
   red), review clean / standing P1-P2 count, scope-matches-ticket. One worker, one
   finding block — not one read per PR by the orchestrator.
2. **`[mid]`** If CI is red: dispatch an implementation lane into that PR's worktree
   to reproduce, fix surgically, run relevant tests, and push. (Worktree isolation
   per the rail.)
3. **`[mid]`** If review has standing P1/P2 findings: dispatch a lane to verify each
   finding against code, fix real issues, reply to false positives with file/line
   evidence, and re-request review.
4. **`[expensive]`** Merge decisions stay with the orchestrator. If CI green and
   review clean, merge using the repo's normal strategy, then delete branch, clean
   the worktree, mark the ticket Done, and pull main. Never delegate a merge past a
   standing review finding.

Output of this phase: merged PRs, fixed PRs, or explicit blockers.

## Phase 3 — ADVANCE

Goal: move every in-flight lane forward.

1. **`[cheap]`** Dispatch a worker to report, per In Progress ticket, whether its
   branch/worktree exists and matches, plus a one-line recent-commit/stall summary.
2. **`[expensive]`** For each stalled lane the orchestrator decides the move:
   finish directly if small, respawn a bounded lane, split a non-atomic ticket, or
   mark Blocked after the 3-strike rule with a clear reason.
3. **`[mid]`** Dispatch the actual implementation/respawn work as a sonnet-tier lane
   in the matching worktree.
4. Ensure the ticket has a truthful progress comment when state changes.
5. Keep concurrency bounded; prefer finishing lanes over opening more.

Output of this phase: advanced branches/PRs or blockers.

## Phase 4 — REFILL

Goal: only create new work when the queue is thin and findings are real.

Trigger: fewer than 3 Todo/In Progress tickets remain, or John explicitly asks for discovery/refinement.

**Routing:** discovery reads are **`[cheap]`** — dispatch workers (one per division,
in parallel) to scan the sources below and return findings in the standard format.
The orchestrator does not read source/logs/docs for discovery itself. Verifying a
finding is real and worth a ticket, and the ticket-creation/read-back, stay
**`[expensive]`** with the orchestrator. For a full refinement pass, hand off to
`refinement-wave.md`.

Discovery sources:

- failing/flaky tests;
- CI failures;
- daemon logs and health endpoints;
- doc-vs-code drift;
- TODO/FIXME debt with live-path impact;
- security seams;
- performance bottlenecks;
- incomplete Longhouse/workflow/canvas/call/surface wiring;
- user-observed failures.

For each finding:

1. Verify in real code, docs, logs, or endpoint output.
2. Write a one-outcome ticket title.
3. Create the Linear ticket.
4. Read back the real ID.
5. Only then plan the branch/worktree.
6. Prefer tickets with disjoint file scopes so a wave can run in parallel.

If Linear is down, queue findings locally and report that ticket minting is blocked.

Output of this phase: new Linear tickets or queued findings.

## Phase 5 — REPORT

End every tick with a concise report:

- PRs merged this tick;
- tickets moved and to what state;
- PRs fixed or sent back for review;
- blockers and who owns them;
- tests/verification actually run;
- first action for the next tick.

Do not claim Done from intention. Report skipped verification as skipped.

## Extension-owned worker lane template

This payload is consumed by an installed orchestration extension; Longhouse/core may recommend it but do not spawn or manage the worker. When the extension dispatches an implementation lane, give the worker:

```text
Tier: mid                          # cheap | mid | expensive — match the rail's tier table
Ticket: OCEAN-___
Repo: ~/dev/<repo>
Worktree: ~/dev/<repo>-worktrees/<ticket-slug>
Branch: <type>/OCEAN-___-<slug>
Scope: <files/directories it may edit>
Objective: <one outcome, user-traceable>
Do not edit: <protected files / other lanes>
Verification: <commands/tests required>
PR requirement: open PR linked to OCEAN-___, then request review if feature/logic>
Safety: do not touch live daemon :4780; use isolated config/port for daemon tests>
Escalation: if the work needs a higher tier (security/architecture call, ambiguous
  review verdict), STOP and return `escalate: <reason>` instead of guessing>
```

For a **`[cheap]`** read/triage worker, the dispatch is lighter — no branch/worktree,
just the read checklist and the required compact output shape:

```text
Tier: cheap
Task: <RECONCILE sweep | PR triage | ADVANCE status | discovery scan of division X>
Read: <repos / PRs / Linear scope / source dirs to inspect — read-only>
Return: a compact structured finding only (see the phase's example block).
  Do NOT paste raw git/PR/CI/diff output. Summarize to the fields asked for.
Escalation: flag anything needing judgment as `escalate: <reason>`; do not decide it.
```

## Merge criteria

A PR can merge only when:

- ticket ID resolves in Linear;
- scope matches ticket;
- CI is green or the failure is unrelated and documented;
- required review is clean;
- relevant local verification passed or is explicitly skipped with reason;
- no protected live process is affected;
- the final diff is reviewable.

## Stop conditions

Stop opening new lanes when:

- Linear cannot create/read tickets;
- a phantom ticket ID is detected;
- the only remaining work requires John's architecture/security/deploy decision;
- concurrency budget is full;
- continuing would require destructive action John did not request.

Do not stop the workflow just because the board looks quiet. If unblocked work exists, keep moving it. If no verified work exists, run discovery or report that the queue is empty and ask for direction.
