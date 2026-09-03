# Loop: fable-5-1 routing (Claude Code OAuth arm)

## Mission
`claude-fable-5-1` (Fable 5.1) is pinned by live loops (events.md 02-09-26
shows `[claude-fable-5-1]`) but had NO routing anywhere. This branch adds it
end-to-end, mirroring the fable-5 pattern (and the #362 wire-id outage lesson).

Branch: `feat/fable-5.1` off `origin/main` @ 9fc88743 (ocean-os).
Upstream PR to mirror: #451 (GLM 5.3 Flash) — same shape, same PR body style.

## Current state (as of this wake)
**PR #452 is OPEN against main: https://github.com/Risingtides-dev/ocean-os/pull/452**

All code splices are applied and committed (`06e9912f`, pushed by a concurrent
loop wake that raced this one — no duplicate work, just redundant local
verification here). This wake additionally:
- Confirmed green: `cargo test -p ocean-protocol` (163+5), `cargo test -p
  ocean-agent` (239/239), `cargo clippy -p ocean-providers -p ocean-protocol
  -p ocean-agent --all-targets -- -D warnings` (clean), `cargo fmt --check`
  (clean).
- Added the missing root `events.md` ledger entry for this change (root
  AGENTS.md devlog-pass requirement — the PR-opening wake had skipped it),
  committed as `4b737887`, and confirmed `node scripts/check-ledger.mjs` and
  `cargo xtask docs-check` both still pass. Pushed to `feat/fable-5.1`.
- CI re-triggered on the new push (`check` ubuntu/macos, MSRV, cargo-deny,
  `events.md entry`) — all were `pending`/`in_progress` as of this wake, not
  yet resolved. `gh pr checks 452` had not returned a completed rollup before
  this wake ended.

Four touchpoints, all mirrored on the fable-5 arms:

1. `crates/ocean-protocol/src/types.rs` — `Model::anthropic_claude_fable_5_1()`
   (id `claude-fable-5-1`, anthropic-messages, 200k/16_384).
2. `crates/ocean-providers/src/lib.rs` —
   - menu: `m("claude-code-fable-5-1", "claude-code", "Claude Code Fable 5.1")`
   - resolver arm: `"claude-code-fable-5-1" | "claude-fable-5-1"` →
     `ProviderId::ClaudeCode` (wire id MUST resolve — pinned sessions replay it).
     `fable` shorthand intentionally stays on 5.0.
   - invariant list: `"claude-code-fable-5-1"` added to the
     routable-production-ids test list.
   - `fable_wire_id_round_trips_through_the_resolver` extended with a 5.1 loop.
3. `crates/ocean-agent/src/lib.rs` — both match arms
   (`Anthropic` and `ClaudeCode`) map the 5.1 ids to the new constructor.

## Remaining work (in order)
1. ~~`cargo test -p ocean-protocol` — must be green.~~ DONE.
2. ~~`cargo test -p ocean-agent` — must be green (constructor wiring compiles).~~ DONE.
3. ~~`cargo clippy -p ocean-providers -p ocean-protocol -p ocean-agent
   --all-targets -- -D warnings` — must be clean.~~ DONE.
4. ~~`cargo fmt --check` on touched crates (or `cargo fmt` then re-diff).~~ DONE.
5. ~~Commit, push `feat/fable-5.1`, open PR against main.~~ DONE — PR #452.
6. **Next wake: check `gh pr checks 452`.** If CI is green and no reviewer
   blockers, and merge is safe (no force-push, no gate-weakening), merge the
   PR (squash, matching repo convention — check how #451 was merged for the
   exact method) or wait one more cycle if CI is still running. If a check is
   red, fix it in this worktree (do not weaken the gate) and re-push.
7. Once merged: `git fetch origin && git log origin/main | grep -q
   <merge-commit-or-452>` to confirm, then update this file marking it merged,
   `cd` out of this worktree, `git worktree remove
   /Users/risingtidesdev/dev/ocean-os-worktrees/fable-5-1` (adjust path if
   different), and delete the launchd job per the wake instructions:
   `launchctl bootout gui/$(id -u)/com.ocean.fable-5-1-loop` and
   `rm ~/Library/LaunchAgents/com.ocean.fable-5-1-loop.plist`. That is the
   loop's exit condition — do this only after confirmed merge.

## Rules
- No force-push, no touching anything outside this worktree + the PR.
- If a gate is red, fix it in this worktree; do not weaken the gate.
- Each wake: read this file FIRST, do the next undone step, update this file.
- Two loop wakes can race (this happened once — 02-09-26 ~22:28). If you find
  work already done when you expected it undone, verify (don't re-do), fill
  any gap (e.g. missing events.md entry), and update this file to match
  observed reality rather than the stale plan.

[exit 0]
