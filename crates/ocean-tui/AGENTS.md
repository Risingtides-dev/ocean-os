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
- `/web` and `/desk` hand the bound session to sibling surfaces owned by the
  `ocean-surface` repo: the web PWA consumes `?session=<id>` at boot (proxy
  default `http://127.0.0.1:8790`, override via `OCEAN_SURFACE_URL`) and the
  Tauri desktop app consumes the `ocean://session/<id>` deep link. Both URL
  shapes are a cross-repo contract — change them only with ocean-surface.
- `/beam` is the cross-device half of that contract: it copies the same
  `?session=<id>` URL and renders it as an inverted Unicode half-block QR in
  the transcript (dark modules on a light field for dark terminals). Point
  `OCEAN_SURFACE_URL` at the public surface (e.g.
  `https://ocean.agentsworld.org`) so beams land on reachable URLs.
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

- Sessions, Files, the session-component tray, and Terminal use the shared
  untitled panel frame: keep the slate bed, hairline, inset, footer, focus,
  selection, and content geometry, but do not render redundant pane-name labels.
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
- Metrics are truthful or absent: tok/s and context occupancy render only from
  daemon-reported values for the LAST finished turn (both clear on
  `TurnStarted`; context also clears on stream gaps or adoption after a missing
  start). Context occupancy uses the provider-reported final request,
  never cumulative multi-round usage; unknown values remain absent. The model
  row falls back to the startup `/v1/models` fetch before the first turn.
- The lower Files rail is a separate session-component tray, never part of
  `FileTreeComponent`. Its context meter uses a btop-inspired terminal grammar:
  a quiet empty bed, per-cell truecolor deep-aqua→cyan→amber→coral ramp, and a
  `░`/`▒`/`▓` frontier for sub-cell dithering (`.`/`:`/`=`/`#` fallback through
  `g()`); numeric warning thresholds remain truthful and separate from texture.
  Its todo adapter applies only successful correlated tool
  finishes and keeps confirmed items pinned while the same session remains
  bound. The runtime's in-memory todo tool uses that same session scope, so the
  display never outlives executable todo state. Explicit todo clear, session
  switch/new session, daemon restart, or SSE-gap invalidation removes the pin.
  Auto-reveal the Files rail only when this tray transitions hidden→visible;
  once mounted, unrelated actions and Terminal render ticks must respect an
  operator's explicit Files close.
  It never parses human-formatted todo output into invented durable state. Todo
  `text` remains authoritative; an optional agent-supplied `title` (at most 36
  terminal cells) is display-only and is preferred in the compact tray. Empty
  or short layouts return the full rail to the file tree; tray selection and
  mouse routing remain pane-bounded.
- Consecutive tool bursts (`shell/components/chat.rs`) collapse under one
  non-wrapping parent summary with truthful running/done/failed counts. Hidden
  Thinking turns do not split a burst; visible prose/cards/errors do. Opening a
  parent reveals the existing independently expandable per-call drawers;
  keyboard focus reveals a nested drawer's parent, and `Ctrl+O` remains the
  global open-all override. Mouse routing uses the exact wrapped + scrolled
  geometry and preserves drag-to-select precedence.
- Assistant Markdown links to existing workspace-local `.md`, `.markdown`, and
  `.mdx` files open in the native editor on a clean mouse click. Resolve and
  canonicalize against the active workspace, reject symlink/`..` escapes and
  non-doc/external/missing targets, derive hit cells from Ratatui's exact wrapped
  + scrolled render, and preserve drag-to-select precedence.
- Chat composer (`shell/components/chat.rs`): `cursor` is a UTF-8 byte offset
  (`None` = end). Every input replacement/insert/delete/kill/completion path
  must preserve that invariant. Readline keys are cursor-relative; `Ctrl+Y`
  remains permission-first, `Ctrl+L` clears only an idle transcript, and
  `Up`/`Down` remain history/picker navigation. Composer sizing, caret paint,
  and scroll use Unicode cell width and follow the cursor row, never the final
  input row.
- Composer dictation is local capture over daemon-owned STT. `Option+Space`
  (Crossterm `ALT+Space`, with macOS non-breaking-space fallback) toggles
  recording on and off while chat is focused; ordinary Space remains ordinary
  typing in every terminal. Esc cancels an active capture or transcription.
  The Kitty keyboard disambiguation protocol remains enabled where supported so
  modifier chords arrive distinctly.
  macOS capture is a bounded 30s mono WAV path in `shell/dictation.rs`; other
  platforms fail visibly without audio build dependencies. The live RMS history
  replaces the prompt box with a btop-style meter, while the draft/cursor remain
  untouched. `/v1/voice/stt` returns one final transcript, so word-by-word paint
  is a generation-tagged insertion animation, not invented interim STT; Esc,
  quit, late results, and dropped handles cancel safely, and dictation never
  auto-submits.
- Bare `/login` categorizes credentials as Agent models and Voice models.
  Voice rows use masked inline API-key entry and dedicated auth blocks: `xai`
  for daemon-owned STT/TTS and `openai-realtime` for Realtime client-secret
  minting. Saving either must preserve agent OAuth/API-key blocks and must not
  change the selected agent model. Do not advertise Embedding models until a
  live typed embedding capability and consumer exist; shared semantic search is
  currently owned by ocean-bedrock.
- `/permissions` is a daemon-backed three-state picker, not a client-side
  approval bypass: manual prompts for every known tool, automatic prompts only
  for runtime-classified unsafe tools, and skip-all suppresses prompts. Render
  the daemon's effective mode and any `OCEAN_YOLO` override truthfully. After
  the daemon confirms effective skip-all, authorize only the request ids already
  pending at that moment and release their pending/later same-turn prompts
  through the normal token-bound decision POST. Clear that authorization at
  request completion; a stale global display must never approve a new turn.
- Prefer clear status/error presentation over hidden failures (see
  `ModelRerouted` — resilience must never silently lie to the operator).
- Keep the launch cwd as the active surface root; resuming a session must not overwrite it with a stored session root.
- Side-rail widths are operator-resizable but must clamp against the body width, the minimum center workspace, and only the currently visible opposite rail; a hidden rail's stored width consumes no layout budget.
- Sessions and Files share `shell/rail.rs` for row selection, focus/blur styling,
  bounded mouse geometry, scroll clamping, and empty states. Preserve one-cell
  accent bars, Unicode-cell fitting, visible hierarchy guides, selected-only
  row actions, and extension identity at narrow widths; do not fork the visual
  grammar between rails.
- Editor viewport behavior is content-aware: prose extensions soft-wrap vertically, source code scrolls horizontally, mouse-wheel scrolling stays independent until the next keyboard edit/navigation, and rendered text/cursor geometry share terminal sanitization plus Unicode cell widths.
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
- Render-protocol components project into terminal-native chat cards. Component
  IDs are unique across inline and pinned slots when `replace` is set; unmount,
  history load, and new-session reset must not leak pinned state. Treat every
  component prop as agent-controlled terminal text: sanitize controls, measure
  Unicode display cells, and clamp to the card viewport before creating spans.

- Normal launch (2026-07-13, owner-directed) opens a centered OCEAN chooser
  over a clean chat-only workspace: `+ new in <cwd>`, `resume session`, blank
  `editor` (which reveals files), or `open graph`. Startup never auto-resumes;
  explicit `--session` remains direct opt-in. Resume is a nested, current-
  workspace picker with keyboard/mouse parity, selection-relative scrolling,
  terminal-safe labels, and off-thread session discovery through `Action`.
- Herdr lifecycle projection (`shell/herdr.rs`) is a fail-soft client adapter,
  not session/runtime authority. It activates only in a Herdr pane, derives
  `idle`/`working`/`blocked` from already-accepted Elm actions, ignores
  permission traffic for other sessions, and emits no prompt/tool/token
  content. State reports never block the event loop; shutdown release waits at
  most 300 ms before relinquishing its official `herdr:ocean` authority. Bound
  session ids are reported via `pane report-agent-session` so a Herdr server
  restart can resume with `ocean --session <id>` once Herdr accepts `ocean` as
  an official agent source. The distributable launcher is owned by the standalone
  [`Risingtides-dev/ocean-herdr`](https://github.com/Risingtides-dev/ocean-herdr)
  repository.

## Verification

- `cargo build -p ocean-tui --release`
- `cargo test -p ocean-tui`
- `cargo check --workspace` (`--tests` too when shared enums changed)

## Child devlog Index

No child boundaries defined within `ocean-tui/` at this time.
