# Minimax Surface/Agents Scout — Takeover Triage

**Role:** Surface/Agents Scout (MiniMax unavailable — cheap fallback)  
**Model:** Ocean daemon agent (current runtime model)  
**Scope:** `../ocean-surface`, `../ocean-agents`, `../ocean-orchestrator` handoff/state files  
**Date:** 2026-06-12

---

## Commands / Files Inspected

```
../ocean-orchestrator/{HANDOFF_E9_E10.md,PROJECT_STATE.md,ORCHESTRATOR_HANDOFF.md,SPRINT_WIKI.md}
git status/branch/worktree list (all 3 repos)
git diff --stat (uncommitted changes)
git log --oneline origin/main..<branch> (for all candidate branches)
git branch --merged origin/main (to identify landed vs unlanded)
gh pr list --state open (all 3 repos)
```

---

## Executive Summary

The E9/E10 handoff claimed **5 open tickets** with partial branches (256/257/261/262/258).  
**Reality:** The v1 branches for 256, 257, 261, 262 are already merged into `origin/main` (tips point to main or are empty-diff). The *real* unfinished tail lives in **v2 follow-up branches** and **locked Claude worktrees** that were never PR'd.  

- **ocean-surface** has **6 unmerged branches** with real commits (257-v2, 261-v2, 270, 279, 280 + uncommitted widget/float-mode work on main checkout).
- **ocean-os** has **3 unmerged branches** with real commits (256-v2, 262-v2, 258).
- **ocean-agents** has **1 unmerged branch** (`fix/ocean-171-slack-bridge-env-doc`) with uncommitted socket-listener hardening + content-agent skill updates. The `feat/ocean-244-slack-canvas-bridge` branch (the Python bridge consumer) is **not merged**.
- **No open PRs** exist for any of the original 5 tickets.

---

## Per-Ticket Reality

| Ticket | Handoff Claim | Actual State | Where the Code Is |
|---|---|---|---|
| **OCEAN-256** | projection rooms, branch started | v1 merged; v2 exists with extra work | `feat/ocean-256-projection-rooms-v2` @ `78e5fd0` (worktree `agent-a5ad5fadf4bc2dd2c`) |
| **OCEAN-257** | multi-canvas, in progress | v1 merged; v2 has named-canvas patches | `feat/ocean-257-multi-canvas-v2` @ `be6bc9f` (worktree `~/dev/ocean-surface-257`) |
| **OCEAN-261** | place-call UI, branch started | v1 merged; v2 has front-door control | `feat/ocean-261-place-call-ui-v2` @ `ba3ff37` (worktree `.claude/worktrees/ocean-261-v2`) |
| **OCEAN-262** | canvas fulfill route, mostly built | v1 merged; v2 has +629 lines in `main.rs` | `feat/ocean-262-canvas-fulfill-v2` @ `2ca7d24` (worktree `agent-ac757dfd7ebe38d98`) |
| **OCEAN-258** | CRDT/collaboration, not started | **NOT merged** — real commit exists | `feat/ocean-258-canvas-crdt` @ `dd238f0` (worktree `agent-a592f4a1795017b97`), +906 lines across SDK + `main.rs` + docs |

**Adjacent unmerged surface work (not in the original 5):**
- **OCEAN-270** — convergent merge for concurrent edits (`feat/ocean-270-surface-canvas-merge` @ `b36fb8d`, worktree `ocean-surface-worktrees/ocean-270`)
- **OCEAN-279** — native multi-canvas render + switcher (`feat/ocean-279-native-multi-canvas` @ `ec14581`)
- **OCEAN-280** — live collaborator presence on canvas (`feat/ocean-280-canvas-collab-presence` @ `8736283`)

---

## Repo Detail

### `ocean-surface` (main @ `11389fc`)
- **Uncommitted changes on main checkout:**
  - `crates/ocean-surface-ui/Cargo.toml` — adds `DomRect`, `HtmlElement`, `NodeList` web-sys features
  - `crates/ocean-surface-ui/src/app.rs` — strips `daemon_url_from_env` and `daemon_url_fallback`, imports them from `daemon` module instead
  - `crates/ocean-surface-ui/src/main.rs` — adds `widget` module, switches between `App` and `FloatingApp` based on `float_mode_active()`
  - `style.css` — +250 lines (dark-theme widget styling)
  - `crates/ocean-surface-ui/src/widget.rs` — **untracked** (floating widget implementation)
  - This is orphan float-mode work with no branch/PR.
- **Worktrees:** 20+ total. Many are on merged branches (`ocean-257-multi-canvas` worktree sits on main). The valuable ones are `ocean-surface-257` (257-v2), `ocean-261-v2`, `ocean-270`, `ocean-279`, `ocean-280`.
- **Branches merged into origin/main:** `feat/ocean-257-multi-canvas`, `feat/ocean-261-place-call-ui`

### `ocean-os` (main @ `949ddb5`)
- **Worktrees:** 80+ total (legacy factory churn). Most are stale.
- **Unmerged branches with real deltas:**
  - `feat/ocean-262-canvas-fulfill-v2` — `main.rs` +629 lines (POST/GET handler, `canvas_fulfillments` store, route registration)
  - `feat/ocean-258-canvas-crdt` — SDK surface merge types + `main.rs` wiring + `OCEAN_CANVAS_CONVERGENT_MERGE.md` docs (+906 total)
  - `feat/ocean-256-projection-rooms-v2` — exists at `78e5fd0`; not yet inspected for delta size
- **Branches merged into origin/main:** `feat/ocean-256-projection-rooms`, `feat/ocean-262-canvas-fulfill-route`
- **Open PRs:** only #209 (OCEAN-307 tree-sitter) and #204 (docs handoff) — none of the 5 tickets.

### `ocean-agents` (HEAD @ `2576866` on `fix/ocean-171-slack-bridge-env-doc`)
- **Not on main.** The checkout is on branch `fix/ocean-171-slack-bridge-env-doc` with uncommitted changes.
- **Uncommitted diff (socket_listener.py):**
  - `AUTOREPLY_CHANNELS` env-gated auto-reply gating (prevents bot spam)
  - Event streamer integration for live progress mirroring into Slack thread
  - Dedupe key fix: `channel:ts` instead of Slack `event_id` (stops double-delivery)
  - Temporary raw-envelope diagnostics logging
- **Uncommitted diff (content-agent/CLAUDE.md):**
  - Video-gen skill integration (`skills/video-gen.md`, `prompts/prompt-log.csv`, `tools/review_clip.py`)
  - Clip delivery rule: gallery link only, never canvas
- **Unmerged branch:** `feat/ocean-244-slack-canvas-bridge` @ `201c14a` — Python bridge consumer that subscribes to SSE, maps ops to Slack Canvas API, POSTs fulfillment back. This is the **other half** of OCEAN-262.
- **No open PRs.**

---

## Blockers

1. **No PRs for any of the 5 tickets.** Work exists in branches/worktrees but never made it to review.
2. **Branch naming confusion.** v1 branches are merged; v2 branches hold the real remaining work. An agent picking up "OCEAN-257" from the handoff will find the v1 branch empty and think it's done.
3. **`main.rs` pipeline serialization required.** 262-v2 (+629 lines) and 258 (+11 lines) both touch `crates/ocean-daemon/src/main.rs`. Factory Rule E says these must be ONE PR at a time, never parallel.
4. **ocean-agents checkout is on a side branch with uncommitted work.** Any factory agent firing into `../ocean-agents` will land on `fix/ocean-171-slack-bridge-env-doc`, not main, and may collide with the uncommitted socket_listener.py changes.
5. **Locked worktrees.** `worktree-agent-a4b837151c678ac0f` in ocean-surface is "locked" — may block cleanup.
6. **Uncommitted float-mode work on ocean-surface main.** If a factory agent starts from main and does `git status`, it will see dirty tree and may abort or commit unrelated widget work.

---

## Recommended Next Action

**Immediate (this tick):**
1. **Stabilize the checkout state:**
   - `ocean-surface`: stash or branch the uncommitted widget work (`git stash` or `git checkout -b feat/floating-widget-wip`) so main is clean.
   - `ocean-agents`: switch checkout back to `main`, stash the uncommitted 171 changes, and decide whether 171 needs a PR or is obsolete.
2. **Open PR for the smallest closed-loop ticket first:**
   - **OCEAN-262** is the best candidate: the daemon route (262-v2, +629 lines) + the Python bridge (244, ocean-agents) together close the Slack canvas loop end-to-end. But 262-v2 touches `main.rs` — needs full workspace build + Codex review.
   - **Alternative:** OCEAN-258 (CRDT) is self-contained in SDK + docs + main.rs wiring, but it's the heaviest ticket. Sequence it after 262 if throughput is the goal.
3. **Clean up merged v1 branches** (`feat/ocean-257-multi-canvas`, `feat/ocean-261-place-call-ui`, `feat/ocean-256-projection-rooms`, `feat/ocean-262-canvas-fulfill-route`) so future scouts don't mistake them for live work.
4. **Before firing any agent:** verify the target worktree's branch tip is actually ahead of main (`git log --oneline origin/main..HEAD`) and that the diff is additive, not a revert/deletion trap (the 257-v2 and 261-v2 diff stats showed massive deletions when diffed against origin/main — investigate whether those branches are based on an older main or contain actual removals).

**Do NOT restart :4780.**
