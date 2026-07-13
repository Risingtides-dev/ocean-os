# Contributing to Ocean OS

Ocean OS is a Rust workspace with daemon-owned runtime authority. Start with the contracts, choose the correct owner, keep changes narrow, and run the owning validation before the workspace gate.

## Before editing

1. Read root [`AGENTS.md`](../AGENTS.md).
2. Route the request through [`docs/OCEAN_PROJECT_MAP.md`](../docs/OCEAN_PROJECT_MAP.md) when it may belong to another Ocean repo.
3. Find the package owner, entry point, exclusion boundary, and narrow test in [`crates/AGENTS.md`](../crates/AGENTS.md).
4. Read the target package's local `AGENTS.md` when one is linked.
5. Check `git status`; preserve unrelated work and stage only files you own.

## Architecture rules

- `ocean-daemon` owns HTTP/SSE execution authority.
- `ocean-agent` owns local session/history behavior.
- `ocean-runtime` owns the permission-gated agent/tool loop.
- `ocean-protocol` owns provider wire encoding; `ocean-providers` owns model/auth routing.
- Clients such as TUI, CLI, ACP, and ocean-surface steer the daemon; they do not invent session or permission authority.
- Do not bypass caller cwd resolution or runtime permission gates.
- Shared enum and serialized protocol changes are additive unless a migration is explicitly approved.

See the cross-crate change-impact matrix in `crates/AGENTS.md` before changing events, sessions, tools, models, routes, or persisted schemas.

## Change shape

- Prefer one bounded concern per change.
- Behavior-neutral extraction must have an extraction manifest and must not bundle redesign, renames, or opportunistic fixes.
- Add focused regression tests for behavior changes.
- Keep public errors and operator-visible fallback behavior honest.
- Update owning docs and append `events.md` after meaningful work.

## Validation

### Fast loop

Run the narrow command from `crates/AGENTS.md` or the target package contract.

### Workspace completion

```bash
cargo check --workspace
```

Use `cargo check --workspace --tests` when shared enums/events affect test-only matches.

### Merge / PR gate

```bash
cargo xtask ci
```

This canonical manifest runs docs/index integrity, workspace build/test, all-target Clippy with denied warnings, format, and `cargo deny check`. Use `cargo xtask ci --dry-run` to print it plus the CI-only lanes. CI also runs `cargo xtask ci --compatibility` on macOS and Ubuntu for supported daemon features and release-profile compilation, and `cargo xtask ci --msrv` under pinned Rust 1.88 on Ubuntu. `cargo-deny` remains a separate Ubuntu job. Run a relevant compatibility lane locally when changing feature-gated, release-sensitive, dependency, or MSRV code.

## Review

Feature, logic, protocol, security, and architecture changes require a fresh independent reviewer. Report changed files, commands and outcomes, remaining risks, and any verification that could not run.
