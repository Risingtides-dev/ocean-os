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

## Hard Rules (violations have broken the build before — 2026-07-08)

1. **Enums are additive, never destructive.** NEVER remove, rename, or replace
   an existing variant of `Action` (`shell/action.rs`) or any shared event enum
   (`AgentEvent`, `AgentTurnEvent`). Add your new variant alongside. A variant
   you don't recognize has call sites you haven't read — `grep -rn <Variant>`
   across the workspace BEFORE touching it. (An agent replaced `SetModel` with
   `SetThinking` and broke four call sites; both now coexist. Keep it that way.)
2. **Compile before you finish.** `cargo check -p ocean-tui` must pass before
   you end your turn. If you touched a shared enum, run
   `cargo check --workspace --tests` — exhaustive matches fan out into
   ocean-daemon, ocean-acp, and the SDK, including test-only matchers.
3. **Concurrent lanes are real.** Multiple agents edit this crate at once.
   Re-read a file immediately before editing it; keep each edit surgical; stage
   only the files you changed. Never `git add -A`.
4. **Rendering is terminal-safe or it smears.** Any text that reaches a ratatui
   `Span` from tool output, file content, or provider errors goes through
   `sanitize_line` (chat.rs) — raw tabs/control chars desync the terminal from
   ratatui's cell math and leave permanent bleed. Long lines clamp
   (`clamp_line`) or wrap explicitly; never assume one logical line = one row.
5. **The Elm loop is the only mutation channel.** Components emit `Action`s;
   `App::dispatch` and component `update`s consume them. No state mutation
   outside that path, no components reaching into `App` internals.

## Work Guidance

- Preserve keyboard-driven workflows and terminal responsiveness.
- Prefer clear status/error presentation over hidden failures (see
  `ModelRerouted` — resilience must never silently lie to the operator).
- Keep the launch cwd as the active surface root; auto-resume must not overwrite it with a stored session root.
- Coordinate API/event changes with `ocean-daemon` and `ocean-core`.
- The model registry lives in `ocean-providers` (`known_models` + resolver
  arms + `Model` constructors in `ocean-protocol` + the claude-code mapping in
  `ocean-agent`). Adding a model touches all four, and the
  `known_models_are_all_routable` / `id_equals_resolved_model` tests enforce
  the invariants — run `cargo test -p ocean-providers` after registry edits.

## Verification

- `cargo build -p ocean-tui --release`
- `cargo test -p ocean-tui`
- `cargo check --workspace` (`--tests` too when shared enums changed)

## Child devlog Index

No child boundaries defined within `ocean-tui/` at this time.
