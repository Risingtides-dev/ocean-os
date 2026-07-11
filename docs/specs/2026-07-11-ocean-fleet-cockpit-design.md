# Ocean Fleet Cockpit — Design Specification

- **Status:** Proposed
- **Date:** 2026-07-11
- **Scope:** ocean-runtime (agent_loop/capability/tools), ocean-agent-sdk (event vocabulary), ocean-agent (session/runtime plumbing), ocean-daemon (new `task` tool host + event bridges), ocean-tui (fleet + todo tree rendering, status line)
- **Out of scope (v1):** child-session disk persistence, fire-and-report task mode, WASM/subprocess capabilities for children, retrofitting `cost_usd` onto the root-turn `TurnFinished`/`AgentTurnResponse` wire contract, ocean-surface UI work (schema is surface-agnostic; surface rendering is a separate ticket)

All file/symbol anchors below were read directly from the live tree on 2026-07-11 and are exact at that revision.

---

## 1. What exists today (verified)

### 1.1 The engine to nest

`crates/ocean-runtime/src/agent_loop.rs`:
- `pub async fn run_agent(config: &AgentConfig, initial_prompt: Message, events: Option<mpsc::UnboundedSender<AgentEvent>>) -> Result<AgentRun>` (line 31) — thin wrapper over…
- `pub async fn run_agent_with_history(config: &AgentConfig, messages: Vec<Message>, events: …) -> Result<AgentRun>` (line 55), `#[instrument(name = "agent_loop", …)]`.
- `pub struct AgentRun { pub messages: Vec<Message>, pub stopped_at_turn_limit: bool, pub usage: ocean_protocol::Usage }` (line 23).
- Tool-call batches are scheduled by `Concurrency` (`crates/ocean-runtime/src/types.rs`, `AgentTool::concurrency()`): consecutive `Concurrency::Shared` tools in one assistant batch run **concurrently**; `Exclusive` (the default) is a barrier. This is the existing mechanism the `task` tool reuses for parallel fan-out — no new scheduler.
- Tool side effects (`ToolSideEffect` enum, types.rs) are forwarded onto the `events` sink at a single site in agent_loop.rs (~line 731 `for effect in &side_effects { match effect { ToolSideEffect::Render{..} => …, ToolSideEffect::SurfacePatch{..} => emit(events, AgentEvent::SurfacePatch{..}) (~768), ToolSideEffect::SlackCanvas{..} => … (~782) } }`) — this is the exact pattern a new `ToolSideEffect::TodoUpdated` plugs into.

### 1.2 Daemon concurrent-turn support + ceiling

`crates/ocean-daemon/src/main.rs`:
- `const DEFAULT_MAX_CONCURRENT_TURNS: usize = 24;` and `fn max_concurrent_turns() -> usize` reading `OCEAN_MAX_CONCURRENT_TURNS` (~line 600-624, doc comment at 583-599, "OCEAN-304").
- `type TurnLimiter = Arc<tokio::sync::Semaphore>;` (~line 594). `AppState.turn_limiter: TurnLimiter` (struct field ~line 234), built `Arc::new(Semaphore::new(max_concurrent_turns()))` inline inside the `AppState{..}` literal (~line 1510).
- The HTTP handler `async fn agent_turn(...)` (line 9179) takes a permit **before any work**: `let _turn_permit = match state.turn_limiter.clone().try_acquire_owned() { Ok(p) => p, Err(_) => return 429 … }` (~line 9213). Held for the whole handler via RAII drop.
- Contrast: `spawn_room_agent_turn` (the daemon's existing OTHER internal-turn-submission path, ~line 6642) does **not** acquire `turn_limiter` — a pre-existing backpressure gap in that path. The fleet cockpit's `task` tool MUST NOT repeat this gap: child turns acquire the SAME `turn_limiter`.

### 1.3 Per-session turn locks

`crates/ocean-agent/src/lib.rs`: `AgentRuntime.session_locks: Arc<std::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>` (struct field ~line 246, doc at 240-246) — "a turn against a session must hold this session's lock across load → run → save". Children in this design **never acquire a session lock** because they never call `AgentRuntime::prompt()` at all (see §4.6) — they run via `ocean_runtime::agent_loop::run_agent` directly, ephemeral, no disk session.

### 1.4 SSE stream + event_id_prefix correlation

`crates/ocean-agent-sdk/src/lib.rs`:
- `AgentTurnResponse { ok, turn_id, session_id, status, event_id_prefix: String, error?, output_tokens?, input_tokens?, cache_read_tokens?, tokens_per_second?, wall_ms? }` (~line 375-397). `event_id_prefix` = first 8 chars of `turn_id` (README.md:48).
- `AgentTurnEvent` (`#[serde(tag="type", rename_all="snake_case")]`, ~line 522) — 16 variants today: TurnStarted, ModelRerouted, AssistantTextDelta, ThinkingDelta, ToolCallStarted, ToolCallChunk, ToolCallFinished, TurnFinished, SessionCreated, ComponentRender, ComponentUnmount, BrowserActivity, SurfacePatch, SlackCanvas, Extension.
- `impl AgentTurnEvent { pub fn session_id(&self) -> Option<AgentSessionId> {..} }` (~line 715) — exhaustive match over all variants; `Extension` uses its own `scope: Option<AgentSessionId>` field (the "Invariant 5" council-wide exception). **New variants require a new match arm here.**

`crates/ocean-tui/src/shell/client.rs`: `DaemonClient::spawn_event_stream(session_id, actions, replay_first)` (line 161) subscribes `GET /v1/agent/events?session_id=…`, self-healing reconnect with `Last-Event-ID` replay, forwards each decoded event as `Action::AgentEvent(Box::new(evt))`.

`crates/ocean-daemon/src/main.rs`:
- `async fn agent_events(...)` (line 10950) — the SSE handler. Server-side scope filter: `fn should_emit_agent_event(want: Option<AgentSessionId>, all: bool, event: &AgentTurnEvent) -> bool` (line 11123): `(Some(want), Some(sid)) => sid == want` — i.e. **delivery is keyed purely on `event.session_id()`**, nothing else. This is the seam the fleet design exploits: every fleet event, at any depth, is stamped `session_id = <root operator session>` so it passes this filter with **zero server-side change**.
- `fn emit_agent(events: &EventBus, agent_events: &AgentEventBus, session_id: AgentSessionId, event: AgentTurnEvent)` (line 11647) — always publishes on `agent_events` (full fidelity); optionally mirrors onto the legacy `events` bus via `agent_to_ocean_event` (line 11703, exhaustive match, one arm per variant, several `=> None`). **New variants need an arm here too** (fleet/todo events → `None`, not mirrored, matching `SurfacePatch`/`SlackCanvas` precedent).
- `fn agent_event_type_name(event: &AgentTurnEvent) -> &'static str` (line 11723) — exhaustive match producing the SSE `event:` field. **New variants need an arm.**
- `mod tests { … RelayClass / classify_agent_event }` (~line 12024) — this classifies the **runtime-level** `ocean_runtime::types::AgentEvent` (not the SDK's `AgentTurnEvent`) into Relayed/Filtered; exhaustive match, doc'd as OCEAN-373 ("forcing whoever adds a variant to consciously choose"). Only the new `AgentEvent::TodoUpdated` runtime variant needs an arm here (Child* events never pass through this runtime `AgentEvent` type at all — see §4.1).

### 1.5 Folder-as-agent resolution (`/v1/agents`)

`docs/specs/folder-as-agent.md` + `crates/ocean-agent/src/agentdir.rs`:
- `pub fn resolve(root: &Path, name: &str) -> Result<AgentDef, ResolveError>` (line 226); `pub struct AgentDef { name, root, …, tools: Vec<String>, subagents: Vec<String> }` (line 155-170, `subagents` field at 167-169: "Names of child agents under `subagents/`, resolvable with `resolve` against `<root>/subagents`").
- `subagents` is **already validated** (a dangling reference in `subagents/` gets a warn diagnostic, lines 477-487) but **is never dispatched at runtime anywhere in the codebase** (confirmed: no `subagents` reference outside agentdir.rs itself and its tests; ROADMAP.md has no subagent-dispatch entry). This design is what makes it live.
- Daemon: `fn agents_root() -> PathBuf` (line 2994, `OCEAN_AGENTS_DIR` env else `<config>/agents`); `agent_turn` resolves `agent: Option<String>` via `ocean_agent::agentdir::resolve(&agents_root(), name)` (~line 9443), extracting `effective_tools()`, `config.model`, `config.subprocess_capabilities`, folded into `PromptControl` via `.with_tool_allowlist(…)` / `.with_agent_model(…)` / `.with_agent_capabilities(…)` (~line 9433-9450, 9906-9922).

### 1.6 Session store — what's currently persisted

`crates/ocean-agent/src/lib.rs`, `mod session` (line 2604): `pub struct Session { id, created_ms, updated_ms, model, provider, messages: Vec<Message>, workspace_root, cwd, git_branch, git_commit, client_type }` (line 2610-2634) — JSON file on disk, one per `SessionId`, loaded/saved by `AgentRuntime::prompt()`. No token/cost/todo fields today.

### 1.7 Token/context accounting (root turns)

`crates/ocean-agent/src/lib.rs`, inside `agent_turn`'s post-`prompt()` handling (main.rs ~9938-9950): `res.usage: ocean_core::TokenUsage { input, output, cache_read, cache_write, total_tokens }` — real provider usage summed across rounds; `output_tokens` falls back to `estimate_visible_tokens(&res.stdout)` when the provider reported none; `tokens_per_second = output_tokens / (wall_ms/1000)`. **No `cost_usd` field or pricing table exists anywhere in the workspace** (grepped, zero hits for `cost_usd`/`price_per_token`). `ocean_protocol::Model { .., context_window: u32, max_tokens: u32 }` (types.rs:202-218) — `context_window` IS available per-model, so `context_percent` is trivially `(input+cache_read)/context_window*100` once a `Model` struct is in hand.

### 1.8 TUI status-line pattern ("OMP-style composable dashboard")

`crates/ocean-tui/src/shell/status.rs` — doc comment literally: "Status-line segments — the workbench's always-on dashboard, ported from oh-my-pi's composable status bar (OMP Slice 4)". Pure `fn segments(d: &StatusData) -> Vec<Segment>`; a segment whose value is `None` is skipped ("never shows empty slots"). Rendered in `crates/ocean-tui/src/shell/app.rs::draw_status` (line 3090). This is the existing machinery the new throughput/fleet segment extends — not a new subsystem.

### 1.9 TUI component/pane conventions

`crates/ocean-tui/src/shell/component.rs`: `trait Component { fn handle_event/handle_key/handle_mouse/handle_paste/update/tick/draw(&mut self, frame, area) }`.
`crates/ocean-tui/src/shell/app.rs`: `enum Center { Chat, Editor, Graph }` (line 76-79), `enum Focus { Sessions, Tree, Center }` (line 84-87). Toggling a pane is a repeated ~8-site pattern (keybind match arm, mouse `Btn::Graph` arm, `handle_event` dispatch, focus-binding `self.graph.focused = …`, crumb string, `draw` dispatch, button-bar highlight, `focus_name` in `draw_status`) — verified at lines 869-871, 908-926, 973-976, 1018-1034, 1109-1176, 2732-2734, 2897-2916, 3050-3103. Adding `Center::Fleet` is this exact repeated pattern, once.
`crates/ocean-tui/src/shell/app.rs::dispatch` (line 1055): `Action::AgentEvent(evt) => { if let (Some(bound), Some(evt_sid)) = (self.session_id, evt.session_id()) { if bound != evt_sid { return; } } … }` (line 1069-1076) — **any AgentEvent whose `session_id()` doesn't match the currently-bound session is dropped before any component sees it.** This is WHY fleet/todo events must be stamped with the root session id (§1.4) — there is no other channel.

### 1.10 Existing bug found while reading: todo state is process-global, not session-scoped

`crates/ocean-runtime/src/tools/todo.rs` (today): `pub struct TodoTool { items: Mutex<Vec<TodoItem>> }`, constructed once in `default_tools()` (`crates/ocean-runtime/src/tools/mod.rs:40`) and returned as the SAME `Arc` clone on every `BuiltinProvider::tools()` call. `BuiltinProvider::tools(ctx)` (`crates/ocean-runtime/src/capability.rs:196-284`) rebinds `component_wait`, `slack_canvas`, `bash`, `read`, `write`, `edit`, `ls`, `grep`, `glob` per-session (keyed by `ctx.session_id`, mirroring `snapshots_for`/`artifacts_for`) — **`todo` is conspicuously absent from that rebind list.** Today, every session and every concurrent turn on the daemon shares ONE todo list. v2 fixes this using the exact same `*_for(session_id)` pattern already established for hashline snapshots and artifact stores.

---

## 2. Task tool semantics

### 2.1 Where it lives (and why not ocean-runtime)

`ocean-agent` depends on `ocean-runtime` (`crates/ocean-agent/Cargo.toml`: `ocean-runtime.workspace = true`); `ocean-runtime` does **not** depend on `ocean-agent` (confirmed via its Cargo.toml — no such dependency). A `task` tool needs to run a nested turn through `AgentRuntime`-shaped machinery (or reuse daemon internals). If the tool were defined in `ocean-runtime`, giving it a way to call back into `AgentRuntime` would require `ocean-runtime → ocean-agent`, closing a cycle (`ocean-agent → ocean-runtime → ocean-agent`). **Resolution:** the concrete `TaskTool`/`TaskProvider` live in `ocean-daemon` (a new file `crates/ocean-daemon/src/fleet.rs`), which already depends on both `ocean-agent` and `ocean-runtime` (confirmed, `crates/ocean-daemon/Cargo.toml`). `ocean-runtime`'s `CapabilityProvider` trait (`crates/ocean-runtime/src/capability.rs:81`) is already the generic "bolt on a tool source" seam (MCP, plugin subprocess, longhouse all register this way) — `TaskProvider` is just one more implementor, registered from the daemon.

### 2.2 The seam: `AgentRuntime::with_task_provider`

`ocean-agent`'s `build_capability_registry` (private, lib.rs:1868) already composes `BuiltinProvider` + MCP/plugin/longhouse providers into `self.capabilities`, and the daemon already calls `AgentRuntime::from_env()?.with_extensions(Some(longhouse.clone())).await` (main.rs:1421-1424) to hand the daemon's own `longhouse` handle down into that composition. Add one more **generic, daemon-agnostic** builder:

```rust
// crates/ocean-agent/src/lib.rs, alongside with_extensions
impl AgentRuntime {
    /// Bolt an extra capability provider onto the registry AFTER the builtin +
    /// MCP/plugin/longhouse providers already assembled by `with_extensions`.
    /// Generic over `CapabilityProvider` so ocean-agent stays daemon-agnostic —
    /// the concrete provider (e.g. the daemon's TaskProvider) is constructed and
    /// owned by the caller. Consuming builder, mirrors `with_extensions`.
    pub fn with_task_provider(mut self, provider: std::sync::Arc<dyn ocean_runtime::capability::CapabilityProvider>) -> Self {
        let mut providers: Vec<_> = self.capabilities.providers().to_vec();
        providers.push(provider);
        self.capabilities = std::sync::Arc::new(ocean_runtime::capability::CapabilityRegistry::new(providers));
        self
    }

    /// Resolve a bare model alias (or `None` → current global model) to a full
    /// `ocean_protocol::Model`. Thin public wrapper around the same resolution
    /// path `prompt()` already uses internally to build `AgentConfig.model` —
    /// needed by the daemon's TaskTool, which builds a child `AgentConfig`
    /// WITHOUT going through `prompt()` (ephemeral children, see §4.6).
    pub fn resolve_model(&self, model_id: Option<&str>) -> anyhow::Result<ocean_protocol::Model> { /* delegates to the existing private alias-resolution match already used at ~lib.rs:2278-2303 */ }
}
```

(`CapabilityRegistry::providers(&self) -> &[Arc<dyn CapabilityProvider>]` already exists, capability.rs:315 — the above just needs it to also be reachable from ocean-agent's Cargo dependency graph, which it already is transitively; only a rebuild-the-Vec-and-reconstruct is new.)

### 2.3 Bootstrapping (`OnceLock<Weak<AgentRuntime>>`)

`TaskProvider` needs a way to reach `AgentRuntime` (for `.capabilities()`/`.resolve_model()`/`.current_model()`) **and** the daemon's `AppState`-shaped internals (`AgentEventBus`, `TurnLimiter`, `RequestRegistry`) — but `AppState.runtime: Arc<AgentRuntime>` is what's being *constructed* when `TaskProvider` must already exist (registry composition happens inside `AgentRuntime::from_env()....with_task_provider(...)`, before the surrounding `Arc::new(..)` at main.rs:1421-1425). Standard two-phase fix, applied at the exact construction site:

```rust
// crates/ocean-daemon/src/main.rs, main(), replacing lines ~1420-1511
let agent_events = AgentEventBus::new(4096); // hoisted OUT of the AppState{} literal
let requests: RequestRegistry = Arc::new(RwLock::new(HashMap::new())); // hoisted
let turn_limiter: TurnLimiter = Arc::new(tokio::sync::Semaphore::new(max_concurrent_turns())); // hoisted

let runtime_cell: Arc<std::sync::OnceLock<std::sync::Weak<AgentRuntime>>> = Arc::new(std::sync::OnceLock::new());
let task_provider = Arc::new(fleet::TaskProvider::new(fleet::FleetCtx {
    runtime: runtime_cell.clone(),
    agent_events: agent_events.clone(),
    turn_limiter: turn_limiter.clone(),
    requests: requests.clone(),
    agents_root: agents_root(),
}));

let runtime = Arc::new(
    AgentRuntime::from_env()?
        .with_extensions(Some(longhouse.clone())).await
        .with_task_provider(task_provider),
);
runtime_cell.set(Arc::downgrade(&runtime)).ok(); // now resolvable

let state = AppState { runtime, events: EventBus::new(1024), agent_events, requests, turn_limiter, /* … unchanged fields … */ };
```

This is mechanical and low-risk: `agent_events`/`requests`/`turn_limiter` are unchanged VALUES, only their *construction order* moves earlier so their `Arc`s can be shared with `FleetCtx` before `AppState` exists. Every existing test helper (`permission_test_state`, `fake_convene_state`, `escrow_state_with_titles_db` — main.rs ~15580, ~16712, ~17231) calls `AgentRuntime::from_env()` **without** `.with_task_provider(...)`, so those tests get a registry with no `task` tool at all — fail-open, zero test breakage.

### 2.4 `TaskTool` — schema, execution, and where it lives in the tree

```json
{
  "name": "task",
  "description": "Dispatch a focused subtask to a child Ocean agent that runs to completion and reports its result back as this call's output. Issue several `task` calls in the SAME assistant turn to fan work out in parallel — Ocean runs concurrent task calls in one batch simultaneously and renders them as sibling nodes in the fleet tree.",
  "parameters": {
    "type": "object",
    "properties": {
      "prompt":   {"type": "string", "description": "The task/instructions for the child agent."},
      "label":    {"type": "string", "description": "Short (<=60 char) human-readable description shown live in the fleet tree, e.g. 'audit auth module'."},
      "agent":    {"type": "string", "description": "Optional named folder-as-agent to run the child as. At the root, any name under GET /v1/agents. When THIS turn is itself a named agent, restricted to that agent's own subagents/ (see docs/specs/folder-as-agent.md). Omit for a generic subagent."},
      "model":    {"type": "string", "description": "Optional model alias override; defaults to the orchestrator's current model."}
    },
    "required": ["prompt", "label"]
  }
}
```

`fn concurrency(&self) -> Concurrency { Concurrency::Shared }` — children have no shared mutable state with each other, so a batch of `task` calls is safe to fan out via the agent loop's EXISTING scheduler (types.rs `Concurrency` enum + its documented "maximal runs of consecutive Shared tools execute concurrently" behavior). **No new scheduling code.**

**Semantics: sync-wait, not fire-and-report.** `execute()` `.await`s the full child run and returns the child's final answer as the tool's `AgentToolResult` — the orchestrating model reads the child's answer exactly like any other tool result and continues its own turn. Rationale: a tool call inside a running turn has no separate "come back later" consumer other than the SAME model in the SAME turn; `Concurrency::Shared` already gives fan-out-then-gather for free when the model issues N `task` calls in one batch. Fire-and-report (`task` returns immediately with a handle; a follow-up `task_status`/`task_wait` tool polls it) is a real, larger v2 feature — explicitly deferred (§7).

### 2.5 `execute()` — step by step (in `fleet.rs`)

```rust
async fn execute(&self, tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
    // 1. Parse args (prompt, label required; agent/model optional).
    // 2. Depth is already capped by TaskProvider not offering this tool past
    //    max depth (§4.3) — defensive re-check here anyway (stale tool list).
    // 3. Resolve `agent` name: at depth 0, resolve against agents_root() directly;
    //    at depth>=1, resolve against `<parent_agent_root>/subagents/<name>`
    //    (ocean_agent::agentdir::resolve) — REJECTS a name outside the calling
    //    agent's declared subagents/ (a real, currently-dormant `AgentDef.subagents`
    //    field, agentdir.rs:167, comes alive here).
    // 4. Acquire a turn_limiter permit: try_acquire_owned() — same OCEAN-304
    //    backpressure as root turns; on failure, return Err("fleet at concurrent-
    //    turn capacity — retry, or reduce fan-out") as an ordinary tool error
    //    (visible to the model, not a panic).
    // 5. Mint child_turn_id = AgentTurnId::new_v4(); register it in `requests`
    //    (RequestRegistry) via a new small helper `insert_running_request` —
    //    EXTRACTED from `register_running_request` (main.rs:8547) so both the
    //    HTTP path and TaskTool share one insertion routine instead of
    //    duplicating RequestControl construction.
    // 6. Build the child's cancel token: self.parent_cancel.child_token() —
    //    native tokio_util propagation; a parent Halt (POST /v1/requests/{id}/
    //    cancel, or the 2026-07-10 immediate-halt fix at the provider-stream
    //    read boundary) trips every descendant's token in the SAME instant,
    //    with no extra polling.
    // 7. Reuse self.parent_permission (the SAME Arc<dyn PermissionPolicy> the
    //    calling turn is running under) UNCHANGED — see §4.1 for why this
    //    alone satisfies "never wider".
    // 8. Resolve the child Model: explicit `model` arg -> runtime.resolve_model
    //    (Some(alias)); else runtime.resolve_model(None) (current global model).
    // 9. Build the child's tool list: registry.tools_for_session(&child_ctx)
    //    where child_ctx = SessionContext { cwd: self.cwd.clone(),
    //    session_id: Some(self.route_session_id.clone()) /* ROOT session id,
    //    UNCHANGED at every depth — see §4.2 */, hashline/artifacts: inherited,
    //    lineage: Some(TurnLineage { turn_id: child_turn_id.to_string(),
    //    depth: self.depth + 1, permission: parent_permission.clone(),
    //    cancel: child_cancel.clone(), agent_name: resolved_child_agent_name }) }.
    //    (This is the SAME registry.tools_for_session the daemon calls for root
    //    turns — reused directly, not reimplemented.)
    // 10. Build AgentConfig { model, system_prompt: <folder-as-agent instructions
    //     OR the minimal built-in "Ocean subagent" prompt>, tools, permission,
    //     session_id: Some(self.route_session_id.clone()) /* stamps every
    //     runtime AgentEvent — reused for the bus-emit mapping below */,
    //     stream_options.cancel: Some(child_cancel) }.
    // 11. Spawn a LOCAL bridge task (mirrors main.rs's existing per-turn bridge,
    //     ~line 9518-9603, but writing the NEW Child* SDK variants instead of
    //     the plain ones): emit ChildTurnStarted immediately, then map each
    //     runtime AgentEvent::{TextDelta,ToolExecutionStart,ToolExecutionEnd} to
    //     ChildAssistantTextDelta/ChildToolCallStarted/ChildToolCallFinished on
    //     self.agent_events. ThinkingDelta is intentionally NOT relayed for
    //     children in v1 (keeps SSE volume bounded under deep fan-out — a
    //     stated simplification, not an oversight).
    // 12. `run_agent(&child_config, Message::user_text(prompt), Some(event_tx)).await`.
    // 13. Drop the turn_limiter permit (RAII); mark the request registry entry
    //     terminal; compute cost_usd/context_percent (§4.5); emit
    //     ChildTurnFinished.
    // 14. Return AgentToolResult::text(<child's last assistant message text> +
    //     a compact stats line, e.g. "\n\n[child completed: 1,204 tokens, 4 tool
    //     calls, 8.2s]") — this IS the tool's result the orchestrator reads.
}
```

---

## 3. Todo tool semantics (v2 — phased, session-scoped)

### 3.1 Schema

```json
{
  "name": "todo",
  "description": "Track a phased execution plan, visible LIVE to the operator in the fleet cockpit's todo tree and shared by every child agent working under this session. action=set_phases replaces the whole plan; add appends an item to a phase (creating the phase if new); update changes one item's status; clear empties the plan; list reads it back.",
  "parameters": {
    "type": "object",
    "properties": {
      "action": {"type": "string", "enum": ["set_phases", "add", "update", "clear", "list"]},
      "phases": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "items": {"type": "array", "items": {"type": "string"}}}}, "description": "for set_phases: the full plan — phase names + item texts, all start pending"},
      "phase":  {"type": "string", "description": "for add: which phase to append to (created if absent)"},
      "text":   {"type": "string", "description": "for add: the item text"},
      "id":     {"type": "string", "description": "for update: the item id returned by a prior add/list/set_phases"},
      "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "blocked"], "description": "for update"}
    },
    "required": ["action"]
  }
}
```

### 3.2 State model (new — `ocean-agent-sdk`, since it's shared daemon↔client vocabulary, matching how `ToolResult`/`SurfacePatch` already live there)

```rust
// crates/ocean-agent-sdk/src/lib.rs (or a new `pub mod todo;` inside it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus { Pending, InProgress, Completed, Blocked }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem { pub id: String, pub text: String, pub status: TodoStatus }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoPhase { pub name: String, pub items: Vec<TodoItem> }
```

### 3.3 Rewiring `TodoTool` (fixes the session-leak bug, §1.10)

`crates/ocean-runtime/src/tools/todo.rs`: replace `Mutex<Vec<TodoItem>>` with a tool constructed **per-session** exactly like `ComponentWaitTool`/`SlackCanvasTool`/`ReadTool` already are:

```rust
pub struct TodoTool { store: Arc<Mutex<Vec<ocean_agent_sdk::TodoPhase>>> }
impl TodoTool {
    pub fn new() -> Self { Self { store: Arc::new(Mutex::new(Vec::new())) } } // unbound (test/ad-hoc)
    pub fn for_session(store: Arc<Mutex<Vec<ocean_agent_sdk::TodoPhase>>>) -> Self { Self { store } }
}
```

`crates/ocean-runtime/src/capability.rs`, `BuiltinProvider`: add a fourth session-keyed map alongside `snapshots`/`artifacts`/`noop_guards` (lines 113-134):

```rust
todos: std::sync::Mutex<std::collections::HashMap<String, Arc<Mutex<Vec<ocean_agent_sdk::TodoPhase>>>>>,
```
plus `fn todos_for(&self, session_id: &str) -> Arc<Mutex<Vec<TodoPhase>>>` (get-or-insert, same shape as `snapshots_for`), and in `tools()`'s `match tool.name() { … }` (line ~200-244) add:
```rust
"todo" => { *tool = Arc::new(crate::tools::todo::TodoTool::for_session(self.todos_for(session_id))); }
```
**Because `ctx.session_id` is left UNCHANGED at every fan-out depth (§2.5 step 9), the root turn and every one of its children share the SAME `todos_for(root_session_id)` store** — i.e. the todo list is the orchestrator's shared execution plan, and a child marking its own phase's item `completed` is immediately visible to the root's next `todo list` call and to the TUI, with zero extra plumbing.

Every mutating action returns `ToolSideEffect::TodoUpdated { phases: <current snapshot> }` in `AgentToolResult.side_effects` (including `clear`; `list` also emits for idempotent-UI-refresh simplicity — no special-casing).

### 3.4 New event plumbing (mirrors the existing `SurfacePatch` side-effect path exactly)

```rust
// crates/ocean-runtime/src/types.rs
pub enum ToolSideEffect { /* … existing … */ TodoUpdated { phases: Vec<ocean_agent_sdk::TodoPhase> } }
pub enum AgentEvent { /* … existing … */ TodoUpdated { session_id: Option<String>, phases: Vec<ocean_agent_sdk::TodoPhase> } }
// + one new arm in AgentEvent::session_id() (types.rs ~line 385)
```
```rust
// crates/ocean-runtime/src/agent_loop.rs, the side-effect forward loop (~line 731)
ToolSideEffect::TodoUpdated { phases } => {
    emit(events, AgentEvent::TodoUpdated { session_id: sid.clone(), phases: phases.clone() });
}
```
```rust
// crates/ocean-agent-sdk/src/lib.rs — new top-level AgentTurnEvent variant
TodoUpdated { session_id: AgentSessionId, turn_id: AgentTurnId, phases: Vec<TodoPhase> },
// + one new arm in AgentTurnEvent::session_id() (~line 715)
```
```rust
// crates/ocean-daemon/src/main.rs — the per-turn bridge match (root turns, ~line 9600-9780)
AgentEvent::TodoUpdated { phases, .. } => {
    bridge_bus.emit(AgentTurnEvent::TodoUpdated { session_id: bridge_session_id, turn_id: bridge_turn_id, phases });
}
// + AgentTurnEvent::TodoUpdated { .. } => None in agent_to_ocean_event (line 11703)
// + "todo_updated" in agent_event_type_name (line 11723)
// + AgentEvent::TodoUpdated{..} added to the Relayed bucket in classify_agent_event (test-only, ~line 12030)
```
(Children's `TodoUpdated` events ride the SAME per-child bridge described in §2.5 step 11 — one more arm there too, same mapping.)

### 3.5 Persistence: session-scoped, root-turn-only

`crates/ocean-agent/src/lib.rs`, `Session` struct (line 2610): add `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub todos: Vec<ocean_agent_sdk::TodoPhase>` (requires adding `ocean-agent-sdk.workspace = true` to `crates/ocean-agent/Cargo.toml` — legal, no cycle: dependency order is `ocean-core ← ocean-agent-sdk ← ocean-runtime ← ocean-agent ← ocean-daemon`, and `ocean-runtime` already depends on `ocean-agent-sdk` directly, `ocean-agent` merely needs it declared to use it directly per Rust's edition-2021 extern-prelude rule). `AgentRun` (agent_loop.rs:23) gains `pub todos: Option<Vec<TodoPhase>>`, populated by tracking the LATEST `AgentEvent::TodoUpdated` seen during the run's own emission loop (no extra plumbing — the value is already flowing through the SAME function). `AgentRuntime::prompt()` (lib.rs:540), after `run_agent_with_history` returns, does `if let Some(todos) = run.todos { session.todos = todos; }` before the existing save call — reusing the SAME end-of-turn disk write, no incremental per-mutation I/O.

Deliberate scope cut: **child (task-tool) todo mutations are visible live over SSE but are NOT separately persisted** — only the root turn's post-run snapshot lands on `Session.todos`, and since children share the root's in-memory store (§3.3), the root's own next todo tool call (or its final snapshot) already reflects everything children did. `AgentSessionResponse` (`ocean-agent-sdk`) gains `#[serde(default)] pub todos: Vec<TodoPhase>`, populated at the daemon's session-detail assembly (`main.rs` ~line 11338, from `SessionDetail`/`Session.todos`, wired alongside the existing `session::detail` path at `ocean-agent/src/lib.rs:1072` and its `fn session_detail(session: Session) -> SessionDetail` helper at ~line 3189) — so a resumed session or a surface loading history renders the plan without replaying the whole SSE history.

---

## 4. Cross-cutting design decisions

### 4.1 Permission inheritance, parent → child, never wider

No new policy type, no new enforcement code. `PromptControl.permission: Arc<dyn PermissionPolicy>` (lib.rs:1676) is captured once per turn into `TurnLineage.permission` (a NEW field, §4.2) and **reused as the literal same `Arc`** for every child spawned from that turn (§2.5 step 7). A child cannot be more permissive than its parent because it isn't a *different* policy at all — it's the identical object making the identical allow/deny decisions (`DaemonPermissionPolicy`, main.rs ~2720, gates on `allow_mutating`/tool name, unaffected by which turn is asking). Grandchildren inherit transitively the same way (the child, when building ITS OWN grandchild ctx, passes `self.parent_permission.clone()` unchanged again). "Never wider" is therefore a structural invariant, not a runtime check — nothing to get wrong.

### 4.2 Cancellation propagation

Reuses `tokio_util::sync::CancellationToken::child_token()` (native to the crate already in every relevant Cargo.toml — `tokio-util.workspace = true` in ocean-runtime, ocean-agent, ocean-daemon). The parent turn's cancel token — the SAME one `register_running_request` (main.rs:8547) stores in `RequestControl.cancel` and the SAME one the 2026-07-10 immediate-halt design (`docs/superpowers/specs/2026-07-10-immediate-provider-halt-design.md`) made the provider-stream read boundary race against — is captured into `TurnLineage.cancel`. Each child's OWN token is `parent_cancel.child_token()`, registered as ITS OWN `RequestControl.cancel` too (so a child can ALSO be halted independently via `POST /v1/requests/{child_turn_id}/cancel`). tokio_util trips every descendant token in the same synchronous instant the ancestor trips — no polling loop, no extra latency, and it composes for free with the just-shipped biased-`select!` fix at the stream-read boundary (agent_loop.rs) since that fix reads `cancelled(config)` off whatever token is wired into `AgentConfig.stream_options.cancel`, and a child's config carries its own (parent-derived) token.

### 4.3 Recursion depth cap

```rust
// crates/ocean-daemon/src/fleet.rs
const DEFAULT_MAX_TASK_DEPTH: u8 = 3;
fn max_task_depth() -> u8 { /* OCEAN_MAX_TASK_DEPTH env, else default — same parse-or-warn-default shape as main.rs's max_concurrent_turns() (line 600) */ }
```
Enforced **fail-closed at the tool-offering seam**, not inside `execute()`: `TaskProvider::tools(ctx: &SessionContext) -> Vec<SharedTool>` reads `ctx.lineage.as_ref().map(|l| l.depth).unwrap_or(0)`; if `depth >= max_task_depth()`, it returns `vec![]` (no `task` tool at all in the toolset that depth's model sees) instead of a tool that errors when called. A model at the cap literally cannot attempt further recursion — cleaner than a runtime error the model has to interpret and retry around. `execute()` still defensively re-checks depth (belt-and-suspenders against a stale cached tool list), returning a plain `Err(String)` tool error in that case.

### 4.4 Child event namespacing (how the TUI builds the tree)

Six new `AgentTurnEvent` variants (`ocean-agent-sdk`), each carrying `session_id: AgentSessionId` (ALWAYS the root session — routing key, unchanged at every depth), `parent_turn_id: AgentTurnId` (the immediate parent's turn — root's own `TurnStarted.turn_id`, or another child's `child_turn_id`), and `child_turn_id: AgentTurnId` (this node's own id):

```rust
ChildTurnStarted { session_id: AgentSessionId, parent_turn_id: AgentTurnId, child_turn_id: AgentTurnId, depth: u8, model: Option<String>, label: String },
ChildAssistantTextDelta { session_id: AgentSessionId, parent_turn_id: AgentTurnId, child_turn_id: AgentTurnId, delta: String },
ChildToolCallStarted   { session_id: AgentSessionId, parent_turn_id: AgentTurnId, child_turn_id: AgentTurnId, call: ToolCall },
ChildToolCallFinished  { session_id: AgentSessionId, parent_turn_id: AgentTurnId, child_turn_id: AgentTurnId, call_id: ToolCallId, result: ToolResult },
ChildTurnFinished {
    session_id: AgentSessionId, parent_turn_id: AgentTurnId, child_turn_id: AgentTurnId,
    status: AgentTurnStatus, error: Option<String>, wall_ms: Option<u64>,
    output_tokens: Option<u64>, input_tokens: Option<u64>, cache_read_tokens: Option<u64>,
    tokens_per_second: Option<f64>,
    context_percent: Option<f32>,   // NEW even for root — see §4.5
    cost_usd: Option<f64>,          // NEW — see §4.5
},
```
(`ToolCall`/`ToolResult` are the EXISTING SDK types, reused verbatim — no duplication.) These are DISTINCT variant names, not a `parent_turn_id: Option<_>` field bolted onto the existing `ToolCallStarted`/`AssistantTextDelta`/etc. This is a deliberate choice: the TUI's `chat.rs::update()` (line 1833 `ToolCallStarted`, etc.) already ends in a catch-all `_ => {}` (line 1877), so new variants are automatically invisible to the main transcript with **zero changes to chat.rs** — no risk of child output leaking into the operator's transcript by an unguarded `if child.is_some()` check someone forgets in one of five match arms. ocean-surface, per its own documented forward-compat contract ("Clients should ignore unrecognised event kinds"), needs no change to keep working; rendering the fleet tree there is a natural follow-up, out of scope here.

The TUI reconstructs the whole tree purely from `(parent_turn_id, child_turn_id)` edges — no `root_turn_id` field needed; the root's own edge is `parent_turn_id == <the root TurnStarted.turn_id the TUI already tracks>`.

### 4.5 Per-child cost + context accounting

**Context %:** `TaskTool` already has the resolved `ocean_protocol::Model` in hand for step 8 of `execute()` (§2.5) — `context_percent = (usage.input + usage.cache_read) as f32 / model.context_window as f32 * 100.0`. No lookup table needed; reuses the field that's been on `Model` all along (`crates/ocean-protocol/src/types.rs:216`).

**Cost:** no pricing machinery exists anywhere in the workspace today (verified, zero hits for `cost_usd`/`price_per_token`). New minimal, explicitly-approximate static table:

```rust
// crates/ocean-protocol/src/pricing.rs (new file)
/// USD per 1,000,000 tokens: (input, output). Deliberately NOT exhaustive and
/// NOT live-updated — an unrecognized model id returns `None`, never a
/// fabricated number. Seeded from the model constructors already in types.rs
/// (claude-sonnet-5, claude-opus-4-8, gpt-4o, gpt-5.x/codex, gemini-2.5-pro, …).
pub fn price_per_million(model_id: &str) -> Option<(f64, f64)> { /* static match/table */ }
```
Used only by `fleet.rs`'s `ChildTurnFinished` emission: `cost_usd = price_per_million(&model.id).map(|(pin, pout)| (usage.input as f64/1e6)*pin + (usage.output as f64/1e6)*pout)`. Deliberately scoped to CHILD events only in v1 (§7 non-goals) — retrofitting onto the root `TurnFinished`/`AgentTurnResponse` is a trivial, purely-additive follow-up (every existing consumer already tolerates new `skip_serializing_if` optional fields) but is not required by this ask and is left out to keep the wire-contract diff minimal.

### 4.6 What persists

**Children are ephemeral by design — no disk session file, no `SessionId`, no entry in `GET /v1/agent/sessions`.** `TaskTool::execute()` calls `ocean_runtime::agent_loop::run_agent` **directly**, never `AgentRuntime::prompt()` — so none of `AgentRuntime`'s session-load/save/lock machinery (`session_locks`, §1.3) is touched for a child at all. Rationale: threading a genuinely separate "routing session id" vs "storage session id" through `AgentConfig`/`SessionContext` (which today conflate the two into one `session_id: Option<String>`) would be materially more plumbing for a benefit (independent resumability of a subagent run) nobody asked for in this ask. What DOES persist: (a) the root operator `Session.todos` snapshot at end of turn (§3.5) — captures the FINAL shared plan state including everything children did to it; (b) the daemon log lines for each child turn (`turn_id`/`request_id`/`session_id` span fields, same tracing infra as root turns); (c) nothing else. A crashed/killed child leaves no orphaned session file to clean up. If child-transcript persistence/resumability is wanted later, it's an additive v2 (give children their own `SessionId` + a `route_session_id` field split out from the storage one) — explicitly deferred (§7).

### 4.7 Event schema is surface-agnostic (ocean-surface)

All six `Child*` variants plus `TodoUpdated` live in `ocean-agent-sdk`, the crate whose own doc comment states it defines "the typed product vocabulary shared by the Ocean daemon and all Ocean clients (TUI, CLI, future SDK consumers)" — no ocean-daemon-only or ocean-tui-only type leaks into the wire schema (`ToolCall`/`ToolResult`/`TodoPhase` are all plain, already-shared SDK types). ocean-surface requires **zero code change** to keep working (unknown `type` tags are ignored per its own documented contract) and can pick up fleet-tree rendering whenever wanted as a separate, independent piece of work.

---

## 5. TUI rendering plan

### 5.1 New pane: `Center::Fleet`

`crates/ocean-tui/src/shell/app.rs`: add `Fleet` to `enum Center` (line 76-79) and repeat the existing 8-site Graph-toggle pattern verbatim (keybind arm ~908-926, mouse `Btn::Fleet` ~1018-1034, `handle_event` dispatch ~973-976, `self.fleet.focused = …` ~2732-2734, crumb string ~2897-2900, `draw` dispatch ~2913-2916, button-bar entry ~3050-3060, `focus_name` match in `draw_status` ~3100-3103).

### 5.2 New component: `FleetComponent`

`crates/ocean-tui/src/shell/components/fleet.rs` (new file, registered in `components/mod.rs`), implementing `Component` (component.rs). Two independent trees sharing the pane (top/bottom split, or tab-switchable — implementer's call, follow `SessionRailComponent`'s two-level `DirGroup`/`BranchGroup`/flattened-`Row` pattern, `crates/ocean-tui/src/shell/components/session_rail.rs:1-103`, for the flatten-for-scroll technique):

**Agent tree** (built from `ChildTurnStarted`/`ChildToolCallStarted`/`ChildToolCallFinished`/`ChildTurnFinished`, keyed by `(parent_turn_id, child_turn_id)` edges — the root itself is a synthetic node keyed off the pane's already-known `TurnStarted`/`TurnFinished`, so a fleet with zero `task` calls just shows one root row, no special-casing an "empty tree"). Per-node line:
```
▸ audit auth module          claude-sonnet-5   ● 12 tools · ctx 34% · $0.08 · 8.2s
  ▸ check JWT expiry          claude-haiku-4-5  ✓ 3 tools · ctx 12% · $0.01 · 2.1s
```
- tool-call count: running tally of `ChildToolCallStarted` seen for that `child_turn_id` (increment on Started, no need to wait for Finished).
- context %: from the node's latest `ChildTurnFinished.context_percent` (live nodes show a `…` placeholder until they finish, or estimate from `ChildAssistantTextDelta` byte-count / `context_window` — implementer's call, static after finish either way).
- cost: `ChildTurnFinished.cost_usd`, formatted via the SAME `fmt_count`-style compact formatter already in `status.rs` (adapt, don't duplicate).
- model: `ChildTurnStarted.model`.
- elapsed: wall-clock from local `Instant` captured on `ChildTurnStarted` receipt (not `wall_ms`, which only arrives at Finished) — ticks live via the existing `Component::tick()` hook, matching how PTY panes already self-refresh.
- status glyph: `●` running / `✓` completed / `✗` failed / `⊘` cancelled — same glyph vocabulary `chat.rs`'s `ToolStatus` already uses (`g("✗", "X")` ASCII-fallback helper, `theme::g`).

**Todo tree** (built from `AgentTurnEvent::TodoUpdated`, which carries the FULL phase list each time — the component just replaces its whole model, no diffing needed):
```
▾ Research                    2/3
  ✓ read auth module
  ✓ list JWT libraries
  ○ audit token refresh flow   (in progress)
▸ Implement                   0/2
```

### 5.3 Wiring into `app.rs::dispatch`

`Action::AgentEvent(evt)` (line 1069) already filters to the bound session before any component sees it — the new `Child*`/`TodoUpdated` variants pass that filter for free (§4.4, stamped with root session id). Add, alongside the existing `self.chat.update(&action)` call (line 1473): `if let Some(next) = self.fleet.update(&action) { self.dispatch(next); }`. `chat.rs`'s existing catch-all `_ => {}` (its `update`, ~line 1877) means **no change to chat.rs is required** — `FleetComponent::update()` matches the six new variants (plus `TodoUpdated`), `chat.rs` never sees them do anything.

### 5.4 Throughput / cost status-line segment

Extends the EXISTING `status.rs` machinery (§1.8), not a new subsystem:
```rust
// crates/ocean-tui/src/shell/status.rs — new field on StatusData
pub fleet: Option<FleetStats>,  // { active_children: usize, total_cost_usd: f64, aggregate_tok_per_s: f64 }
// new segment fn, same shape as fmt_rate/fmt_count:
fn fleet_segment(f: &FleetStats) -> Segment { // e.g. "⛴ 3 active · $0.24 · 3.1k/s"
```
Skipped entirely (no empty slot) when `fleet.active_children == 0` — matches the file's own stated invariant ("a segment whose value is absent … is simply skipped"). `app.rs::draw_status` (line 3090) computes `FleetStats` by folding over `FleetComponent`'s current node set (cheap, computed once per frame, not per event). New unit tests in `status.rs`'s existing `#[cfg(test)] mod tests` (mirroring `advisor_and_tokens_segments_present_when_set`) assert the empty-fleet case emits no segment and a populated one formats correctly.

---

## 6. Build sequence — 6 independently-landable slices

Dependency graph: **Wave 1** `{1, 2}` fully parallel (disjoint files, no shared new types). **Wave 2** `{3, 4, 5}` parallel once Wave 1 lands (3 depends only on 1; 4 depends only on 2; 5 depends on both 1's and 2's new SDK types, but not on 3 or 4's daemon-side code). **Wave 3** `{6}` depends on 4.

### Slice 1 — Todo tool v2 (phased, session-scoped) + wire types
**Files:** `crates/ocean-agent-sdk/src/lib.rs` (TodoStatus/TodoItem/TodoPhase, `AgentTurnEvent::TodoUpdated` + `session_id()` arm), `crates/ocean-runtime/src/types.rs` (`ToolSideEffect::TodoUpdated`, `AgentEvent::TodoUpdated` + `session_id()` arm), `crates/ocean-runtime/src/tools/todo.rs` (rewrite: phased, `for_session` constructor), `crates/ocean-runtime/src/capability.rs` (`BuiltinProvider.todos` map + `todos_for` + `todo` rebind arm in `tools()`), `crates/ocean-runtime/src/agent_loop.rs` (forward `ToolSideEffect::TodoUpdated` → `AgentEvent::TodoUpdated`, plus `AgentRun.todos: Option<Vec<TodoPhase>>` tracking).
**Contract:** `todo` is phased (set_phases/add/update/clear/list over named phases), state is keyed per `session_id` (not process-global), every mutation emits a `ToolSideEffect`/`AgentEvent`.
**Acceptance:** (a) new `cargo test -p ocean-runtime` regression test in `capability.rs`'s existing test module proving two DISTINCT `session_id`s in `SessionContext` each see an independent todo store (the exact bug fixed, §1.10) — construct `BuiltinProvider`, call `tools_for_session` twice with different `ctx.session_id`, mutate via each `TodoTool`, assert no cross-talk; (b) a test in `agent_loop.rs`'s existing test module asserting a `todo set_phases` call surfaces `AgentEvent::TodoUpdated` on the `events` channel with the right phases.

### Slice 2 — Fleet plumbing: `SessionContext.lineage` + `AgentRuntime` seams
**Files:** `crates/ocean-runtime/src/capability.rs` (`TurnLineage` struct + manual `Debug`, `SessionContext.lineage: Option<TurnLineage>` field), `crates/ocean-agent-sdk/src/lib.rs` (the six `Child*` `AgentTurnEvent` variants + `session_id()` arms — no behavior, just the type vocabulary), `crates/ocean-agent/Cargo.toml` (+`ocean-agent-sdk.workspace = true`), `crates/ocean-agent/src/lib.rs` (populate `tool_ctx.lineage` at the existing `SessionContext{..}` construction site, line ~1358, from `permission`/`cancel` already destructured from `control` just above; add `AgentRuntime::with_task_provider` and `AgentRuntime::resolve_model` per §2.2).
**Contract:** every root turn's `SessionContext` now carries `lineage = Some(TurnLineage{ depth: 0, turn_id, permission, cancel, agent_name: None })`; `AgentRuntime` exposes the two new builder/accessor methods; **zero observable behavior change** (nothing reads `lineage` yet, no provider uses the new builder yet).
**Acceptance:** unit test in `ocean-agent`'s test module driving a real `prompt()` call (reusing the existing fake-provider harness at ~lib.rs:5177-5181) and asserting, via a test-only capability provider that inspects `ctx.lineage`, that `depth == 0` and `Arc::ptr_eq` holds between the lineage's `permission`/`cancel` and what `PromptControl` was constructed with.

### Slice 3 — Daemon `TodoUpdated` bridge + session persistence
**Files:** `crates/ocean-daemon/src/main.rs` (per-turn bridge match arm for `AgentEvent::TodoUpdated`, ~line 9600-9780; `agent_to_ocean_event`/`agent_event_type_name`/`classify_agent_event` new arms, lines 11703/11723/12030; `AgentSessionResponse.todos` wiring at ~line 11338), `crates/ocean-agent/src/lib.rs` (`Session.todos` field, `session_detail`/`SessionDetail` plumbing at ~line 1072/3189, `prompt()` post-run `session.todos = run.todos` assignment).
**Depends on:** Slice 1 (needs `TodoPhase`/`AgentEvent::TodoUpdated`/`AgentTurnEvent::TodoUpdated` to exist).
**Contract:** a root turn calling `todo` streams `todo_updated` over `/v1/agent/events`; `GET /v1/agent/sessions/{id}` returns the persisted plan after the turn completes and across a daemon restart.
**Acceptance:** daemon integration test (reusing the `fake_convene_state`/fake-provider pattern, main.rs ~16712) driving a turn whose fake provider calls `todo` with `set_phases`, asserting (a) the SSE bus received one `AgentTurnEvent::TodoUpdated` with the expected phases, (b) `state.runtime.session_detail(session_id)` (or the HTTP handler) returns those phases in `todos` after the turn.

### Slice 4 — `TaskProvider`/`TaskTool` + daemon bootstrap wiring
**Files:** new `crates/ocean-daemon/src/fleet.rs` (`FleetCtx`, `TaskProvider`, `TaskTool`, `max_task_depth`, subagents/-scoped agent-name resolution, child bridge task, `insert_running_request`/`finish_running_request` helpers extracted from `register_running_request`), `crates/ocean-daemon/src/main.rs` (`mod fleet;`, the `OnceLock<Weak<AgentRuntime>>` bootstrap reorder per §2.3, `register_running_request` refactored to call the new shared helper), `crates/ocean-protocol/src/pricing.rs` (new file, `price_per_million`).
**Depends on:** Slice 2 (`SessionContext.lineage`, `with_task_provider`, `resolve_model`).
**Contract:** a root turn's toolset includes `task` (depth 0 < cap); calling it spawns a real child `run_agent`, streams `Child*` events scoped to the root session, respects `turn_limiter`, inherits permission/cancel per §4.1/§4.2, returns the child's answer as the tool result; at `max_task_depth` a child's OWN toolset omits `task` entirely.
**Acceptance:** daemon integration test with a fake `Provider` (reusing the OCEAN-130 fake-tool test pattern already in the daemon's dev-dependencies, per Cargo.toml's `futures = "0.3"` dev-dep comment) scripting a root turn that calls `task` once, asserting: `ChildTurnStarted`→`ChildTurnFinished` observed on the bus with matching `parent_turn_id`==root turn id; a SECOND test scripting a Halt on the parent mid-child-run asserts the child's own token trips (child run returns `AgentError::Cancelled`) within a sub-second budget (mirroring the immediate-halt design's own `never_yields` test technique); a THIRD test pins `OCEAN_MAX_TASK_DEPTH=1` and asserts a child's own resolved toolset contains no `task` tool.

### Slice 5 — TUI Fleet tree + todo tree pane
**Files:** new `crates/ocean-tui/src/shell/components/fleet.rs`, `crates/ocean-tui/src/shell/components/mod.rs` (register), `crates/ocean-tui/src/shell/app.rs` (`Center::Fleet` + the 8 touch points per §5.1/§5.3).
**Depends on:** Slices 1 and 2's `ocean-agent-sdk` types only (compiles and is fully unit-testable against synthetic `Action::AgentEvent` dispatches — same technique the existing `app.rs` test at line 3538 uses for `TurnStarted` — with zero dependency on the daemon actually running any of Slice 3/4's code).
**Contract:** dispatching a synthetic `ChildTurnStarted`→`ChildToolCallStarted`→`ChildTurnFinished` sequence via `Action::AgentEvent` builds a two-row tree with the right counts/status glyphs; a `TodoUpdated` dispatch replaces the todo tree; toggling `Center::Fleet` follows the exact same keybind/mouse/focus contract as `Center::Graph`.
**Acceptance:** new `#[cfg(test)]` tests in `fleet.rs` (component-local, no terminal needed — ratatui components are drawn into an in-memory `Buffer` in this codebase's existing test style, see `chat.rs`'s own tests) asserting the tree-building logic from a scripted event sequence; one `app.rs` test (mirroring line 3538) asserting `Center::Fleet` toggles and restores exactly like `Center::Graph`.

### Slice 6 — Throughput/cost status-line segment
**Files:** `crates/ocean-tui/src/shell/status.rs` (`FleetStats`, `fleet_segment`, `StatusData.fleet`), `crates/ocean-tui/src/shell/app.rs::draw_status` (fold `FleetComponent`'s live node set into `FleetStats` once per frame).
**Depends on:** Slice 4 (`cost_usd`/`context_percent` must exist on `ChildTurnFinished`) and Slice 5 (`FleetComponent` must exist to fold over).
**Contract:** the status line gains one more segment, `⛴ N active · $X.XX · Yk/s`, present only while `active_children > 0`, matching every other segment's "skip when absent" contract.
**Acceptance:** unit tests in `status.rs`'s existing test module (mirroring `empty_state_is_just_the_focus_chip` and `advisor_and_tokens_segments_present_when_set`): zero active children → no fleet segment; populated `FleetStats` → correctly formatted segment using the SAME `fmt_count`/rate-style formatting already tested there.

---

## 7. Non-goals for v1 (explicit)

- **Child session persistence / independent resumability.** Children are ephemeral (§4.6); no `SessionId`, no disk file, not listed in `GET /v1/agent/sessions`.
- **Fire-and-report task mode.** v1 is sync-wait only (§2.4); a `task_status`/`task_wait` polling pair for a non-blocking dispatch mode is a distinct, larger v2 feature.
- **WASM or fresh subprocess capabilities scoped specifically to a child.** Children inherit whatever the SAME `CapabilityRegistry` already resolves for their `session_id`/`cwd` (built-ins + whatever MCP/plugin providers the daemon already has connected) — no new sideloading tier for subagents specifically.
- **Retrofitting `cost_usd`/`context_percent` onto the root-turn `TurnFinished`/`AgentTurnResponse` wire contract.** Scoped to `ChildTurnFinished` only; extending the root event is a trivial, purely-additive follow-up, deliberately left out here to keep this change's wire-contract diff minimal.
- **A live-updated/accurate pricing feed.** The new `pricing.rs` table is a small static, explicitly-approximate seed (`None` for unknown models, never fabricated) — not a billing-grade cost ledger.
- **ocean-surface fleet-tree UI.** The event schema is verified surface-agnostic (§4.7) and requires zero daemon-side accommodation for ocean-surface specifically; actually rendering the tree there is separate, out-of-repo work.
- **`ThinkingDelta` relay for children.** Intentionally dropped in the child bridge (§2.5 step 11) to bound SSE volume under deep fan-out.
- **Root-turn `agent_name` flowing into subagents/-restriction.** Only depth≥1 children are restricted to their calling agent's own `subagents/` folder; the root operator turn may dispatch any top-level named agent (§2.5 step 3) — simplifies the design by not needing to thread a new `agent_name` field through `PromptRequest`/`PromptControl` for the root case.

## 8. Risks & tradeoffs (chosen resolution stated)

| Risk | Resolution |
|---|---|
| `ocean-agent → ocean-runtime → ocean-agent` cycle if the task tool lived in ocean-runtime | Concrete `TaskProvider` lives in ocean-daemon; ocean-runtime only gains the already-generic `CapabilityProvider` seam it already has. |
| Bootstrapping `TaskProvider` needs `Arc<AgentRuntime>` before it exists | `OnceLock<Weak<AgentRuntime>>`, resolved after construction (§2.3) — standard, no unsafe. |
| `SessionContext` gaining an `Arc<dyn PermissionPolicy>` field breaks `#[derive(Debug)]` | Wrapped in a `TurnLineage` newtype with a manual, redacted `Debug` impl (§4.2/Slice 2) instead of touching `SessionContext`'s own derive. |
| Deep/wide fan-out could blow past provider rate limits despite `turn_limiter` | `turn_limiter` is the SAME semaphore root turns use — a fleet saturates the SAME 24-slot (default) ceiling root turns would, so it degrades exactly like today's burst-of-root-turns case (fast 429-equivalent tool error), not a new failure mode. |
| New `AgentTurnEvent`/`AgentEvent` variants break several exhaustive `match`es across 3 crates | Enumerated precisely in §1.4/§3.4/§4.4 — each is a mechanical one-arm addition at a named line, not a design risk, just enumerated build-plan surface area. |
| Todo-list sharing between root and children could let a child accidentally clobber the orchestrator's phase names | Accepted as intentional (§3.3) — the todo list IS the shared plan; if this proves too permissive in practice, a v2 could scope `todos_for` by `(session_id, turn_id)` instead of `session_id` alone, at the cost of the TUI needing to merge multiple lists. |
