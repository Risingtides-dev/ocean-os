# Ocean TUI ↔ Tides Mesh parity contract

This is the no-feature-drop contract for migrating the current Tides Mesh/operator UX into Rust Ocean TUI.

## Rules

- Keep the existing tmux/operator layout working as a temporary visual reference until Rust parity is proven.
- Read the same file-backed sources first; the tmux floor is not runtime authority.
- Preserve the four core views: board, events, inbox, agents.
- Preserve operator shortcuts, narrow-pane behavior, and visible failure/reconnect states.
- Coordinate through live `pi_messenger` feed/mailbox; do not hide team state.

## Current operator layout

1. `#1` — main operator Pi session, untouched.
2. `#2 WritersRoom` — `notedash` + Henry + `filetree`.
3. `#3 Tides-mesh Orchestrator` — `pimesh-tui` board + live orchestrator pane.
4. `#4 WorldMap` — `worldmap`.
5. `#5 TideDash` — `tidedash`.
6. `#6 WorkOps` — `workdash` + `opsdash`.
7. `#7 Rev Review` — review / PR workspace.

## `pimesh-tui` contract

### Tabs

- `board` — crew tasks plus mirrored external work tasks.
- `events` — newest-first event feed.
- `inbox` — DM history for the current agent.
- `agents` — live presence / hygiene view.

### Keys

- `1`..`4` — direct tab selection
- `Tab` — cycle tabs
- `r` — refresh
- `p` — pause/resume auto-refresh
- `q` — quit

## Data paths

### Crew / mesh state

- `.pi/messenger/crew/tasks/*.json`
- `.pi/messenger/crew/tasks/*.md`
- `.pi/messenger/feed.jsonl`
- `.pi/messenger/mailboxes/by-agent/<agent>.jsonl`
- `.pi/messenger/live/agents/*.json`
- `.pi/messenger/registry/*.json`
- `~/.pi/agent/messenger/registry/*.json`
- `~/.pi/messenger/registry/*.json`

### Adjacent cache layer

- `.pi/workdash/external-tasks.json`
- `.pi/unified/state.json`
- `.pi/unified/events.jsonl`

## Status semantics

### Tasks

- `todo` / `pending` / `ready` → to-do bucket
- `in_progress` / `progress` → active bucket
- `blocked` / `review` / `milestone` → blocked/review bucket
- `done` → done bucket

### Agents

- `active` — good PID + heartbeat age <= 2m
- `away` — good PID + heartbeat age <= 15m
- `stale` — missing PID, zombie PID, no heartbeat, or old heartbeat
- duplicate PID records collapse to the newest record

### Events

- newest first
- compact preview text is enough for the rail, but source/type/actor must stay visible

## Launcher contract

- `scripts/load-rising-env.sh` loads repo-local and user-local env files.
- `scripts/start-tides-agent.sh` sets `TIDES_MESH_ENABLE=1`, `PI_MESH_MODE=1`, and the callsign vars before launching `pi` with the persona prompt.
- `scripts/start-tides-orchestrator.sh` must identify as `TIDES-MESH orchestrator`.
- `scripts/operator-layout.sh` preserves `#1` and rebuilds `#2`-`#7` around the canonical Tides Mesh layout.
- `scripts/pimesh-layout.sh` owns the live Tides Mesh operator pane geometry.

## Adjacent dashboards

These remain separate tools, not Ocean runtime authority:

- `notedash` — notes / writing / drafts
- `worldmap` — timezones / teammate clock / sun line
- `workdash` — task / repo / review queue
- `opsdash` — services / cloud / process health
- `tidedash` — campaigns / content / finance / Slack / Postgres

`scripts/start-rising-unified-services.sh` keeps the read-only sync cache and loopback API warm for the adjacent dashboards.

## Ocean TUI parity gate

Rust Ocean TUI may replace `scripts/pimesh-tui.mjs` only after it matches:

- the same four views
- the same keyboard behavior
- the same file-backed sources
- the same agent presence labels
- the same board/task grouping
- the same visible error/reconnect behavior
- no change to runtime authority
- decommission gate: F1-F4 must render, submit, stream, and cancel through Ocean runtime before tmux can be retired as the primary visual floor

## Acceptance checks

- `scripts/operator-layout.sh --replace` still rebuilds the live tmux layout.
- `pimesh-tui` renders all four tabs from repo-local state.
- Ocean TUI can mirror board/events/inbox/agents without mutating mesh state.
- No Tailscale, SSH, VPN, or other remote-access service changes.
