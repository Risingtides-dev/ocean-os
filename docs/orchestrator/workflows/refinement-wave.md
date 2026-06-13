---
name: ocean-os-refinement-wave
description: Discover verified, ticketable improvements across the Ocean ecosystem by scanning divisions and synthesizing findings into real Linear tickets.
---

# Workflow: Ocean OS Refinement Wave

This is the Ocean-native discovery/refinement workflow for finding the next real work in Ocean OS without relying on external workflow files. The Ocean agent runs it through normal daemon tools. Longhouse may recommend divisions, skills, or subagent specs; it does not execute local side effects.

## Identity

- **Name:** `ocean-os-refinement-wave`
- **Purpose:** discover verified, ticketable improvements across the Ocean ecosystem.
- **Repos:** `~/dev/ocean-os`, `~/dev/ocean-surface`, `~/dev/ocean-agents`
- **Ledger:** Linear OCEAN team
- **Primary output:** real Linear tickets, or locally queued findings if Linear is unavailable.

## When to run

Run this workflow when:

- the factory queue has fewer than 3 Todo/In Progress tickets;
- a major merge epoch just completed;
- John asks for refinement/discovery;
- repeated failures suggest the current ticket set missed root cause;
- docs and code likely drifted after a heavy wave.

Do **not** invent speculative work. Every finding must be tied to code, docs, logs, endpoint output, CI, or a user-observed failure.

## Inputs

Read:

1. `docs/orchestrator/FACTORY_GOAL.md`
2. `docs/orchestrator/workflows/factory-tick.md`
3. current repo status and recent git history for each repo
4. current open/active Linear OCEAN tickets
5. recent CI failures and open PR reviews
6. relevant docs for the focus domains

Do not depend on `../ocean-orchestrator`. It is legacy reference only.

## Severity rubric

- **P0:** security bypass, data loss, live daemon crash/hang, permission-gate failure.
- **P1:** live-path breakage, session loss, major protocol mismatch, unusable core surface.
- **P2:** important incomplete feature, reliability/perf issue, missing route or tool integration.
- **P3:** polish, docs drift, small UX gaps, cleanup with low live-path risk.

## Phase 1 — Select focus divisions

Pick 3–6 divisions based on current system risk. Default set:

1. **Live daemon / runtime reliability**
   - `crates/ocean-daemon`, `crates/ocean-runtime`, session/event flows, permissions, cancellation.
2. **Protocol parity**
   - daemon SDK/core types vs web/native/ACP/Slack/voice consumers.
3. **Longhouse / workflow / subagent readiness**
   - `crates/ocean-longhouse`, prep hook, skills, workflow briefs, subagent specs, council tooling.
4. **Surface completion**
   - `../ocean-surface`, GUI/PWA/voice/canvas/client state.
5. **Docs vs code**
   - architecture/operator docs vs shipped behavior.
6. **Ops / CI / deploy safety**
   - GitHub Actions, launchd specs, health routes, build/test ergonomics.

## Phase 2 — Research each division  `[cheap]`

This is the heaviest read-spend in the whole factory. **Dispatch one cheap-tier
worker per division, in parallel** (worktree isolation not needed — these are
read-only). Each worker scans its division and returns findings in the format
below; the orchestrator never reads division source/docs/logs into its own context.
The worker's output is a list of finding blocks, nothing else.

Each division worker (not the orchestrator) does:

1. Read the relevant docs and source.
2. Run targeted commands when cheap and safe.
3. Compare intended behavior to actual behavior.
4. Record only findings that are specific and reproducible.
5. Include file paths and, when useful, line references.

Finding format:

```text
Title: <short user-impact title>
Repo: ocean-os | ocean-surface | ocean-agents | cross
Severity: P0 | P1 | P2 | P3
Evidence: <file:line, command output, endpoint result, CI link, or observed failure>
Impact: <what user/system behavior is wrong>
Proposed ticket: <one atomic outcome>
Suggested scope: <files/directories likely touched>
Verification: <test/build/check that would prove fixed>
```

## Phase 3 — Synthesize  `[expensive]`

The orchestrator (or an opus-tier worker) combines the cheap workers' findings. This
is judgment — de-dup, root-cause, ordering — so it stays at the top tier. It runs on
the compact findings the workers returned, not on re-read source.

Combine findings across divisions:

1. De-duplicate root causes.
2. Merge duplicate symptoms into one atomic root ticket.
3. Split multi-outcome findings into smaller tickets.
4. Identify order dependencies:
   - security/trust-boundary first;
   - daemon `main.rs` pipeline serialized;
   - protocol type changes before surface consumers;
   - docs-only can parallelize freely.
5. Prefer a wave of small disjoint tickets over a broad epic.

## Phase 4 — Ticket or queue

For each synthesized finding:

1. Confirm it is not already covered by an open Linear ticket.
2. Create the Linear ticket in team OCEAN.
3. Read back the real identifier.
4. Add evidence, scope, and verification to the ticket body.
5. Only after ID read-back, propose branch/worktree names.

If Linear cannot create/read:

- append the finding to `docs/orchestrator/queued-findings.md`;
- do not mint fake IDs;
- report the block.

## Phase 5 — Wave plan

Produce a wave plan with:

- tickets grouped by repo;
- file scopes for each ticket;
- dependencies/order constraints;
- which tickets can run in parallel;
- required verification commands;
- review requirements.

Keep initial concurrency to ~3 implementation lanes unless John explicitly asks for a larger wave.

## Phase 6 — Report

End with:

- number of findings researched;
- tickets created or queued;
- highest severity item;
- recommended next implementation wave;
- blockers.

## Guardrails

- No destructive actions.
- No daemon restart.
- No branch creation before ticket ID read-back.
- No false completion claims.
- No dependence on external orchestrator repos or tools as runtime.
