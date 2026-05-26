# Ocean TUI visual mock-ups

Purpose: operator-review mock-ups for the expected Ocean TUI experience based on the current implemented mesh floor, daemon event stream, board/events/inbox/agents views, and recent PTY/review findings.

Scope of this artifact:
- visual/design notes only
- no code behavior changes
- no daemon/service/auth/env/deploy changes

---

## 1. Ocean TUI daemon steering view

Intent:
- preserve the upper-left glyph/header anchor
- keep live event stream visibly separate from request/status/transcript flow
- show operator next action clearly

```text
┌ Ocean ───────────────────────────────────────────────────────────────────────────────┐
│ ◉  ok  Ocean TUI                                                                    │
│ daemon URL: http://127.0.0.1:4780                                                   │
│ health: ok  service: ocean-daemon  backend: ocean-native-deepseek                   │
├ Event Stream ────────────────────────────────────────────────────────────────────────┤
│ [4ab2c991] user_message: scan ocean-rs critique                                     │
│ [4ab2c991] assistant_delta: checking runtime state…                                 │
│ [4ab2c991] tool_started: bash args={"command":"cargo check -p ocean-tui"}         │
│ [4ab2c991] tool_output: Finished `dev` profile                                      │
│ [4ab2c991] turn_finished: ok=true wall=822ms                                        │
├ Activity Summary ────────────────────────────────────────────────────────────────────┤
│ request_created [4ab2c991] running                                                  │
│ [4ab2c991] turn_finished: ok=true wall=822ms                                        │
├ Requests ────────────────────────────────────────────────────────────────────────────┤
│ * 4ab2c991  running    prompt accepted                                              │
│   0fd912ee  completed  prompt completed                                             │
├ Transcript ──────────────────────────────────────────────────────────────────────────┤
│ > scan ocean-rs critique                                                            │
│ Found the main issue: daemon health can look ok while prompts fail…                 │
│                                                                                     │
├ Sessions ────────────────────────────────────────────────────────────────────────────┤
│ 8c1c…  4 turns  ocean-rs critique                                                   │
├ Composer ────────────────────────────────────────────────────────────────────────────┤
│ >                                                                                   │
└ Enter send | Ctrl-C cancel | s sessions | r refresh | q quit | stream connected ───┘
```

Operator notes:
- glyph remains first/top-left visual anchor
- event stream does not displace glyph
- event stream is the primary live feedback rail
- request list remains compact and secondary to the stream

---

## 2. Ocean TUI MeshFloor at 213x56 baseline

Intent:
- match the tmux floor blueprint closely enough for operator recognition
- preserve fixed landmarks: Glyph top-left, board/events center-top, Orchestrator center-bottom
- keep side agents visible even when mostly status-only

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

### Known non-blocking polish items
- Agents tab currently maps center content back to Board in MeshFloor.
- MeshFloor parity is proportionally accurate, not cell-exact test-locked yet.
- PTY smoke has verified startup/render shape, but not every interactive keypath.

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
