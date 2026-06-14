# Ocean GPUI Masterbuild Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` or `superpowers:executing-plans` to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Ocean GUI as an agent-native desktop collaboration surface where humans and agents share a native canvas, voice/video presence, workspace context, and a durable spatial ledger.

**Architecture:** Ocean GUI must not be "chat plus tldraw." The core product is an Ocean-owned scene graph and patch protocol: `agent intent -> structured surface patch -> x/y ledger mutation -> native GPUI canvas render -> optional tldraw/LiveKit sync -> next-turn context`. tldraw can remain a sketch/freehand adapter, but the authoritative agent-controlled surface belongs to Ocean.

**Tech Stack:** Rust, GPUI, gpui-component/Longbridge, ocean-daemon, ocean-agent, ocean-runtime tools, LiveKit Rust SDK, optional tldraw webview adapter, optional layout engines such as Taffy/ELK/Dagre where they fit, optional CRDT engine such as Loro for local-first sync.

---

## 0. Non-Negotiable Product Truth

The app only matters if agents can control the surface.

The target is not:

```text
agent describes a diagram in chat
human draws it manually
```

The target is:

```text
agent receives surface context
agent emits structured patches
Ocean validates and applies patches
native canvas changes visibly
ledger updates
next agent turn sees the updated ledger
```

An agent preview is not valid unless an agent-originated command changes the canvas and the changed ledger is present in the next turn context.

## 1. Current Reality Check

### Exists today

- GPUI desktop shell under `ocean-surface/crates/ocean-gui`.
- Embedded tldraw webview under `ocean-surface/crates/ocean-gui/canvas-web`.
- Rust `SurfaceState`, `SurfaceLedger`, `LedgerComponent`, and `SurfaceIpcCommand`.
- Webview bridge that can apply `upsert_component` into tldraw.
- Manual "pin to canvas" style path from GPUI to ledger to webview.
- Session-scoped daemon event stream.
- LiveKit state/types/prototype hooks.

### Missing or incomplete

- Agent-native canvas patch protocol.
- Agent tool that emits surface patches intentionally.
- GPUI handler that maps agent patch events into `SurfaceLedger`.
- Native GPUI renderer for Ocean canvas primitives.
- Stable x/y/w/h patch contract.
- Patch validation.
- Patch event persistence.
- Next-turn injection of the authoritative Ocean canvas state.
- Clear distinction between native Ocean canvas and optional tldraw projection.

### Immediate correction

Do not ship or preview another "agent in GPUI" demo until this path works:

```text
agent tool call
  -> daemon event
  -> GPUI patch handler
  -> CanvasLedger mutation
  -> visible canvas render
  -> ledger included in next prompt injection
```

## 2. North Star

Ocean GUI is a desktop collaboration cockpit for:

- shared planning sessions,
- agent-assisted work,
- live meeting rooms,
- automation design,
- campaign/proposal/storyboard work,
- browser and terminal workflows,
- Longhouse decisions and quorum work,
- remote team collaboration.

The chat transcript is supporting infrastructure. The canvas is the shared working memory.

## 3. Build vs Buy

### Use existing solutions for shell furniture

Use Longbridge/gpui-component for native GPUI application furniture where it is stable enough:

- buttons,
- tooltips,
- icons,
- tabs,
- dropdowns,
- popovers,
- modals,
- resizable panes,
- lists,
- virtual lists,
- tables,
- charts,
- tree/sidebar patterns,
- webview fallback.

Reason: Ocean should not waste cycles rebuilding ordinary UI widgets.

### Use GPUI for native desktop rendering

GPUI remains the native shell and canvas renderer. It is the right fit for:

- fast native Rust state,
- direct GPU UI,
- custom desktop interactions,
- Zed-style compact workflows,
- tight integration with daemon/session state.

### Use LiveKit for media and presence

LiveKit should own:

- audio,
- video,
- participant presence,
- room metadata,
- participant attributes,
- low-latency data/RPC where useful.

LiveKit must not own the canvas CRDT or become the reasoning authority.

### Use tldraw as optional sketch/freehand projection

tldraw can remain useful for:

- manual drawing,
- quick sketching,
- freehand annotation,
- importing/exporting rough diagrams,
- multiplayer whiteboard fallback,
- proving webview bridge mechanics.

tldraw must not be the core Ocean product. The authoritative agent-native surface is the Ocean scene graph and ledger.

### Use layout/rendering libraries selectively

Candidates:

- Taffy: layout engine for nested cards/panels if GPUI layout is too manual for components.
- ELK or Dagre: automatic graph layout for workflows and dependency graphs.
- Vello: future lower-level 2D vector renderer if GPUI primitives are not enough for high-density canvas work.
- Loro: candidate CRDT for Ocean-owned local-first ledger sync.

Rule: adopt these only behind Ocean interfaces. Do not let an external library's data model become the product model.

## 4. Core Architecture

```text
ocean-daemon
  owns agent turns, tool execution, event stream, project/workspace/session binding

ocean-gui
  owns the native user surface, canvas state, panes, presence UI, and local rendering

ocean-runtime
  owns agent tools and emits side-effect events

Ocean canvas ledger
  owns durable spatial working memory

tldraw adapter
  optional projection/import/export/freehand layer

LiveKit adapter
  optional realtime media/presence/data layer
```

Dependency direction:

```text
daemon events / tools -> surface patch DTOs -> GUI applies patches
GUI state -> prompt context injection
GUI renderer -> projects CanvasLedger
```

No daemon table should become the source of truth for the canvas. The daemon can persist patch events later, but the app/session document owns the surface state.

## 5. Domain Model

### CanvasLedger

```rust
pub struct CanvasLedger {
    pub canvas_id: CanvasId,
    pub session_id: AgentSessionId,
    pub revision: u64,
    pub mode: CanvasMode,
    pub viewport: Viewport,
    pub components: IndexMap<ComponentId, CanvasComponent>,
    pub edges: IndexMap<EdgeId, CanvasEdge>,
    pub selection: SelectionState,
    pub patch_log: Vec<SurfacePatchEnvelope>,
    pub metadata: serde_json::Value,
}
```

Responsibilities:

- store visible surface state,
- allocate x/y placement,
- keep component ids stable,
- keep edge/connection state,
- expose compact context to agents,
- emit render invalidation hints,
- support undo/redo through patch log,
- support sync later through snapshot + patch replay.

### CanvasComponent

```rust
pub struct CanvasComponent {
    pub id: ComponentId,
    pub kind: ComponentKind,
    pub rect: Rect,
    pub z_index: i32,
    pub content: ComponentContent,
    pub ports: Vec<Port>,
    pub children: Vec<ComponentId>,
    pub metadata: serde_json::Value,
    pub created_by: ActorRef,
    pub updated_by: ActorRef,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
```

### ComponentKind

Start small:

```rust
pub enum ComponentKind {
    Card,
    TextBlock,
    Frame,
    Node,
    Port,
    EdgeLabel,
    Lane,
    Table,
    MediaSlot,
    Stat,
}
```

Then add templates on top:

```text
brief_card = Card + title/body/metadata
workflow_node = Node + ports + status
kanban_column = Lane + child cards
storyboard_frame = Frame + media slot + caption
longhouse_proposal = Card + tally metadata + edges
```

### CanvasEdge

```rust
pub struct CanvasEdge {
    pub id: EdgeId,
    pub from: Endpoint,
    pub to: Endpoint,
    pub kind: EdgeKind,
    pub label: Option<String>,
    pub route: EdgeRoute,
    pub metadata: serde_json::Value,
}
```

Edges are first-class. Workflow builders, dependency graphs, source maps, Longhouse decisions, and browser flows all need edges.

## 6. Surface Patch Protocol

Surface mutation must be structured, typed, and validated.

### Envelope

```rust
pub struct SurfacePatchEnvelope {
    pub patch_id: PatchId,
    pub session_id: AgentSessionId,
    pub surface_id: SurfaceId,
    pub canvas_id: CanvasId,
    pub actor: ActorRef,
    pub created_at_ms: i64,
    pub patch: SurfacePatch,
}
```

### Operations

```rust
pub enum SurfacePatch {
    UpsertComponent(CanvasComponentPatch),
    MoveComponent { component_id: ComponentId, x: f32, y: f32 },
    ResizeComponent { component_id: ComponentId, width: f32, height: f32 },
    DeleteComponent { component_id: ComponentId },
    Connect { edge: CanvasEdgePatch },
    Disconnect { edge_id: EdgeId },
    Focus { target: FocusTarget },
    Select { ids: Vec<ComponentId> },
    SetViewport { viewport: Viewport },
    Layout { target: LayoutTarget, strategy: LayoutStrategy },
    Group { frame_id: ComponentId, children: Vec<ComponentId> },
}
```

### Minimal JSON tool shape

```json
{
  "canvas_id": "canvas:main",
  "patches": [
    {
      "op": "upsert_component",
      "component": {
        "id": "brief-1",
        "kind": "brief_card",
        "rect": { "x": 420, "y": 120, "w": 320, "h": 220 },
        "content": {
          "title": "Sales Brief",
          "body": "Draft brief for the Warner campaign"
        },
        "metadata": {
          "source": "longhouse.sales"
        }
      }
    }
  ]
}
```

### Placement rules

Agents may provide exact x/y. If omitted, the app must allocate:

```text
requested rect present -> validate and apply
near component id present -> place adjacent with collision avoidance
lane/frame target present -> place inside container
none present -> next available slot
```

Agents must never be asked to manually solve collision avoidance. They can suggest intent; the app owns final placement.

## 7. Agent Tool Contract

Add a real tool, not a prompt trick:

```text
surface_patch
```

Tool purpose:

```text
Apply one or more structured patches to the current Ocean surface canvas.
Use this whenever the user asks for visual/canvas/workflow/storyboard/board output.
Do not draw ASCII diagrams in chat when the canvas is available.
```

Tool parameters:

```json
{
  "type": "object",
  "required": ["canvas_id", "patches"],
  "properties": {
    "canvas_id": { "type": "string" },
    "patches": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["op"],
        "properties": {
          "op": {
            "type": "string",
            "enum": [
              "upsert_component",
              "move_component",
              "resize_component",
              "delete_component",
              "connect",
              "disconnect",
              "focus",
              "select",
              "layout",
              "group"
            ]
          }
        }
      }
    }
  }
}
```

Tool result:

```json
{
  "ok": true,
  "applied": 3,
  "canvas_id": "canvas:main",
  "revision": 12,
  "component_ids": ["brief-1", "proposal-1"]
}
```

The tool should emit an `AgentTurnEvent::SurfacePatch` or an `Extension` event with `extension:"surface_patch"` until the SDK gets a native variant.

## 8. Prompt Contract

GPUI surface guidance must say:

```text
You are inside Ocean GUI, an agent-native desktop work surface.
When the user asks for canvas, board, workflow, storyboard, diagram, or spatial work, use `surface_patch`.
Do not draw ASCII diagrams in chat.
Do not tell the user to draw manually.
Use the injected canvas ledger to choose ids, coordinates, containers, and update targets.
If exact x/y is not important, omit it and let the app place the component.
Always include a short text summary after patching.
```

GPUI guidance must not say:

```text
Do not use component_render
```

Better:

```text
Do not use Leptos/web-only component rendering for chat UI. Use `surface_patch` for native canvas mutations.
```

## 9. Native Canvas Renderer

### Why native

tldraw is a web canvas editor. Ocean needs a native agent surface:

- dense operational UI,
- explicit state,
- predictable rendering,
- native controls,
- low-latency patch application,
- no webview overlay/compositing trap,
- canvas objects that map to agent memory.

### Rendering responsibilities

`OceanCanvasView` should render:

- background grid,
- viewport pan/zoom transform,
- components,
- selection outlines,
- edge routes,
- port anchors,
- hover/focus states,
- active agent write highlight,
- live cursors/presence later.

### First primitives

1. Card
2. Text block
3. Frame/group
4. Node
5. Port
6. Edge/connector
7. Column/lane
8. Table/grid
9. Media placeholder
10. Stat tile

Everything else composes from these.

## 10. Renderable Taxonomy

### Work objects

- Markdown brief cards
- Task cards
- Proposal cards
- Decision records
- Research cards
- Source citation cards
- Approval cards
- Form/input panels
- Status/progress cards
- Agent activity cards

### Workflow objects

- Workflow nodes
- Trigger nodes
- Condition nodes
- Action nodes
- Error handler nodes
- Ports
- Edges
- Parallel branches
- Retry/backoff blocks
- Automation run traces

### Planning boards

- Kanban columns
- Roadmap lanes
- Release checklists
- Bug triage boards
- Sprint boards
- Dependency maps
- Git branch/worktree maps
- PR review boards
- Deployment pipelines
- Incident timelines

### Creative/campaign boards

- Storyboard frames
- 9:16 video shot boards
- Mood boards
- Image galleries
- Asset bins
- Prompt boards
- Audio waveform clips
- Campaign briefs
- Creator profile cards
- Performance dashboards

### Technical objects

- File trees
- Code snippets
- Diffs
- Terminal output blocks
- Browser snapshots
- Network request cards
- API endpoint maps
- Database result grids
- Schema diagrams
- JSON inspectors

### Collaboration objects

- Live meeting participant tiles
- Voice transcript blocks
- Camera tiles
- Agenda blocks
- Action item cards
- Longhouse proposal boards
- Quorum/tally boards
- Agent assignment boards
- Room/presence maps
- Shared notes

## 11. LiveKit Integration

LiveKit is for human/agent presence and media, not canvas state.

Use LiveKit for:

- mic/camera publishing,
- participant list,
- participant attributes,
- room metadata,
- lightweight data/RPC messages,
- voice agent participation later.

Do not use LiveKit for:

- full canvas CRDT,
- transcript authority,
- daemon session routing,
- persistent ledger storage.

The LiveKit room metadata should contain compact pointers:

```json
{
  "session_id": "agent-session-id",
  "surface_id": "gpui:local",
  "active_canvas_id": "canvas:main",
  "canvas_revision": 42
}
```

## 12. Persistence And Sync

### Local first

Start with local app-owned JSON snapshots and patch logs:

```text
~/.ocean/surfaces/<session_id>/canvas/<canvas_id>.json
~/.ocean/surfaces/<session_id>/canvas/<canvas_id>.patches.jsonl
```

### Sync-ready

Keep patch operations deterministic so the model can move to CRDT later.

Possible sync progression:

1. Local snapshot + patch log.
2. Daemon-served patch stream.
3. Loro-backed CRDT document.
4. Cloud sync for team spaces.

Do not add a database until query patterns require it.

## 13. Module Layout

### In `ocean-os`

Add shared protocol types here so all surfaces agree:

```text
crates/ocean-agent-sdk/src/surface.rs
  SurfacePatchEnvelope
  SurfacePatch
  CanvasComponentPatch
  CanvasEdgePatch
  CanvasId / ComponentId / PatchId newtypes

crates/ocean-runtime/src/tools/surface.rs
  SurfacePatchTool

crates/ocean-daemon/src/main.rs
  stream SurfacePatch event through /v1/agent/events
```

### In `ocean-surface`

Native GUI state and rendering:

```text
crates/ocean-gui/src/shell/canvas/
  mod.rs
  ledger.rs
  patch.rs
  layout.rs
  render.rs
  hit_test.rs
  interaction.rs
  templates.rs

crates/ocean-gui/src/shell/view.rs
  route AgentEvent::SurfacePatch to canvas state
```

Optional tldraw adapter:

```text
crates/ocean-gui/src/shell/tldraw_adapter.rs
crates/ocean-gui/canvas-web/src/oceanBridge.ts
```

## 14. Execution Slices

### Slice 1: Patch protocol in shared SDK

Files:

- `ocean-os/crates/ocean-agent-sdk/src/surface.rs`
- `ocean-os/crates/ocean-agent-sdk/src/lib.rs`

Steps:

- [ ] Add newtypes: `SurfaceId`, `CanvasId`, `ComponentId`, `PatchId`, `EdgeId`.
- [ ] Add `Rect`, `Viewport`, `CanvasComponentPatch`, `CanvasEdgePatch`.
- [ ] Add `SurfacePatch`, `SurfacePatchEnvelope`, `SurfacePatchResponse`.
- [ ] Add serde tests for snake_case JSON.
- [ ] Run `cargo test -p ocean-agent-sdk surface`.

Acceptance:

- JSON contract is stable.
- x/y/w/h roundtrip as numbers.
- unknown metadata can pass through as `serde_json::Value`.

### Slice 2: Runtime `surface_patch` tool

Files:

- `ocean-os/crates/ocean-runtime/src/tools/surface.rs`
- `ocean-os/crates/ocean-runtime/src/tools/mod.rs`
- `ocean-os/crates/ocean-runtime/src/types.rs`

Steps:

- [ ] Add `SurfacePatchTool`.
- [ ] Tool validates `canvas_id` and non-empty `patches`.
- [ ] Tool returns structured result.
- [ ] Tool emits side effect event.
- [ ] Register tool with built-ins.
- [ ] Add unit tests for valid and invalid patch payloads.
- [ ] Run `cargo test -p ocean-runtime surface_patch`.

Acceptance:

- Agent can call one tool to mutate the surface.
- Tool result references applied ids.
- No filesystem or daemon global state is needed.

### Slice 3: Daemon event streaming

Files:

- `ocean-os/crates/ocean-agent-sdk/src/lib.rs`
- `ocean-os/crates/ocean-daemon/src/main.rs`
- `ocean-os/crates/ocean-agent/src/lib.rs`

Steps:

- [ ] Add `AgentTurnEvent::SurfacePatch` or wrap as `Extension { extension: "surface_patch" }`.
- [ ] Bridge runtime side effect into `/v1/agent/events`.
- [ ] Preserve session id filtering.
- [ ] Add tests proving unrelated sessions do not receive patch events.
- [ ] Run `cargo check -p ocean-daemon`.

Acceptance:

- GPUI receives only its session's surface patches.
- Browser extension cannot adopt a GPUI session through global stream bleed.

### Slice 4: Native CanvasLedger in GPUI

Files:

- `ocean-surface/crates/ocean-gui/src/shell/canvas/ledger.rs`
- `ocean-surface/crates/ocean-gui/src/shell/canvas/patch.rs`
- `ocean-surface/crates/ocean-gui/src/shell/canvas/layout.rs`

Steps:

- [ ] Implement `CanvasLedger`.
- [ ] Implement `apply_patch`.
- [ ] Implement `next_available_slot`.
- [ ] Implement collision detection.
- [ ] Implement `compact_context`.
- [ ] Add tests for upsert/move/delete/connect/layout.
- [ ] Run `cargo test -p ocean-gui canvas`.

Acceptance:

- One patch mutates ledger.
- Missing x/y gets a deterministic slot.
- Next-turn context includes updated component ids and rects.

### Slice 5: Native OceanCanvasView

Files:

- `ocean-surface/crates/ocean-gui/src/shell/canvas/render.rs`
- `ocean-surface/crates/ocean-gui/src/shell/canvas/hit_test.rs`
- `ocean-surface/crates/ocean-gui/src/shell/view.rs`

Steps:

- [ ] Render background grid.
- [ ] Render card primitive.
- [ ] Render frame primitive.
- [ ] Render edge primitive.
- [ ] Render node primitive.
- [ ] Add selection and focus outline.
- [ ] Add pan/zoom viewport state.
- [ ] Add hit tests for components.
- [ ] Add screenshot/manual validation.

Acceptance:

- Canvas is visible without tldraw.
- Agent-created card appears natively.
- Selection and focus are usable.

### Slice 6: GPUI agent event application

Files:

- `ocean-surface/crates/ocean-gui/src/shell/view.rs`
- `ocean-surface/crates/ocean-gui/src/shell/canvas/patch.rs`

Steps:

- [ ] Parse daemon patch event into SDK type.
- [ ] Apply to active session ledger.
- [ ] Request repaint.
- [ ] Update LiveKit compact metadata if connected.
- [ ] Add a regression test that a patch event produces a ledger component.
- [ ] Add a visual/manual test that a patch appears on canvas.

Acceptance:

- Agent patch event changes the surface.
- Chat fallback is not required for visual output.

### Slice 7: Prompt and context contract

Files:

- `ocean-os/crates/ocean-agent/src/lib.rs`
- `ocean-surface/crates/ocean-gui/src/shell/canvas/context.rs`
- `ocean-surface/crates/ocean-gui/src/shell/view.rs`

Steps:

- [ ] Replace GPUI guidance that blocks surface tools.
- [ ] Inject canvas ledger context.
- [ ] Include active canvas id, all canvas ids, components, selection, mode, viewport.
- [ ] Include explicit instruction: use `surface_patch`, not ASCII diagrams.
- [ ] Add tests that prompt contains `surface_patch` guidance.
- [ ] Run `cargo test -p ocean-agent gpui_surface`.

Acceptance:

- Model is told exactly how to control the surface.
- It knows the canvas exists.
- It has a tool to mutate it.

### Slice 8: Templates

Files:

- `ocean-surface/crates/ocean-gui/src/shell/canvas/templates.rs`

Steps:

- [ ] Implement `brief_card`.
- [ ] Implement `workflow_node`.
- [ ] Implement `kanban_column`.
- [ ] Implement `storyboard_frame`.
- [ ] Implement `stat_tile`.
- [ ] Implement `longhouse_proposal`.
- [ ] Add tests converting template JSON into primitives.

Acceptance:

- Agents can create real work objects without hand-authoring every primitive.

### Slice 9: tldraw adapter demotion

Files:

- `ocean-surface/crates/ocean-gui/src/shell/tldraw_adapter.rs`
- `ocean-surface/crates/ocean-gui/canvas-web/src/oceanBridge.ts`

Steps:

- [ ] Keep tldraw as optional pane mode.
- [ ] Add import from Ocean ledger to tldraw.
- [ ] Add export from tldraw shape meta to Ocean ledger.
- [ ] Do not make tldraw the default agent render target.

Acceptance:

- Ocean canvas works without tldraw.
- tldraw remains useful as a sketch/freehand adapter.

### Slice 10: Collaboration and LiveKit

Files:

- `ocean-surface/crates/ocean-gui/src/shell/surface_livekit.rs`
- `ocean-surface/crates/ocean-gui/src/shell/surface_livekit_client.rs`
- `ocean-daemon` token route/proxy if needed.

Steps:

- [ ] Publish compact surface metadata.
- [ ] Show participant list.
- [ ] Mic toggle.
- [ ] Camera toggle.
- [ ] Presence cursor state later.
- [ ] Never sync full canvas document through LiveKit metadata.

Acceptance:

- Humans can join the same working space.
- Media presence does not interfere with session routing or canvas state.

## 15. Test Gates

No slice is complete unless the relevant gate passes.

### Gate A: Agent canvas mutation

Required proof:

```text
agent calls surface_patch
GPUI receives event
CanvasLedger revision increments
component appears visibly
next prompt context includes component id and rect
```

### Gate B: No cross-session bleed

Required proof:

```text
GPUI session A receives only A events
browser extension session B receives only B events
same session attached by two surfaces receives the same session stream intentionally
```

### Gate C: No chat-only fake canvas

Required proof:

```text
canvas request does not produce ASCII diagram as primary output
model uses surface_patch for visual work
chat contains only short summary
```

### Gate D: Native fallback

Required proof:

```text
native Ocean canvas renders without tldraw webview mounted
```

## 16. Performance Posture

Keep the hot path explicit:

```text
patch event -> ledger mutation -> dirty rect/component invalidation -> GPUI repaint
```

Avoid:

- reserializing the whole ledger on every frame,
- rebuilding all layout on every mouse move,
- allocating strings for every component every frame,
- using async for local canvas state mutation,
- letting webview/tldraw become required for native render.

Use:

- stable ids,
- compact patch structs,
- dirty component sets,
- cached text layout,
- viewport culling,
- coarse spatial index once component counts demand it.

## 17. Open Technical Decisions

### CRDT

Start with local snapshot + patch log. Evaluate Loro after patch semantics stabilize.

Decision test:

```text
Can two users move/update different components and converge without stomping?
Can the patch log replay deterministically?
Can the agent receive a compact, stable context?
```

### Graph layout

Start with simple deterministic placement. Add ELK/Dagre only for workflow/dependency layout commands.

Decision test:

```text
Does the workflow have enough nodes/edges that manual next-slot placement is useless?
```

### Renderer

Start with GPUI primitives. Evaluate Vello only when GPUI rendering becomes a measurable bottleneck for thousands of vector objects or complex paths.

### Longbridge

Use Longbridge for controls and shell UI. Do not use it as the canvas architecture.

## 18. Source Links

Primary/current references used for this plan:

- GPUI official site: https://www.gpui.rs/
- Zed GPUI README: https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md
- Longbridge gpui-component docs: https://longbridge.github.io/gpui-component/docs/components
- Longbridge gpui-component releases: https://github.com/longbridge/gpui-component/releases
- LiveKit Rust SDK: https://github.com/livekit/rust-sdks
- LiveKit server/realtime stack: https://github.com/livekit/livekit
- Taffy layout engine: https://taffylayout.com/docs/about-taffy
- ELK layout kernel: https://eclipse.dev/elk/
- Vello renderer: https://github.com/linebender/vello
- Dagre graph layout: https://github.com/dagrejs/dagre

## 19. First Real Milestone

Milestone name:

```text
M1: Agent Writes One Native Canvas Card
```

Scope:

```text
surface_patch tool
daemon event stream
GPUI ledger apply
native card render
next-turn context injection
```

Out of scope:

```text
LiveKit media
tldraw sync
advanced graph layout
mobile
full template library
```

Acceptance script:

```text
1. Launch Ocean GUI.
2. Open Surface.
3. Ask: "put a card on the canvas that says hello from the agent".
4. Agent calls surface_patch.
5. A card appears on the native canvas.
6. Ask: "what is on the canvas?"
7. Agent answers using the ledger context and names the card id/position.
```

If this does not pass, the GPUI agent surface is not ready for demo.

