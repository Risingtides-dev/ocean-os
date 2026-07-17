# Ocean Observatory — Surface Reducer Contract

**Date:** 2026-07-17
**Status:** Spec (Gate 1 checkpoint)
**Scope:** Pure deterministic client-side reducer for the Ocean Surface Observatory module
**Governing Spec:** `2026-07-17-ocean-observatory-architecture.md` (sections 10–11, 13 Gate 2)
**Wire Contract:** `2026-07-17-observatory-gate1-implementation-manifest.md` (sections 1, 2, 7)

---

## 1. Scope

This document defines the pure deterministic reducer that the Ocean Surface Observatory UI
consumes. Every client — Browser/PWA, Tauri, extension, and future mobile — uses the same
reducer module from `ocean-surface-ui/src/observatory/`. No shell gains runtime authority.

The reducer is a synchronous, pure, deterministic fold over events. It produces the same
semantic state at a given cursor regardless of whether events arrived via live SSE streaming
or via batch replay from durable storage. The only source of truth for the UI is the reducer's
output state.

---

## 2. Module Structure

The Observatory module lives in `src/observatory/` within ocean-surface-ui:

```text
src/observatory/
  mod.rs        mode boundary and stage integration
  domain.rs     normalized IDs, events, state, filters
  reducer.rs    pure deterministic fold and bounded eviction
  adapter.rs    snapshot/live/replay transport adapter
  layout.rs     stable topology layout and semantic zoom
  scene.rs      retained visual renderer and lifecycle
  inspector.rs  semantic DOM detail, trace, and actions
```

### 2.1 Module Responsibilities

| Module | Role | Stateful? | Owner |
|--------|------|-----------|-------|
| `mod.rs` | Mode boundary, stage registration, mount/unmount lifecycle | No | Surface |
| `domain.rs` | Immutable value types: `ObservatoryEvent`, `ExecutionState`, `TopologyGraph`, `AttentionShelf`, `FilterPredicates` | No | Reducer |
| `reducer.rs` | Pure function `(ObservatoryState, ObservatoryEvent) -> ObservatoryState` + bounded eviction | No | Reducer |
| `adapter.rs` | Transport adapter: snapshot fetch, SSE stream management, replay cursor tracking | Yes | Adapter |
| `layout.rs` | Deterministic topology layout from reducer state → positioned nodes | No | Renderer |
| `scene.rs` | Retained canvas/WebGL renderer, animation loop, pixel output | Yes | Renderer |
| `inspector.rs` | Semantic DOM: detail panel, trace view, action controls, transcript links | Yes | Renderer |

### 2.2 Mount Lifecycle

- Mounted via the existing command registry as a primary stage (header overflow).
- `app.rs` integration is minimal — the module self-registers and self-manages.
- Mounting creates one reducer instance, one adapter, one SSE connection, one renderer loop.
- Unmounting tears down all state: cancels SSE, clears pending frames, drops retained state.
- Multiple mount/unmount cycles (up to 100) must leave zero leaks: no streams, RAFs,
  timers, observers, workers, or renderer resources.

---

## 3. Reducer Contract

### 3.1 Determinism Guarantee

```
reducer(state, event) ≡ reducer(state', event)  — same output for same input sequence
replay(cursor_N) ≡ fold(live_events[0..N])      — same state at cursor from either path
```

**Proof sketch:** The reducer is a pure function with no side effects, no I/O, no random
number generation, no clock access, and no mutable globals. Given the same initial state
and the same ordered sequence of events, the output is identical regardless of delivery
path (SSE live or replay).

### 3.2 State Scope

The reducer state is scoped by a 3-tuple:

```
(observatory_id: String, daemon_instance_id: String, cursor: Cursor)
```

- **`observatory_id`** — stable daemon instance identity, set once at daemon startup.
  Changes only when the operator resets the daemon permanently.
- **`daemon_instance_id`** — changes on every daemon restart.
- **`cursor`** — last processed event cursor. Monotonic, daemon-allocated.

If any of these three values differ between the reducer's current scope and an incoming
event, the reducer must either:
- **Reject** the event (if `observatory_id` or `daemon_instance_id` mismatch — stale generation).
- **Reset** state and request a fresh snapshot (on `StreamReset` or explicit `daemon_instance_id`
  change).

### 3.3 Duplicate Rejection

Events are identified by `(event_id, cursor)`. If an event arrives with an `event_id` that
already exists in the reducer's state, it is silently ignored (idempotent replay). Cursor
monotonicity is verified but gaps are handled separately (section 3.6): a subsequent event
with a lower cursor than the last processed cursor is rejected as stale.

**Idempotency guarantee:** Applying the same event twice produces identical state.

### 3.4 Stale Generation Rejection

An event whose `observatory_id` or `daemon_instance_id` does not match the reducer's current
scope is rejected and logged. This prevents:
- Replaying events from a previous daemon instance into current live state.
- Mixing events from different daemons into a single view.

On `StreamReset` (section 7.2 of Gate 1 Manifest), the reducer clears all state and enters
a `stale` phase awaiting a fresh snapshot. During `stale` phase, the UI shows a degraded
state (section 7).

### 3.5 Independent Channels

The reducer maintains three independent channels that evolve in parallel:

| Channel | Tracks | Events that affect it | Never mixed with |
|---------|--------|-----------------------|------------------|
| **Lifecycle** | Execution nodes, edges, phases, topology | `DaemonStarted`, `DaemonStopping`, `ExecutionAdmitted`, `ExecutionPhaseChanged`, `ExecutionFinished` | Tool/permission activity, latency metrics |
| **Activity** | In-flight tool calls, pending permissions, model reroutes | `ToolStarted`, `ToolFinished`, `PermissionWaiting`, `PermissionResolved`, `ModelRerouted` | Execution topology, attention |
| **Attention** | Items requiring operator notice | See §4 (Attention Shelf) | Lifecycle phase changes, activity completion |

**Invariant:** An action in one channel never produces a side effect in another.
Channel updates are strictly additive to their respective collections.

### 3.6 Gap Handling

When the reducer encounters a gap (cursor jump without an intervening `StreamGap` event,
or an explicit `StreamGap` event):

1. **Freeze lifecycle state** at the cursor just before the gap.
2. **Mark activity as incomplete** — all in-flight tool calls and pending permissions at the
   gap boundary are labeled `interrupted`.
3. **Flag attention items** that were active at the gap as `interrupted`.
4. **Enter `gap` phase** — the reducer continues accepting events after the gap (if they
   arrive), but the state is labeled as potentially incomplete.
5. **Request fresh snapshot** — the adapter must fetch a new snapshot to reconcile.
6. On snapshot receipt, the reducer resets to snapshot state and resets to normal phase.

The UI shows a banner during `gap` phase: "Observatory data may be incomplete due to a
connection interruption. Requesting fresh snapshot..."

When an explicit `StreamGap` event is received with `from_cursor` and `to_cursor`, the
reducer records the gap range in state for auditing. Events in the gap range are not
expected to arrive.

### 3.7 Deterministic Caps

The reducer enforces two resource limits:

| Cap | Default | Behavior at limit |
|-----|---------|-------------------|
| **Visible** | 500 executions | Executions beyond the visible cap are aggregated: `aggregate(N+more)` in the semantic list. Detail view is unavailable for aggregated executions. |
| **Tracked** | 2000 executions | Executions beyond the tracked cap are evicted from the reducer state entirely (LRU by last_activity_at). The cap MUST be enforced deterministically — two reducers processing the same events in different order must evict the same set of executions. |

**Deterministic eviction proof:** Eviction uses `last_activity_at` (cursor-ordered, not
wall-clock-ordered). Since cursor is monotonic and events are well-ordered, the LRU order
is deterministic. When `tracked > 2000`:
1. Sort active executions by `last_activity_at` (ascending).
2. Remove the oldest nonterminal executions beyond the cap.
3. If all nonterminal executions fit, remove the oldest terminal executions beyond the cap.
   Terminal executions are evicted strictly after nonterminal ones.
4. Emit an eviction event for audit (not forwarded to SSE).

### 3.8 Fresh Snapshot Request

The reducer requests a fresh snapshot from the adapter when:

| Condition | Mechanism |
|-----------|-----------|
| Initial mount | Adapter fetches snapshot on `init()` |
| `StreamReset` event | Reducer clears state → `stale` phase → adapter fetches |
| Overflow (cap exceeded) | Reducer evicts + adapter fetches to reconcile |
| Gap detected | Reducer freezes → adapter fetches for reconciliation |
| Manual refresh | Operator triggers refresh via UI control |

While a snapshot is being fetched, the reducer continues accepting live events in a
buffer. When the snapshot arrives, the reducer reconciles: it replays any buffered events
that are newer than the snapshot's watermark cursor.

**Reconciliation sequence:**
1. Adapter fetches snapshot at cursor `C_snap`.
2. Adapter returns any buffered events with cursor > `C_snap`.
3. Reducer resets to snapshot state, then folds buffered events in order.
4. UI transitions to normal phase with consistent state.

### 3.9 Replay Equivalence

The reducer guarantees that folding the same event sequence from any source produces
identical state. This is tested via fixture files (section 9) that encode both the event
sequence and the expected state at specific cursor positions.

```
Given fixture F with events E[0..N] and expected states S[c1..ck]:
  reducer(INIT, E[0..c1]) ≣ S[c1]
  reducer(INIT, E[0..c2]) ≣ S[c2]
  ...
```

---

## 4. Attention Shelf

The attention shelf is the highest-visibility area in the Observatory UI. It shows
items requiring operator notice, ordered by priority and recency.

### 4.1 Priority Levels

| Priority | Code | Color | Example |
|----------|------|-------|---------|
| Critical | `critical` | Red | Permission blocked, error, timeout |
| High | `high` | Orange | Permission waiting, extended runtime |
| Medium | `medium` | Yellow | Model reroute, topology rejection |
| Low | `low` | Blue | Execution finished, tool completed |
| Info | `info` | Neutral (gray) | Execution admitted, daemon started |

### 4.2 Attention Sources

Items appear in the attention shelf from:

| Source | Priority | Trigger |
|--------|----------|---------|
| Permission wait | High → Critical (by duration) | `PermissionWaiting` event |
| Permission denied | Critical | `PermissionResolved { outcome: denied }` |
| Execution error | Critical | `ExecutionPhaseChanged { to_phase: Error }` |
| Execution timeout | Critical | `ExecutionFinished { error_classification: timeout }` |
| Execution finished | Low | `ExecutionFinished { phase: Finished }` |
| Model reroute | Medium | `ModelRerouted` event |
| Topology rejection | Medium | `TopologyAttestationRejected` event |
| Daemon started | Info | `DaemonStarted` event |
| Gap interruption | High | Gap detection (section 3.6) |
| Stream reset | High | `StreamReset` event |

### 4.3 Resolution

An attention item is resolved (removed from the shelf) when:
- The operator explicitly dismisses it.
- The condition is resolved (e.g., permission approved → permission wait item removed).
- A fresh snapshot arrives that no longer contains the condition.

### 4.4 Shelf Capacity

The shelf shows up to 10 items. If more than 10 attention conditions exist, items with
lower priority are aggregated: `+N more`. Within the same priority level, the 10 most
recent items by `occurred_at` are shown first.

### 4.5 Empty Shelf State

When no attention items exist, the shelf displays a single row:
> "No items need attention. All executions are progressing normally."

---

## 5. Semantic List

The semantic list is the primary topology navigation view. It shows executions organized
by relationship and phase.

### 5.1 Default Organization

Executions in the semantic list are organized by:

1. **Root executions** first, ordered by `started_at` (newest first).
2. **Children** nested under their parent, indented, ordered by `started_at`.
3. Within each group, **active executions** (Running, Admitted) before terminal ones
   (Finished, Error, Canceled, TimedOut).
4. **Terminal executions** are grouped separately under a collapsible "Completed" section.

### 5.2 Phase Badges

Each execution row shows a phase badge:

| Phase | Badge | Color |
|-------|-------|-------|
| Admitted | `ADMITTED` | Blue outline |
| Running | `RUNNING` | Green pulse (animated) |
| Finished | `FINISHED` | Green solid |
| Error | `ERROR` | Red solid |
| Canceled | `CANCELED` | Gray solid |
| TimedOut | `TIMEOUT` | Orange solid |

### 5.3 Activity Indicators

On each running execution row, in-flight activity is shown as compact pill badges:

| Activity | Badge |
|----------|-------|
| Tool running | `🔧 tool_name` |
| Permission waiting | `⚠️ permission` |
| Model reroute | `🔄 model_name` |

### 5.4 Aggregated Row

When the visible cap (500) is exceeded, an aggregated row appears at the bottom:

```
+ N more executions — aggregate view only
```

Clicking the aggregated row expands a compact, non-interactive list of execution IDs
and phases.

### 5.5 Empty List State

When no executions exist (or all have been filtered out):
> "No executions to display. The daemon has started. Awaiting agent activity..."

---

## 6. Inspector

The inspector shows detailed information about a single selected execution.

### 6.1 Detail Sections

| Section | Content | Source |
|---------|---------|--------|
| **Identity** | Execution ID, root ID, parent ID, producer, session/turn/request IDs | `Topology` |
| **Timeline** | Phase transitions with timestamps, duration | Lifecycle events |
| **Tools** | Tool calls with name, duration, outcome, byte count | Activity events |
| **Permissions** | Permission requests with reason code, outcome, duration | Activity events |
| **Model** | Model alias, any reroute events | Activity events |
| **Topology** | Children list (expandable), parent link (navigable) | `SnapshotEdge` |

### 6.2 Empty Inspector State

When no execution is selected, or the selected execution has been evicted:
> "Select an execution from the list to inspect its details."

### 6.3 Aggregated Execution Inspector

Inspector for an execution that is part of the aggregated cap shows:
> "This execution is in the aggregate view. Full details are not available because the
> visible execution limit has been reached. Consider filtering to narrow the set."

---

## 7. Empty, Degraded, and Error States

### 7.1 Empty State (Daemon Running, No Activity)

Displayed when the daemon is running but no executions exist:
- Attention shelf: hidden (or "No items need attention").
- Semantic list: "No executions to display. The daemon has started. Awaiting agent activity..."
- Inspector: hidden or "Select an execution to inspect."
- Replay controls: disabled.
- A "Check daemon health" link is available.

### 7.2 Degraded State (Gap / Stale / Interrupted)

Displayed when the reducer is in `gap` or `stale` phase:
- Banner across the top: "Observatory data may be incomplete."
- Attention shelf: frozen at last known state, labeled with a caution icon.
- Semantic list: frozen with a "Data may be stale" overlay.
- Inspector: disabled if the selected execution's data is in the gap range.
- A "Request fresh snapshot" button is prominent.
- Replay is disabled.

### 7.3 Error State (Auth Failure / Unreachable)

Displayed when the adapter cannot reach the Observatory API:
- Full-view error card: "Unable to connect to Ocean Observatory."
- Error details: HTTP status, error code, actionable message.
- "Retry" and "Check daemon status" buttons.
- No attention shelf, no semantic list — the connection is a precondition.

### 7.4 Unsupported State

Displayed when the daemon does not support Observatory (missing routes):
> "Observatory is not available in this daemon version. Upgrade to ocean-daemon 0.x+ or
> enable the observatory feature."

---

## 8. Compact and Coarse-Pointer Mode

### 8.1 Design Principle

Compact mode is DOM-first — it is not a shrunken desktop map. It provides the same
information content with reduced visual density, suitable for small viewports, coarse
pointer input, and reduced cognitive load.

### 8.2 Layout Changes

| Element | Compact Behavior |
|---------|------------------|
| Navigation | Stacked vertically, full-width rows |
| Attention shelf | Collapsed to 3 items maximum with "+N more" affordance |
| Semantic list | Single-column, compact row height (36px vs 48px) |
| Inspector | Full-width overlay panel (not side-by-side) |
| Replay controls | Pill-style, below content |
| Header | Minimal: daemon status dot + instance label only |

### 8.3 Interaction Changes

- All touch targets ≥ 44×44px (WCAG 2.5.8).
- Coarse pointer (touch) uses larger hit areas without visual expansion.
- No hover-dependent interactions.
- Swipe gestures for navigation (list → inspector → back).
- Pinch-to-zoom disabled (use semantic zoom controls instead).

### 8.4 Semantic Zoom

Compact mode provides a zoom control with three levels:
1. **Summary** — Executions grouped by phase count. One row per phase with count badge.
2. **List** — Full semantic list with phase badges and activity indicators (default for compact).
3. **Detail** — Expands selected execution inline (no separate inspector panel).

---

## 9. Accessibility Requirements

### 9.1 Keyboard Navigation

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Navigate between regions (attention shelf, semantic list, inspector, replay) |
| `↑` / `↓` | Navigate items within a region |
| `Enter` | Select/focus an item |
| `Escape` | Deselect / close inspector / close overlay |
| `Space` | Toggle play/pause replay |
| `Ctrl+←` / `Ctrl+→` | Replay scrub backward/forward |
| `F5` | Request fresh snapshot |
| `/` | Focus filter/search input |

### 9.2 Screen Reader Requirements

- All dynamic content uses `aria-live="polite"` regions with limited announcements.
  No more than one announcement per second.
- Canvas element is `aria-hidden="true"` with `role="presentation"`.
- A complete DOM-based alternative is always present alongside the canvas.
- Execution rows use `role="treeitem"` with `aria-expanded` for children.
- Attention items use `role="alert"` when they first appear.
- Phase badges use `aria-label="Phase: Running"` (not just color).
- Aggregated items use `aria-label="N more executions. Select to expand."`.
- Timeline, tool, and permission sections in the inspector use `role="list"`.

### 9.3 Reduced Motion

When the user's system has `prefers-reduced-motion: reduce` or the Observatory
reduced-motion setting is enabled:

- No continuous animation: no pulsing phase badges, no flowing edge indicators,
  no ambient canvas motion.
- Single-frame state transitions only: materialize, color change, position change
  happen in one frame with no intermediate states.
- Canvas uses static render (no animation loop ticking).
- Semantic list phase badges use solid colors (no glow, no pulse).
- The "Running" phase is indicated by a static green badge with text label, not animation.
- Replay scrub updates on frame boundary, not continuously.

**Invariant:** Reduced motion removes no information. Phase, attention, activity,
and topology are all equally visible without animation.

### 9.4 Forced Colors

When `forced-colors: active` (Windows High Contrast mode):

- All information is conveyed through text labels and borders, not color alone.
- Phase badges use text labels, not color swatches.
- Attention items use icon + text, not color tint.
- Edge lines use `CanvasText` color with 1.5px minimum.
- Selection uses `Highlight` / `HighlightText` system colors.
- Canvas element shows a DOM-based fallback table.
- No reliance on color gradients or opacity for meaning.

---

## 10. Interaction Grammar

Per the architecture spec §11, the following motion/interaction rules govern the UI.
This section is a direct operational translation of §11 for the reducer contract.

### 10.1 Truthful Motions

| Event | Motion | Animation |
|-------|--------|-----------|
| Execution admitted | One bounded materialization | Node appears with brief scale-up (300ms, ease-out) |
| Thinking observed | Low-amplitude state glow | Subtle luminance pulse on node border (continuous while thinking) |
| Tool running | One activity port per in-flight call | Compact pill badge appears per tool call |
| Output delta | Coalesced luminance/material update | Brief brightness bump on node, then settle |
| Permission wait | Warning halo + attention row enters | Orange ring appears on node; attention row slides in |
| Successful finish | One transition to static completed state | Green badge materializes, node opacity settles |
| Error | One transition to static failure state | Red badge materializes, node shifts to error section |
| Parent/child confirmed | One directed flow packet | Subtle edge line draws from parent border to child border |
| Gap | Freeze all operational motion + label stale | All animation stops; stale overlay appears |

### 10.2 Never Animated

- Pacing, typing avatars, coffee breaks, or ambient activity without a recorded event.
- Smoke, ambient creatures, weather, or decorative motion.
- Fake progress indicators.
- Topology edges without a recorded parent/child relationship.

### 10.3 Interaction Conventions

| Control | Behavior |
|---------|----------|
| Select execution | Semantic list row highlights; inspector updates; scene pans to center node |
| Expand/collapse children | Tree arrow rotates; children appear/disappear with fade (150ms) |
| Replay play/pause | Toggle button; play starts replay from cursor; pause stops at frame |
| Replay scrub | Drag on timeline; updates reducer state to scrubbed cursor |
| Dismiss attention item | Fade out (200ms); shelf reflows remaining items |
| Request fresh snapshot | Button triggers adapter fetch; reducer reconciles |

---

## 11. Mount via Command Registry

The Observatory module is mounted via the existing command registry:

```typescript
// Registration (conceptual — ocean-surface command registry)
commands.register({
  id: "observatory.open",
  label: "Observatory",
  shortcut: "Cmd+Shift+O",
  stage: "primary",
  action: () => mountObservatory(),
  
  // Conditional availability
  available: async () => {
    const health = await daemonHealth();
    return health.capabilities.includes("observatory");
  },
  
  // Icon and overflow
  icon: "telescope",
  overflowGroup: "monitoring",
});
```

Mount state is managed in `mod.rs`. The stage lifecycle:

1. `init()` — Command registry registration, capability detection.
2. `mount()` — Create reducer instance, create adapter, connect SSE, start render loop.
3. `unmount()` — Teardown: close SSE, cancel pending, drop state, release resources.
4. `reconfigure()` — Handle resize, theme change, accessibility preference change
   (reduced-motion, forced-colors, font-size).

---

## 12. Validation and Test Fixtures

### 12.1 Fixture Property Tests

The following properties are verified by the shared test fixtures (section 9 of this spec):

1. **Replay equivalence:** Applying fixture events in bulk produces the same state as
   applying them one-at-a-time through live fold.
2. **Idempotency:** Applying the same event twice produces identical state.
3. **Stale rejection:** Events with mismatched `observatory_id` or `daemon_instance_id`
   are rejected.
4. **Gap freeze:** After a gap, lifecycle state matches the pre-gap snapshot.
5. **Cap enforcement:** With >2000 events, exactly 2000 tracked executions remain after
   eviction. With >500 visible executions, exactly 500 are visible and the rest aggregated.
6. **Clean unmount:** After unmount, no timers, streams, or state remain.

### 12.2 Fixture Files

See `crates/ocean-observatory/tests/fixtures/` for the shared fixture files:

| File | Description |
|------|-------------|
| `daemon_lifecycle.json` | Daemon start, root execution, tool use, finish |
| `execution_lifecycle.json` | Multiple execution admits, phase changes, errors |
| `topology_tree.json` | Parent/child topology with depth, admission then binding |
| `restart_interruption.json` | Daemon restart, previous executions marked interrupted |
| `gap_and_resume.json` | Stream gap, freeze, snapshot reconciliation |

Each fixture contains:
- `events`: ordered array of Observatory events
- `expected_state_at_cursor_N`: expected reducer state at cursor N (for each N in the fixture)
- `description`: human-readable purpose
- `schema_version`: 1

---

## 13. References

- `docs/specs/2026-07-17-ocean-observatory-architecture.md` (sections 10–11, 13 Gate 2)
- `docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md` (sections 1–2, 7)
- `docs/specs/2026-07-17-observatory-gate0-decisions.md` (decisions 1, 3, 4, 5, 8)
