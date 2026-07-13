# Ocean Agent-Loop History-Cost Benchmark

**Date:** 2026-07-13
**Plan checkpoint:** Phase 0B-4
**Harness revision:** pending commit
**Status:** Harness smoke-tested; clean-revision baseline pending

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

Pending a clean-revision run and review.
