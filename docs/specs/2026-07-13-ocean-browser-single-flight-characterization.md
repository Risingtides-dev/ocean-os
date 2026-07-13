# Ocean Browser Single-Flight Characterization

**Date:** 2026-07-13
**Plan checkpoint:** Phase 0B-3 / Phase 1B-3
**Baseline:** `762ee25b`
**Status:** Complete — macOS/Linux gates and independent concurrency/process review passed

## Question

Can concurrent browser-tool callers reuse or start exactly one browser, while healthy/dead/stalled/cancelled startup paths finish under explicit deadlines and leave retryable state without an orphan launch process?

## Baseline ownership and behavior

`ocean-runtime::tools::browser::LazyBrowser` owned one `Mutex<Option<Arc<BrowserHandle>>>` and intentionally held the mutex across both cached-handle liveness probing and `BrowserHandle::launch`. This already provided correct single-flight serialization: only the lock owner could launch and it populated the cache only after launch completed.

The baseline had no LazyBrowser-level deadlines for:

- waiting behind the current single-flight owner;
- `BrowserHandle::is_alive()` (which locks the Chrome handle and performs a CDP request);
- the full attach-or-launch operation.

Chromiumoxide has an internal 20-second fresh-launch websocket deadline and a kill-on-drop child, but that does not bound LazyBrowser lock wait, a blocked liveness probe, or every attach/connect path. A stalled phase could therefore consume the much larger turn deadline. This is a RED boundedness finding, not evidence that duplicate launch already occurred.

Cancellation itself was structurally retryable: dropping `get()` released Tokio's mutex guard, and cache assignment occurred only after a complete launch. The missing seams prevented deterministic coverage of that behavior.

## Test seams and finite matrix

The single-flight state machine is now a private generic helper, `get_or_launch_with`, parameterized by:

- the cached-handle slot;
- per-phase deadlines;
- an async liveness operation;
- an async launcher operation.

Production still supplies real `BrowserHandle` operations. Tests supply deterministic fake handles and launch leases without changing the public provider/tool surface.

Eight runtime tests cover:

1. healthy cached handle: reused, zero launches;
2. dead cached handle: exactly one replacement launch and cache update;
3. eight concurrent callers: all receive the same handle, exactly one launch;
4. near-deadline waiters consume a flight completed after their request without serial liveness re-probes;
5. stalled single-flight owner: waiter returns at its deadline without launching;
6. stalled liveness: returns at its deadline, preserves the cached handle, and retries successfully;
7. stalled launch: drops in-flight launch work, caches nothing partial, and retries successfully;
8. cancelled launch: drops the launch future and mutex guard, leaves no active launch lease/partial cache, and retries successfully.

Synthetic liveness/launch phases are bounded at 30–500 ms; default test lock waits and spawned cancellation joins have one-second ceilings.

An additional Unix `ocean-browser` regression launches a test-owned executable through the real chromiumoxide path, waits for its PID marker, cancels before any DevTools endpoint appears, and verifies the spawned process dies within a finite two-second polling budget. Abort-on-drop and PID cleanup guards cover assertion/marker failures.

## Smallest fix

The existing mutex single-flight pattern remains because characterization confirmed its exactly-one-launch behavior. Only boundedness and testability changed:

- single-flight lock wait: **40 seconds**;
- cached-handle liveness: **3 seconds**;
- attach/fresh launch: **30 seconds**.

A liveness timeout returns an explicit error and retains the cached handle rather than treating a merely busy browser as dead. A definitive `false` still launches a replacement. The slot records successful validation/launch completion time, allowing callers that were already waiting for that flight to consume its handle without serialized re-probes. Launch timeout/cancellation drops the launcher future and never writes a partial cache entry.

The real fresh-launch path is cancellation-safe because chromiumoxide sets `tokio::process::Command::kill_on_drop(true)` before spawn; the PID regression exercises that exact path.

## Focused result

On macOS 26.3.1 arm64:

- `cargo test -p ocean-runtime lazy_browser_ -- --test-threads=1`: **8 passed**;
- `cargo test -p ocean-browser cancelled_browser_launch_does_not_orphan_spawned_process -- --test-threads=1`: **1 passed**;
- strict all-target Clippy passes for both `ocean-runtime` and `ocean-browser`.

Repository validation also passed:

- `cargo xtask ci` (workspace build/tests, strict all-target Clippy, format, `cargo-deny`, and docs integrity);
- `cargo check --workspace --tests`;
- daemon checks with `--features livekit-tap` and `--features deepgram-stt`;
- complete `ocean-browser` and `ocean-runtime` test suites;
- independent concurrency and process-safety review after fixes for completed-flight re-probing and PID cleanup identity.

GitHub Actions run [29226786986](https://github.com/Risingtides-dev/ocean-os/actions/runs/29226786986) passed the full repository gate on both `ubuntu-latest` and `macos-latest`, plus `cargo-deny`, completing the supported-platform checkpoint.

The fix was deployed from clean synchronized `main` at `6459b7907c60` after confirming zero turns in flight. The supervised daemon restarted under neutral cwd `/Users/risingtidesdev`; health returned the deployed revision with zero persistence or GC failures.
