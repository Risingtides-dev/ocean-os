# Handoff — Ocean TUI/UX + model-provider lane

**As of:** 2026-07-09 ~3:40am · main @ `73b4ed4` · both binaries deployed from clean main
**Lanes:** this doc covers the TUI-UX + providers lane. A second agent lane owns runtime/browser
(currently mid-flight: browser_stream.rs / daemon main.rs WIP with compile errors in the shared
working tree — do not touch, do not build `--workspace` from the dirty tree).

## Current state (all landed on main, deployed, live-verified)

**Deploy discipline:** binaries always build from a clean `git archive <main-commit>` temp checkout
with `CARGO_TARGET_DIR=<repo>/target` (never from the dirty shared tree), then
`launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon` for the daemon. TUI binary =
`target/release/ocean-tui` (the `ocean` symlink target). Daemon logs: `/private/tmp/ocean-daemon.log`.

### TUI (workbench shell)
- Session rail: two-level tree — directory nodes → git-branch nodes → sessions (branch stamped on
  records at creation; legacy records bucket as "(no branch)"). `＋ new` on both header levels.
- Auto-resume drops into the latest session for cwd with transcript from disk.
- `/models` picker (bare `/model` opens it too): live registry fetch, ready-first grouping,
  not-ready greyed with reason, ↑↓/⏎/esc/click/wheel, ←→ cycles per-turn thinking level.
  `/thinking <level>` sets the same override directly. `thinking_override` rides
  `AgentTurnRequest.thinking_level` on every turn.
- Failover honesty: `ModelRerouted` event (runtime→agent→daemon SSE→all surfaces). Chat renders a
  concern card + status line; ACP gets a message chunk; legacy TUI a transcript line.
- Mouse text selection: drag sweeps, release auto-copies (pbcopy) + "copied N chars" status.
- Composer soft-wraps + grows (cap 8 rows, then scrolls to cursor). Slash palette scrolls with
  selection. Tool cards clamp to one row per line collapsed (⌃O expands) and sanitize
  tabs/control chars (`sanitize_line`) — raw tabs desync ratatui and smear the screen.
- The other lane added: `/providers` auth popup (bare `/login`), `/login claude|codex` OAuth (real
  PKCE, `ocean-oauth` crate), resizable dock, mid-turn daemon-blip retry.
- `/advisor` picker: per-session second-opinion reviewer (off-row + ready models). Rides turns as
  `AgentTurnRequest.advisor` (`AdvisorControl{enabled,model}`); daemon `resolve_advisor_alias`
  gives the per-turn override precedence over global `[roles].advisor`.
- Status-line dashboard (`shell/status.rs`): focus · model · git(±dirty ↑↓) · tok/s · session
  tokens · session · advisor · message. Git cached, refreshed on the 1s tick.
- `/memory` browser: `GET /v1/memory` (over `ocean_agent::list_memories`) + overlay with search +
  enter-to-copy. `/lsp` panel: `GET /v1/lsp?cwd=` (over `ocean_agent::lsp_servers`, pure fs/$PATH,
  no spawn) + ready/install list; live diagnostics stay the agent's in-turn `lsp` tool.
- Inline images (`shell/kitty.rs`): `![alt](path)` at line-start → 🖼 card; `/image [path]` (bare =
  newest in transcript) → full-screen kitty viewer (PNG via native file transmission, emitted after
  ratatui paints, cleared on close). Non-kitty/non-PNG → honest note. Full-screen (not
  inline-in-scroll) by design — avoids the smear. Markdown also now renders tables/rules/links/
  strikethrough/task-lists; collapsed tool cards are one line each.

### Models / providers (registry current as of 2026-07-08, all verified with live turns)
- Claude generation: `claude-opus-4-8`, `claude-sonnet-5`, `claude-haiku-4-5` (anthropic) +
  `claude-code-{fable-5,opus-4-8,sonnet-5,haiku-4-5}` (subscription). Old 4-6/4-7 ids menu-retired
  but still routable. Wire is bare `Bearer` + `anthropic-version` — NO beta header needed. 429s on
  big models = John's sub quota window (haiku usually still has quota); NOT auth.
- GLM = Z.ai coding plan default (`api.z.ai/api/coding/paas/v4`, `OCEAN_GLM_BASE_URL` override);
  glm-4.6/4.7/5.2 live on John's zai key. MiniMax = international default (`api.minimax.io/v1`,
  `OCEAN_MINIMAX_BASE_URL` override) — John's key 401s on the mainland host.
- Keys live in `~/.config/ocean-rs/auth.json` (glm + minimax imported from `~/.pi/agent/auth.json`;
  backups alongside). Anthropic API key deliberately NOT wired (valid, zero credit). Kimi key
  wired but the account is suspended (needs recharge upstream).
- `GET /v1/models` returns per-model `ready` + `credential_source`
  (`known_models_with_readiness`). Readiness = config truth, not liveness.
- Adding a model touches FOUR places: ocean-providers `known_models` + resolver arms,
  ocean-protocol `Model` constructors, ocean-agent claude-code mapping.
  `cargo test -p ocean-providers` enforces the invariants.

## Key decisions
- Failover stays (unattended loops need it) but must announce itself — never silently swap models
  on an operator. Extend `ModelRerouted`, don't suppress routing.
- Old model ids: menu-retired, never deleted (pinned sessions must keep resolving).
- Picker shows not-ready models greyed with the reason (discoverability > hiding).
- In-TUI agents editing ocean-os get hard rules via `crates/ocean-tui/AGENTS.md` (enums
  additive-only + grep call sites, compile before finishing, re-read before edit, sanitize
  rendered output, Elm loop only). Born from a real incident (`SetModel` replaced → 4 call sites
  broke; reconciled — `SetModel` and `SetThinking` coexist).

## Next steps (this lane's backlog, none started)
- Roadmap slash commands STILL `soon` (need backends): /compact /context /diff /rules /goal
  /handoff. (/memory, /lsp, /advisor, /image, /models, /thinking are now LIVE.)
- Richer `/lsp`: human-facing on-demand diagnostics (needs the daemon to drive the stateful
  language-server spawn + wait — deferred; today's /lsp is discovery/status only).
- Inline images: PNG-only + full-screen viewer today. Follow-ups: non-PNG (needs decode dep),
  clickable image cards (transcript hit-testing), truly inline-in-scroll render.
- Transcript jump-navigation for long sessions; image PASTE into composer; richer rail previews.
- Consider surfacing provider quota state (429 history) in the picker as a soft "limited" badge.

## Blockers / cautions
- Shared working tree with the runtime/browser lane — stage only your own files, never `git add -A`,
  re-read files immediately before editing (they shift mid-edit).
- Codex (chatgpt.com) intermittently hangs ~10s/attempt from this box; retry ladder makes gpt-5.x
  turns feel slow when it happens. Not a daemon bug.
- PTY verification harness (pyte) lives in `$CLAUDE_JOB_DIR/tmp/*.py` patterns — drive the real
  binary and inspect frames before any "done" claim (definition of done = works in John's hands).

## Key paths
- TUI shell: `crates/ocean-tui/src/shell/` — app.rs (dispatch/modals/selection), components/chat.rs
  (transcript/composer/palette/cards), components/session_rail.rs, client.rs, slash.rs, theme.rs
- Providers/registry: `crates/ocean-providers/src/lib.rs` · Models: `crates/ocean-protocol/src/types.rs`
- Turn/failover: `crates/ocean-agent/src/lib.rs` (resolve_turn_state_with_failover,
  run_turn_with_failover, ModelRerouted emits) · Events: `crates/ocean-runtime/src/types.rs`,
  `crates/ocean-agent-sdk/src/lib.rs`, daemon bridge in `crates/ocean-daemon/src/main.rs` (~9200)
- Contracts: `crates/ocean-tui/AGENTS.md` (hard rules) · Ledger: `events.md` (append-only, worktree tag)
