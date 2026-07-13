# Ocean Build Compatibility Characterization

**Date:** 2026-07-13
**Plan checkpoint:** Phase 1B-4
**Baseline revision:** `d1e06109`
**Status:** Truthful MSRV and compatibility lanes implemented/reviewed; local gates passed, hosted timings pending

## Question

Can Ocean continuously compile its supported feature combinations and release profile, and is the declared Rust 1.80 minimum actually compatible with the resolved workspace?

## Supported matrix

The production daemon contract has three supported configurations:

| Configuration | Command coverage | Meaning |
|---|---|---|
| Default | repository lane + MSRV workspace all-target check | ordinary daemon/runtime build |
| `livekit-tap` | stable Clippy + MSRV check | LiveKit room audio plus XAI speech features |
| `deepgram-stt` | stable Clippy + MSRV check | implies `livekit-tap`, adds Deepgram websocket STT |

`ocean-call` also exposes lower-level `xai-stt`, `xai-tts`, `livekit-tap`, and `deepgram-stt` features. The daemon's supported feature paths compose the production combinations; arbitrary power-set combinations are not advertised as separate support contracts. `ocean-plugin`'s default feature already enables its `runtime` path in the workspace gate.

## Rust 1.80 result

The declared Rust 1.80 workspace minimum was not truthful for the current lockfile.

```bash
cargo +1.80 check --workspace --all-targets
```

Cargo 1.80.1 exited 101 before compiling Ocean because `agent-client-protocol-derive 0.13.1` uses Edition 2024, which Cargo 1.80 cannot parse. The dependency is required by `agent-client-protocol`, then `ocean-acp`. A narrower `cargo +1.80 check -p ocean-hashline --all-targets` also failed because `twox-hash 2.1.2` requires Rust 1.81, so the crate's explicit 1.80 declaration was not a viable exception.

The resolved graph contains multiple default-path dependencies declaring Rust 1.88, including `agent-client-protocol-schema`, `darling`, `jsonwebtoken`, `plist`, `serde_with`, and `time`. Pinning or downgrading that set would create broader dependency/API/schema risk than correcting the declared floor.

Raw 1.80 workspace failure: `docs/specs/2026-07-13-ocean-msrv-1.80-failure.raw.txt` (SHA-256 `104ac342e0b64a1e24bf18a017b0dd3cea484d83794fde33ea362afe7d159775`).

## Smallest compatibility fix

- Raise `[workspace.package].rust-version` from 1.80 to **1.88** and make `ocean-hashline` inherit it.
- Replace one compiler-version-sensitive `str == PathBuf` comparison in session workspace binding with the explicit, behavior-equivalent `Path::new(r) == new_root.as_path()` form.
- Apply Clippy's MSRV-enabled `Option::is_none_or` equivalents at four existing `map_or(true, ...)` sites.
- Collapse one feature-only nested LiveKit track match without changing event behavior.
- Keep serialized schemas, public paths, routes, runtime behavior, feature definitions, and lockfile unchanged.

Rust 1.88 is now the build-compatibility contract, not a new runtime requirement: the current dependency graph already made older builds impossible.

## Executable lanes

The dependency-free xtask manifest now owns three lanes:

```bash
# Existing repository lane
cargo xtask ci

# Stable Rust on macOS and Ubuntu
cargo xtask ci --compatibility
# - strict Clippy: ocean-daemon --features livekit-tap
# - strict Clippy: ocean-daemon --features deepgram-stt
# - cargo check --workspace --all-targets --release

# Pinned Rust 1.88 on Ubuntu (local equivalent shown)
cargo +1.88.0 xtask ci --msrv
# - cargo check --workspace --all-targets
# - both supported daemon feature checks
```

`cargo xtask ci --dry-run` reports every command and CI-only lane. Exact-command tests pin each static manifest, and a workflow-consumption test requires GitHub Actions to invoke all three lanes and pinned Rust 1.88.

The stable compatibility commands run after the existing repository gate in the same macOS/Ubuntu jobs to reuse their caches. MSRV runs separately on Ubuntu. In an isolated worktree with a fresh target directory, stable compatibility completed in 4m15s and the Rust 1.88 default/feature lane in 4m19s. The first hosted attempt proved that Ubuntu's feature build additionally needs `libglib2.0-dev`; both Ubuntu jobs now install that explicit native prerequisite. The existing 40-minute job ceilings are retained pending the corrected hosted timings.

## Local verification

- `cargo test -p xtask`: 22 passed.
- `cargo test -p ocean-agent bind_workspace`: 3 passed.
- `cargo xtask ci --compatibility`: passed.
- `cargo +1.88.0 xtask ci --msrv`: passed for workspace all-targets plus both supported daemon features.
- The stable feature checks deny Clippy warnings. The release and MSRV lanes are compile checks, matching the plan's compatibility scope.
- `cargo xtask ci`, `cargo check --workspace --tests`, formatting, docs integrity, and diff checks passed in the isolated worktree.
- Independent implementation review passed with no blocker after verifying all 25 package declarations, behavior-equivalent source rewrites, lane/workflow parity, raw evidence, and timing scope.

Hosted macOS/Ubuntu compatibility and Ubuntu MSRV timings remain the completion gate.
