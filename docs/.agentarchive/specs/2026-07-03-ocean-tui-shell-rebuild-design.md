# Ocean TUI Shell Rebuild — Design Spec

**Date:** 2026-07-03
**Status:** Approved (shape), Phase 1 starting
**Owner:** Smaths / Ocean
**Crate:** `crates/ocean-tui`

## 1. Problem

`ocean-tui` today is a ~9,000-line single-file monolith (`main.rs`) on ratatui 0.29
with blocking reqwest and threads+mpsc. It exposes **7 function-key "rooms"** but
only **one** — PM (F7) — is a real interactive agent surface. The other six are
read-only projections (Writers/Rev/Orchestrator show room snapshots;
TideDash/WorkOps/WorldMap are live-ish dashboards with no interaction). The result
is a good streaming-chat buried under five-plus dead tabs, with no reusable
component layer, no diff view, no file tree, no syntax highlighting, no command
palette, and no local model/role configuration.

Separately, the operator has a second Rust TUI — **CTRL** (`/Users/risingtidesdev/dev/ctrl`)
— a polished "workbench" whose **session sidebar** already discovers and resumes
Ocean agent sessions from disk (`~/.config/ocean-rs/sessions`), plus an embedded
PTY stack (portable-pty + vt100 + tui-term). CTRL's last two commits were
explicitly "show ocean sessions in sidebar" and "hydrate ocean sessions by id".

## 2. Goal

Fuse the two into a **session-first Ocean workbench**: CTRL's session rail (stripped
to Ocean-only) as the left shell, ocean-tui's proven streaming chat as the main
surface, rebuilt on the modern ratatui **component architecture** so new
capability is cheap to add. Then layer in the oh-my-pi–style intelligence
(model roles, advisor observer, better edit format) that steers the agent toward
better code. End state: a first-class coding-agent TUI that only runs Ocean
agents and lets the operator configure any coding model / plan / API behind them.

Non-goals: multi-provider session discovery (Claude/Codex/pi rails — CTRL's other
scanners are cut), CTRL's IDE half (editor/graph/git/finder), cloud/multi-machine.

## 3. Architecture

### 3.1 The spine (component + async)

Adopt the canonical ratatui **component-template** pattern (the one gitui and
television converge on):

- A `Component` trait: `fn draw(&mut self, frame, area)`, `fn handle_event(&mut self, Event) -> Option<Action>`, `fn update(&mut self, Action) -> Option<Action>`.
- **Two tokio mpsc channels**: an `Event` channel (key/mouse/tick/render/resize)
  feeding the loop, and an `Action` channel components emit onto. All mutation
  flows through `update(action)` — Elm/TEA style.
- A thin `App` owns the component tree + the active-session state and does nothing
  but route events/actions and call `draw`. **No business logic in the render path.**
- **Runtime: tokio.** Daemon SSE, multiple live sessions, PTY reads, and the advisor
  observer all want real concurrency. The PM room's streaming/coalescing logic is
  *re-housed*, not rewritten, into a `ChatComponent`.

Rendering discipline (from the ratatui perf research): decouple tick from render
(redraw on a `Render` action, ~60fps cap, only on state change/animation); never
`clear()` each frame; heavy work off the render thread, reported via the action
channel; virtualize large scrollback (render only the visible window).

### 3.2 Layout — session shell, not room tabs

The 7-room function-key metaphor is **removed**. The shell is three regions:

```
┌────────────┬───────────────────────────────┬──────────────┐
│ SESSION    │  ACTIVE SESSION (main)        │ CONTEXT RAIL │
│ RAIL       │  native chat: streaming       │ swaps:       │
│ (ocean     │  blocks, tool calls,          │  · Diff      │
│  sessions, │  permissions, advisor cards   │  · Tools     │
│  live dot) │                               │  · Files     │
│            │  [PTY view one keystroke away]│  · Advisor   │
└────────────┴───────────────────────────────┴──────────────┘
  status line: model · role · tokens · cost · daemon health
```

- **Left = session rail** (harvested from CTRL `panel_sessions.rs`): lists Ocean
  sessions for the project, program badge, live dot when hydrated, relative time,
  expand-on-select, grouped by worktree/date. Select → open in main.
- **Main = active session.** Two views, toggled per session:
  - **Native view (primary):** the re-housed PM chat — structured `PmBlock`s
    (Text/Thinking/ToolCall), permission prompts with decision-token binding,
    inline advisor cards. This is where roles/advisor/diffs/permissions live.
  - **PTY view (secondary escape hatch):** CTRL's embedded terminal running
    `ocean --resume <id>`. One keystroke from native. For raw-CLI moments.
- **Right = context rail**, switchable: Diff review (syntect-colored), Tool
  timeline, File tree, Advisor notes. Replaces the dead dashboard tabs with
  surfaces that are *about the active session*.
- **Tabs become per-session tabs**, not rooms.

### 3.3 Harvest map

| Source | Piece | Action |
|---|---|---|
| CTRL `sessions.rs` | `discover_ocean` + `Session` + `resume_command` | Lift; delete Claude/Codex/pi scanners; collapse `enum Program` to Ocean-only |
| CTRL `panel_sessions.rs` | sidebar render + hit-test + hydrate | Lift as `SessionRailComponent`; rebind ~6 god-struct fields to component state |
| CTRL `term.rs` | portable-pty + vt100 + tui-term | Lift as `PtyComponent`; adapt reader thread→mpsc to tokio task→action |
| CTRL `theme.rs` | Tokyo Night palette + depth-fill | Lift as base theme |
| ocean-tui `main.rs` | PM streaming engine, SSE reconnect/replay, `DaemonClient`, permission decision-tokens | Re-house into `ChatComponent` + `DaemonClient` (make async) |

### 3.4 New dependencies (off-the-shelf, per ratatui ecosystem research)

`tokio`, `tui-textarea` (prompt), `syntect` + `tui-markdown` (highlighted
output/diffs), `tui-tree-widget` (file tree), `nucleo` (command palette + session
fuzzy-find), `throbber-widgets-tui` (thinking spinner), `portable-pty` + `vt100` +
`tui-term` (PTY, already CTRL deps).

### 3.5 The intelligence layer (mostly daemon-side)

Surfaced by the TUI, implemented in `ocean-daemon` / `ocean-providers`. Phased last.

- **Model roles** — config (`~/.config/ocean-rs/tui.toml` or shared ocean config):
  `default` / `plan` / `advisor` / `smol` etc., each `provider/modelId[:level]`.
  This is the "configure any coding model/plan/api" requirement. TUI sends role
  config; daemon agent loop honors it. A roles switcher + `/model` in the TUI.
- **Advisor observer** — a second model on its **own context window** watching every
  turn, injecting inline concern/blocker cards (amber). Cheap to bolt onto the
  agent loop; the key property is *separate context* so it isn't polluted by the
  doer's rationalizations. Off unless a model is assigned to the `advisor` role.
- **Hashline edits** — content-hash-anchored edit format; stale anchors rejected
  before they corrupt. Biggest reliability/token win. Daemon-side; optional stretch.

## 4. Phasing

1. **Spine** — component/tokio skeleton, `Component` trait, event/action channels,
   thin `App`. Re-house PM chat as `ChatComponent`. Async `DaemonClient`.
   *Nothing visible changes; everything becomes extensible.* Verification: TUI
   builds, connects to daemon, PM chat streams a turn end-to-end as before.
2. **Session shell** — harvest CTRL rail + PTY. Left session rail lists Ocean
   sessions, select opens native chat, one keystroke to PTY. **Delete the six dead
   rooms.** Verification: rail lists real `~/.config/ocean-rs/sessions`, selecting
   drives the chat, PTY hydrate works.
3. **First-class surfaces** — context rail: diff view (syntect), file tree
   (tui-tree-widget), command palette (nucleo), tui-textarea prompt, syntax
   highlighting, status line (model/tokens/cost/health).
4. **Intelligence** — roles config + advisor observer in the daemon, surfaced in
   the TUI; hashline edits if hungry.

## 5. Risks

- **Async rewrite of the spine** is the biggest lift and touches everything. Mitigation:
  re-house proven logic rather than rewrite it; keep each phase independently
  shippable; Phase 1 is behaviorally a no-op (same features, new structure).
- **Both CTRL modules reach into a 4,530-line `App` god-struct.** Lifting means
  defining a small state struct/trait for the ~6 fields each touches — bounded work.
- **CTRL is sync (no tokio).** Discovery is cheap fs reads (wrap directly); the PTY
  reader thread→mpsc adapts to a tokio task→action channel.
- **Daemon-side intelligence** (roles/advisor) is a separate large chunk in another
  crate — deliberately last so the shell ships without waiting on it.

## 6. Success criteria

- One coherent Ocean workbench: session rail + native chat + context rail, no dead tabs.
- Component architecture — a new panel is a new `Component`, not a new god-struct field.
- Session rail lists/opens/resumes real Ocean sessions; PTY escape hatch works.
- Diff view, file tree, command palette, syntax highlighting, status line present.
- Model roles configurable; advisor observer surfaces inline when enabled.
- `cargo build -p ocean-tui --release` clean; `ocean` symlink rebuilt per TUI change.
