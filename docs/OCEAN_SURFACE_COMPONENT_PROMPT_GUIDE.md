# Ocean Surface Component Prompt Guide

This is the short, agent-facing guidance that Ocean should inject when the current client is `surface-web` or `surface-tauri`.

Ocean Surface renders live Leptos components from agent-emitted `component_render` events. The browser PWA and Tauri desktop shell host the same canonical Leptos/WASM bundle, so both support these components as real UI, not decoration.

Do not inject this guide for `tui`, `cli`, or voice clients unless they explicitly advertise matching component capabilities. The shared web and Tauri Surface UI renders these Leptos components.

## Core rule

Prefer a rendered component whenever the answer has structure, state, interaction, progress, location, media, or code presentation.

Always follow a component render with a short text summary so terminal/plain clients still have context.

## Component schemas

### `progress` — live work status

Use for running tasks. Reuse the same `id` with `replace: true` as work advances.

```json
{
  "id": "build-progress",
  "kind": "progress",
  "props": {
    "label": "Building workspace",
    "value": 0.4,
    "max": 1,
    "indeterminate": false
  },
  "replace": true
}
```

Pattern:

1. Render progress at task start.
2. Update it after meaningful steps.
3. Finish with `value: max` and a `callout`, `diff`, or concise summary.

### `timeline` — multi-step plan/status

Use for visible plans or staged workflows.

```json
{
  "id": "task-plan",
  "kind": "timeline",
  "props": {
    "steps": [
      { "label": "Inspect", "status": "done", "detail": "Read Surface map renderer" },
      { "label": "Patch", "status": "active", "detail": "Fix marker attachment" },
      { "label": "Verify", "status": "pending" }
    ]
  },
  "replace": true
}
```

Statuses: `done`, `active`, `pending`, `error`.

### `interactive_plot` — locally explorable numeric relationships

Use when users should change bounded numeric parameters and immediately see the
resulting curve or derived metrics. Use `chart` instead for display-only data.
The Surface evaluates the declared math locally; do not use this component to
execute tools or external actions.

```json
{
  "id": "decay-lab",
  "kind": "interactive_plot",
  "props": {
    "title": "Exponential decay",
    "parameters": [
      { "id": "rate", "label": "Decay rate", "min": 0.1, "max": 3,
        "step": 0.1, "value": 1, "unit": "s⁻¹" }
    ],
    "plot": {
      "x": { "id": "t", "label": "Time", "min": 0, "max": 10, "samples": 160 },
      "y_label": "Amplitude",
      "series": [{ "label": "x(t)", "expression": "exp(-rate*t)" }]
    },
    "metrics": [
      { "label": "Half-life", "expression": "ln(2)/rate", "unit": " s", "precision": 2 }
    ]
  }
}
```

Expressions may use numeric literals, lowercase ASCII parameter/x identifiers,
`pi`, `e`, `+ - * / ^`, parentheses, and `sin`, `cos`, `tan`, `exp`,
`ln`/`log`, `sqrt`, `abs`, `min`, and `max`. Limits: 12 parameters, 6 series,
12 metrics, 512 samples, and 512 characters per expression. A committed control change emits
`parameters_changed` with the complete parameter map and the changed id/value.
Use `component_wait` only when the running turn truly needs that committed
choice; local preview does not require an agent round trip.

### `table` — structured data

Use instead of markdown pipe tables.

```json
{
  "id": "component-matrix",
  "kind": "table",
  "props": {
    "columns": ["Component", "Use", "Interaction"],
    "rows": [
      ["table", "Structured data", "row_clicked"],
      ["form", "Collect input", "form_submit"],
      ["map", "Locations/POIs", "marker_clicked"]
    ]
  }
}
```

Emits `row_clicked`.

### `callout` — important note/result

Use for success, warnings, errors, or decisions.

```json
{
  "id": "maps-fixed",
  "kind": "callout",
  "props": {
    "variant": "success",
    "title": "Map renderer fixed",
    "body": "Markers now attach to `gmp-map.innerMap`, so POIs render correctly."
  }
}
```

Variants: `info`, `success`, `warn`, `error`.

### `diff` — code changes

Use for meaningful edits instead of dumping raw text.

```json
{
  "id": "map-diff",
  "kind": "diff",
  "props": {
    "filename": "index.html",
    "lines": [
      { "kind": "ctx", "text": "const mapEl = document.createElement(\"gmp-map\");" },
      { "kind": "del", "text": "map: mapEl," },
      { "kind": "add", "text": "map: mapEl.innerMap," }
    ]
  }
}
```

Line kinds: `ctx`, `del`, `add`. Alternative: `props.unified`.

### `code` — copyable snippet

Use for commands, config, or source snippets the user may copy.

```json
{
  "id": "run-surface-command",
  "kind": "code",
  "props": {
    "language": "bash",
    "filename": "dev.sh",
    "code": "cd ../ocean-surface\n./run-surface.sh"
  }
}
```

### `form` — collect input

Render a form, then call `component_wait` for the submit when the answer depends on it.

```json
{
  "id": "workspace-form",
  "kind": "form",
  "props": {
    "title": "Open workspace",
    "fields": [
      { "name": "path", "label": "Workspace path", "type": "text", "required": true },
      { "name": "mode", "label": "Mode", "type": "select", "options": ["web", "native"] }
    ],
    "submit_label": "Open"
  }
}
```

Field types: `text`, `textarea`, `select`, `number`. Emits `form_submit`.

### `confirm` — yes/no before important action

Use before destructive or high-impact operations, then call `component_wait`.

```json
{
  "id": "delete-confirm",
  "kind": "confirm",
  "props": {
    "title": "Delete generated artifacts?",
    "body": "This removes files under `dist/`.",
    "confirm_label": "Delete",
    "cancel_label": "Keep",
    "variant": "error"
  }
}
```

Emits `confirm_response`.

### `map` — locations, POIs, routes/search areas

Use for geographic answers. Include `markers` for POIs and `fit_markers: true` when there are multiple points.

```json
{
  "id": "arlington-pois",
  "kind": "map",
  "props": {
    "center": { "lat": 38.8816, "lng": -77.0910 },
    "zoom": 11,
    "fit_markers": true,
    "markers": [
      { "label": "1", "lat": 38.8576, "lng": -77.3581, "title": "Micro Center Fairfax" },
      { "label": "2", "lat": 38.8629, "lng": -77.0598, "title": "Best Buy Pentagon City" }
    ]
  }
}
```

Emits `marker_clicked`.

### `dashboard` — compose several components

Use when the user benefits from multiple live views at once.

```json
{
  "id": "task-dashboard",
  "kind": "dashboard",
  "props": {
    "children": [
      {
        "id": "status",
        "width": 1,
        "kind": "stat",
        "props": { "stats": [{ "label": "Checks", "value": "3/4", "trend": "up" }] }
      },
      {
        "id": "steps",
        "width": 2,
        "kind": "timeline",
        "props": { "steps": [{ "label": "Verify", "status": "active" }] }
      }
    ]
  }
}
```

## UX patterns

### Long-running dev task

```text
progress(start) → progress(update) → diff/table/callout → short text summary
```

### Code edit

```text
timeline(plan) → progress(while editing/testing) → diff(show change) → callout(result)
```

### User decision

```text
callout(context) → confirm → component_wait → act on result
```

### Data-heavy answer

```text
table/stat/chart/interactive_plot/map first → concise text interpretation second
```

## Do not

- Do not fake tables with markdown when `table` fits.
- Do not dump giant raw diffs when `diff` fits.
- Do not ask textual yes/no before destructive actions when `confirm` is available.
- Do not over-componentize normal prose; long explanations can stay markdown.
- Do not end a turn with only a component. Always include short text.
