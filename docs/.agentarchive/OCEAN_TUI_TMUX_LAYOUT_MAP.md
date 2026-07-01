# OceanTUI Tides Mesh Layout Blueprint

Purpose: capture the live tmux Tides Mesh floor as the blueprint for building the OceanTUI mesh view. This document is measurements-first: panes, windows, coordinates, sizes, and intended role mapping.

Snapshot source: `tmux list-sessions`, `tmux list-windows -a`, and `tmux list-panes -a` on 2026-05-25 after the operator-directed top-left Glyph split.

## Build rule

OceanTUI should model the real tmux floor before inventing new layout. The Tides Mesh Orchestrator window is the primary blueprint:

- Preserve the three-column command-center shape.
- Preserve a dedicated top-left Glyph/ledger pane.
- Keep Orchestrator center-bottom as the operator control pane.
- Keep live board/events center-top.
- Keep implementation agents visible in side panes.
- Prefer proportional constraints derived from these measurements, with terminal-size guards for short terminals.

## Coordinate convention

- Coordinates are tmux character cells.
- `x,y` is pane upper-left.
- `w,h` is pane width/height excluding borders.
- Window baseline for active Tides Mesh windows: `213x56`.

## Session summary

| Session | Windows |
| --- | ---: |
| `0` | 2 |
| `tidemesh` | 8 |

## Window summary

| Session | Win | Name | Size | Layout role |
| --- | ---: | --- | --- | --- |
| `0` | 1 | `pi` | `240x66` | single Pi shell |
| `0` | 2 | `pi` | `240x66` | single Pi shell |
| `tidemesh` | 2 | `pi` | `213x56` | single Pi shell |
| `tidemesh` | 3 | `Writers Room` | `213x56` | NoteDash + Henry + shell |
| `tidemesh` | 4 | `Tides-mesh Orchestrator` | `213x56` | primary mesh floor |
| `tidemesh` | 5 | `Rev Review` | `213x56` | review/PR room |
| `tidemesh` | 6 | `TideDash` | `213x56` | board |
| `tidemesh` | 7 | `WorkOps` | `213x56` | ops split |
| `tidemesh` | 8 | `WorldMap` | `213x56` | map board |
| `tidemesh` | 9 | `test window` | `213x56` | scratch/test |

## Primary blueprint: `tidemesh:4` Tides-mesh Orchestrator

Window size: `213x56`.

### Column geometry

| Column | x range | Width | Approx % | Role |
| --- | --- | ---: | ---: | --- |
| Left | `0..65` | 66 | 31% | Glyph + KNOX + Charlotte |
| Center | `67..168` | 102 | 48% | board/events + Orchestrator |
| Right | `170..212` | 43 | 20% | BRICK + PIXEL |

There are tmux border columns between panes; use ratios rather than hard-coded border cells when building OceanTUI.

### Pane map

| Pane | Inferred role | Title | Cmd | Active | x | y | w | h |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 0 | Glyph ledger | `π - ocean-rs` / Glyph session | `pi` | 0 | 0 | 0 | 66 | 14 |
| 1 | KNOX review | `Tides Mesh · KNOX` | `pi` | 0 | 0 | 15 | 66 | 21 |
| 2 | Charlotte research | `Tides Mesh · Charlotte` | `pi` | 0 | 0 | 37 | 66 | 19 |
| 3 | live board/events | `tide-net` | `node` | 0 | 67 | 0 | 102 | 28 |
| 4 | Orchestrator control | `π - ocean-rs` | `pi` | 1 | 67 | 29 | 102 | 27 |
| 5 | BRICK backend | `Tides Mesh · BRICK` | `pi` | 0 | 170 | 0 | 43 | 27 |
| 6 | PIXEL frontend/TUI | `pi:c` / PIXEL session | `pi` | 0 | 170 | 28 | 43 | 28 |

### Visual plot

```text
Tides-mesh Orchestrator — 213x56

x=0                              x=67                                      x=170
┌────────────────────────────────┬──────────────────────────────────────────┬─────────────────────┐ y=0
│ Glyph ledger                    │ Live board/events/inbox                 │ BRICK               │
│ 66x14                           │ 102x28                                  │ 43x27               │
├────────────────────────────────┤                                          │                     │ y=14/15
│ KNOX review                     │                                          │                     │
│ 66x21                           ├──────────────────────────────────────────┼─────────────────────┤ y=28/29
│                                 │ Orchestrator control                    │ PIXEL               │
├────────────────────────────────┤ 102x27                                  │ 43x28               │ y=36/37
│ Charlotte research              │                                          │                     │
│ 66x19                           │                                          │                     │
└────────────────────────────────┴──────────────────────────────────────────┴─────────────────────┘ y=56
```

### OceanTUI constraints derived from tmux

For a normal `213x56` terminal:

```text
Horizontal:
- left rail:   31% / min 48 cols / target 66
- center:      48% / min 80 cols / target 102
- right rail:  20% / min 36 cols / target 43

Left vertical:
- Glyph:       25% / target 14 rows
- KNOX:        38% / target 21 rows
- Charlotte:   34% / target 19 rows

Center vertical:
- Board/events: 50% / target 28 rows
- Orchestrator: 48% / target 27 rows

Right vertical:
- BRICK:       48% / target 27 rows
- PIXEL:       50% / target 28 rows
```

Recommended Ratatui-style model:

```rust
// Pseudocode constraints, not final API.
horizontal = [
    Constraint::Percentage(31),
    Constraint::Percentage(49),
    Constraint::Percentage(20),
];

left = [
    Constraint::Length(14), // Glyph on 56-row baseline
    Constraint::Min(18),    // KNOX
    Constraint::Length(19), // Charlotte on baseline; make flexible on short terminals
];

center = [
    Constraint::Percentage(50),
    Constraint::Min(20),
];

right = [
    Constraint::Percentage(49),
    Constraint::Min(20),
];
```

For short terminals, do not cram all panes. Collapse in this order:

1. Preserve Glyph top-left as a compact ledger strip.
2. Preserve Orchestrator control.
3. Preserve board/events.
4. Collapse side agents to tabs/badges.
5. Hide non-active detail panes behind focus navigation.

## Other tmux windows

### `tidemesh:3` Writers Room — `213x56`

| Pane | Role | Title | Cmd | Active | x | y | w | h |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 0 | NoteDash | `NoteDash` | `python` | 1 | 0 | 0 | 106 | 56 |
| 1 | Henry | `Tides Mesh · Henry` | `pi` | 0 | 107 | 0 | 106 | 28 |
| 2 | shell/context | `tide-net` | `bash` | 0 | 107 | 29 | 106 | 27 |

Plot:

```text
┌──────────────────────────────┬──────────────────────────────┐
│ NoteDash 106x56              │ Henry 106x28                 │
│                              ├──────────────────────────────┤
│                              │ shell/context 106x27         │
└──────────────────────────────┴──────────────────────────────┘
```

OceanTUI lesson: writer/research views can be a 50/50 split with right-side stacked assistant/context panes.

### `tidemesh:5` Rev Review — `213x56`

| Pane | Role | Title | Cmd | Active | x | y | w | h |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 0 | Rev reviewer | `Tides Mesh · Rev` | `pi` | 1 | 0 | 0 | 100 | 56 |
| 1 | review board | `tide-net` | `python` | 0 | 101 | 0 | 112 | 20 |
| 2 | git UI | `tide-net` | `lazygit` | 0 | 101 | 21 | 56 | 35 |
| 3 | review context | `tide-net` | `python` | 0 | 158 | 21 | 55 | 35 |

Plot:

```text
┌────────────────────────────┬────────────────────────────────┐
│ Rev 100x56                 │ Review board 112x20            │
│                            ├───────────────┬────────────────┤
│                            │ lazygit 56x35 │ context 55x35  │
└────────────────────────────┴───────────────┴────────────────┘
```

OceanTUI lesson: review mode needs one dominant reviewer pane plus a top status strip and two lower evidence panes.

### `tidemesh:7` WorkOps — `213x56`

| Pane | Role | Title | Cmd | Active | x | y | w | h |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 0 | ops board | `tide-net` | `python` | 0 | 0 | 0 | 106 | 56 |
| 1 | ops shell | `tide-net` | `bash` | 1 | 107 | 0 | 106 | 56 |

Plot:

```text
┌──────────────────────────────┬──────────────────────────────┐
│ ops board 106x56             │ ops shell 106x56             │
└──────────────────────────────┴──────────────────────────────┘
```

OceanTUI lesson: operational mode can be a symmetric board/shell split.

### Single-pane windows

| Window | Pane role | Size |
| --- | --- | --- |
| `0:1 pi` | Pi shell | `240x66` |
| `0:2 pi` | Pi shell | `240x66` |
| `tidemesh:2 pi` | Pi shell | `213x56` |
| `tidemesh:6 TideDash` | board | `213x56` |
| `tidemesh:8 WorldMap` | map board | `213x56` |
| `tidemesh:9 test window` | scratch | `213x56` |

## OceanTUI implementation targets

### 1. Mesh floor view

Build this first from `tidemesh:4`:

```text
MeshFloor {
  glyph: Pane(top_left, role=ledger),
  knox: Pane(left_middle, role=review),
  charlotte: Pane(left_bottom, role=research),
  board: Pane(center_top, role=mesh_board_events),
  orchestrator: Pane(center_bottom, role=operator_control),
  brick: Pane(right_top, role=backend_runtime),
  pixel: Pane(right_bottom, role=frontend_tui),
}
```

Required behavior:

- Glyph is always first/top-left.
- Operator/Orchestrator remains center-bottom and visually dominant.
- Review and implementation panes show presence/status even when collapsed.
- Event stream never displaces Glyph.

### 2. Mode views

Use other windows as secondary blueprints:

- Writers Room mode from `tidemesh:3`.
- Review mode from `tidemesh:5`.
- WorkOps mode from `tidemesh:7`.
- Single-pane boards from `tidemesh:6` and `tidemesh:8`.

### 3. Measurement-driven tests

Add layout tests that assert the `213x56` baseline produces panes close to the tmux snapshot:

```text
Glyph:        x=0   y=0   w≈66  h≈14
KNOX:         x=0   y≈15  w≈66  h≈21
Charlotte:    x=0   y≈37  w≈66  h≈19
Board/events: x≈67  y=0   w≈102 h≈28
Orchestrator: x≈67  y≈29  w≈102 h≈27
BRICK:        x≈170 y=0   w≈43  h≈27
PIXEL:        x≈170 y≈28  w≈43  h≈28
```

Use tolerances of 1-2 cells because Ratatui and tmux border math differ.

## Raw tmux layout strings

```text
tidemesh:3 Writers Room
2e38,213x56,0,0{106x56,0,0,30,106x56,107,0[106x28,107,0,31,106x27,107,29,32]}

tidemesh:4 Tides-mesh Orchestrator
5012,213x56,0,0{66x56,0,0[66x14,0,0,76,66x21,0,15,37,66x19,0,37,38],102x56,67,0[102x28,67,0,34,102x27,67,29,75],43x56,170,0[43x27,170,0,39,43x28,170,28,40]}

tidemesh:5 Rev Review
1c71,213x56,0,0{100x56,0,0,45,112x56,101,0[112x20,101,0,46,112x35,101,21{56x35,101,21,47,55x35,158,21,72}]}

tidemesh:7 WorkOps
a6f1,213x56,0,0{106x56,0,0,43,106x56,107,0,44}
```

## Next build handoff

Route OceanTUI implementation to PIXEL when the operator asks to build from this blueprint. KNOX should review for:

- coordinate parity at `213x56`
- Glyph fixed top-left
- no event-stream displacement
- short-terminal collapse behavior
- no shared protocol expansion unless explicitly needed
