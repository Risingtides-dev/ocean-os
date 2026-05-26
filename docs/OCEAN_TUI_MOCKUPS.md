# Ocean TUI visual mock-ups

Purpose: operator-review mock-ups for the expected Ocean TUI experience based on the current implemented mesh floor, daemon event stream, board/events/inbox/agents views, and recent PTY/review findings.

Scope of this artifact:
- visual/design notes only
- no code behavior changes
- no daemon/service/auth/env/deploy changes

---

## 1. Ocean TUI default coding-agent workspace

Intent:
- make `cargo run -p ocean-tui` open directly into the operator shell, not a daemon dashboard
- keep transcript/composer/tool/approval/diff/session surfaces visible together
- expose TIDES-MESH rooms as first-class tabs while demoting daemon health to support status

```text
┌ Ocean ───────────────────────────────────────────────────────────────────────────────┐
│ ◉  ok  Ocean TUI Coding Workspace                                                   │
│ root ocean-rs · room Orchestrator · session new session                             │
│ [F1 Orchestrator]  F2 Writers  F3 Rev  F4 TideDash  F5 WorkOps  F6 WorldMap  F7 PM │
│ daemon health: ok … · stream connected · approvals 1 · active request 4ab2c991     │
├ Agent Session Transcript ───────────────┬ Tool Timeline ────────┬ Orchestrator Room ┤
│ > scan ocean-rs critique                │  3s [4ab2c991] bash   │ Orchestrator shell │
│ checking runtime state…                 │     started cargo…    │ tasks 16/0/0       │
│ found the main issue…                   │  2s [4ab2c991] bash   │ inbox 3 · feed 120 │
│                                         │     output Finished   │ latest mesh event… │
├ Event Rail ─────────────────────────────┤ Diffs / Edits ────────┼ Approvals (1) ────┤
│ [4ab2c991] tool_started bash …          │ diff --git …          │ * bash [perm123]   │
│ [4ab2c991] tool_output Finished …       │ @@ …                  │ permission req …   │
├ Composer ────────────────────────────────────────────────────────┼ Sessions / Help ──┤
│ > scan ocean-rs critique                                                    │ > new session │
│ · compare daemon and tui state                                              │ > [8c1c…] …   │
│ session target: new session                                                 │ F10/? help    │
└ workspace first | Tab rooms | F1-F7 jump | Enter send | Ctrl-J newline ───┴─────────┘
```

Operator notes:
- the first visible/default screen is now the coding-agent workspace shell
- rooms are navigation primitives, not a separate mode hidden behind support screens
- daemon/health state stays visible, but no longer owns the entire surface
- session detail and some non-Orchestrator room widgets may still show honest placeholders

### Product framing correction after task-26

This mockup should now be read with the corrected product target:

- Ocean TUI is a permanent Ratatui multi-room command center.
- The coding-agent workspace is the primary surface; daemon health and events are supporting primitives.
- The coding-agent workspace is the default shell, while daemon health/events/request state become supporting primitives or an eventual Ops/Systems room.

Future Rust-native room work should mine `rising-tuis/*` for primitives, not treat those Python TUIs as the end state:

- `opsdash.py` → systems / ports / service / cloud-health primitives
- `workdash.py` → review / PR / Linear / local-git primitives
- `notedash.py` + `notedash_api.py` → Writers Room notes / sources / actions primitives
- `tidedash.py` → campaign/content/finance/deal-flow primitives
- `world_time_map.py` → world/timezone/team-presence primitives
- `file_tree_tui.py` → file tree / repo context primitives

Target room model to preserve in future Ocean TUI planning:

1. `PM` — operator communication / PM terminal space
2. `Writers Room` — NoteDash-derived workspace plus Henry/context terminal
3. `Tides Mesh` — main mesh command center
4. `Review Room` — Rev chat plus WorkDash-derived review primitives
5. `TideDash`
6. `WorkOps / OpsDash`
7. `WorldMap` (with file-tree/context primitives composed where useful)

---

## 2. Ocean TUI MeshFloor at 213x56 baseline

Intent:
- match the tmux floor blueprint closely enough for operator recognition
- preserve fixed landmarks: Glyph top-left, board/events center-top, Orchestrator center-bottom
- keep side agents visible even when mostly status-only
- treat this MeshFloor as one room/window inside the broader permanent command center, not the entire Ocean TUI product by itself

```text
┌────────────────────────────────┬──────────────────────────────────────────┬─────────────────────┐
│ Glyph                          │ Board / Events / Inbox                  │ BRICK               │
│ ◉ Glyph / ledger               │ [1:BOARD] 2:EVENTS 3:INBOX 4:AGENTS     │ role: backend       │
│ tasks: 16 total                │ TO DO / ACTIVE / BLOCKED / DONE         │ presence: active    │
│ events: 120 live               │ or                                       │ last: 22s           │
│ agents: 4 active / 1 away      │ event/inbox content when selected        │ hotfix complete     │
├────────────────────────────────┤                                          ├─────────────────────┤
│ KNOX                           │                                          │ PIXEL               │
│ role: review gate              ├──────────────────────────────────────────┤ role: frontend/tui  │
│ presence: active               │ Orchestrator                             │ presence: active    │
│ last: 15s                      │ agent: Ocean-Orchestrator                │ last: 18s           │
│ review ready                   │ checked: just now                        │ task-10 SHIP        │
├────────────────────────────────┤ status: task-17 in progress              │                     │
│ Charlotte                      │ done: 16  active: 0                     │                     │
│ role: research                 │ next: review mockups                     │                     │
│ presence: away                 │                                          │                     │
│ last: 7m                       │                                          │                     │
│ package path research          │                                          │                     │
└────────────────────────────────┴──────────────────────────────────────────┴─────────────────────┘
```

Operator notes:
- this is a read-only floor in the current slice
- Agents tab remains a center-content selector/fallback rather than replacing side rails
- side rails are presence/status panes, not full transcript panes

---

## 3. MeshFloor center-top: Board state

Intent:
- make task wave shape obvious at a glance
- keep counts visible without scanning every card

```text
┌ Board ───────────────────────────────────────────────────────────────────────────────┐
│ [1:BOARD] 2:EVENTS 3:INBOX 4:AGENTS                                                 │
│                                                                                      │
│ TO DO (1)            ACTIVE (0)          BLOCKED / REVIEW (0)      DONE (16)        │
│ ─────────────────    ─────────────────    ─────────────────────     ─────────────     │
│ task-6 Add Telegram  No tasks.            No tasks.                 task-10 Mesh      │
│ /reload product req                                                     presence fix   │
│                                                                                      │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

Operator notes:
- keeps current board grouping semantics
- useful for shift handoff and quick state scan

---

## 4. MeshFloor center-top: Events state

Intent:
- operator can switch center-top from board to events without losing floor context
- useful during active runs, reviews, or routing decisions

```text
┌ Events ──────────────────────────────────────────────────────────────────────────────┐
│  3s task.done PIXEL → Ocean-Orchestrator  task-10 complete and handoff-ready        │
│ 11s send      Ocean-Orchestrator → PIXEL  ACK task-10 done/ready handoff            │
│ 18s task.start PIXEL                    started task-13                              │
│ 29s send      KNOX → PIXEL              review ready when routed                     │
│ 44s task.done BRICK                    provider/auth subsystem                       │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

Operator notes:
- newest-first compact event rail
- source/type/actor must remain visible

---

## 5. MeshFloor center-top: Inbox state

Intent:
- keep direct coordination visible without leaving the floor
- useful when a single operator lane is waiting on routing/review

```text
┌ PIXEL Inbox ─────────────────────────────────────────────────────────────────────────┐
│  5m IN  Ocean-Orchestrator  ACK task-10 done/ready handoff                          │
│ 12m OUT KNOX               Thanks, confirmed. Reservations released.                │
│ 18m IN  Charlotte          Board visible.                                           │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 6. MeshFloor short-terminal collapse plan

Intent:
- do not cram all seven panes into an unreadable mess
- preserve operator landmarks in order of importance

### Short terminal target behavior

1. Keep Glyph as a compact upper-left strip.
2. Keep Orchestrator visible.
3. Keep center content selector (board/events/inbox/agents).
4. Collapse side rails to compact badges/status rows.
5. On very short terminals, show one focused detail pane at a time.

### Example collapsed layout

```text
┌ TIDES-MESH // ocean-rs // Ocean-Orchestrator ───────────────────────────────────────┐
│ ◉ Glyph | KNOX active | Charlotte away | BRICK active | PIXEL active               │
├ Center ──────────────────────────────────────────────────────────────────────────────┤
│ [1:BOARD] 2:EVENTS 3:INBOX 4:AGENTS                                                 │
│ … selected content only …                                                           │
├ Orchestrator ────────────────────────────────────────────────────────────────────────┤
│ status: task-17 in progress | checked: just now                                     │
└ r refresh | p pause | Tab cycle | q quit ───────────────────────────────────────────┘
```

---

## 7. Review notes / current gaps

### Confirmed-good from current slices
- Glyph stays top-left and is not displaced by event content.
- Daemon event stream has its own dedicated panel.
- MeshFloor preserves the tmux mental model.
- Presence fallback can resolve named agents from home registry when project-local live agents are empty.
- Default Ocean TUI framing now points toward a workspace-first, room-based operator shell.

### Known non-blocking polish items
- Agents tab currently maps center content back to Board in MeshFloor.
- MeshFloor parity is proportionally accurate, not cell-exact test-locked yet.
- PTY smoke has verified startup/render shape, but not every interactive keypath.
- The docs now capture the primitive-dissection roadmap, but the Rust-native room primitives themselves still need later implementation tasks.

---

## 8. Recommended operator review questions

1. Is the MeshFloor close enough to the real tmux floor to be recognizable immediately?
2. Should center-top default to Board or Events during active operations?
3. Should side rails remain status-only, or should the active agent pane be expandable?
4. Is the collapsed short-terminal view acceptable, or should it bias harder toward Orchestrator control?
5. Should Agents tab become a true detailed center view later, instead of current Board fallback?

---

## Artifact path

- `docs/OCEAN_TUI_MOCKUPS.md`
