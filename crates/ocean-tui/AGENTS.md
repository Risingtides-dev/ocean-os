# ocean-tui — Terminal Steering Cockpit

## Purpose

This crate owns the full-screen terminal steering cockpit (`ocean` binary) for interacting with the Ocean daemon.

## Ownership

- **Scope:** `crates/ocean-tui/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** TUI layout, terminal interaction, daemon client UX, release `ocean` binary behavior

## Local Contracts

- After any TUI change, build the release binary: `cargo build -p ocean-tui --release`.
- Installing a fresh build over `~/.cargo/bin/ocean-tui` MUST use remove+copy
  (`rm dest && cp src dest`) or a temp-file + atomic rename — NEVER an
  in-place overwrite of the existing Mach-O: on this host the next `ocean`
  exec dies with an instant SIGKILL (observed 2026-07-11; stale kernel
  code-signature cache is the leading explanation). After installing, verify
  with `codesign --verify --deep --strict` AND a real PTY launch that stays
  alive several seconds — `--help` exits before terminal setup and proves
  nothing.
- Keep TUI behavior aligned with daemon API contracts; clients do not own sessions.
- The `shell/` workbench is the sole TUI. Do not reintroduce `--legacy`, nested TUI session resume, Track-0 room tabs, the mesh parity subcommand, or room-scoped `AgentTurnRequest` fields.
- Do not introduce agent/session logic into the TUI; session state lives in the daemon via `ocean-agent`.

## Hard Rules (violations have broken the build before — 2026-07-08)

1. **Shared event enums are additive.** NEVER remove, rename, or replace a
   shared wire variant (`AgentEvent`, `AgentTurnEvent`) without an explicit
   protocol migration. Local `Action` variants may be deleted only as part of
   an owner-approved feature removal after workspace-wide reference search and
   tests; ordinary feature work remains additive. A variant you don't recognize
   has call sites you haven't read — `rg <Variant>` across the workspace first.
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
- Chrome contract (2026-07-11, owner-directed): the TITLE row is identity only
  — `workspace › surface`, no branding, no controls. The BOTTOM row is the
  control + info bar: six nav buttons on the LEFT (mouse-first, closest to the
  prompt — `≡` sessions, `◒` chat (the owner's ocean mark, U+25D2), `✎` editor,
  `⟠` graph, `⊟` terminal,
  `◨` files; toggle semantics via `App::press`, hit rects filled by
  `draw_status` using DISPLAY width, never `chars().count()`), then the status
  segments from `shell/status.rs`: model · branch · health · error · activity
  · tok/s. Layout order and survival are SEPARATE: on overflow, segments drop
  by rank (tok/s, then activity, then branch; health/error outlive extras; the
  model never drops). Do not resurrect key legends, counters, or branding.
- Mouse text selection is pane-scoped (2026-07-11, owner-directed): Down arms
  only inside a content pane (sessions/tree/center/terminal — never title,
  status, breadcrumb, or splitters); the drag head clamps into that pane; the
  reverse-video highlight and the copied text share one bounded-span geometry
  (`bounded_span`, app.rs) so highlight == copy and a selection never crosses
  into a sibling lane.
- Metrics are truthful or absent: tok/s renders only as the daemon reported it
  for the LAST finished turn (cleared on `TurnStarted`); the model row falls
  back to the startup `/v1/models` fetch before the first turn; there is NO
  context-window display until the daemon exposes real occupancy
  (`ModelSelection.context_window` + usage provenance are the pending daemon
  follow-ups — never estimate from cumulative turn usage).
- Tool drawers (`shell/components/chat.rs`): one per call, independently
  expandable (`▸`/`▾` via `g()`), collapsed header never wraps; consecutive
  VISIBLE tool turns render single-spaced — a suppressed Thinking turn between
  tool calls must not reintroduce the blank row.
- Chat composer (`shell/components/chat.rs`): `cursor` is a UTF-8 byte offset
  (`None` = end). Every input replacement/insert/delete/kill/completion path
  must preserve that invariant. Readline keys are cursor-relative; `Ctrl+Y`
  remains permission-first, `Ctrl+L` clears only an idle transcript, and
  `Up`/`Down` remain history/picker navigation. Composer sizing, caret paint,
  and scroll use Unicode cell width and follow the cursor row, never the final
  input row.
- Prefer clear status/error presentation over hidden failures (see
  `ModelRerouted` — resilience must never silently lie to the operator).
- Keep the launch cwd as the active surface root; auto-resume must not overwrite it with a stored session root.
- Coordinate API/event changes with `ocean-daemon` and `ocean-core`.
- The model registry lives in `ocean-providers` (`known_models` + resolver
  arms + `Model` constructors in `ocean-protocol` + the claude-code mapping in
  `ocean-agent`). Adding a model touches all four, and the
  `known_models_are_all_routable` / `id_equals_resolved_model` tests enforce
  the invariants — run `cargo test -p ocean-providers` after registry edits.
- Daemon lifeline (`shell/daemon_boot.rs`): the shell runs a health monitor
  (3s probes offline, 15s healthy) and may auto-start `ocean-daemon` — via
  `launchctl kickstart` (no `-k`) when the LaunchAgent supervises it, direct
  spawn (cwd=$HOME, reaped child) only when unsupervised. Eligibility gates
  run BEFORE any process probe: default `127.0.0.1:4780` only,
  `OCEAN_TUI_AUTOSTART=0` disables, `OCEAN_DAEMON_BIN` overrides discovery.
  Blocking work stays in `spawn_blocking`; never on an async worker.
- Error copy (`shell/errfmt.rs`): daemon/provider error text reaching the user
  goes through `errfmt::humanize` (no raw reqwest blobs); credential-shaped
  errors carry `/login` recovery hints; `is_connect_shaped` picks the
  "couldn't reach the daemon" vs "turn could not start" transcript prefix.
  A request timeout or generic post-connect transport failure is outcome-unknown,
  never proof that the daemon was unavailable.
- Turn submission uses its dedicated no-whole-request-timeout HTTP client;
  provider rounds own their runtime timeout. Retry/restore only definitely-unsent
  connect failures or pre-execution 4xx rejection. HTTP 408 is a known executed
  runtime failure in Ocean and must decode its normal response. On
  `TurnOutcomeUnknown`, unwind
  `busy` but never restore or replay the prompt because tools may have run.
- Turn lifecycle: only `TurnFinished`/`TurnSendFailed`/`TurnOutcomeUnknown` (or
  explicit new/clear/history reset) may clear `busy` — never generic SSE
  reconnect statuses; failed turns render `Turn::ErrorNotice`, not advisor
  cards.

## Verification

- `cargo build -p ocean-tui --release`
- `cargo test -p ocean-tui`
- `cargo check --workspace` (`--tests` too when shared enums changed)

## Child devlog Index

No child boundaries defined within `ocean-tui/` at this time.
