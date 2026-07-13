# Ocean Strict Production Lint Inventory

**Date:** 2026-07-13
**Plan checkpoint:** Phase 0B-5
**Revision:** `546287bd6ab5ed1aca5e5ce891bdc0f9aec258cd`
**Status:** Complete — clean inventory, independent reproduction, and macOS/Linux gates passed

## Purpose

This is an invariant inventory of panic-adjacent production lint sites, not a defect count and not a request to enable blanket denial. The canonical all-target Clippy gate remains strict with `-D warnings`; these additional lints are intentionally sampled at warning level so each site can be evaluated in its owning subsystem rather than mechanically rewritten.

## Command

Run from a clean repository root with `LC_ALL=C`:

```bash
cargo clippy --workspace --lib --bins --examples --message-format=short -- \
  -W clippy::unwrap_used \
  -W clippy::expect_used \
  -W clippy::panic \
  -W clippy::unreachable \
  -W clippy::await_holding_lock
```

The command exited **0**.

## Scope

Included:

- all workspace packages, including non-default members;
- compiler-selected library, binary, and example targets;
- default feature sets;
- source-located diagnostics emitted by Clippy.

Excluded:

- test and benchmark targets;
- non-default feature combinations;
- diagnostics inside expanded macro internals.

Counting rule: count each source-located line matching `<path>:<line>:<column>: warning: <message>` once; do not count Cargo's per-target summary lines. `expect_err` diagnostics belong to `expect_used`.

## Environment

- `rustc 1.97.0 (2d8144b78 2026-07-07)`
- `cargo 1.97.0 (c980f4866 2026-06-30)`
- Apple M4, 32 GiB RAM
- macOS 26.3.1 arm64 / Darwin 25.3.0

The worktree was clean immediately before the command. Artifacts were created afterward.

## Results

| Lint | Count |
|---|---:|
| `clippy::unwrap_used` | 16 |
| `clippy::expect_used` | 57 |
| `clippy::panic` | 0 |
| `clippy::unreachable` | 6 |
| `clippy::await_holding_lock` | 0 |
| **Total** | **79** |

### Counts by package

| Package | Unwrap | Expect | Panic | Unreachable | Await holding lock | Total |
|---|---:|---:|---:|---:|---:|---:|
| `ocean-runtime` | 0 | 9 | 0 | 4 | 0 | 13 |
| `ocean-acp` | 1 | 9 | 0 | 0 | 0 | 10 |
| `ocean-agent` | 0 | 10 | 0 | 0 | 0 | 10 |
| `ocean-context` | 0 | 8 | 0 | 0 | 0 | 8 |
| `ocean-daemon` | 1 | 5 | 0 | 2 | 0 | 8 |
| `ocean-tui` | 4 | 3 | 0 | 0 | 0 | 7 |
| `ocean-hashline` | 5 | 0 | 0 | 0 | 0 | 5 |
| `ocean-protocol` | 3 | 1 | 0 | 0 | 0 | 4 |
| `ocean-store` | 0 | 4 | 0 | 0 | 0 | 4 |
| `ocean-memory` | 0 | 3 | 0 | 0 | 0 | 3 |
| `ocean-longhouse` | 0 | 2 | 0 | 0 | 0 | 2 |
| `ocean-mcp` | 2 | 0 | 0 | 0 | 0 | 2 |
| `ocean-oauth` | 0 | 2 | 0 | 0 | 0 | 2 |
| `ocean-providers` | 0 | 1 | 0 | 0 | 0 | 1 |

## Comparison with the initial audit

The initial audit at `b5d56416` recorded 17 unwrap, 48 expect, 0 panic, 4 unreachable, and 0 await-holding-lock warnings (69 total). The current revision records 16, 57, 0, 6, and 0 respectively (79 total).

This is directional context only: the repository changed substantially between revisions and the initial raw diagnostic set was not retained. The ten-warning net increase must not be attributed to one change or treated as a regression without a site-by-site revision comparison.

## Interpretation

- Zero `panic` and zero `await_holding_lock` diagnostics remain the strongest signals in this inventory.
- The 79 warnings identify explicit invariant assumptions or failure boundaries. A warning alone does not prove the assumption is unsafe.
- The largest concentrations are in runtime (13), ACP (10), agent/session code (10), context extraction (8), and daemon code (8). Any remediation should be split by owner and preserve existing behavior/error contracts.
- No blanket `unwrap_used` or `expect_used` denial is proposed. Future checkpoints can compare the machine-readable sites and counts on the same target/feature scope.

## Artifacts

- Machine-readable counts and all 79 source locations: `docs/specs/2026-07-13-ocean-strict-lint-inventory.json`
- Exact compiler output: `docs/specs/2026-07-13-ocean-strict-lint-inventory.raw.txt`
- Raw-output SHA-256: `56e2e4a1b7767deb879a5a3b77ad9e4e58b7b5a0d9177dd15a4fa3213e4aa0c4`

## Verification

- An independent rerun exited 0 and reproduced the exact 79-site diagnostic set, counts, package totals, source locations, messages, raw byte length, and SHA-256.
- Machine-readable validation passed for category/package sums, raw hash/length, source-path existence, and source-line bounds.
- `cargo xtask ci`, `cargo check --workspace --tests`, `cargo xtask docs-check`, and `git diff --check` passed locally.
- GitHub Actions run [29228821337](https://github.com/Risingtides-dev/ocean-os/actions/runs/29228821337) passed the full repository gate on `macos-latest` and `ubuntu-latest`, plus `cargo-deny`.
