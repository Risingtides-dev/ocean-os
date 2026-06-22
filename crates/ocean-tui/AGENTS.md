# ocean-tui — Terminal Steering Cockpit

## Purpose

This crate owns the full-screen terminal steering cockpit (`ocean` binary) for interacting with the Ocean daemon.

## Ownership

- **Scope:** `crates/ocean-tui/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** TUI layout, terminal interaction, daemon client UX, release `ocean` binary behavior

## Local Contracts

- After any TUI change, build the release binary: `cargo build -p ocean-tui --release`.
- Keep TUI behavior aligned with daemon API contracts; clients do not own sessions.
- Do not introduce agent/session logic into the TUI; session state lives in the daemon via `ocean-agent`.

## Work Guidance

- Preserve keyboard-driven workflows and terminal responsiveness.
- Prefer clear status/error presentation over hidden failures.
- Keep the launch cwd as the active surface root; auto-resume must not overwrite it with a stored session root.
- Coordinate API/event changes with `ocean-daemon` and `ocean-core`.

## Verification

- `cargo build -p ocean-tui --release`
- `cargo test -p ocean-tui`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-tui/` at this time.
