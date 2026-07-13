# Ocean Agent-Loop History-Cost Benchmark

**Date:** 2026-07-13
**Plan checkpoint:** Phase 0B-4
**Harness revision:** `7ad3bd8d9c6e9d04cb4e0b18e723acd4bcaa3514`
**Status:** Complete — clean baseline, independent review, and macOS/Linux gates passed

## Scope

This benchmark isolates the repeated history-preparation kernel executed before every provider round:

- `agent_loop::trim_to_context_window`;
- JSON-shaped token estimation for each history message;
- tool-call/result validity filtering;
- cloning the provider-bound history;
- appending the assistant tool-call and tool result between simulated rounds.

It deliberately excludes provider/network latency, tool execution, Tokio scheduling, event delivery, and session persistence. This makes history-size/round-count scaling reproducible rather than measuring a mock provider or runtime noise. It is a kernel baseline, not an end-to-end turn latency claim.

## Matrix and fixture

- Starting histories: **10 / 100 / 1,000 messages**.
- Provider rounds: **1 / 5 / 20**.
- Context window: **128,000 tokens**.
- Reserved output: **8,192 tokens**.
- System prompt: **4,096 bytes**.
- History alternates representative user/assistant text messages; intermediate rounds append a valid no-op tool-call/result pair.
- All 1,000-message cells remain within the configured input budget, so the benchmark exercises full-history traversal rather than measuring an early trim cutoff.

## Measurement policy

- Build/profile: Cargo `--release`; the executable rejects debug runs.
- Warm-up: **5 unrecorded iterations per cell**.
- Samples: **30 recorded iterations per cell**.
- Timing: monotonic wall clock around the kernel only; base fixture construction and initial history clone are excluded.
- Allocation metrics: a process-global counting wrapper over `System`; the executable is single-threaded. Counts include allocations/reallocations and requested bytes during the kernel, not retained/live bytes. Deallocation is not counted.
- Output: raw sorted sample arrays plus median, p95, min, max, median per round, median allocations, and median allocated bytes.
- Optimizer resistance: history input, outbound length, and aggregate checksum pass through `std::hint::black_box`.

## Reproduction

From a clean repository root at the recorded revision:

```bash
cargo run --release -p ocean-runtime --example history_cost_bench -- \
  --warmup 5 \
  --samples 30 \
  --output docs/specs/2026-07-13-ocean-agent-loop-history-cost-baseline.json
```

The JSON captures the Git revision/status, Rust/Cargo toolchain, OS/kernel, CPU, memory, policy, and all nine result cells.

## Regression interpretation

Flag a cell for investigation when its median wall time or median allocated bytes increases by **20% or more** against the same machine/toolchain/profile. For timing, also require an absolute increase of at least **10 microseconds** so noise in the smallest cells does not masquerade as a regression. Compare p95 and raw samples before attributing cause; a threshold crossing is a review trigger, not an automatic product failure.

## Baseline

Artifact: `docs/specs/2026-07-13-ocean-agent-loop-history-cost-baseline.json`

Clean-tree command wall time (including a 1.68s incremental release rebuild): **2.54s real / 0.53s user / 0.18s sys**.

| Messages | Rounds | Median | p95 | Median allocs | Median allocated bytes |
|---:|---:|---:|---:|---:|---:|
| 10 | 1 | 4.875 µs | 4.917 µs | 69 | 26,579 |
| 10 | 5 | 32.875 µs | 33.292 µs | 569 | 165,795 |
| 10 | 20 | 323.208 µs | 455.417 µs | 4,893 | 1,063,938 |
| 100 | 1 | 47.541 µs | 73.750 µs | 654 | 154,019 |
| 100 | 5 | 242.833 µs | 388.458 µs | 3,494 | 836,115 |
| 100 | 20 | 1,152.167 µs | 1,390.417 µs | 16,591 | 3,623,778 |
| 1,000 | 1 | 492.500 µs | 2,450.833 µs | 6,504 | 1,428,419 |
| 1,000 | 5 | 2,488.709 µs | 4,256.625 µs | 32,744 | 7,539,315 |
| 1,000 | 20 | 9,316.042 µs | 14,499.542 µs | 133,591 | 29,442,978 |

### Interpretation

- Single-round median cost scales near-linearly with starting history size. Across repeated rounds the transcript grows by a tool-call/result pair, so total work follows roughly `rounds × starting_history + rounds²`; small starting histories therefore rise faster than linearly in round count.
- On this M4 fixture, the largest median is ~9.32 ms and is small relative to typical remote-provider latency; this checkpoint does not justify a runtime redesign.
- Allocation traffic is the clearer future optimization signal: the largest cell requests ~29.4 MB across ~133.6k allocations. These are cumulative allocation bytes, not retained memory.
- p95 outliers in the 1,000-message cells are visible in the raw arrays and should be compared on the same host/toolchain before drawing regression conclusions.

No performance fix is proposed from this baseline. Use it to evaluate later history, serialization, or transcript-structure changes.

## Verification

- Machine-readable artifact validation passed: clean revision/status, release profile, 9-cell matrix, 30 raw samples per metric/cell, sorted timing arrays, and both regression thresholds.
- `cargo xtask ci` passed locally, including the repository tests and strict Clippy gate.
- `cargo check --workspace --tests`, `cargo fmt --all -- --check`, `cargo xtask docs-check`, and `git diff --check` passed.
- Independent methodology review passed after the absolute timing floor was added to policy metadata and the interpretation was bounded to the measured fixture.
- GitHub Actions run [29228061344](https://github.com/Risingtides-dev/ocean-os/actions/runs/29228061344) passed the full repository gate on `macos-latest` and `ubuntu-latest`, plus `cargo-deny`.
