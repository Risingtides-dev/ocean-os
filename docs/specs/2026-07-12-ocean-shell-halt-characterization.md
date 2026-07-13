# Ocean Shell Halt Characterization

**Date:** 2026-07-12
**Plan checkpoint:** Phase 0B-2 / Phase 1B-2
**Baseline:** `8c190e86`
**Status:** Complete — macOS/Linux characterization, full gates, and independent security review passed

## Question

When a user Halts a turn while `BashTool` is running, does cancellation terminate both the direct shell child and descendants, without waiting for the tool timeout or leaking processes?

## Existing behavior

- The agent loop races tool execution against its `CancellationToken` and drops the tool future promptly on Halt.
- `BashTool` uses `tokio::process::Command::kill_on_drop(true)`, which terminates the direct `bash` child when that future is dropped or its timeout elapses.
- The existing timeout regression proved a direct child did not later write a marker, but did not observe PIDs, Halt cancellation, or descendants.

## Finite PID characterization

Two Unix-only tests use commands bounded to 30 seconds, PID marker files, a two-second disappearance polling budget, and layered drop cleanup. An abort-on-drop task guard is installed immediately after spawn, before marker parsing can fail; after PIDs are known, another guard sends `SIGKILL` to the test-owned group/PIDs even when later assertions fail:

1. `bash_halt_kills_direct_child_by_pid`
   - starts `bash`, records its PID, then `exec sleep 30`;
   - aborts the in-flight `BashTool::execute` future (the same drop boundary used by agent-loop Halt);
   - asserts the direct PID disappears promptly.
2. `bash_halt_kills_descendant_process_tree_by_pid`
   - records the parent shell PID, starts a signal-resistant background descendant, and records that PID;
   - aborts the tool future;
   - separately asserts both PIDs disappear.

Process checks use signal 0 plus `ps` state and treat zombies as terminated. Cleanup is independent of the assertion path.

## Baseline result

**RED.** On macOS 26.3.1 arm64:

- direct child: PASS;
- descendant tree: FAIL — the background PID survived after the shell died.

This proves `kill_on_drop(true)` is necessary but insufficient for command trees.

## Smallest fix

On Unix, `BashTool` now:

- requests a fresh child-owned process group with `Command::process_group(0)` before spawn;
- creates an RAII guard from the returned child PID/PGID;
- sends `SIGKILL` to the negative PGID when the execute future is dropped or times out;
- retains `kill_on_drop(true)` as direct-child defense;
- drains inherited stdout/stderr pipes before reaping the group leader, keeping its PID unavailable for reuse if an escaped descendant holds a pipe open;
- disarms the group guard immediately after `child.wait()` succeeds, before another await, so a later PID/PGID reuse cannot be signalled.

Non-Unix behavior is unchanged: direct-child `kill_on_drop` remains, while descendant-tree termination is explicitly unsupported by this Unix process-group implementation.

Commands that deliberately escape into a new session/process group are outside this guarantee; ordinary shell descendants remain in the child-owned group.

## Post-fix result

On macOS:

- `cargo test -p ocean-runtime bash_halt_ -- --test-threads=1`: **2 passed**;
- `cargo test -p ocean-runtime bash_timeout_kills_the_child_no_orphan -- --test-threads=1`: **1 passed**;
- both PID assertions complete in well under the two-second disappearance budget;
- no process cleanup was left to manual intervention.

Repository validation also passed:

- `cargo xtask ci` (workspace build/tests, strict all-target Clippy, format, `cargo-deny`, and docs integrity);
- `cargo check --workspace --tests`;
- daemon checks with `--features livekit-tap` and `--features deepgram-stt`;
- independent process-safety and test-portability review after fixes for PGID-reuse and pre-marker cleanup hazards.

Linux uses the same Unix `setpgid`/negative-PGID signal contract and the same tests. GitHub Actions run [29225077002](https://github.com/Risingtides-dev/ocean-os/actions/runs/29225077002) passed the full repository gate on both `ubuntu-latest` and `macos-latest`, completing the supported-platform checkpoint.
