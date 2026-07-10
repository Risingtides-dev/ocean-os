# Handoff — Ocean TUI/UX + model-provider lane

**As of:** 2026-07-10 · **source of truth = `origin/main` @ `a0eef51c`** · today's TUI readline + paste/tool-card fixes landed and verified live.
**Git caution:** local `refs/heads/main` is STALE at `88aad1b`; the working tree is on `feat/ocean-tui-shell-rebuild` @ `80ac2d04`. Before touching anything: `git fetch origin && git checkout main && git pull` (or rebase) so you build the real tip, not `88aad1b`.
**Lanes:** this doc = TUI-UX + providers lane. A second lane owns runtime/browser (voice STT/TTS landed at `fc8f5000`; its remaining WIP — `browser_stream.rs` / daemon `main.rs` — is still dirty in the shared working tree: do NOT touch, do NOT `cargo build --workspace` from the dirty tree).

## Deploy discipline
Build binaries from an ISOLATED checkout of the target main commit — `git worktree add --detach /tmp/… <commit>` or a fresh `git clone` — with `CARGO_TARGET_DIR=<repo>/target`, NEVER from the dirty shared tree. Daemon reload: `launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon`. Daemon runs from a neutral cwd (`$HOME`) on `:4780` (health = `GET /health`, not `/v1/health`). TUI binary = `target/release/ocean-tui` (the `ocean` symlink). Daemon log: `/private/tmp/ocean-daemon.log`.

## Current state (all landed on origin/main, verified)

### NEW today — readline composer (`80ac2d04`)
- `ChatComponent` now has a real cursor: `cursor: Option<usize>` = UTF-8 **byte** offset into `input`, `None` = end-of-line. Every insert/delete/kill/completion path keeps it on a char boundary.
- Bindings: Left/Right (Unicode-scalar steps), Home/End, Ctrl-A/E (line start/end), Ctrl-B/F (char), Ctrl-D (delete-forward), Ctrl-K (kill to end), Ctrl-U (kill line-prefix), Ctrl-W (kill word-back), Ctrl-Y (yank, kill-ring cap 10), Backspace/Delete mid-line. Ctrl-L clears the idle transcript only (no-op while busy), preserving composer + cursor.
- Render is cursor-cell aware: `line_visual_rows` via `UnicodeWidthStr`, wide CJK/emoji occupy 2 cells, cursor never sits on the phantom final row; composer scrolls to follow the cursor.
- Mention (`@`) + slash (`/`) completion and paste insert at the cursor and are cursor-relative; mention detection uses the whole-word prefix around the cursor, suffix-preserving, single-separator, UTF-8/NBSP-safe.
- Covered by ~18 `cursor_readline_*` / `review_regression_*` / mention-safety tests inside `cargo test -p ocean-tui` (**297 pass, 4 ignored**).

### Today's fix — bracketed paste + tool-card render (`3b1eef8a`)
- Paste is ONE `Event::Paste`, never a replayed key stream → multi-line paste no longer auto-submits mid-paste. `Component::handle_paste` inserts verbatim (CRLF/CR normalized, tabs expanded, control bytes dropped; `^R` search gets the query). Editor inserts at cursor; overlays (advisor/models/settings/providers) swallow paste so it can't leak to the composer; `/providers` API-key entry takes a pasted key as one event.
- PTY passes paste through bracket-aware (vt100 mode 2004 → `ESC[200~ … ESC[201~`).
- Collapsed tool cards can no longer paint control bytes — `one_line()` + `sanitize_line` strip ESC/CSI/bell (raw bytes desync ratatui and smear). A burst compacts to the newest 3 cards behind a "· N earlier tools" line, errors always kept visible; edit-diff bodies render only at the transcript tail. **Verified live**: cards render `✓ bash <cmd> · N lines`, clean, no smear.

### Still-true landed TUI surface (prior lane work)
- Session rail: dir → git-branch → session tree, `＋ new` on both header levels, auto-resume into latest session for cwd.
- `/models` (bare `/model`) picker: live registry, ready-first, greyed not-ready w/ reason, ←→ cycles thinking level; `/thinking <level>` direct. Rides `AgentTurnRequest.thinking_level`.
- Failover honesty: `ModelRerouted` event → concern card + status line (never a silent swap).
- Mouse drag selection → release auto-copies (pbcopy). `/providers` auth popup (bare `/login`), `/login claude|codex` real PKCE (`ocean-oauth`). `/advisor` per-session reviewer (rides `AgentTurnRequest.advisor`). Status dashboard (`shell/status.rs`). `/memory` + `/lsp` panels. Inline images (`shell/kitty.rs`): `![alt](path)` card + `/image [path]` full-screen kitty viewer.

### Models / providers (registry as of 2026-07-08)
- **Anthropic wire (CORRECTED — the old handoff was wrong):** OAuth / `AuthMethod::Bearer` turns MUST carry `anthropic-beta: oauth-2025-04-20` **plus** a Claude Code identity system block (exact first line: `You are a Claude agent, built on Anthropic's Claude Agent SDK.`). API-key turns NEVER send the beta (a test asserts ApiKey grows no `anthropic-beta`). Const `ANTHROPIC_OAUTH_BETA` in `anthropic.rs`; landed `ffca90c`.
- Claude ids: `claude-opus-4-8 / -sonnet-5 / -haiku-4-5` (anthropic) + `claude-code-*` (subscription). Plain claude ids route through `ProviderId::ClaudeCode` — **OAuth-only, no API-key path** (John's explicit choice). 429 on big models = subscription quota window, NOT auth.
- GLM = Z.ai coding plan (`api.z.ai/api/coding/paas/v4`, `OCEAN_GLM_BASE_URL` override); glm-4.6/4.7/5.2. MiniMax intl (`api.minimax.io/v1`, `OCEAN_MINIMAX_BASE_URL` override). Keys in `~/.config/ocean-rs/auth.json`. Anthropic API key deliberately unwired (valid, zero credit); Kimi account suspended (needs recharge upstream).
- `GET /v1/models` = per-model `ready` + `credential_source` (config truth, not liveness). Adding a model touches FOUR places: ocean-providers `known_models` + resolver arms, ocean-protocol `Model` ctors, ocean-agent claude-code map. `cargo test -p ocean-providers` enforces the invariants.

## Key decisions
- Cursor = byte offset with `None`=end; all mutation sites keep it char-aligned (never split a UTF-8 scalar). Tests written RED first (TDD), then greened.
- Paste is a first-class `Event`, never a key replay — that is the actual fix for mid-paste auto-submit.
- OAuth ≠ API-key on the wire: Bearer needs the beta + identity fingerprint; enforced by tests both directions.
- Failover announces (`ModelRerouted`), never silent. Old model ids menu-retired, never deleted (pinned sessions must keep resolving). Picker shows not-ready greyed w/ reason (discoverability > hiding).
- In-TUI edits obey `crates/ocean-tui/AGENTS.md`: enums additive-only + grep call sites, compile before finishing, re-read before edit, sanitize rendered output, Elm loop only.

## Next steps (backlog, none started)
- Roadmap slash still `soon` (need backends): `/compact` `/context` `/diff` `/rules` `/goal`. (`/handoff` is manual via this skill; `/memory /lsp /advisor /image /models /thinking` are LIVE.)
- Richer `/lsp`: on-demand human-facing diagnostics (needs daemon-driven stateful language-server spawn + wait — today's `/lsp` is discovery/status only).
- Images: non-PNG decode, clickable transcript cards, truly inline-in-scroll render, image PASTE into composer.
- Transcript jump-navigation for long sessions; richer rail previews; provider quota "limited" badge from 429 history.

## Blockers / cautions
- **Live-verification isolation (learned 2026-07-10):** a fresh `ocean` launched in a project cwd AUTO-RESUMES that workspace's latest session (`latest_resumable()`, `app.rs:~428`) — a verification prompt lands in a REAL session's transcript and streams to any bound SSE client. To test live: run `/new` first, OR launch from a throwaway cwd. Sessions persist read-only at `~/.config/ocean-rs/sessions/<slug-cwd>/<uuid>.json` (`id/created_ms/updated_ms/client_type/messages`) — read them to characterize a session or prove a running TUI is NOT bound (a session created after the process launched cannot have been auto-resumed at startup).
- **John's open `ocean` (PID as of this session: 64078) is still the July 9 binary** (inode 349090717); the rebuilt release is a newer inode. macOS keeps the old mapped binary until relaunch — he must quit + relaunch `ocean` to get today's fixes. Do NOT kill it (may hold an unsent composer draft).
- Local `main` is behind `origin/main` (see Git caution up top) — never commit on top of stale local `88aad1b`; fetch/rebase onto `origin/main` first.
- Shared working tree still dirty with the runtime/browser lane — stage only your own files, never `git add -A`, re-read files immediately before editing (they shift mid-edit).
- Definition of done = works in John's hands: drive the real binary in tmux and inspect frames before any "done" claim.

## Key paths
- Composer / cursor: `crates/ocean-tui/src/shell/components/chat.rs` (`cursor` field ~110, `clamp_cursor`, `cursor_left/right`, `kill_*`, `insert_mention`, `line_visual_rows`, draw). Paste chain: `chat.rs handle_paste` + `shell/component.rs` + `shell/pty.rs` + `shell/components/pty_pane.rs`.
- Dispatch / overlays / selection: `crates/ocean-tui/src/shell/app.rs`. Actions: `crates/ocean-tui/src/shell/action.rs`.
- Providers / registry: `crates/ocean-providers/src/lib.rs`. Models / wire: `crates/ocean-protocol/src/types.rs`, `crates/ocean-protocol/src/providers/anthropic.rs` (`ANTHROPIC_OAUTH_BETA`, `apply_auth`).
- OAuth: `crates/ocean-oauth/` (PKCE + localhost callback). Turn-time refresh: `crates/ocean-agent/src/oauth_refresh.rs`.
- Turn / failover: `crates/ocean-agent/src/lib.rs`. Daemon routes: `crates/ocean-daemon/src/main.rs` (`/v1/agent/turns`, ~1574), SSE bus `crates/ocean-daemon/src/bus.rs`.
- Contracts: `crates/ocean-tui/AGENTS.md` (hard rules). Ledger: `events.md` (append-only, `worktree:` tag).
- Archived prior handoff: `.agentignore/handoff-2026-07-09-tui-ux-providers.md`.
