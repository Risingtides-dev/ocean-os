# Loop: fable-5-1 routing (Claude Code OAuth arm)

## Mission
`claude-fable-5-1` (Fable 5.1) is pinned by live loops (events.md 02-09-26
shows `[claude-fable-5-1]`) but had NO routing anywhere. This branch adds it
end-to-end, mirroring the fable-5 pattern (and the #362 wire-id outage lesson).

Branch: `feat/fable-5.1` off `origin/main` @ 9fc88743 (ocean-os).
Upstream PR to mirror: #451 (GLM 5.3 Flash) — same shape, same PR body style.

## Current state (as of loop start)
All code splices are APPLIED and `cargo test -p ocean-providers` was 51/51
green. Four touchpoints, all mirrored on the fable-5 arms:

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
1. DONE — `cargo test -p ocean-protocol`: 163 + 5 doc green.
2. DONE — `cargo test -p ocean-agent`: 239/239 green.
3. DONE — clippy clean across all three crates.
4. DONE — fmt clean (menu line wrapped per rustfmt).
5. DONE — committed 06e9912f, pushed `feat/fable-5.1`, **PR #452 open**:
   https://github.com/Risingtides-dev/ocean-os/pull/452
6. DONE — this update.
7. OPEN — watch PR #452 CI. If CI is red for a reason in this branch, fix in
   this worktree and push. When #452 MERGES: `git worktree remove` this
   directory (from ~/dev/ocean-os), then `launchctl bootout gui/$(id -u)/
   com.ocean.fable-5-1-loop && rm ~/Library/LaunchAgents/com.ocean.fable-5-1-loop.plist`
   — the loop's exit condition.

## Rules
- No force-push, no touching anything outside this worktree + the PR.
- If a gate is red, fix it in this worktree; do not weaken the gate.
- Each wake: read this file FIRST, do the next undone step, update this file.
