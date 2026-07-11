# Ocean TUI Current-Shell Completion Design

**Date:** 2026-07-09  
**Status:** Approved direction; written design under review  
**Owner:** Ocean TUI  
**Crate:** `crates/ocean-tui`

## 1. Decision

Ocean keeps the current session-first workbench. This wave does not replace the shell, introduce a new room model, or turn the TUI into a different product.

The work is a **completion and responsive-polish pass**:

1. finish the user-visible flows that are already promised and substantially present;
2. make the fixed three-column shell usable at smaller terminal sizes;
3. make navigation and live activity obvious;
4. remove roadmap-only actions from the shipped palette and help until they work end to end.

The invariant is simple: **if Ocean advertises an action, that action works.**

## 2. Current Baseline

The shipped default is the shell under `crates/ocean-tui/src/shell`, not the legacy seven-room interface.

Its stable spatial map is:

```text
┌ Sessions ─────┬ Chat / Editor / Graph ─────────────┬ Files ─────────┐
│ repo          │ active session                      │ project tree   │
│ worktree      │ transcript, tools, permissions      │               │
│ branch        │ composer                            │               │
│ sessions      ├─────────────────────────────────────┤               │
│               │ optional terminal dock              │               │
└───────────────┴─────────────────────────────────────┴───────────────┘
 status: surface · git · session · connection · model · activity
```

Already-working surfaces remain intact:

- repository/worktree/branch/session rail;
- native session resume from a selected rail row;
- streaming transcript, thinking, tools, permissions, advisor notes, and errors;
- inline edit diff cards with full/collapsed rendering;
- file tree, editor, graph, and embedded terminal;
- model, thinking, advisor, provider/OAuth, memory, LSP, settings, image, history, and mention overlays;
- daemon outage recovery and prompt preservation.
- the merged offshore tool family on `main` (`ab64d568`, PR #273): remote Ocean dispatch over the tailnet, per-job Linux worktrees, and git ship/fetch/clean.

This wave reuses those surfaces. It does not fork or rebuild them.

## 3. The Unfinished-Promise Ledger

The current command registry exposes roadmap entries through the live palette and `/help`. One nominally live command does not fulfill its label, and the merged offshore command is wired only in the legacy TUI rather than this default shell.

| Command | Current behavior | This wave |
|---|---|---|
| `/resume` | Marked live, but aliases `/sessions` and only focuses the rail (`components/chat.rs::run_slash`) | **Finish.** Open a searchable session picker; Enter resumes the selected session through the existing `Action::ResumeSession` path. |
| `/diff` | Marked `soon`; inline edit diff rendering already exists, but no workspace review surface exists | **Finish.** Open a read-only workspace change-review surface. |
| `/offshore` | Merged on `main`, but `/offshore on|off|status` exists in legacy `main.rs`; the current shell only re-reads the shared mode flag when submitting a turn | **Finish.** Add the working toggle/status command to this shell’s action catalog and show a compact `offshore` status segment while ON. |
| `/compact` | Advertised `soon`; automatic runtime trimming is not a manual, durable session compaction contract | **Hide.** Restore only with real checkpoint/compaction semantics. |
| `/context` | Advertised `soon`; no typed daemon context snapshot exists for the TUI | **Hide.** Restore only when the daemon exposes truthful context state. |
| `/rules` | Advertised `soon`; runtime stream-rule capability is not a user-facing rule CRUD contract | **Hide.** Restore with the W6 rule-management contract. |
| `/goal` | Advertised `soon`; sessions do not yet own a durable goal field | **Hide.** Restore with durable session metadata and turn injection. |
| `/handoff` | Advertised `soon`; no native session handoff lifecycle exists | **Hide.** Restore with a typed handoff contract, not a prompt macro. |

`/sessions` continues to focus the rail. It is not a synonym for `/resume` after this wave.

`/memory` and `/lsp` are already implemented and remain live even though older roadmap documents describe them as future work.

The `soon` mechanism is removed from the shipped command registry. Future work belongs in roadmap/spec documents, not in the executable command surface.

### 3.1 Offshore merge follow-through

PR #273 is part of the product baseline even when an implementation checkout has not yet integrated the latest `main`. The implementation branch must absorb `ab64d568` before editing overlapping TUI files; offshore behavior is preserved rather than reimplemented from memory.

The current shell adds `/offshore [on|off|status]` to the truthful action catalog:

- `on` and `off` persist the existing shared flag at `~/.config/offshore/mode`;
- `status` reports the mode flag without claiming remote readiness it has not checked;
- ON injects the merged `OFFSHORE_GUIDANCE` into the very next turn;
- the status line shows a compact `offshore` segment only while ON;
- confirmation prose says the guidance is enabled and that tools still require an enabled `[offshore]` provider.

The legacy and current shell use one shared ocean-tui mode/guidance helper; they do not keep parallel copies of the flag path, parser, writer, and guidance string.

Two correctness findings already recorded on merged PR #273 are required follow-through before offshore is called complete:

1. A disabled `[offshore]` table can never break unrelated configuration — at either layer. `remote_url` and `ssh_host` become optional at deserialization so a stale/placeholder `[offshore] enabled = false` block (with or without those fields, valid or not) still loads `ocean.toml`; they are required — and semantically validated — only when `enabled == true`, at which point missing or invalid values fail with the existing specific errors. A disabled table registers no offshore provider or tools.
2. `offshore_events` subscribes with `replay=1`, updates its description from “live-only,” and returns buffered events for a synchronous dispatch that already completed.

The ten `offshore_*` tools, remote worktree lifecycle, permission gates, Tailscale transport, and git shipping protocol remain runtime-owned. This TUI wave does not redesign them.

## 4. Interaction Architecture

### 4.1 One action catalog

Create one static catalog for user-invokable UI actions. It is the source for:

- the global action palette;
- composer slash-command results;
- `/help` output;
- contextual footer hints;
- displayed keybindings.

The catalog describes user intent. `Action` remains the single mutation/event channel and `Nav` remains the pane target type.

Each catalog entry has:

- a stable `ActionId`;
- label and one-line description;
- group;
- the presentations in which it is valid (global palette, slash palette, help, footer);
- optional slash name;
- optional keybinding;
- runtime precondition check;
- argument policy for bare/global versus typed slash invocation.

Both the global palette and `ChatComponent::run_slash` emit one typed `Action::Invoke { id, args }`. `App::dispatch` is the sole catalog executor. Chat-local operations that currently bypass `Action`—`/clear`, `/help`, and the transcript-clearing half of `/new`—move behind that executor through narrow `ChatComponent` methods. `/copy` and bare `/image` similarly resolve the current chat state in `App`, so invoking them outside composer focus does not need a second path. This is consolidation of existing behavior, not a new command runtime.

There are no disabled roadmap entries. Every catalog entry is compiled and executable in each presentation it names. Runtime preconditions such as “no image exists” or “workspace is not a git repository” return a specific status/empty state; build-time-future actions are absent from all presentations.

The existing subsequence scorer in `slash.rs` is reused. No new fuzzy-search dependency is added.

### 4.2 Global palette

`Ctrl-P` opens the action palette from every non-terminal focus target. The terminal keeps raw `Ctrl-P`; the title/status affordance remains clickable so terminal-focused users can open the palette without stealing PTY input.

The palette searches action label, slash name, group, and description. Results include the canonical keybinding and slash name when present.

`/` inside the composer remains the fast command-specific view. It filters the same catalog instead of a separate registry.

`Esc` closes the palette and restores the prior focus. Executing an action closes it before dispatch.

### 4.3 Contextual footer

The footer shows at most three actions relevant to the focused surface, followed by `Ctrl-P actions`. Help remains available through `/help` and the global palette; the shell does not steal printable `?` input from the editor, terminal, or composer.

Examples:

- chat: `Enter send · Ctrl-J newline · Ctrl-R history`;
- sessions: `Enter resume · t terminal · r refresh`;
- files: `Enter open · Space expand · r refresh`;
- editor: `Ctrl-S save · Esc chat`;
- terminal: `double-Esc chat`.

Footer labels are catalog-derived where an action is global. Component-local bindings remain owned by the component but are rendered from a small component binding table, not duplicated format strings.

## 5. Real Resume Flow

### 5.1 Entry points

- `/resume` opens the session picker.
- `Ctrl-P` → `Resume session…` opens the same picker.
- Enter on a session in the existing rail continues to resume immediately.
- Startup auto-resume (launching `ocean` in a project re-opens its most recent native session) is a native resume entry point, not a side channel.
- `/sessions` only reveals and focuses the left rail.

### 5.2 Picker contents

Extract session discovery output into a shared read-only session list consumed by both `SessionRailComponent` and the picker. The picker must not reach into the rail’s private grouped-row state or run a second discovery implementation.

The native picker contains UUID-backed Ocean sessions only. Legacy/non-UUID records remain visible in the rail and retain the existing explicit `t` terminal-resume path; they are not rendered as disabled dead rows in `/resume`.

Each native row shows:

- title;
- project/worktree and branch;
- relative age;
- provider/model when present;
- short session ID.

Typing filters title, path/worktree, branch, model, and session ID with the existing subsequence scorer. Newest activity breaks score ties.

### 5.3 Resume invariant

All native resume entry points — picker, rail, and startup auto-resume — resolve through one shared pair: `resume_preflight(id, path, cwd) -> Result<ResumePayload, ResumeError>` and an atomic `commit_resume(payload)`. Interactive entry points dispatch `Action::ResumeSession { id, path, cwd }`, whose `App::dispatch` arm calls exactly that pair; startup auto-resume calls the same pair directly instead of hand-rolling `load_transcript` + `bind_session_with` (today's `App::new` path binds the latest session without re-rooting cwd and cannot distinguish a corrupt record from an empty one). The shared session list backing the picker and `latest_resumable` carries `cwd` alongside `id` and `path` so startup resume re-roots identically.

`resume_preflight` verifies before any live state changes: the typed UUID is valid, the session record still exists, and transcript loading returns `Result<Vec<HistoryMsg>, ResumeError>`. A valid session with zero user/assistant messages returns `Ok(Vec::new())`; missing files, invalid JSON, and a missing/non-array `messages` field are distinct errors. Preflight failure refreshes discovery, shows a humanized error, and leaves the current workbench untouched — at startup that means Ocean opens fresh in the launch project instead of half-binding a broken session.

After successful preflight, resume updates as one synchronous state transition:

- transcript history;
- bound session ID and SSE subscription;
- live-session marker;
- active workspace root used for future turns;
- file-tree root;
- graph root/cache;
- mention index root;
- git status, reset immediately so the prior worktree’s branch/dirty counts cannot render and scheduled for an immediate refresh against the new root;
- center surface and focus.

The terminal dock is deliberately not re-rooted when it already hosts a live shell.

## 6. Read-Only Change Review

### 6.1 Scope

`/diff` and `Ctrl-P` → `Review workspace changes` open a new center surface. The Sessions and Files landmarks remain where they are; this does not create a new right rail or replace the shell layout.

The surface is read-only in this wave. There are no apply, revert, stage, unstage, or discard controls.

### 6.2 Source of truth

The active worktree is `App::workspace_root`, which already follows native session resume.

The existing local git helper is extended to return a bounded change snapshot containing:

- staged paths and unified diff;
- unstaged paths and unified diff;
- untracked paths, shown as path/status rows without inventing a diff against nonexistent content;
- binary-file markers;
- collection error, if any.

Git keeps the helper’s existing `git -C <root> ...` convention and fixed argument arrays; it never builds a shell command string. Collection uses these explicit limits:

- status output: 512 KiB and at most 4,096 complete NUL-delimited path records;
- staged diff stdout: 2 MiB;
- unstaged diff stdout: 2 MiB;
- stderr: 64 KiB per command.

The helper reads piped output with a cap instead of calling `Command::output()` for diff snapshots. If a diff exceeds its cap, Ocean terminates and waits for that child, discards the final incomplete `diff --git` file section, keeps earlier complete file sections, and adds a visible “additional diff omitted at 2 MiB cap” record. If status exceeds either limit, Ocean keeps complete records only and marks the file list truncated. Truncation is data, not a parse failure.

Collection runs only when the view opens, on explicit refresh, and after a completed turn while the view is open. It never shells out per frame. A non-git workspace shows a neutral empty state and keeps the existing inline edit cards available in chat.

### 6.3 Presentation

The review surface has:

- a compact changed-file list;
- selected-file hunks;
- staged/unstaged/untracked status;
- line and word-level highlighting using the existing diff styling vocabulary;
- `j/k` or arrows for movement;
- `Enter` to focus a file;
- `r` to refresh;
- `Esc` to return to chat.

`diff.rs` remains the pure diff-classification/rendering module. Its existing edit-tool renderer is retained; unified-diff parsing is added beside it rather than replaced.

The command description changes from “review pending edits” to “review workspace changes” because edits have already been applied to the worktree.

## 7. Responsive Layout

The current `SESS_W = 30`, `TREE_W = 30`, and `Constraint::Min(40)` require roughly 102 columns when both rails are visible. The layout becomes width-aware while preserving manual visibility choices.

### 7.1 Width modes

- **Full (`width >= 102`)**: honor both rail visibility settings; current three-column layout.
- **Single rail (`72 <= width < 102`)**: show the focused or most recently focused rail with the center; hide the other rail ephemerally.
- **Center (`width < 72`)**: show the center surface only. Rails remain available through `Ctrl-P`, `/sessions`, `/files`, title buttons, and focus actions.

Automatic hiding does not overwrite the user’s requested visibility choices for the current run. Growing the terminal restores the requested rails; no new cross-run layout persistence is introduced.

When a hidden rail is explicitly focused, it replaces the other rail in single-rail mode or temporarily becomes the center-width surface in center mode.

### 7.2 Height modes

- **Dock (`height >= 24`)**: honor the terminal-dock visibility and clamped height.
- **Compact (`height < 24`)**: hide the dock from the chat/editor/graph layout. Focusing Terminal makes it the center surface until double-Esc returns to chat.

The transcript/composer always receives the minimum usable height before optional chrome.

### 7.3 Tiny terminals

Below the minimum usable area, Ocean renders one bounded message naming the minimum size and the current dimensions. It does not render overlapping panels or panic.

## 8. Live Activity Signal

This wave reports the activity of the currently bound session only. It does not imply background multi-session execution.

The activity reducer has three typed input sources:

1. `AgentTurnEvent` values whose `session_id()` matches the bound session;
2. legacy `OceanEvent::PermissionRequest`/`PermissionDecision` envelopes whose `EventEnvelope::session_id` matches the bound session;
3. local typed actions for submit retry, cancellation, permission decision, and terminal send failure.

Unscoped permission envelopes and envelopes for another session are ignored by both the current transcript and activity reducer. The reducer does not infer state by parsing human-readable `Action::Status` strings. Submit retry callbacks emit a typed retry action carrying phase, attempt, and total; status prose is rendered from that action.

The status line derives one state:

```text
offline | idle | reasoning | streaming | tool:<name> <elapsed> |
approval waiting | retrying | completed | failed
```

Rules:

- `TurnStarted` enters reasoning;
- `AssistantTextDelta` enters streaming;
- `ToolCallStarted` enters `tool:<name>` and records a start instant;
- `ToolCallFinished` returns to reasoning/streaming unless the turn already finished;
- a matching permission request overrides other active labels with `approval waiting`;
- local `PermissionDecided` clears the wait state and returns to the prior active state;
- typed retry/reconnect state does not clear chat busy state;
- `TurnFinished`, cancellation, and terminal send failure end the active state;
- humanized error text remains in the transcript/status rather than being replaced by a generic red state.

The session rail keeps its current live-session marker. Additional per-session running state is later multi-session work and is not fabricated here.

## 9. Error Handling

- Palette actions whose local preconditions are not met show a specific status message and do not mutate focus partially.
- Resume failure preserves the current transcript, binding, project root, and SSE subscription.
- Diff collection failure preserves the previous successful snapshot and shows the new error timestamp.
- Daemon-backed overlays continue to use existing humanized connection/auth errors.
- Narrow-layout transitions preserve selection, scroll, open editor tabs, terminal process, and composer input.
- No user input is discarded by a focus or layout transition.

## 10. Code Boundaries

Primary reuse points:

- `shell/action.rs`: mutation actions and `Nav`;
- `shell/slash.rs`: subsequence scorer; current command data migrates into the action catalog;
- `shell/components/chat.rs::run_slash`: becomes catalog dispatch for slash actions;
- `shell/components/session_rail.rs`: session discovery rows and native resume action;
- `shell/app.rs::update`: remains the sole mutation owner;
- `shell/app.rs::set_active_project`: remains the re-root primitive;
- `shell/app.rs::draw`: delegates width/height mode calculation to a pure layout function;
- `shell/diff.rs`: existing diff vocabulary and edit-card classification;
- `shell/git.rs`: throttled local git collection;
- `shell/status.rs`: status-line projection.
- shared ocean-tui offshore mode/guidance helper used by both the legacy and current shell;
- merged `ocean-agent` offshore configuration and `ocean-runtime::tools::offshore` provider/tool family.

Expected additive components:

- action-palette state/renderer;
- resume-picker state/renderer, backed by session data;
- workspace-change review component;
- pure responsive-layout mode calculation;
- current-session activity state.

This wave should not require a new daemon endpoint or a shared protocol change.

## 11. Non-Goals

- replacing the current shell or restoring the legacy seven-room UI;
- redesigning colors, typography, borders, or visual identity;
- concurrent/background session execution;
- per-session status for sessions the TUI is not subscribed to;
- apply/revert/stage/discard mutation controls;
- manual context compaction or context-inspection APIs;
- stream-rule management UI;
- durable goal or handoff models;
- plugin command discovery, command macros, or user-defined commands;
- a new fuzzy-search dependency;
- redesigning the offshore remote-worktree, transport, dispatch, permission, or git-shipping protocol;
- moving daemon/runtime ownership into the TUI.

## 12. Acceptance Criteria

### Command truthfulness

- Every action visible in the global palette, slash palette, `/help`, or footer executes its documented behavior through the catalog’s declared presentation and the typed invoke path.
- `/clear`, `/help`, `/new`, `/copy`, and bare `/image` behave identically whether invoked from their declared global/slash presentation; none requires composer focus to reach chat state.
- `/resume` opens a searchable picker of native UUID sessions and resumes the selected session.
- `/sessions` only focuses the rail; legacy terminal resume remains on the rail’s `t` binding.
- `/diff` opens a real read-only snapshot of the active worktree’s changes.
- `/offshore on|off|status` works in the current shell; ON affects the next turn, persists across restarts, and never claims the provider is ready without evidence.
- Offshore mode ON has a compact persistent status segment; OFF consumes no status-line space.
- A disabled `[offshore]` block cannot fail daemon config validation, and `offshore_events` replays completed dispatch events.
- `/compact`, `/context`, `/rules`, `/goal`, and `/handoff` appear in none of the executable discovery surfaces.
- There is no `soon` badge or “not wired on this branch” execution path in the shipped registry.

### Layout

- The shell remains recognizable at 180×48 and 120×32.
- At 80×24, the transcript/composer remains usable with at most one rail.
- At 60×20, one center surface remains usable and optional rails/dock do not overlap it.
- Returning to a larger terminal restores requested rails without losing state.

### State and navigation

- Palette, slash, help, footer, and displayed keybindings agree on action identity and labels.
- Resume preflight distinguishes a valid empty transcript from missing/corrupt records; failure changes no bound-session or project state.
- Successful resume updates every project-scoped surface together and never renders cached git status from the previous root.
- Current tool name, elapsed time, matching permission wait, completion, and failure render through one deterministic activity state; unscoped and foreign-session permissions do not enter the transcript or reducer.
- Existing transcript, editor tabs, graph cache behavior, terminal process, composer input, and overlay flows do not regress.

### Review surface

- Staged, unstaged, untracked, binary, empty, non-git, git-error, path-list truncation, and diff truncation states render without panic or misleading partial hunks.
- Diff collection does not run per frame and does not retain more than the specified caps before parsing.
- The surface performs no worktree mutation.

## 13. Verification Contract

Focused tests:

1. action-catalog contract: declared presentations resolve the same IDs, labels, bindings, argument policy, and typed invocation path;
2. chat-local command parity through global/slash invocation;
3. `/offshore` mode parsing, shared-file persistence, legacy/current-shell helper parity, next-turn guidance, and status segment;
4. disabled offshore tables deserialize and load with missing, empty, or invalid `remote_url`/`ssh_host` and register no provider; enabled missing/invalid configuration still fails specifically;
5. `offshore_events` requests replay and returns events emitted before the watcher attached;
6. no hidden roadmap action leaks into executable discovery;
7. session-picker filtering, tie-breaking, native-only rows, and legacy terminal-resume preservation;
8. startup auto-resume shares the preflight/commit pair: a latest-session-in-worktree launch re-roots cwd/tree/graph/mentions/git, and a corrupt latest record opens fresh instead of half-binding;
9. resume preflight for empty, missing, and corrupt transcripts plus atomic project re-root invariants;
10. bounded git collection, complete-hunk truncation, and staged/unstaged/untracked/binary states;
11. responsive layout at boundary widths/heights and state restoration after resize;
12. current-session activity transitions, including scoped permission, typed retry, decision, reconnect, cancellation, and failure precedence;
13. existing shell, ocean-agent offshore-config, and ocean-runtime offshore-tool tests remain green.

Render checks use Ratatui `TestBackend` at:

- 180×48;
- 120×32;
- 102×24 and 101×24;
- 72×24 and 71×24;
- 80×24;
- 60×20.

Live smoke checks use the release binary in tmux:

- real daemon, real resume, and real worktree re-root;
- `/diff` before and after a controlled file edit;
- `/offshore status`, ON, next-turn remote guidance/tool availability, OFF, and restart persistence;
- completed remote dispatch followed by `offshore_events` replay;
- palette from chat, sessions, files, and editor;
- dead custom daemon URL with local navigation still usable;
- terminal focus with raw `Ctrl-P` preserved and mouse/title access to actions.

## 14. Delivery Sequence

1. Integrate merged `main`/`ab64d568` into the implementation line before touching overlapping ocean-tui files.
2. Finish offshore follow-through: shared TUI helper/current-shell command and status, disabled-config validation, and event replay.
3. Replace the executable command registry with the truthful action catalog; hide the five backend-less commands.
4. Add the real resume picker using existing session discovery and `Action::ResumeSession`.
5. Add the read-only workspace change-review surface using existing git/diff styling.
6. Extract responsive layout calculation and cover boundary sizes.
7. Add the current-session activity state and catalog-derived footer/help.
8. Run focused tests and live tmux/offshore smoke; then refresh the owning crate devlogs and root event ledger.

The sequence finishes existing promises before applying presentation polish. It does not create scaffolding for later architecture.
