# Ocean Floor Renderer Implementation Spec — Isometric Pixel-Art Scene

**Status:** Accepted for implementation in `ocean-surface` (renderer code lives there; this document is its contract).
**Authority:** Operator ruling R1 (`2026-07-17-observatory-gate0-decisions.md`): the 90s-CPU-game isometric pixel-art Ocean Floor is the flagship deliverable. The Claude Design `Agent Floor` export is concept evidence only — **no code, asset, layout, or event model may be derived from it** (Gate 0 decision 1).
**Upstream contracts:**
- `2026-07-17-ocean-observatory-architecture.md` §10 (Surface architecture), §11 (interaction/motion grammar), §13 (Gate 3 performance targets).
- `2026-07-17-observatory-surface-reducer-contract.md` (the reducer — the renderer's **only** data source).
- `2026-07-17-observatory-gate1-implementation-manifest.md` §7 (API wire shapes the reducer consumes).

This spec is written so an `ocean-surface` worker can implement without further design decisions. Where a choice is left open, it is enumerated with a default.

---

## 1. Visual Direction

### 1.1 North Star

A 90s CPU-era isometric pixel-art scene — think *SimCity 2000* / *Transport Tycoon* readability, *Little Computer People* charm — rendered with modern discipline: limited palette, integer-pixel grid, no gradients on sprites, no anti-aliased edges in artwork, deterministic animation keyed to recorded facts only.

The scene is a **bathymetric containment hierarchy**, three levels deep:

| Level | Metaphor | Contains | Maps to (reducer state) |
|-------|----------|----------|-------------------------|
| 1 | **Project shelf** — a raised rock/reef formation | workspace basins | project grouping (reducer §3.2 scope) |
| 2 | **Workspace basin** — a sandy/floored depression in a shelf | execution stations | session/workspace grouping |
| 3 | **Execution station** — a machine/console with a sprite operator | one execution | one execution node |

Containment is spatial and unambiguous: a station sits *inside* a basin, a basin sits *on* a shelf. No floating nodes. Aggregated executions (reducer §5.4) render as a **stacked station cluster** with a count badge, never as individual overlapping sprites.

### 1.2 Readability rules

1. At 100% zoom, an operator must identify phase, attention state, and parent/child relationship of any visible execution in under 2 seconds.
2. Phase is communicated by **three redundant channels**: sprite pose/frame, station status lamp color, and the DOM badge (reducer §5.2). Never color alone (§9.4 forced-colors).
3. The scene reads as a *machine floor*, not a chat UI: no text rendered inside the canvas except count badges ≥ 2 (as part of the atlas). All labels live in the DOM.
4. The viewport defaults to fit-content: on first mount with ≤ 40 stations, camera frames all shelves; above that, camera frames the attention shelf's region first (§8).

---

## 2. Canvas Architecture

### 2.1 Single canvas, DOM-owns-semantics

```html
<div class="ocean-floor" data-ocean-floor>
  <canvas aria-hidden="true" tabindex="-1"></canvas>
  <!-- DOM layer: all semantics, controls, and interaction targets -->
  <div class="ocean-floor-dom">…</div>
</div>
```

- Exactly **one** `<canvas>` element per Observatory mount, `aria-hidden="true"`, focusable never.
- The **DOM layer owns**: all controls (zoom, pan reset, replay rail), all labels (project/workspace/execution names as safe labels), the full hierarchy (nested lists mirroring reducer §5), selection state, focus management, transcript links, and the semantic list alternative (reducer §5 — always available, toggleable to replace the canvas view entirely).
- Every station has a **DOM proxy element** (absolutely positioned over its canvas anchor, transparent, `role="button"`, labelled) so keyboard focus, selection, and activation never depend on canvas hit-testing. Pointer input may hit-test the canvas as a shortcut, but focus/selection/activation must round-trip through the DOM proxy.
- DOM proxy positions are updated only on layout commits (§5.4), never per-frame.

### 2.2 Layer stack (paint order, bottom to top)

1. **Water backdrop** — flat palette color + subtle static dither pattern (baked in atlas, no runtime animation).
2. **Shelves** (project formations).
3. **Basins** (workspace floors).
4. **Tethers** (parent/child flow paths — static routing, animated packets over them, §7.3).
5. **Stations + sprites** (sorted by isometric depth, §4.3).
6. **Attention halos** (§8) — above the station they ring, below DOM proxies.
7. **DOM overlay** (proxies, labels, shelf UI, replay rail).

### 2.3 Context loss and resize

- On `webglcontextlost`/`2d context reset`, the renderer must rebuild from reducer state alone (no canvas-derived state is authoritative).
- Resize via `ResizeObserver` on the container: recompute viewport transform, repaint once. No per-resize-frame work.

---

## 3. Sprite State Machine

One sprite per visible execution. The state machine is driven **exclusively by reducer state transitions** (reducer §3: monotonic cursor, duplicate rejection, gap handling). Motion fires **only on a recorded fact change** (architecture §11) — never on a timer, never ambient.

### 3.1 States

| # | State | Visual (100% zoom) | Entered on (reducer fact) | Exit |
|---|-------|--------------------|---------------------------|------|
| 1 | `idle` | sprite seated at console, status lamp off; static frame | `ExecutionAdmitted{Admitted}` | any phase change |
| 2 | `thinking_glow` | sprite leaning in; low-amplitude 2-frame lamp pulse (amber, 600 ms period, amplitude ≤ 25% brightness) | `ExecutionPhaseChanged{→Running}` | phase change |
| 3 | `tool_running_port` | one **activity port** (side pod) per in-flight tool call, spinning 4-frame loop (500 ms); sprite turns toward newest port | `ToolStarted` (one port per `tool_call_id`) | `ToolFinished` removes that port |
| 4 | `generating_luminance` | station screen luminance steps up one level (coalesced: max 1 level per reducer batch) | output-bearing batch while running (§3.3) | phase change |
| 5 | `permission_wait_halo` | sprite raises hand; **warning halo** (red/yellow 2-frame alternation, 800 ms) rings the station; mirrored on attention shelf (§8) | `PermissionWaiting` fact | `PermissionResolved` |
| 6 | `completed_static` | sprite stamps a crate/seal; lamp solid green; **static** thereafter | `ExecutionFinished{Finished}` | — (terminal) |
| 7 | `error_static` | station lamp solid red; one-frame "burst" on entry (150 ms); sprite slumped; **static** thereafter | `ExecutionFinished{Error\|TimedOut}` | — (terminal) |
| 8 | `interrupted_faded` | sprite + station at 50% palette brightness, dashed tether; **static** | `ExecutionPhaseChanged{→Canceled}` (incl. restart sweep) | — (terminal) |

### 3.2 Transition legality

- Terminal states (6, 7, 8) never exit. A late fact for a terminal execution updates the DOM badge only, not the sprite.
- `tool_running_port` composes with `thinking_glow` (ports are additive pods, not a separate top-level state).
- Every visual transition completes in ≤ 400 ms except the permission halo (continuous while waiting) and tool ports (continuous while in-flight). Continuous animations are **state displays**, not events; they freeze on gap (§3.4) and halt on reduced motion (§9).
- Unknown/unmapped reducer facts must not move any sprite. Log at debug.

### 3.3 Coalescing

Reducer batches (≤ 10/s per Gate 3) may carry many facts for one execution. The renderer applies **net state** per batch: e.g. three `ToolStarted` + two `ToolFinished` in one batch = +1 net port. Luminance (state 4) steps at most one level per batch regardless of delta count.

### 3.4 Gap behavior

On a reducer gap (`stream.gap`, stale generation, or replay reset — reducer §3.6/§3.8):

1. **Freeze all operational motion immediately** (ports stop mid-frame, glow halts).
2. Mark affected regions with the incomplete-state overlay (dithered veil + DOM "state incomplete" badge, reducer §7.2).
3. On the resynced snapshot, reconcile every station to its reducer state with a single 200 ms cross-fade, then resume fact-driven motion.

---

## 4. Isometric Grid Math

### 4.1 Tile system

- Base tile: **64 × 32 px** diamond (2:1 isometric). All station footprints are whole tiles (station = 2×2 tiles, basin = variable, shelf = variable).
- World coordinates are integer tile coordinates `(gx, gy, gz)` with `gz` the elevation (shelf tops are `gz=1`, basins `gz=0`, stations sit on basin floor `gz=0` or shelf top).
- Sprites are drawn at 1× scale; zoom is a viewport transform (§4.4), never asset rescaling (pixel art must stay integer-crisp; fractional zoom snaps to the nearest 25%).

### 4.2 Grid → screen

```
screen_x = (gx - gy) * (TILE_W / 2) + origin_x
screen_y = (gx + gy) * (TILE_H / 2) - gz * ELEVATION_H + origin_y
```

with `TILE_W = 64`, `TILE_H = 32`, `ELEVATION_H = 16`. `origin_*` is the camera translation in screen pixels.

Screen → grid (pointer hit-test):

```
gx = floor( ((sx - origin_x) / (TILE_W/2) + (sy - origin_y) / (TILE_H/2)) / 2 )
gy = floor( ((sy - origin_y) / (TILE_H/2) - (sx - origin_x) / (TILE_W/2)) / 2 )
```

### 4.3 Depth sort

Painters' algorithm on `(gx + gy, gz, insertion_index)` ascending per tile layer. Sort only dirty regions (§5.2); full sort only on layout commit.

### 4.4 Camera and zoom

- Camera = `{ origin_x, origin_y, zoom }`; zoom ∈ {50%, 75%, 100%, 150%, 200%} (integer-snap). Default 100%.
- Pan: pointer drag on canvas background or arrow-key pan when the DOM region has focus (DOM proxy focus never moves the camera).
- Zoom: buttons + `Ctrl/Cmd + wheel`, centered on pointer anchor. Zoom recomputes `origin` so the anchor tile stays under the pointer.
- Viewport culling: stations whose 2×2 footprint bounds fall outside `viewport_rect ± 1 tile` are skipped in draw (but not in state).

---

## 5. Rendering Pipeline

### 5.1 One loop

Exactly **one** `requestAnimationFrame` loop per mount (architecture §13: one renderer, one reducer, one stream, one animation loop). The loop is *event-driven*: it runs only when there is pending work (dirty regions, in-flight transitions, active continuous states) and **stops** (cancels the RAF) when the frame diff is empty.

### 5.2 Dirty regions

- Reducer batch → compute affected world rects (station footprints, tether segments, halo bounds) → merge into a dirty-rect list (merge overlapping, cap 64; beyond that, full repaint).
- Continuous states (ports, glow, halo) dirty only their own bounds per animation tick.
- Nothing dirty → no draw calls that frame.

### 5.3 Sprite atlas and batching

- All artwork ships in **one atlas PNG** (§10) + one JSON manifest (frame rects, pivots, per-state frame lists, tint table).
- Draw calls batch by atlas region + tint; target ≤ 32 draw calls/frame at 500 visible stations.
- Tinting (state brightness, interrupted fade, incomplete veil) is applied via the renderer's cheapest channel (2D: `globalAlpha` + pre-tinted atlas variants; WebGL: vertex tint). Choose per Gate 3 benchmark (§12).

### 5.4 Layout commits

Topology changes (new/removed shelf/basin/station, aggregation transitions) are **layout commits**: recompute tile assignments (§7.1), DOM proxy positions, depth sort, and repaint once. Never animate layout; the materialization animation (§11) plays *after* the commit at the committed position.

### 5.5 Zero-work guarantees

The renderer must do **zero** continuous work when any of: document hidden (`visibilitychange`), Observatory paused, reducer idle with no active continuous states, or `prefers-reduced-motion` (§9). "Zero" = RAF cancelled, no timers, no animation frames. Verified by the §12 soak tests.

---

## 6. Replay Rail Integration

- The replay rail (DOM control: scrub slider, play/pause, speed 0.25×/0.5×/1×/2×/4×) is driven **only by the reducer's replay cursor** (reducer §3.9 replay equivalence): scrubbing sets a target cursor; the reducer re-derives state; the renderer reconciles as in §3.4 (one cross-fade, then static at the target state).
- During replay, fact-driven motion plays **only for facts inside the replay window** as the cursor passes them (same animation table, §11), at the selected speed. Continuous states never run ahead of the cursor.
- The scene must be **seek-safe**: scrubbing backward then forward across the same cursor range produces pixel-identical frames (deterministic animations — no wall-clock seeding; animation phase is a pure function of `(state_enter_cursor, current_cursor)`).
- Exiting replay restores live tail behavior (§3) with one reconcile cross-fade.

---

## 7. Room/Station Layout

### 7.1 Stable assignment

- Tile positions are assigned by a **stable placement function** of `(observatory_id, execution_id)`: hash-based slot selection within the owning basin's free-tile set, so an execution keeps its tile across reloads and resyncs. No physics, no force-directed drift.
- Basins are laid out on their shelf in scan order of workspace id; shelves in scan order of project id. New siblings append at the next free slot; the scene grows right-down.
- Relocation is forbidden except on aggregation transitions (§1.1) and basin overflow (§7.2); any relocation uses one 300 ms slide along the tether path.

### 7.2 Basin overflow

Basins have a soft capacity of 24 visible stations. Beyond that, the reducer's deterministic aggregation (reducer §3.7/§5.4) is authoritative: the renderer draws the aggregated cluster (stacked station + count badge) and never overflows tiles.

### 7.3 Parent/child tethers and flow packets

- Tethers are static orthographic paths (manhattan routing along tile edges, 2 px, palette `line.dim`) from parent station to child station, arrowless (direction is shown by packets, not glyphs).
- On a confirmed parent/child activity fact (admission with `parent_execution_id`, architecture §11), exactly **one directed flow packet** (4×4 px sprite, 300 ms, along the tether path, parent→child) travels the tether. No packets on a timer; no packets without a recorded relationship.
- A tether with an `interrupted_faded` endpoint renders dashed.

---

## 8. Attention Shelf Visual Prominence

Mirroring reducer §4 (priority levels) and architecture §11 (hierarchy position 1):

- The shelf is a **DOM region** pinned to the scene's top edge (collapsible, never overlapping station proxies).
- Each attention item shows: priority chip (shape + color: critical = diamond/red, high = triangle/amber, medium = square/yellow, low = circle/blue, info = pill/neutral), safe reason code, execution label, and a "locate" action that pans/zooms the camera to the station and flashes its DOM proxy outline once.
- The canvas mirrors attention at the station: `permission_wait_halo` (state 5) for waits; a static priority-colored underlay ring for error/stale while the item is on the shelf. Dismissed items (reducer §4.3) remove the ring within one frame.
- Empty shelf state renders collapsed with zero canvas cost (reducer §4.5).

---

## 9. Reduced Motion and Accessibility

### 9.1 prefers-reduced-motion

When the media query matches (or the OS control toggles — listen for changes live):

- All continuous animation **stops** (ports, glow, halo pulse, packets).
- Every state remains fully visible via **static frames + status lamp + DOM badge** (state table §3.1 column 2 still holds; animation columns drop).
- Transitions become instant frame swaps. The 150 ms error burst and 400 ms transitions are skipped.
- No information is removed (architecture §11: "removes continuous animation without removing information").

### 9.2 Forced colors / high contrast

With `forced-colors: active`, the canvas is replaced by the semantic list view (reducer §5) — canvas rendering cannot respect the system palette. The toggle remains available but is annotated "decorative scene unavailable in forced-colors mode".

### 9.3 Keyboard

All renderer interactions are reachable: DOM proxies in tab order (scan order), Enter/Space selects, L pans to selection, `+`/`-` zoom, 0 reset camera, R toggles replay rail (when replay is active, architecture §11 hierarchy). Focus ring drawn by the DOM (never on canvas).

---

## 10. Asset Requirements

### 10.1 Palette (16 colors, 90s CPU-game register)

| Slot | Hex | Use |
|------|-----|-----|
| `water.0` | `#1a3a4a` | backdrop base |
| `water.1` | `#24506a` | dither pattern |
| `rock.0` | `#5a4a3a` | shelf sides |
| `rock.1` | `#7a6550` | shelf tops |
| `sand.0` | `#c2a878` | basin floor |
| `sand.1` | `#a8906a` | basin rim |
| `metal.0` | `#4a5560` | station body |
| `metal.1` | `#8a99a8` | station trim |
| `screen.0` | `#0a1a2a` | screen off |
| `screen.1` | `#3ae0c0` | screen on (teal phosphor) |
| `lamp.green` | `#40c040` | completed |
| `lamp.red` | `#e04030` | error |
| `lamp.amber` | `#e0a030` | thinking / warning |
| `line.dim` | `#3a4a5a` | tethers |
| `packet` | `#f0e060` | flow packets |
| `sprite.skin` | `#e8c8a0` | operator sprites (with 3 outfit tints: `#c06040`, `#4080c0`, `#60a050`) |

Final values may shift ±1 step per channel during art direction, but the count stays ≤ 16 and names stay semantic.

### 10.2 Atlas contents (minimum frame sets)

- **Tiles:** water (1 + dither), rock side/top, sand floor/rim — 64×32 each.
- **Station:** body variants (idle, active, error, faded) — 128×64 (2×2 tiles + elevation).
- **Operator sprite:** 24×24, frames: seated, lean-in, reach-port, hand-raised, stamp, slump — 1–2 frames each.
- **Ports:** spinning pod 16×16 × 4 frames.
- **Halo:** 96×48 ring, 2 frames (red/yellow alternate).
- **Packet:** 4×4. **Badges:** digit glyphs 0–9 + "+" (8×8).
- **Veil:** 8×8 dither tile (incomplete overlay, tiled).

Budget: atlas ≤ 256 KiB PNG, one HTTP fetch, `image-rendering: pixelated`.

---

## 11. Truthful Motion Grammar (exhaustive mapping)

Every animation is keyed to a recorded fact. This table is exhaustive — anything not listed must not move.

| Recorded fact (reducer event) | Animation |
|-------------------------------|-----------|
| execution admitted | one bounded materialization: station rises 8 px + settles, 300 ms |
| thinking observed (running batch) | `thinking_glow` state entered; 2-frame lamp pulse while true |
| tool started | one activity port appears on the station (300 ms pop), spins while in-flight |
| tool finished | that port retracts (200 ms); outcome reflected in DOM badge |
| output-bearing batch while running | `generating_luminance` screen steps +1 level (max 1/batch) |
| permission waiting | `permission_wait_halo` enters; attention row appears on shelf |
| permission resolved | halo exits (200 ms); shelf item resolves |
| successful finish | one transition to `completed_static` (400 ms incl. stamp) |
| error finish | one 150 ms burst, then `error_static` |
| interrupted / canceled | one fade to `interrupted_faded` (300 ms) |
| confirmed parent/child activity | one directed flow packet along the tether (300 ms) |
| telemetry gap | freeze all operational motion; incomplete veil on affected regions |
| replay seek | reconcile cross-fade (200 ms) to cursor state |

**Never animate** (architecture §11, binding): pacing, typing avatars, coffee breaks, smoke, ambient creatures, fake progress, topology edges without a recorded relationship, idle bobbing, blinking, "breathing" UI, particle ambience, anything on a wall-clock timer unrelated to a fact.

---

## 12. Performance Targets (Gate 3, binding)

From architecture §13 Gate 3 — these are acceptance thresholds, measured on the §12.1 harness:

- **500 visible** and **2,000 tracked** executions, with deterministic aggregation above the visible cap (§7.2).
- ≤ **10 coalesced delta batches/s** per observer.
- **p95 reducer/layout ≤ 8 ms** per batch (measured in the ocean-surface layout pass).
- **p95 desktop frame ≤ 16.7 ms** at 500 visible stations with worst-case continuous states (50 spinning ports + 10 halos).
- **Zero continuous animation work** while hidden, idle, paused, or reduced-motion (§5.5): RAF cancelled, zero timers.
- **No monotonic memory growth** in a 60-minute churn soak (fact rate: 5 admissions/s, 20 tool events/s, 2 finishes/s).
- **100 mount/unmount cycles** leave zero streams, RAFs, timers, observers, workers, or renderer resources.
- Draw calls ≤ 32/frame at 500 visible stations (§5.3).

### 12.1 Benchmark harness

A headless fixture driver in ocean-surface feeds reducer fixtures (reducer §12) at the rates above and records frame timing, reducer batch timing, draw-call counts, heap snapshots, and resource leaks. Renderer selection (virtualized DOM/SVG vs Canvas 2D vs WebGL) is decided **by these measurements** (Gate 3: "choose from measurements, not ambition"), with Canvas 2D as the default starting point.

---

## 13. Data Contract (binding)

1. The renderer consumes **only** reducer state and reducer events (reducer contract §3). **No direct API calls, no store access, no daemon imports, no `EventEnvelope` handling.** Wire-format knowledge stops at the reducer.
2. Renderer inputs are exactly: (a) the reducer state tree (shelves → basins → stations with phase, safe labels, aggregation), (b) reducer batch events (fact list + cursor + generation), (c) reducer status (live/gap/stale/replay).
3. Renderer outputs are exactly: selection intents, camera state, replay-rail intents (target cursor, speed) — all dispatched as reducer/DOM commands, never side-channel mutations.
4. Forbidden fields never reach the renderer by construction (Gate 1 redaction), and the renderer must not reintroduce them: no prompt text, no tool args/output, no paths, no titles, no credentials — even if a future reducer bug exposes them, the renderer's types must not have fields for them.

---

## 14. Acceptance Checklist

An ocean-surface implementation of this spec is acceptable when **all** hold:

1. §3 state machine implemented with the exact eight states and transition legality; unknown facts move nothing.
2. §11 motion table exhaustive; the never-animate list is test-enforced (no animation without a fact key).
3. §2 DOM-owns-semantics: canvas `aria-hidden`, DOM proxies for every station, full keyboard operability, semantic list alternative present.
4. §5.5 zero-work verified (hidden/idle/paused/reduced-motion) by automated test.
5. §6 replay: seek-safe determinism proven (scrub backward/forward → pixel-identical frames) over reducer fixtures.
6. §12 performance harness meets every Gate 3 threshold, run in CI or a documented manual lane.
7. §9 reduced-motion and forced-colors behaviors verified.
8. §13 data contract enforced by the type system (renderer package has no API/store dependency).
9. 100 mount/unmount cycles leak nothing (§12).
10. Redaction: zero forbidden fields across the reducer property fixtures (Gate 3 final bullet).

## 15. Open Items (non-blocking, tracked)

- Sprite character designs beyond the base operator (per-producer variants) — art direction follow-up; base operator ships first.
- Sound design is **out of scope** for V1 (no audio channel exists in the reducer contract).
- Multi-observer presence cursors are out of scope (single-operator product).
