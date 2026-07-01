# Capability Registry + MCP client — the dynamic tool layer

Status: **shipped** — registry (Phase 2) + MCP client over stdio (Phase 3).
This is the Goose-comparison audit's #1 unlock
(`docs/.agentarchive/GOOSE_COMPARISON_AND_EXTENSIONS_GUIDANCE.md`, archived): tools load *dynamically*
through one seam instead of being hardcoded into the agent. The audit explicitly
warns against wiring the `tools.env` keys (Linear/Slack/Brave/…) as one-off
native Rust tools — they plug in here, as MCP servers, behind one trait.

## The one rule

> The agent loop never builds tools. It asks the `CapabilityRegistry` for the
> tools available to a session. Adding a new tool source = registering a new
> provider. It must never mean editing the agent loop.

Before this, `ocean-agent`'s `run_prompt` called `default_tools()` directly —
the single hardcoded seam. That call is now
`self.capabilities.tools_for_session(&ctx).await`.

## Crate layout & dependency direction

```
ocean-runtime  (capability.rs)  ── defines the seam: CapabilityProvider, CapabilityRegistry, BuiltinProvider
      ▲
      │ depends up (never down)
ocean-mcp      ── implements CapabilityProvider for MCP servers (transport, jsonrpc, client, config, provider)
      ▲
      │
ocean-agent    (config.rs)      ── loads ocean.toml, assembles the registry, holds it on AgentRuntime
      ▲
      │
ocean-daemon                    ── AgentRuntime::from_env()?.with_extensions().await
```

`ocean-runtime` stays lean — it gains **no** new dependencies. All the heavy
machinery (process spawning, JSON-RPC, toml) lives in `ocean-mcp` / `ocean-agent`,
which depend *up* into the runtime for the trait. The runtime never depends back.

## The seam (`ocean-runtime::capability`)

```rust
pub type SharedTool = Arc<dyn AgentTool>;

pub struct SessionContext { pub cwd: PathBuf, pub session_id: Option<String> }

#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    fn id(&self) -> &str;                                   // "builtin", "mcp:linear"
    async fn tools(&self, ctx: &SessionContext) -> Vec<SharedTool>;
    async fn health(&self) -> ProviderHealth { ProviderHealth::Ready }
}

pub struct CapabilityRegistry { /* Vec<Arc<dyn CapabilityProvider>> */ }
impl CapabilityRegistry {
    pub fn new(providers: Vec<Arc<dyn CapabilityProvider>>) -> Self;
    pub fn builtin_only() -> Self;
    pub async fn tools_for_session(&self, ctx: &SessionContext) -> Vec<SharedTool>;
}
```

`BuiltinProvider` wraps the unchanged `default_tools()` and caches them.

## Decisions (locked, with rationale)

- **Reuse the runtime's tool model; don't invent a context.** `AgentTool::execute`
  takes `(tool_call_id, args)` — no context. `SessionContext` is *only* a
  provider selection hint (cwd, session id) for future per-workspace/per-room
  toolsets; built-ins ignore it. Grow this struct rather than adding a parallel
  context type.
- **Caching is the provider's job, not the registry's.** The registry is a pure
  flattener with no notion of invalidation. `tools()` must be cheap (runs once
  per turn). Built-ins clone an `Arc` vec; an MCP provider caches its discovered
  list at connect time and serves the snapshot — a turn never blocks on a live
  handshake.
- **Snapshot per turn.** `tools_for_session` runs once at turn start; the owned
  `Vec` goes to `run_agent`. Tools are not live-mutable within a turn.
- **Collisions: first-wins + `tracing::warn!`, never a hard error.** Built-ins
  are registered first, so they can never be shadowed (an MCP server can't
  override `bash`).
- **`Agent` holds `Arc<CapabilityRegistry>` explicitly** — no `Option` fallback
  to built-ins, because a hidden fallback is exactly the hardcoded path the
  audit forbids.

## MCP client (`ocean-mcp`)

- **Transport: a `Transport` trait, stdio implemented.** `StdioTransport` spawns
  the server as a child (`kill_on_drop`), speaks newline-delimited JSON over
  stdin/stdout, inherits the child's stderr for its logs (per the MCP stdio
  spec). HTTP/SSE slots behind the same trait later — `McpTransportKind::Http`
  already parses; configuring it today is a startup warning.
- **Protocol:** JSON-RPC 2.0. Lifecycle is `initialize` (protocolVersion
  `2025-06-18`) → `notifications/initialized` → `tools/list` (follows
  `nextCursor` pagination) → `tools/call`. Server-initiated notifications are
  logged and skipped. Per-request timeout (default 30s) so a hung server can't
  block forever.
- **Each server → one `McpProvider`** (id `mcp:<name>`). Its tools are exposed
  namespaced `mcp__<name>__<tool>`, each an `AgentTool` that forwards `execute`
  to the live server through a `Mutex`-guarded `McpClient`. MCP tools
  `requires_permission() == true` (same gate as bash/write/edit). A tool result
  with `isError: true` becomes an `Err` so the model can react.
- **Discovery: once, at startup, timed, non-fatal.** `McpProvider::connect`
  spawns + handshakes + lists under a 10s overall deadline. A server that fails
  to start, times out, or has missing secrets yields an empty provider
  (`ProviderHealth::Unavailable`) and contributes no tools — it never wedges
  startup or a turn. `connect` returns `Err` only for an unusable *config*
  (stdio with no command).

## Config (`ocean-agent::config` → `<config_dir>/ocean.toml`)

First real daemon-level config (the daemon previously read only `OCEAN_BIND`).
**Optional**: absent/empty → built-ins only (zero behaviour change). Malformed →
logged as an error, daemon continues with built-ins (the agent shouldn't die
over a bad MCP entry). The `[[mcp.server]]` array is its first content; see
`docs/ocean.toml.example`.

**Secrets are referenced by env-var name only.** A server lists
`env = ["LINEAR_API_KEY"]`; the daemon resolves each name at spawn — checking the
process environment first, then `<config_dir>/tools.env` (parsed by
`load_tools_env`: `KEY=VALUE`, `#` comments, optional `export`, quoted values) —
and injects the value into the child. The manifest never holds a secret;
`ocean-mcp` never logs a secret value — only names appear in logs (e.g. a
"missing env var" warning), and the daemon logs only the *count* of `tools.env`
keys loaded.

**Server names are validated at load.** `DaemonConfig::validate` rejects
duplicate names and names containing `__` or non-`[A-Za-z0-9-]` chars — both
would make the `mcp__<name>__<tool>` namespace ambiguous and silently drop
tools. Surfaced at load (actionable) instead of as a confusing runtime warning.

## Hardening (post-review)

A multi-agent review pass (security + idiom + architecture) drove these fixes,
all covered by tests:

- **Bounded reads.** `StdioTransport::recv` caps a single message at 16 MB
  (`MAX_MESSAGE_BYTES`) so a buggy/hostile `npx` server can't OOM the daemon with
  one newline-less line. Over-cap → connection error, folded into the provider's
  unavailable path.
- **Whole-request timeout.** `McpClient::request` uses an absolute `timeout_at`
  deadline, not a per-line timeout — a server dripping notifications under the
  per-line bound can no longer stall a call indefinitely. And `connect` no longer
  shrinks the client's live-call timeout to the (short) connect budget.
- **String-id responses.** `Incoming.id` is a `Value`; `matches_id` accepts both
  numeric and string ids. Servers that echo a string id no longer get
  misclassified as notifications and hang the request.
- **No raw server output in logs.** The unparseable-message warning logs the
  parse error only, not the raw line (which is attacker-controlled).

Known follow-up (tracked separately, NOT in this change): the daemon's
`/v1/agent/turns` route hardcodes `yolo: true`, which auto-approves every
mutating + MCP tool with no operator prompt. The MCP permission gate
(`requires_permission() == true`) is correct and honored by the loop, but inert
on that route until `yolo` is gated. Pre-existing; affects bash/write/edit too.

## Invariant the loop relies on

`tools_for_session` returns a name-unique vec — what `run_agent`'s dispatch
`HashMap` assumed. The registry's dedup makes the schema list sent to the model
agree with the dispatch map. Don't re-add dedup inside `run_agent`.

## Wiring summary

| File | Change |
|---|---|
| `ocean-runtime/src/capability.rs` | **new** — trait, registry, `BuiltinProvider`, `SessionContext`, tests |
| `ocean-runtime/src/lib.rs` | `pub mod capability;` + re-exports |
| `ocean-mcp/` | **new crate** — `jsonrpc`, `transport`, `client`, `config`, `provider` + e2e test w/ a live fake server |
| `ocean-agent/src/config.rs` | **new** — `DaemonConfig` from `ocean.toml`, `[[mcp.server]]` |
| `ocean-agent/src/lib.rs` | `AgentRuntime` holds `Arc<CapabilityRegistry>`; `from_env` = built-ins, `with_extensions().await` connects MCP; `run_prompt` calls `tools_for_session` |
| `ocean-daemon/src/main.rs` | `from_env()?.with_extensions().await` (also collapsed a latent double-construction) |
| `Cargo.toml` (workspace) | add `ocean-mcp` member + `toml` dep |

## Tests

- `ocean-runtime` (`capability.rs`): 6 — builtin faithfulness, behaviour
  preservation (`builtin_only` == old `default_tools()`), merge, dedup
  first-wins (bash can't be shadowed), ordering, name-uniqueness.
- `ocean-mcp`: 7 — config parse + `resolve_env` (present/missing/empty), and 5
  e2e against a **live child** fake MCP server: connect+namespace, call, tool
  error → Err, bad command non-fatal, and built-ins+MCP merged through the
  registry.
- `ocean-agent`: existing 16 still pass — the refactor is behaviour-preserving.

## What's next on this track

- **HTTP/SSE transport** behind the existing `Transport` trait (hosted MCP
  servers). Config already accepts `transport = "http"`.
- **Re-discovery on `notifications/tools/list_changed`** (currently logged +
  ignored; tools are snapshotted at connect).
- **Resources + prompts** (MCP also exposes those; we consume tools only).
- **Skills** as another `CapabilityProvider` (Phase 4) — markdown/TOML packs
  contributing tools + guidance, same seam.
