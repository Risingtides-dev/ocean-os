# ocean-minimizer — Standalone Output Minimization

## Purpose

Own the dependency-free M1 library that conservatively minimizes selected
human command output from already-tokenized invocations.

## Ownership

- **Scope:** `crates/ocean-minimizer/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** shell parsing/execution, daemon/runtime/agent/TUI wiring,
  harness policy, capture, configuration, artifacts, or persistence
- **Consumer:** `ocean-runtime` output-economy wrapper (minimizer M2), reached
  only behind the default-off `SessionContext::command_output_minimization`
  gate that no production path sets yet; the dependency is one-way

## Local Contracts

- Preserve unknown, machine-readable, NUL-delimited, oversized, and ambiguous
  captures byte-for-byte.
- Keep the public API already-tokenized and deterministic.
- `original_text` exists only for changed output; accounting is exact.
- Keep the crate dependency-free and compatible with Rust 1.88.
- Fixed filters and the final line cap are compile-time behavior; do not add
  regex, TOML, user configuration, artifact references, or output footers.
- Preserve pinned OMP/RTK attribution and fixture provenance.

## Work Guidance

Prefer passthrough when a shape cannot be proven safe. Runtime integration
lives in `ocean-runtime` (M2, default-off); profile enablement (M2c) is a
separate reviewed checkpoint and must not be smuggled into filter work.

## Verification

- `cargo test -p ocean-minimizer`
- `cargo clippy -p ocean-minimizer --all-targets -- -D warnings`
- `cargo +1.88.0 test -p ocean-minimizer` when the pinned toolchain exists

## Child devlog Index

No child boundaries defined.
