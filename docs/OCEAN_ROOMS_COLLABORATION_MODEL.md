# Ocean Rooms — Collaboration Model

Ocean Rooms are local-first Slack-style channels where humans, agents, bots, tools, and clients collaborate around a workspace.

## Product thesis

Ocean should not be "one chat box talking to one agent."

Ocean should be a local-first collaboration cockpit where:

- multiple humans + agents + bots participate in the same room
- agents can post messages, run tools, and render structured UI
- rooms have shared state: transcript, component UI, page-level regions
- execution happens in explicit worktree/sandbox contexts
- every participant has identity, capabilities, and attribution

## Architecture layers

```txt
Room          = collaboration / event layer      (who says what, when)
Surface UI    = rendering / cockpit layer         (maps, dashboards, sidebars)
Worktree      = execution / sandbox layer         (where tools run safely)
```

A room is not a 1:1 chat session. A 1:1 chat is just a special case of a room with one human and one agent participant.

## What `#claude-ops` proves

The existing Slack channel `#claude-ops` already operates as an Ocean Room prototype:

- Multiple humans (Eric, John, Jake)
- Multiple Claude agents (smaths-bot, eric-claude, Jake's Claude)
- Mixed human/agent message streams
- Bot identity and attribution discipline
- Mandatory read-before-answer context loading
- Agent mentions/tags to wake event-driven listeners
- Multiple transport architectures (bridge, MCP-as-user, cloud scheduled)
- Shared protocol skill synced across all agents

Ocean Rooms should make this pattern native.

## Room primitive

```rust
struct Room {
    id: RoomId,
    workspace_id: WorkspaceId,
    name: String,
    topic: Option<String>,
    participants: Vec<Participant>,
    messages: Vec<RoomMessage>,
    turns: Vec<Turn>,
    ui_state: RoomUiState,
    policy: RoomPolicy,
}
```

Rooms are workspace-bound (git toplevel or cwd).

A session maps to a room:

```txt
legacy session_id == room_id
```

## Participants

Every entity in a room has identity:

```rust
struct Participant {
    id: ParticipantId,
    kind: ParticipantKind,
    display_name: String,
    role: RoomRole,
    capabilities: CapabilitySet,
    transport: TransportMode,
}

enum ParticipantKind {
    Human,
    Agent,
    Bot,
    Tool,
    System,
}

enum RoomRole {
    Owner,
    Driver,
    Collaborator,
    Observer,
}
```

Capabilities are per-participant, not global:

```rust
struct CapabilitySet {
    can_post_messages: bool,
    can_start_turns: bool,
    can_approve_tools: bool,
    can_cancel_turns: bool,
    can_render_components: bool,
    can_render_page_level: bool,
    can_access_filesystem: bool,
    allowed_paths: Option<Vec<PathBuf>>,
    execution_mode: ExecutionMode,
}
```

## Agent profiles

Agents are just participants with extra config:

```rust
struct AgentProfile {
    participant_id: ParticipantId,
    model: String,
    system_prompt: String,
    tools: Vec<ToolSpec>,
    trigger_policy: TriggerPolicy,
    execution_policy: ExecutionPolicy,
}

struct TriggerPolicy {
    on_mention: bool,
    on_thread_reply: bool,
    on_component_event: bool,
    on_schedule: Option<CronExpr>,
    on_file_change: Option<WatchSpec>,
}
```

## Mentions and triggers

Agents are woken by mentions, not direct invocation:

```txt
@ocean implement this
@reviewer check the patch
@testbot run smoke
@docsbot update protocol
```

Under the hood, a mention creates a turn request queued for that agent.

> **Live.** `POST /v1/rooms/persistent/{key}/messages` (the `room_post_message`
> handler) parses `@id` mentions from the body, runs `evaluate_trigger_policy`
> (the tested evaluator in `ocean-core`) against the room's stored trigger
> policy, and on a positive decision that resolves to an **agent** participant in
> the roster it does three things: (1) emits a `room_trigger` notice onto the
> agent event bus, (2) appends an `auto-convene` audit line to the transcript,
> and (3) **queues a real agent turn** for that participant via
> `spawn_room_agent_turn`. The turn hydrates the recent transcript as context,
> runs through the same `runtime.prompt` path every other turn uses (registered
> as a tracked request, permission-gated, resumable under a deterministic
> per-(room, agent) session id), and posts the agent's reply back into the room
> as an `Agent`-authored message. So an `@agent` mention in a persistent room
> actually wakes the agent and gets an answer in the transcript.
>
> A mention that resolves to a human/bot/tool id (or an unknown id) still fires
> no convene footprint — no notice, no audit line, no turn — even if the policy
> matched, because there is no agent to wake.
>
> **Loop safety.** Agent-authored messages are never evaluated for triggers, and
> the agent's own reply is posted as `Agent`-kind, so a reply that @-mentions
> another agent (or itself) can never ping-pong the room. Only
> human/bot/system-authored lines can convene an agent. (OCEAN-111 / OCEAN-225.)

## Read-before-answer

Every agent turn must hydrate room context before acting.

The daemon assembles a context packet:

```rust
struct TurnContext {
    recent_messages: Vec<RoomMessage>,      // last N messages
    thread_context: Option<Vec<Message>>,    // if replying in thread
    open_asks: Vec<OpenAsk>,                 // unresolved questions/asks
    participants: Vec<Participant>,          // current room roster
    ui_state: RoomUiState,                   // current component/page state
    workspace_state: WorkspaceSnapshot,      // git status, changed files, etc.
    relevant_files: Vec<FileContext>,        // files mentioned or changed
}
```

Agents should not have to manually ask "what's going on?" every turn.

## Attribution

Every room event carries author identity:

```rust
struct RoomEvent {
    room_id: RoomId,
    seq: u64,
    author: ParticipantId,
    author_kind: ParticipantKind,
    delegated_by: Option<ParticipantId>,   // human who delegated to agent
    agent_id: Option<AgentId>,
    turn_id: Option<TurnId>,
    kind: RoomEventKind,
    created_at: DateTime,
}
```

The UI can show:

```txt
Eric
Eric's Claude
Ocean acting for John
TestBot reported
```

No ambiguity about who authored what.

## Transport independence

Transport is not the product. Room logic is.

```txt
Slack / TUI / Surface / CLI / Voice
        ↓
transport adapters (thin)
        ↓
Ocean Room event bus (the real product)
        ↓
agent runtime
        ↓
room events + components
```

Multiple clients can subscribe to the same room simultaneously:

```txt
TUI       ┐
Surface   ├── same room events
Voice     │
Slack     ┘
```

## Room UI state

Rooms carry shared component/page state beyond the transcript:

```rust
struct RoomUiState {
    components_by_target: HashMap<(RenderTarget, ComponentId), RenderEvent>,
}

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

Agents can render into any target:

```json
{
  "room_id": "ocean-surface-map-fix",
  "target": "main.canvas",
  "kind": "map",
  "props": { "center": {...}, "markers": [...] },
  "replace": true
}
```

All participants see the updated room UI.

This is the merge of the two threads:

1. **Page-level agent-driven Surface UI** — agent can drive whole cockpit
2. **Room-based collaboration** — shared state across participants

## Execution isolation

Agent turns run in explicit contexts:

```rust
enum ExecutionMode {
    MainCheckout,              // current behavior, risky
    GitWorktree,               // preferred: isolated branch
    DetachedTempWorktree,      // disposable
    ReadOnly,                  // no filesystem mutations
}

struct ExecutionContext {
    mode: ExecutionMode,
    workspace_root: PathBuf,
    worktree_path: Option<PathBuf>,
    branch: Option<String>,
    base_commit: Option<String>,
}
```

Default for coding agents should be `GitWorktree`:

```txt
.ocean/worktrees/room-{room_id}-turn-{turn_id}/
branch: ocean/{room_slug}
```

Promotion to main checkout requires explicit confirmation.

## Room hydration / snapshot

Switching rooms must load full state, not just subscribe to future events.

```http
GET /v1/rooms/{room_id}/snapshot
```

Returns:

```json
{
  "room": { "id": "...", "name": "...", "workspace": "..." },
  "messages": [...],
  "turns": [...],
  "components": [...],
  "participants": [...],
  "active_turn": null,
  "last_seq": 1842
}
```

Client then subscribes:

```http
GET /v1/rooms/{room_id}/events?after_seq=1842
```

Two-step: snapshot first, then live tail.

Client guards against stale loads via requested-vs-current room check.

## Turn queue and locking

One active turn per agent at a time. Multiple agents can act concurrently.

```rust
struct TurnQueue {
    active: Option<(AgentId, TurnId)>,
    queued: VecDeque<(AgentId, TurnRequest)>,
}
```

Humans/observers can:

- queue prompts for agents
- cancel active turns
- approve/reject tool actions
- take/release "driver" role

## Component interactions in rooms

Component events carry room and participant attribution:

```json
{
  "room_id": "ocean-surface-map-fix",
  "component_id": "map-1",
  "participant_id": "surface-web:abc",
  "event": {
    "type": "marker_clicked",
    "payload": { "index": 2, "title": "Micro Center Fairfax" }
  }
}
```

The agent knows who clicked what.

## Example room

```txt
Room: #ocean-surface-map-fix

Participants:
  John (owner, human)
  @ocean (agent, coder)
  @reviewer (agent, review)
  @docsbot (agent, docs)
  TUI (client)
  Surface (client)

Events:
  John: @ocean fix POI markers in Surface
  @ocean: starting turn → renders progress, timeline, map
  @ocean: applied patch → renders diff, callout
  @ocean: @reviewer review this
  @reviewer: reviewing → found issue, @ocean look at ...
  @ocean: fixed → @reviewer re-review?
  @reviewer: ✅ approved
  @ocean: @docsbot update protocol docs for map fixes
  @docsbot: updated AGENT_RENDER_PROTOCOL.md → renders diff
  John: merge to main
  @ocean: confirmation? rendered confirm component
  John: confirmed
  @ocean: merged → success callout

UI state:
  main.canvas      → live map with POIs
  sidebar.right    → diff viewer
  status.bar       → turn progress
  chat.inline      → conversation stream
```

## MVP plan

### Phase 1 — Room primitive + participants

- `Room` data model in `ocean-core`
- `Participant` identity in turn/event model
- Room-scoped event subscriptions
- Backward-compatible: `session_id == room_id`

### Phase 2 — Agent profiles

- Named agent participants with configs
- Trigger policies (mention, schedule, component event)
- Capability sets per agent
- Context assembly for read-before-answer

### Phase 3 — Execution isolation

- Git worktree-backed execution contexts
- Branch/worktree per agent action
- Promotion/apply flow with confirmation

### Phase 4 — Shared UI state

- Room-scoped component registry
- Page-level render targets
- Snapshot + live tail hydration
- Client-room UI sync

### Phase 5 — Multi-transport

- Transport adapter layer
- Slack bridge as Ocean client
- Room access from TUI / Surface / CLI / Voice / Slack

## Related docs

- Agent render protocol: `docs/AGENT_RENDER_PROTOCOL.md`
- Surface component prompt guide: `docs/OCEAN_SURFACE_COMPONENT_PROMPT_GUIDE.md`
- Page-level Surface UI note: `docs/PAGE_LEVEL_AGENT_SURFACE_UI_NOTE.md`
- Architecture overview: `docs/ARCHITECTURE.md`
