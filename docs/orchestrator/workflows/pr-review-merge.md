---
name: ocean-os-pr-review-merge
description: Daemon-mediated PR gate for Ocean OS repos — verifies ticket scope, CI, and review findings before merge, then cleans up branch and worktree state.
---

# Workflow: Ocean OS PR Review and Merge

This workflow handles open PRs for Ocean OS repos using Ocean itself. It replaces external orchestrator review loops with an in-repo, daemon-mediated protocol.

## Identity

- **Name:** `ocean-os-pr-review-merge`
- **Repos:** `~/dev/ocean-os`, `~/dev/ocean-surface`, `~/dev/ocean-agents`
- **Ledger:** Linear OCEAN team
- **Goal:** land only truthful, reviewed, verified PRs and keep branch/worktree state clean.

## Inputs

For each PR:

- GitHub PR title/body/diff/checks/reviews;
- linked Linear ticket;
- local branch/worktree if present;
- relevant CI logs;
- relevant local verification command.

## Model routing

Per the model routing rail in `FACTORY_GOAL.md`: a **`[cheap]`** worker does the
reading — classify (Phase 1), verify ticket/scope (Phase 2), read CI status + first
failing job (Phase 3), summarize review findings (Phase 4) — and returns one compact
status block. **`[mid]`** lanes do the fixes (Phases 3–4). The **merge decision
(Phase 6)** and any security/trust-boundary scrutiny are **`[expensive]`** and stay
with the orchestrator. The orchestrator never reads CI logs or full diffs itself.

## Phase 1 — Identify PR class  `[cheap]`

Classify the PR:

- **security/trust-boundary:** permission gates, auth, token minting, local execution authority.
- **daemon pipeline:** `crates/ocean-daemon/src/main.rs`, agent turn path, SSE/events, sessions.
- **protocol/API:** `ocean-core`, `ocean-agent-sdk`, route schemas, surface consumers.
- **feature/logic:** runtime/tool/provider/call/canvas/surface behavior.
- **docs/test-only:** docs, tests, comments, CI-only changes.

Security/trust-boundary PRs require extra scrutiny and may require John's decision even when green.

## Phase 2 — Verify ticket and scope

1. Confirm the linked ticket ID resolves in Linear.
2. Confirm the PR branch/title/body references the same ID.
3. Confirm the diff matches the ticket's one-outcome scope.
4. If scope drift exists, either send it back or split follow-up tickets.

Do not merge PRs with phantom/non-resolving ticket IDs.

## Phase 3 — Check CI

1. Read CI status.
2. If red, inspect failing logs.
3. Reproduce locally when practical.
4. Fix in the PR branch/worktree.
5. Push and wait for CI again.

Daemon-touching PRs should be verified with workspace-level commands when appropriate, not only crate-local builds.

## Phase 4 — Check review

1. Read all review comments.
2. Treat P1/P2 findings as blocking until addressed or proven false with evidence.
3. If a finding is real, fix surgically and push.
4. If false positive, reply with file/line evidence.
5. Re-request review for feature/logic/security changes.

Never blind-merge a flagged PR.

## Phase 5 — Local verification

Run the smallest sufficient verification:

- docs-only: markdown/link sanity when relevant;
- Rust unit change: targeted `cargo test -p <crate> <test>`;
- daemon/API change: relevant route tests and, if needed, `cargo build --workspace`;
- protocol changes: producer and consumer compile/test;
- surface changes: relevant package checks/builds.

Record what passed and what was skipped.

## Phase 6 — Merge  `[expensive]`

The merge decision is the orchestrator's, never a worker's. Merge only when:

- ticket resolves;
- scope is right;
- CI is green or unrelated failure is documented;
- blocking review findings are closed;
- verification is done or explicitly skipped with reason;
- no live daemon restart/deploy is required.

After merge:

1. mark Linear ticket Done;
2. attach/comment the PR link if missing;
3. switch main checkout to `main`;
4. pull latest;
5. delete local/remote branch if appropriate;
6. remove worktree;
7. report the merge.

## Phase 7 — Send back / block

If the PR cannot merge:

- set ticket status honestly;
- comment the blocker with evidence;
- assign owner if known;
- apply 3-strike circuit breaker for repeated failed attempts.

## Output

A short report:

```text
PR: #___ <title>
Ticket: OCEAN-___ <state>
Decision: merged | fixed+pushed | sent back | blocked
Verification: <commands/results>
Next: <follow-up or none>
```
