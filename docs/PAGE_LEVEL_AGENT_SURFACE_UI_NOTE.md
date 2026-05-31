# Note — Page-Level Agent-Driven Surface UI

Ocean Surface should evolve from “agent renders components inside chat” to “agent can drive trusted UI regions across the whole Surface.”

Current model:

```txt
agent event → chat transcript component registry → Leptos component inside message
```

Possible next model:

```txt
agent event → Surface UI bus → named Surface region
```

Chat remains one render target, but not the only one.

## Why this matters

This is the difference between:

> chatbot with tools

and:

> local operating cockpit the agent can reshape around the task

Ocean already has the core primitive: structured `component_render` events. The next unlock is adding a trusted `target`/region layer so components can mount outside the chat transcript.

## Candidate Surface regions

```txt
chat.inline       # current behavior
main.canvas       # large workspace / visualization / preview area
sidebar.left      # file tree, workspace nav, branches
sidebar.right     # inspector, details, search results, test failures
status.bar        # current model, daemon state, progress, running tool
modal             # confirmation, forms, permission prompts
drawer.bottom     # logs, voice waveform, background tasks
background        # ambient/visual layer, e.g. ocean/Bevy scene
notification      # transient toast-style events
```

## Protocol sketch

Current render event:

```rust
RenderEvent {
    id: String,
    kind: String,
    props: serde_json::Value,
    replace: bool,
}
```

Potential extension:

```rust
RenderEvent {
    id: String,
    kind: String,
    props: serde_json::Value,
    replace: bool,
    target: Option<RenderTarget>,
    priority: Option<RenderPriority>,
    ttl_ms: Option<u64>,
}
```

Where:

```rust
enum RenderTarget {
    ChatInline,
    MainCanvas,
    LeftSidebar,
    RightSidebar,
    StatusBar,
    Modal,
    BottomDrawer,
    Background,
    Notification,
}
```

Default behavior should remain `ChatInline` for backward compatibility.

## Example: coding session cockpit

```txt
main.canvas       → code diff / preview / git graph
sidebar.left      → project file tree
sidebar.right     → selected file, test failure, symbol details
status.bar        → running tool/progress/model/daemon state
chat.inline       → conversation and inline results
modal             → confirmations and permissions
```

## Example: local shopping/search task

```txt
main.canvas       → live map
sidebar.right     → POI list/table
chat.inline       → summary and recommendations
status.bar        → search/provider status
```

## Example: repo maintenance

```txt
main.canvas       → git graph visualization
sidebar.left      → branches/worktrees
sidebar.right     → selected commit details
status.bar        → checks/build progress
```

## Bevy / 3D visualization idea

A future Surface region could host a trusted Bevy/WASM scene renderer. The agent should not emit arbitrary code; it should emit scene data.

Example:

```json
{
  "id": "git-viz",
  "target": "main.canvas",
  "kind": "scene3d",
  "props": {
    "engine": "bevy",
    "scene": "git_graph",
    "nodes": [],
    "edges": [],
    "effects": {
      "atmospheric_fog": true,
      "bloom": true
    }
  }
}
```

This could support things like:

- git history as glowing nodes/curves
- branches fading into atmospheric fog
- workspace maps
- agent activity streams
- voice assistant ambience
- long-running task visualizations

## Guardrails

Page-level rendering needs stricter rules than inline chat rendering.

1. Agent can only render into whitelisted regions.
2. User can dismiss/clear agent-owned regions.
3. High-impact UI changes require confirmation.
4. No arbitrary HTML or JavaScript.
5. Components remain JSON-schema driven.
6. Surface owns final rendering.
7. Permissions distinguish inline rendering from page-level rendering.
8. Every page-level component has an owner/session/id.
9. Render targets should be auditable in the event stream.
10. Native/Tauri surfaces may expose extra regions, but web remains compatible.

## Implementation direction

1. Keep current `component_render` behavior unchanged.
2. Add optional `target` to render events.
3. Add a Surface-side region registry.
4. Route render events by `target`, defaulting to `chat.inline`.
5. Add user controls to clear/dismiss page-level agent UI.
6. Add permissions/policy for non-chat targets.
7. Later, add specialized component kinds like `scene3d`, `workspace_graph`, or `git_graph`.

## Product thesis

Ocean should not just answer inside a chat box. It should be able to assemble a task-specific cockpit around the user, using trusted local components rendered by Ocean Surface.
