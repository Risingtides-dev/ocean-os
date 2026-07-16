# Agent Render Protocol — dynamic UI from agent output

Status: implemented protocol with client-dependent rendering coverage. Typed render and unmount events, component tools, and the pending-interaction route are shipped; not every client renders every component kind.

## Purpose

Ocean can emit typed live UI components alongside markdown and tool activity. Capable clients project those events into their own interface and may return a bounded interaction when the running agent is explicitly waiting for one.

## Constraints

- The agent runs in `ocean-daemon`. It emits an SSE stream of `AgentEvent` to subscribers for the owning session; the explicit `?all=1` debug/global stream can observe all sessions. The protocol is event-driven — no new connection type, websocket upgrade, or polling.
- Clients (TUI, web surface, voice surface, CLI) each render components differently.
  The protocol describes **what** to render, not **how**.
- Components are **agent-session-scoped**. A kanban board for project X lives in the
  session that created it. Component state does not persist across daemon restarts
  (the agent can recreate it).
- The agent must remain stateless with respect to component state. If it needs to
  remember what cards are on the board, it stores that in its own message history
  (tool results, assistant text) or on disk.

## Protocol

### Render event

```rust
pub struct RenderEvent {
    /// Opaque component id, scoped to the session. The agent picks it,
    /// the client echoes it back on interactions.
    pub id: String,
    /// Component kind — one of the 18 built-in kinds (see "Built-in component
    /// kinds" below): kanban, form, table, progress, markdown, dashboard, chart,
    /// interactive_plot, timeline, stat, file_tree, diff, code, callout, gallery,
    /// confirm, map, video.
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

### Unmount event

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

The daemon resolves the event only when that session/component has a pending `component_wait`. A successful interaction completes that waiter so the running tool call can continue. If no matching waiter exists, the route returns `404`; it does not queue the event and does not start a new turn. Clients that want a separate follow-up turn must submit one through the normal agent-turn API.

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

#### `interactive_plot`

```json
{
  "id": "decay-lab",
  "kind": "interactive_plot",
  "props": {
    "title": "Exponential decay",
    "description": "Change the rate to recompute the curve locally.",
    "parameters": [
      { "id": "rate", "label": "Decay rate", "min": 0.1, "max": 3,
        "step": 0.1, "value": 1, "unit": "s⁻¹" }
    ],
    "plot": {
      "x": { "id": "t", "label": "Time (s)", "min": 0, "max": 10, "samples": 160 },
      "y_label": "Amplitude",
      "series": [
        { "label": "x(t)", "expression": "exp(-rate*t)" }
      ]
    },
    "metrics": [
      { "label": "Half-life", "expression": "ln(2)/rate", "unit": " s", "precision": 2 }
    ]
  }
}
```

The web Surface evaluates expressions and updates the plot and metrics locally as
controls move. A committed slider or number-input change emits:

```json
{
  "type": "parameters_changed",
  "payload": {
    "parameters": { "rate": 1.4 },
    "changed": { "id": "rate", "value": 1.4 }
  }
}
```

Expression support is deliberately bounded: numeric literals, lowercase ASCII
parameter/x identifiers, parentheses, unary `+`/`-`, operators `+ - * / ^`,
constants `pi`/`e`, and `sin`, `cos`, `tan`, `exp`, `ln`/`log`, `sqrt`, `abs`,
`min`, and `max`. Limits are 12 parameters, 6 series, 12 metrics, 512 samples,
and 512 characters per expression. Invalid or non-finite work renders an inline error instead of plot
geometry. This kind is currently interactive anywhere the canonical Leptos/WASM
Surface runs: browser PWA, extension, and Tauri desktop. Other clients may
project a fallback. It does not execute tools or external actions.

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
                     # | chart | interactive_plot | timeline | stat | file_tree
                     # | diff | code | callout | gallery | confirm | map | video
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

- Projects supported render/unmount events into bounded terminal cells and maintains shell-owned component state.
- The current session component tray derives todo projections from correlated tool events and is separate from the file tree.
- Posting `/v1/component/event` from TUI interactions remains a future phase. The current TUI does not enter a general component-interaction mode.

### ocean-gui (GPUI native)

- GPUI is a native desktop surface, not a Leptos/WebView renderer.
- Do not assume Leptos components, HTML, maps, dashboards, or web widgets render inside chat.
- Until GPUI has native component implementations for a kind, agents should prefer concise markdown, file paths, command/status summaries, and native surface-state descriptions.
- GPUI clients identify turns with `client_type: "surface-gpui"` so the daemon can inject GPUI-safe guidance instead of the web component playbook.

### CLI

- The CLI has no general component renderer.
- `component_wait` still blocks the running tool call until a matching interaction or timeout because waiting is daemon/runtime behavior, not a client-rendering choice. Avoid wait-based component flows when no attached client can post the interaction.

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

- The interaction endpoint requires string `session_id` and `component_id` values and resolves only an exact pending `(session_id, component_id)` waiter key. That correlation is not authentication or proof of session ownership; callers still rely on the daemon's local trust boundary.
- `component_render` and unmount are ungated presentation operations. `component_wait` is permission-gated because it blocks the turn while awaiting an external interaction.
- Component props are typed JSON. Clients must treat agent-provided content as untrusted presentation data. Ocean Surface sanitizes rendered markdown before assigning it through its HTML rendering sink; new component kinds must preserve an equivalent no-script boundary.

## Future

- **Live update** — the agent can push `Render` with `replace: true` to update
  in-place without a full re-render. Clients diff the props.
- **Streaming props** — for progress bars and live logs, the agent could emit
  `Render` events with partial updates (e.g. `{ "value": 0.7 }` merged into
  existing props).
- **Nested components** — a form field could itself be a component that the
  agent defines dynamically.
