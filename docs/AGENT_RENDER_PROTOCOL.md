# Agent Render Protocol — dynamic UI from agent output

## Problem

Ocean is a coding agent, but coding is not the only thing users do with a long-running
agent. They ask for status boards, input forms, kanban trackers, inline tables, charts.
Today the agent can only emit text (markdown) and tool calls. Text-to-UI is a dead end:
the user reads, the agent waits, the turn ends. No interactivity.

We want the agent to be able to **render live, interactive UI components** into the client,
and have those components **call back into the agent** when the user interacts with them
(button click, form submit, card drag).

## Constraints

- The agent runs in `ocean-daemon`. It emits an SSE stream of `AgentEvent` to all
  connected clients. The protocol must be **event-driven** — no new connection types,
  no websocket upgrade, no polling.
- Clients (TUI, web surface, voice surface, CLI) each render components differently.
  The protocol describes **what** to render, not **how**.
- Components are **agent-session-scoped**. A kanban board for project X lives in the
  session that created it. Component state does not persist across daemon restarts
  (the agent can recreate it).
- The agent must remain stateless with respect to component state. If it needs to
  remember what cards are on the board, it stores that in its own message history
  (tool results, assistant text) or on disk.

## Protocol

### New event type: `AgentEvent::Render`

```rust
pub struct RenderEvent {
    /// Opaque component id, scoped to the session. The agent picks it,
    /// the client echoes it back on interactions.
    pub id: String,
    /// Component kind — one of the 17 built-in kinds (see "Built-in component
    /// kinds" below): kanban, form, table, progress, markdown, dashboard, chart,
    /// timeline, stat, file_tree, diff, code, callout, gallery, confirm, map, video.
    pub kind: String,
    /// Component props — a JSON object whose schema is defined per kind.
    pub props: serde_json::Value,
    /// If true, replace any existing component with the same id.
    /// If false, mount a new component (id must be unused).
    pub replace: bool,
}
```

This is emitted on the same `AgentEvent` enum as `TextDelta`, `ThinkingDelta`,
`ToolExecutionStart`, etc. Clients filter `Render` events and forward them to
their component registry.

### New event type: `AgentEvent::Unmount`

```rust
pub struct UnmountEvent {
    /// Remove the component with this id.
    pub id: String,
}
```

### Component interaction flow (user -> agent)

Clients POST to a new daemon route:

```
POST /v1/component/event
{
    "session_id": "uuid",
    "component_id": "kanban-1",
    "event": {
        "type": "card_clicked",
        "payload": { "card_id": "card-3" }
    }
}
```

The daemon:
1. Looks up the active session's agent loop (if still running) or queues the event
   for the next turn.
2. Injects a synthetic tool result or user message into the agent's message stream
   describing the interaction.
3. The agent processes it and may emit new `Render` events in response.

If the session has no active run, the daemon starts a new turn with the component
event as the prompt.

### Built-in component kinds

#### `kanban`

```json
{
    "id": "kanban-1",
    "kind": "kanban",
    "props": {
        "columns": [
            { "id": "todo", "title": "To Do" },
            { "id": "in-progress", "title": "In Progress" },
            { "id": "done", "title": "Done" }
        ],
        "cards": [
            { "id": "card-1", "column": "todo", "title": "Fix login bug", "description": "..." },
            { "id": "card-2", "column": "in-progress", "title": "Add tests", "description": "..." }
        ]
    }
}
```

Interactions: `card_clicked`, `card_moved` (column change).

#### `form`

```json
{
    "id": "form-1",
    "kind": "form",
    "props": {
        "title": "Report a bug",
        "fields": [
            { "name": "title", "label": "Title", "type": "text", "required": true },
            { "name": "severity", "label": "Severity", "type": "select",
              "options": ["low", "medium", "high", "critical"] },
            { "name": "description", "label": "Description", "type": "textarea" }
        ],
        "submit_label": "Submit"
    }
}
```

Interaction: `form_submit` with `{ "title": "...", "severity": "high", ... }`.

#### `table`

```json
{
    "id": "table-1",
    "kind": "table",
    "props": {
        "columns": ["Name", "Status", "Priority", "Assignee"],
        "rows": [
            ["Fix login", "open", "high", "alice"],
            ["Add tests", "in-progress", "medium", "bob"]
        ]
    }
}
```

Interactions: `row_clicked` with `{ "row_index": 0 }`.

#### `progress`

```json
{
    "id": "progress-1",
    "kind": "progress",
    "props": {
        "label": "Building release",
        "value": 0.6,
        "max": 1.0,
        "indeterminate": false
    }
}
```

No interactions — display only.

#### `markdown` (embedded)

```json
{
    "id": "msg-1",
    "kind": "markdown",
    "props": {
        "content": "## Heading\n\nParagraph with **bold** text."
    }
}
```

This is the default renderer for assistant text anyway, but explicit `markdown`
components let the agent place rendered blocks anywhere in a dashboard layout.

#### `chart`

```json
{ "id": "c1", "kind": "chart",
  "props": { "title": "Plays", "type": "bar",
             "series": [ { "label": "Mon", "value": 12 }, { "label": "Tue", "value": 30 } ] } }
```

`type` is `"bar" | "line"`; `value` is numeric. Display only.

#### `timeline`

```json
{ "id": "t1", "kind": "timeline",
  "props": { "steps": [ { "label": "Plan", "status": "done", "detail": "..." },
                        { "label": "Build", "status": "active" },
                        { "label": "Ship", "status": "pending" } ] } }
```

`status` is `"done" | "active" | "pending" | "error"`. Re-render with `replace:true`
to advance a step. Display only.

#### `stat`

```json
{ "id": "s1", "kind": "stat",
  "props": { "stats": [ { "label": "Views", "value": "1.2M", "delta": "+8%", "trend": "up" },
                        { "label": "Saves", "value": 4210, "trend": "flat" } ] } }
```

KPI cards. `trend` is `"up" | "down" | "flat"`; `value` is string or number. Display only.

#### `file_tree`

```json
{ "id": "ft1", "kind": "file_tree",
  "props": { "root": "src", "entries": [
      { "name": "main.rs", "type": "file", "path": "src/main.rs" },
      { "name": "lib", "type": "dir", "children": [ { "name": "mod.rs", "type": "file", "path": "src/lib/mod.rs" } ] } ] } }
```

Dirs nest via `children`. Files emit `file_clicked` with `{ "path": "..." }`.

#### `diff`

```json
{ "id": "d1", "kind": "diff",
  "props": { "filename": "main.rs",
             "lines": [ { "kind": "ctx", "text": "fn main() {" },
                        { "kind": "del", "text": "    old();" },
                        { "kind": "add", "text": "    new();" } ] } }
```

Line `kind` is `"add" | "del" | "ctx"`. Alternatively pass `{ "unified": "+new\n-old" }`.
Display only.

#### `code`

```json
{ "id": "code1", "kind": "code",
  "props": { "language": "rust", "filename": "main.rs", "code": "fn main() {}" } }
```

A copy-able code block. Display only.

#### `callout`

```json
{ "id": "cl1", "kind": "callout",
  "props": { "variant": "warn", "title": "Heads up", "body": "Body supports **markdown**." } }
```

`variant` is `"info" | "success" | "warn" | "error"`. Display only.

#### `gallery`

```json
{ "id": "g1", "kind": "gallery",
  "props": { "images": [ { "src": "https://...", "caption": "Cover" },
                         { "src": "data:image/png;base64,...", "caption": "Frame" } ] } }
```

`src` is a URL or `data:` URI. Display only.

#### `confirm`

```json
{ "id": "cf1", "kind": "confirm",
  "props": { "title": "Delete campaign?", "body": "This can't be undone.",
             "confirm_label": "Delete", "cancel_label": "Keep", "variant": "error" } }
```

Emits `confirm_response` with `{ "confirmed": true|false }`. Pair with `component_wait`
for a yes/no before a destructive action.

#### `map`

```json
{ "id": "m1", "kind": "map",
  "props": { "center": { "lat": 34.05, "lng": -118.24 }, "zoom": 9,
             "markers": [ { "lat": 34.05, "lng": -118.24, "title": "LA" } ],
             "fit_markers": true } }
```

A live Google Map (Places UI Kit). Modes: plain `markers`; a `place_id` →
place-details card; a `query` or `nearby` → place search list. `zoom` 1–20;
`fit_markers:true` auto-frames the markers. Emits `marker_clicked` / `place_selected`.
Requires the proxy to serve a Maps key via `/api/config` (`GOOGLE_MAPS_API_KEY`,
optional `GOOGLE_MAPS_MAP_ID` for custom styling).

#### `video`

```json
{ "id": "v1", "kind": "video",
  "props": { "url": "https://www.tiktok.com/@user/video/123", "title": "Clip", "autoplay": false } }
```

`url` is a TikTok / Instagram Reel / YouTube / Vimeo link, or a direct
`.mp4/.webm/.m3u8` file — the surface auto-selects the embed. `start` is a seconds
offset (YouTube/file). Display only.

## Agent interface

Exposed to the agent as a new tool:

```
tool: component_render
  args:
    id: string       # component id, agent-chosen
    kind: string     # kanban | form | table | progress | markdown | dashboard
                     # | chart | timeline | stat | file_tree | diff | code
                     # | callout | gallery | confirm | map | video
    props: object    # component-specific props (see "Built-in component kinds")
    replace: bool    # default false (true overwrites a component with the same id)

tool: component_unmount
  args:
    id: string

tool: component_wait
  args:
    id: string       # component to wait for
    timeout_ms: int  # max wait, default 60000
```

`component_wait` blocks the agent turn until the user interacts with the component.
The result includes the interaction event. This lets the agent do:

```
1. render kanban with current tasks
2. wait for user to click a card
3. render form to edit that card
4. wait for form submit
5. update data, re-render kanban
```

## Client implementation

### ocean-surface-ui (Leptos)

- `ComponentRegistry` — a `Vec<(String, RenderEvent)>` held in reactive state.
- On `Render` event: insert/replace entry.
- On `Unmount` event: remove entry.
- Each component kind maps to a Leptos component (`Kanban`, `Form`, `Table`, etc.)
  that receives `props: JsonValue` and an `on_event: Callback<ComponentInteraction>`.
- `on_event` POSTs to `/v1/component/event`.

### ocean-tui (ratatui)

- Same `ComponentRegistry` in app state.
- Renders components as inline widgets. `kanban` could be a multi-pane layout,
  `form` could be a modal overlay, `table` is a table widget.
- User interaction via keyboard shortcuts (tab between columns, enter to click,
  type into focused form field).
- `component_wait` pauses the agent loop and enters a "component interaction mode"
  in the TUI event loop.

### CLI

- Skip render events silently.
- `component_wait` doesn't block — return an error or noop.

## Layout

Components are positioned by the agent in a **dashboard layout**. The agent sends
a `dashboard` component that is a container:

```json
{
    "id": "dash-1",
    "kind": "dashboard",
    "props": {
        "children": [
            { "id": "kanban-1", "width": 2 },
            { "id": "progress-1", "width": 1 },
            { "id": "table-1", "width": 1 }
        ]
    }
}
```

Clients render a grid layout. Width is in fractional units (like CSS grid `fr`).
The agent can re-render the dashboard with different children at any time.

## Security

- Component events are scoped to the session. The daemon validates `session_id`
  on the `component/event` endpoint.
- The agent's permission policy gates `component_render` and `component_wait`
  like any other tool. `component_wait` could be persistently denied if the
  user doesn't want blocking interactions.
- Component props are JSON — no inline HTML, no script execution. The web surface
  renders them with Leptos, not `innerHTML`.

## Future

- **Live update** — the agent can push `Render` with `replace: true` to update
  in-place without a full re-render. Clients diff the props.
- **Streaming props** — for progress bars and live logs, the agent could emit
  `Render` events with partial updates (e.g. `{ "value": 0.7 }` merged into
  existing props).
- **Nested components** — a form field could itself be a component that the
  agent defines dynamically.
