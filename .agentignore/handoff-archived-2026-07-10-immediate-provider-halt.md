# Handoff — Immediate provider Halt cancellation

**As of:** 2026-07-10
**Repository:** `/Users/risingtidesdev/dev/ocean-os`
**Source of truth:** `origin/main` at or after `a0eef51c8d0bd2402f78049e53093954c1f93155`
**Status:** Design approved and committed. No production code or test changes made yet.

## Mandatory context-consumption protocol

1. Read this handoff once, then read the approved spec below.
2. Copy the active implementation facts into your own short todo/implementation plan.
3. **Before editing code, move this file out of active context**:
   `/Users/risingtidesdev/dev/ocean-os/handoff.md` →
   `/Users/risingtidesdev/dev/ocean-os/.agentignore/handoff-archived-2026-07-10-immediate-provider-halt.md`.
4. Do not keep rereading the archived handoff during implementation. The committed spec and current code are authoritative; re-derive any disputed detail from them.
5. At completion: if nothing remains, leave no new live handoff. If work remains, create a fresh concise `handoff.md` containing only current unresolved state, then archive it the same way when the next agent consumes it.

This prevents a stale handoff from competing with the code/spec and drifting across sessions. `.agentignore` is a directory, not a file.

The superseded TUI/provider handoff from the prior lane is already archived at:
`/Users/risingtidesdev/dev/ocean-os/.agentignore/handoff-archived-2026-07-10.md`.

## Goal

Make user-initiated **Halt** interrupt a provider stream immediately when the socket is silent. Implement the smallest proven runtime fix with TDD. Do not add automatic turn/semantic-idle timeouts.

## Approved design

Committed specification:
`/Users/risingtidesdev/dev/ocean-os/docs/superpowers/specs/2026-07-10-immediate-provider-halt-design.md`

Remote commit:
`a0eef51c8d0bd2402f78049e53093954c1f93155` (`docs(runtime): specify immediate provider Halt cancellation`)

John explicitly approved **Immediate Halt only**:

- Race the blocking provider `stream.next()` read against the existing cancellation token using a biased `tokio::select!` in `ocean-runtime`.
- Cancellation wins immediately and returns `AgentError::Cancelled` through the existing unwind/terminalization path.
- Preserve the existing 10s connect timeout, 120s byte-idle read timeout, and 300s provider-round deadline.
- Preserve clean-round transient retry behavior and the shared round deadline.
- Do not add a semantic-idle watchdog.
- Do not add daemon, SSE, browser/surface, client, or provider-loop timeout plumbing.
- Do not emit another terminal event. The daemon remains the single owner of exactly-once `TurnFinished` publication.

## Corrected root cause

The earlier caveat that the daemon could hold a turn open forever was overstated.

Current committed behavior already bounds provider work:

- `connect_timeout = 10s`
- byte-idle `read_timeout = 120s`
- provider-round deadline = 300s via `OCEAN_TURN_TIMEOUT_SECS`
- `runtime.prompt` emits exactly one rich failed terminal event when the agent loop returns; `accepted_provider_error_emits_failed_turn_finished_and_clears_running` proves this.

The observed ~484s DeepSeek failure was not an unbounded provider stall. Round 1 executed two long bash tools (~180s each; ~429s including tool work), then round 2 retried provider failures and ended after ~55s. Tool and permission execution intentionally sit outside the 300s provider-round deadline.

The actual defect is in:
`crates/ocean-runtime/src/agent_loop.rs` around the stream-consumption loop (line ~187 on the approved-spec snapshot).

Current shape:

```rust
while let Some(ev) = stream.next().await {
    if is_cancelled(config) {
        return Err(AgentError::Cancelled);
    }
    // event handling
}
```

The cancellation check is post-yield. If `stream.next().await` is blocked on a silent socket, Halt is not observed until bytes arrive or a timeout fires. The nearby comment claiming this breaks out “immediately” is false.

Use the already-shipped in-flight tool cancellation race as the in-tree pattern (`cancelled(config)` + biased `tokio::select!`). Do not invent a second cancellation mechanism.

## TDD execution order

Use subagent-driven development in an isolated worktree:

1. **Tester agent first:** add one high-signal regression test in the existing `ocean-runtime` test structure. The provider stream must never yield. Start `run_agent`, trip the existing `CancellationToken` from another task, and require `AgentError::Cancelled` within a sub-second test budget.
2. Run that focused test against pre-fix code and observe the expected failure/timeout. If it passes immediately, the test is wrong.
3. **Rust runtime agent second:** replace only the post-yield blocking read boundary with a biased race:

```rust
let next = tokio::select! {
    biased;
    () = cancelled(config) => return Err(AgentError::Cancelled),
    event = stream.next() => event,
};
let Some(ev) = next else { break };
```

Adapt names to the actual current code; do not refactor adjacent retry/event handling.
4. Re-run the new test and observe it pass.
5. Run focused regressions:
   - `cancel_after_tool_round_unwinds_clean_no_orphan`
   - `cancel_during_long_tool_aborts_promptly_without_awaiting_completion`
   - all four tests in `crates/ocean-runtime/tests/round_retry.rs`
   - `accepted_provider_error_emits_failed_turn_finished_and_clears_running` in `ocean-daemon`
6. Run the smallest package-level tests covering modified code. Do not claim success from compile-only evidence.
7. Ask a reviewer agent to inspect cancellation precedence, retry preservation, and exactly-once terminalization before landing.
8. Commit only the test, minimal runtime change, plan/spec bookkeeping, and append-only `events.md` entry. Use GitButler (`but`) for version-control writes.

## Critical repository hazard

**Do not edit the shared checkout.** It contains foreign in-progress work and a large revert of the exact runtime area:

- Shared working tree branch was `feat/ocean-tui-shell-rebuild` at `80ac2d04`.
- Local `main` was stale.
- Dirty `crates/ocean-runtime/src/agent_loop.rs` removes the committed clean-round transient retry loop.
- Dirty state also deletes `crates/ocean-runtime/tests/round_retry.rs`.
- The committed retry implementation is from `508085d7` and must remain intact.

Create an isolated checkout/worktree from the current `origin/main` (which includes `a0eef51c` and the retry commit). Verify `round_retry.rs` exists before adding the failing test. Never copy the dirty shared `agent_loop.rs` into the isolated worktree. Never use `git add -A`, `but commit` without selected change IDs, or any whole-tree staging operation.

If GitButler cannot operate because the shared checkout is not on `gitbutler/workspace`, do not run `but setup` there—it attempts a checkout and collides with foreign dirty files. Work from the clean isolated checkout and land only selected paths.

## Plan status

A read-only planning subagent mapped the exact symbols and tests but was cancelled before delivering a plan file. No implementation-plan document exists yet. Create a concise plan from the committed spec before editing; do not repeat broad investigation.

Mapped anchors from committed code/spec:

- Defect: `crates/ocean-runtime/src/agent_loop.rs` stream read around line ~187 (line numbers may shift on current main).
- Existing tool-exec `tokio::select!` pattern: same file around lines ~512–516 on the inspected snapshot.
- `is_cancelled` / `cancelled` helpers: same file around ~1094 / ~1107.
- Round deadline + retry: same file around ~171–267.
- Retry tests: `crates/ocean-runtime/tests/round_retry.rs` (four tests).
- Daemon terminalization regression: `accepted_provider_error_emits_failed_turn_finished_and_clears_running`.

## Done means

- A never-yielding provider stream is cancelled promptly by user Halt.
- The new regression failed before the fix and passes after it.
- Existing tool-cancel, between-round cancel, round-retry, and terminal-event tests remain green.
- No timeout values or ownership changed.
- No provider, daemon/SSE, or surface code changed.
- The clean committed branch is landed without the foreign shared-tree revert.
- `events.md` records the fix and verification.
