# ocean-tui — Terminal Steering Cockpit

## Purpose

This crate owns the full-screen terminal steering cockpit (`ocean` binary) for interacting with the Ocean daemon.

## Ownership

- **Scope:** `crates/ocean-tui/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** TUI layout, terminal interaction, daemon client UX, release `ocean` binary behavior

## Local Contracts

- Every TUI behavior change must compile and test on its feature branch; the
  production-install provenance rule is never a reason to defer a build.
  Before finishing, run `cargo check -p ocean-tui`, focused tests, and
  `cargo build -p ocean-tui --release`.
- The operator command is `~/.local/bin/ocean` (not the obsolete
  `~/.cargo/bin/ocean-tui`). After a reviewed change lands on `origin/main`,
  install it with `ops/install-ocean-tui.sh`. The installer publishes an
  immutable revision-named artifact and atomically flips the symlink; never
  overwrite a running Mach-O in place. Verify code signing and perform a real
  multi-second PTY launch—`--help` exits before terminal setup and proves
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
- `/cd <path>` is the runtime half of `--project` / `OCEAN_PROJECT`: it routes
  through the same `set_active_project` re-root the session rail already uses,
  so the turn cwd, file tree, graph, and `@` picker all follow. Relative paths
  resolve against the CURRENT root (not the process cwd — after one `/cd` those
  differ), and the path is canonicalized and proven to be a directory before
  anything moves, so a typo cannot leave the workbench rooted somewhere
  unreadable. A project switch refuses while an editor buffer is modified, so
  rerooting cannot silently discard unsaved work. The session rail deliberately
  stays on the launch project, and
  the PTY is left alone because it may hold a live shell.
- The `shell/` workbench is the sole TUI. Do not reintroduce `--legacy`, nested TUI session resume, Track-0 room tabs, the mesh parity subcommand, or room-scoped `AgentTurnRequest` fields.
- Do not introduce agent/session logic into the TUI; session state lives in the daemon via `ocean-agent`.
- Preserve the Kairav Mittal / `aclfe/inertia` attribution and audited donor revision in `src/shell/spatial.rs`; the project-graph implementation reuses its 3D camera/projection mathematics under the elected MIT terms.
- `/providers` keeps Kimi's two credential products distinct: `kimi-coding`
  configures the K3 coding-plan key (`KIMI_CODING_API_KEY` /
  `OCEAN_KIMI_CODING_KEY`), while `kimi` configures raw metered Moonshot API
  keys. Never save a coding-plan key into the raw Moonshot block.

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
4. **Do not bury product behavior.** A TUI feature is not shipped from a stash,
   worktree, local commit, or unmerged branch. Preserve unfamiliar behavior
   until repository-wide history/reference review proves removal is intended;
   do not replace expected interaction with an error/no-op merely because it is
   simpler. After review and merge, update the canonical checkout, run the TUI
   installer, and smoke the command the operator actually invokes.
5. **Rendering is terminal-safe or it smears.** Any text that reaches a ratatui
   `Span` from tool output, file content, or provider errors goes through
   `sanitize_line` (chat.rs) — raw tabs/control chars desync the terminal from
   ratatui's cell math and leave permanent bleed. Long lines clamp
   (`clamp_line`) or wrap explicitly; never assume one logical line = one row.
6. **The Elm loop is the only mutation channel.** Components emit `Action`s;
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
  row falls back to the startup `/v1/models` fetch only before a session is bound.
  A bound session's model is daemon-owned config: `/model` and the picker must
  both dispatch `Action::SetModel` and persist through
  `PATCH /v1/agent/sessions/{id}/config`; the picker must never mutate only its
  local footer projection. Binding reloads `GET .../config`, and generation
  guards reject late load/save responses across session switches.
- The upper right rail is a mutable Work Surface slot, not a Files-owned pane.
  Its typed representations currently include explicit `Files`, session-scoped
  `Usage`, and daemon-wide `Workflow`; domain projections keep their real schemas
  rather than flattening into one generic graph model. Incoming events may select
  a representation only while the rail is hidden and auto-reveal remains allowed;
  they never replace a visible operator selection. Explicit Files navigation
  restores `FileTreeComponent`.
- The `USAGE` representation records a bounded history of only daemon-reported
  final-request context measurements from correlated finished turns. It never
  substitutes cumulative input tokens or local estimates, follows model reroutes,
  clears on session switch, and labels retained history partial after an SSE gap.
  A todo may reveal an honestly empty Usage surface while awaiting the first
  measured turn; absence must not be backfilled with invented data.
- An authoritative Observatory inactive→active execution transition may
  auto-reveal the hidden Work Surface as the read-only `FLOW` graph unless the
  operator explicitly closed the rail. The graph consumes the boot-bound summary
  token, baselines from `/v1/observatory/snapshot`, resumes
  `/v1/observatory/events` from that cursor, and rebaselines on
  auth/reset/gap/instance or cursor discontinuity; it never derives execution
  truth from chat cards or owns orchestration. Terminal nodes remain inspectable
  until the operator deliberately changes representation. Enter expands the same
  workflow projection into the center graph surface. The renderer keeps all
  authoritative nodes in state, uses immutable execution-id placement, bounds
  only the painted subset with an honest hidden count, and preserves the lower
  session-component tray and operator rail width.
- The lower right rail is a separate session-component tray, never part of
  `FileTreeComponent`. Its context meter uses a btop-inspired terminal grammar:
  a quiet empty bed, per-cell truecolor deep-aqua→cyan→amber→coral ramp, and a
  `░`/`▒`/`▓` frontier for sub-cell dithering (`.`/`:`/`=`/`#` fallback through
  `g()`); numeric warning thresholds remain truthful and separate from texture.
  Its todo adapter applies only successful correlated tool finishes and keeps
  confirmed items pinned while the same session remains bound. The runtime's
  in-memory todo tool uses that same session scope, so the display never outlives
  executable todo state. Explicit todo clear, session switch/new session, daemon
  restart, or SSE-gap invalidation removes the pin. Auto-reveal the Work Surface
  only when this tray transitions hidden→visible; once mounted, unrelated actions
  and Terminal render ticks must respect an operator's explicit rail close. It
  never parses human-formatted todo output into invented durable state. Todo
  `text` remains authoritative; an optional agent-supplied `title` (at most 36
  terminal cells) is display-only and is preferred in the compact tray. Empty or
  short layouts return the full rail to its selected representation; tray
  selection and mouse routing remain pane-bounded.
- Consecutive tool bursts (`shell/components/chat.rs`) collapse under one
  non-wrapping parent summary with truthful running/done/failed counts. Keep the
  parent label neutral and color only each status segment; tool-local failures
  use explicit `failed` copy plus warning-orange, while persistent red is
  reserved for turn/session-level failure. Hidden Thinking turns do not split a
  burst; visible prose/cards/errors do. Opening a parent reveals the existing
  independently expandable per-call drawers; keyboard focus reveals a nested
  drawer's parent, and `Ctrl+O` remains the global open-all override. Mouse
  routing uses the exact wrapped + scrolled geometry and preserves
  drag-to-select precedence.
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
  input row. Enter during an active turn appends a FIFO follow-up prompt; only
  authoritative `TurnFinished` releases the next queued submission. While a
  turn is active, `Esc` posts cancellation for the exact bound
  `TurnStarted.turn_id` request and leaves `busy` set until the authoritative
  `TurnFinished` arrives; cancelled turns render a quiet interrupted marker.
  Overlays and dictation retain first claim on `Esc`. Detached scrollback keeps
  its viewport anchored when live content grows. `/compact` is an idle, bound-session action over the daemon-owned
  atomic compaction route. Replace chat only from the response's bounded public
  `SessionSyncSnapshot`, then restart the scoped SSE stream strictly after its
  `SessionEventFence`; never bridge compact with an independent raw-session GET.
  Binding, stream, operation, activity-probe, and local submission generations
  must reject A→B→A, finish-before-ack, and queued stale completions. Resuming a
  session probes the same bounded `/sync` surface: an active-operation conflict
  latches the composer until authoritative `TurnFinished` or a fenced snapshot;
  an idle fenced snapshot replaces history. A busy turn-submit 409 is a typed UX
  state, never raw HTTP: remove only the tagged rejected optimistic user row,
  preserve its prompt once, and keep Enter latched. Generic pre-execution failures
  also remove only their tagged optimistic row; 408/5xx/connected transport or
  decode uncertainty never restores or rolls back the prompt. Submitted images
  remain submission-scoped until admission is known: restore them only for a
  definite pre-execution rejection, and never for accepted or unknown outcomes.
  Activity probes
  must not install across a newer submission or any compact-owned synchronization
  marker. A per-session unresolved-sync marker survives rebinding, blocks prompts,
  and forces refresh when that session returns. Treat typed replay gaps
  and scoped `ocean.session_changed` extensions as invalidations: clear derived
  projections, abort/increment the old stream generation, and recover only via
  `GET /v1/sessions/{id}/sync`. Compact's own lease-scoped invalidation may
  arrive before its HTTP response: invalidate the old stream immediately but
  defer `/sync` without replacing the compact operation generation, then run
  refresh-only sync after the response. Reject the entire snapshot if its
  identity, fence, visible-role/metadata shape, 512-row cap, or 1 MiB text cap
  is invalid.
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
- Editor viewport behavior is content-aware: every text file first opens as a
  read-only peek with a bounded file-summary header; `Enter` commits that tab to
  normal editing and `Esc` closes it without mutation. Markdown peeks render the
  terminal-safe preview, and `Ctrl-P` flips a committed Markdown tab between that
  live unsaved-buffer projection and raw source without rewriting source, cursor,
  dirty, or source-scroll state. Preview uses the existing Ocean Markdown renderer
  for text and a conservative Kitty-protocol overlay only for resolved local images;
  it owns independent wrapped-row keyboard/mouse scroll and keeps raw paste/edit
  disabled. Other prose extensions soft-wrap vertically, source code scrolls
  horizontally, mouse-wheel scrolling stays independent until the next keyboard
  edit/navigation, and rendered text/cursor geometry share terminal sanitization
  plus Unicode cell widths.
- Inline image rendering is local-only and PNG-native at the terminal boundary: descriptor-confine regular-file sources to the active Markdown/workspace base on Unix and fail closed on platforms without exact descriptor identity, accept only complete CRC- and decoder-validated byte/dimension-bounded local PNGs, snapshot them read-only into a private per-process 64-entry/64-MiB cache removed on exit, bound negative resolution attempts, and never decode data URIs, invoke converters, or fetch remote/embedded resources. Reserve stable logical rows, place only fully visible beds from the currently painted center, clear out-of-band Kitty pixels across scrolling/resize/overlay/viewer transitions, and preserve ratatui's real cursor around every placement. Unknown terminals, multiplexers, other formats, and remote/escaping paths keep a text fallback.
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
  goes through the context-appropriate humanizer (no raw reqwest blobs);
  credential-shaped errors carry `/login` recovery hints; `is_connect_shaped`
  picks the "couldn't reach the daemon" vs "turn could not start" transcript
  prefix. A request timeout or generic post-connect transport failure is
  outcome-unknown only while the TUI lacks acknowledgement. An authoritative
  failed `TurnFinished` is definitive, must never claim an unknown outcome, and
  must state that any completed tool checkpoints remain saved.
- Turn submission uses a dedicated HTTP client with a 30-minute deadman timeout
  and opens a fresh connection for each non-idempotent POST; do not reuse idle
  keep-alive sockets whose server-side close can race the next acknowledgement.
  Provider rounds own their runtime timeout. Retry/restore only definitely-unsent
  connect failures or pre-execution 4xx rejection. HTTP 408 is a known executed
  runtime failure in Ocean and must decode its normal response. On
  `TurnOutcomeUnknown`, never restore or replay the prompt because tools may
  have run; keep input latched and reconcile through the generation-scoped
  fenced session activity probe until `TurnFinished` or an idle snapshot is
  authoritative.
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
