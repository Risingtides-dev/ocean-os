# Immediate Provider Halt — Design Specification

- **Status:** Approved (immediate Halt only)
- **Date:** 2026-07-10
- **Scope:** `ocean-runtime` agent loop — the blocking provider stream-read boundary
- **Out of scope:** No production code touched by this document. No daemon, SSE, surface, or client changes.

---

## 1. Problem statement (corrected)

A user **Halt** issued while the provider stream is mid-flight does not take effect
promptly when the socket is silent.

In `crates/ocean-runtime/src/agent_loop.rs`, each round drives the provider stream
inside `stream_work`, whose body is:

```rust
while let Some(ev) = stream.next().await {
    if is_cancelled(config) { return Err(AgentError::Cancelled); }
    ...
}
```

The cancellation check is **post-yield**: it runs only *after* `stream.next().await`
resolves and hands back an event. If the provider accepts the connection and then
goes silent (no bytes — the "accepts then stalls" hang), `stream.next().await`
blocks. Until it yields, `is_cancelled(config)` is never evaluated, so a Halt
landing on the silent socket waits for whichever transport/deadline bound fires
first:

- the 120s byte-idle `read_timeout` (which surfaces as a stream error), or
- the 300s provider-round deadline (which surfaces as `AgentError::Timeout`).

So Halt during a silent socket is **not immediate** — it is bounded only by the
slowest wall-clock limit, not by the user's intent. This is the proven defect.

> Note on the in-tree comment: the stream-loop comment claims the post-yield check
> breaks out "immediately." It does not — that claim is the bug this design fixes.

## 2. Non-goals

This design is deliberately narrow. The following are **explicitly rejected**:

- **No automatic semantic-idle watchdog.** A timer that cancels a round because
  the stream "looks idle" would conflate user intent with heuristics and would kill
  legitimate slow-but-alive streams (long reasoning, slow tool-using turns). The
  120s `read_timeout` is a *transport-level* no-bytes bound, not a semantic one,
  and it stays exactly as-is. Cancellation remains **user-initiated only**.
- **No daemon / SSE / surface timeout plumbing.** The fix lives entirely in the
  runtime's stream-read boundary. There is no new client deadline, no SSE timeout,
  no surface-side cancel watchdog. `OCEAN_TURN_TIMEOUT_SECS` and the reqwest
  timeouts are untouched.
- **No change to provider SSE loops.** The four providers (anthropic, codex,
  google, openai) each duplicate a post-yield cancel check, but `agent_loop` is the
  single consumer of every provider stream. Fixing the read boundary there is one
  cancellation boundary for the whole system — the provider loops are not edited.

## 3. Architecture & cancellation data flow

```
client Halt ──► POST /v1/requests/{id}/cancel
                         │  (sets StreamOptions::cancel CancellationToken)
                         ▼
              ocean-runtime run_agent (per round)
                         │
        ┌────────────────┴────────────────┐
        │ round_deadline = now + 300s     │   (shared across retries)
        │ loop { attempt ──► stream_work }│
        └────────────────┬────────────────┘
                         │
                tokio::time::timeout_at(round_deadline, stream_work)
                         │
              stream_work: stream.next() ◄── THE BOUNDARY THIS DESIGN FIXES
                         │
          ┌──────────────┴──────────────────────────┐
          │ Cancelled  → AgentError::Cancelled      │
          │ Timeout    → AgentError::Timeout        │
          │ provider err → AgentError::Other/…      │
          └──────────────┬──────────────────────────┘
                         │  (single unwind path out of run_agent)
                         ▼
              daemon runtime.prompt — terminalization (SINGLE OWNER)
                         │
                  exactly one TurnFinished
```

The `CancellationToken` is read by two helpers in `agent_loop.rs`:

- `is_cancelled(config)` — a non-async poll (`token.is_cancelled()`); returns
  `false` when no token is wired (ad-hoc/embedded runs).
- `cancelled(config)` — an async future that resolves once the token trips; when
  no token is present it returns `std::future::pending()` and **never resolves**.

The fix converts the post-yield `is_cancelled` poll at the read boundary into a
pre-yield race using `cancelled(config)`. Because `cancelled()` is `pending()`
without a token, the no-cancel/embedded path is provably unchanged — the `select!`
reduces to awaiting `stream.next()` alone.

## 4. Preservation of the existing time bounds

All three bounds are preserved exactly; none are added, removed, retuned, or
reinterpreted.

| Bound | Value | Owner | Role | After this design |
|---|---|---|---|---|
| `connect_timeout` | 10s | `ocean-protocol/src/http.rs` (`DEFAULT_CONNECT_TIMEOUT`) | Hard cap on connection establishment (DNS+TCP+TLS). Surfaces as `is_connect()`. | **Unchanged** |
| `read_timeout` | 120s | `ocean-protocol/src/http.rs` (`DEFAULT_READ_TIMEOUT`) | Idle **between-reads** timeout — fires only when no bytes arrive for the window. Not a total deadline. Surfaces as `is_timeout()`. | **Unchanged** |
| round deadline | 300s (`OCEAN_TURN_TIMEOUT_SECS`, default `DEFAULT_TURN_TIMEOUT_SECS`) | `ocean-runtime` agent loop | Total per-**round** deadline wrapping **only** `stream_work` (provider request + full stream consumption), shared across the in-loop retries via `tokio::time::timeout_at(round_deadline, …)`. | **Unchanged** |

Two ownership facts this design relies on and must not disturb:

1. **The 300s is per-round, not per-turn.** It wraps only `stream_work`. Tool
   execution and permission waits sit *outside* it — they already race cancel via
   the existing biased `tokio::select!` at the tool-execution site. The Halt fix
   brings the stream-read boundary to the same standard the tool boundary already
   meets.
2. **No total request `timeout()` is set on the streaming reqwest client.** That is
   intentional (so long streaming completions are never truncated) and stays that
   way. The round deadline is the only total bound, and it is owned by the runtime,
   not by reqwest.

Timeout ownership is therefore unambiguous: 10s/120s are transport concerns owned
by `ocean-protocol`; 300s is a round concern owned by `ocean-runtime`. Halt is a
user-intent concern owned by the cancel token, and it now wins the race at the one
boundary it previously could not.

## 5. Exactly-once terminalization responsibility

Terminalization — emitting the single rich `TurnFinished` that closes a turn — is
**not** owned by the agent loop. `run_agent` returns a terminal `AgentError`
(`Cancelled`, `Timeout`, `Other`, …); the daemon's `runtime.prompt` is the sole
owner that converts that into exactly one `TurnFinished` and clears the running
flag.

The existing test `accepted_provider_error_emits_failed_turn_finished_and_clears_running`
proves the contract for the provider-error path: one failed `TurnFinished`, running
flag cleared, no duplicate. A Halt that now unwinds promptly as
`AgentError::Cancelled` flows through the **same** single terminalization path, so
it inherits exactly-once for free. **This design adds no new terminal event, no
second `TurnFinished`, and no parallel cancel-finalization.** The fix is strictly
about *when* the loop observes the cancel, not *how* the turn is closed.

## 6. The fix: biased `tokio::select!` at the stream-read boundary

The blocking read is `stream.next().await`. Today it is followed by a post-yield
`is_cancelled(config)` check. The change wraps the read itself in a biased
`tokio::select!` that races the read against cancellation — mirroring the
already-shipped, already-tested tool-execution race:

```rust
// Tool-execution boundary (already in tree):
tokio::select! {
    biased;
    () = cancelled(config) => return Err(AgentError::Cancelled),
    outcomes = futures::future::join_all(futs) => outcomes,
}
```

Applied at the stream-read boundary, the round becomes (sketch):

```rust
loop {
    // Race the blocking read against the cancel token. `cancelled()` is
    // `pending()` when no token is wired, so this select! is a pure
    // `stream.next().await` on the embedded/no-cancel path.
    let next = tokio::select! {
        biased;
        () = cancelled(config) => return Err(AgentError::Cancelled),
        ev = stream.next() => ev,
    };
    let Some(ev) = next else { break };
    // ... existing event handling (Done / Error / TextDelta / ThinkingDelta) ...
}
```

Properties this guarantees:

- **One cancellation boundary.** Every provider stream is consumed through this
  loop, so every stream is now halt-aware regardless of which of the four providers
  produced it.
- **No provider edits.** anthropic/codex/google/openai are untouched.
- **No-cancel path unchanged.** `cancelled(config)` returns `pending()` with no
  token (see §3); the `select!` resolves to the read arm only.
- **Precedence is explicit and bounded.** With `biased`, the cancel arm is polled
  first each iteration, so a token already tripped wins over a chunk ready in the
  same poll. Cancel vs. deadline vs. provider-error resolve to distinct terminal
  errors (`Cancelled` / `Timeout` / `Other`), all funneled through the single
  terminalization owner in §5.
- **Cleanup is drop.** When the cancel arm wins, the `stream.next()` future is
  dropped (no longer polled), releasing the in-flight HTTP connection — no leak,
  no orphan task. This matches the tool-execution race's documented behavior
  ("we drop the in-flight futures … no leak").

### 6.1 Cancellation precedence and cleanup summary

1. **Halt (user)** — highest precedence at the read boundary; wins the biased race
   immediately, even on a silent socket. → `AgentError::Cancelled`.
2. **Round deadline (300s)** — outer `timeout_at(round_deadline, stream_work)`;
   fires only if the round never completes. → `AgentError::Timeout`.
3. **Provider error / RetryExhausted** — surfaced from the stream; transient
   clean-round failures retry under the shared deadline; exhaustion bubbles to
   daemon model-failover. → `AgentError::Other`/etc.

All three unwind through one path, terminate through one owner (§5), and release
their in-flight resource by drop.

## 7. TDD plan

### 7.1 New test — never-yielding stream resolves promptly on Halt

The defining regression: a stream that **never** yields must still be halt-able
promptly. A mock provider whose `stream(...)` returns a stream whose `next()` is
`std::future::pending()` (never resolves) reproduces the silent socket. The test:

1. Starts a run on the never-yielding stream (no `Done`, no error, no bytes).
2. From a second task, trips the `CancellationToken` a short, fixed interval after
   the round begins.
3. Asserts the run unwinds with `AgentError::Cancelled` in a time **far below** the
   120s read-timeout / 300s round-deadline (e.g. a sub-second budget), proving the
   `select!` — not a wall-clock timeout — broke the blocking read.

Pre-fix this test blocks for the full read/deadline window and fails the time
budget; post-fix it passes immediately. This is the single load-bearing test for
the change.

### 7.2 Existing regression tests that must stay green

- `cancel_after_tool_round_unwinds_clean_no_orphan` — between-rounds cancel still
  unwinds with `Cancelled` and requests no extra round (the `run_agent` start-of-loop
  `is_cancelled` guard is untouched).
- The in-flight tool cancellation test (slow tool + cancel from another task) —
  tool-execution race is untouched.
- `accepted_provider_error_emits_failed_turn_finished_and_clears_running` — exactly-
  once terminalization (§5) is untouched; a now-prompt `Cancelled` must still
  produce exactly one `TurnFinished`.
- `round_retry.rs` — clean-round transient retry under the shared round deadline is
  untouched; the read-boundary change does not alter retry eligibility, the
  `attempt_emitted` guard, or backoff.

The never-yielding test is added; no existing test is weakened or deleted.

## 8. Implementation environment: isolated HEAD-based worktree (required)

The shared checkout at `~/dev/ocean-os` carries a **foreign ~249-line revert** in
its working tree that **undoes the committed clean-round retry**: `agent_loop.rs`
is shown modified and `tests/round_retry.rs` is shown deleted relative to `HEAD`.
The committed tree at `HEAD` (`80ac2d04`, with `508085d7` "in-loop round retry" as
ancestor) is the source of truth: it contains the retry loop and the shared
`tokio::time::timeout_at(round_deadline, …)` deadline.

Therefore the implementation **must not** branch from the dirty shared tree. It
must:

1. Create an isolated worktree pinned to committed `HEAD` (`git worktree add` from
   `80ac2d04`), so the retry loop and `round_retry.rs` are present.
2. Apply the read-boundary `select!` change and the new never-yielding test there.
3. Land via that worktree, never rebasing onto or committing the foreign revert.

Basing work on the shared tree would silently inherit the pre-retry `stream_work`
shape and the deleted retry tests, desynchronizing the fix from the real committed
contract.
