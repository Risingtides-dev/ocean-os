//! Capability registry: the seam between the agent loop and its tool sources.
//!
//! The agent loop never builds tools directly. It asks a [`CapabilityRegistry`]
//! for the tools available to a session. The registry holds an ordered list of
//! [`CapabilityProvider`]s — sources of tools. The built-in toolset is just one
//! provider ([`BuiltinProvider`]); later, an MCP client and skill packs register
//! as additional providers behind the same trait. Nothing downstream of the
//! registry knows or cares where a tool came from.
//!
//! This is the abstraction the project's Goose-comparison audit calls for: tools
//! load *dynamically* through one seam instead of being hardcoded into the agent.
//! Adding a new tool source must never mean editing the agent loop or this crate
//! — the `ocean-mcp` crate, for example, depends on `ocean-runtime` and registers
//! an `McpProvider`; this crate never depends back on it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ocean_protocol::Content;
use serde_json::Value;

use crate::artifacts::{ArtifactStore, SharedArtifacts};
use crate::tools::default_tools;
use crate::types::{AgentTool, AgentToolResult};

/// Shared, reference-counted tool handle. The loop and registry pass these
/// around; cloning is a cheap `Arc` bump.
pub type SharedTool = Arc<dyn AgentTool>;

const MAX_SESSION_TODOS: usize = 1_024;

/// In-memory todo-tool cache keyed by bound session id, with a soft bound at
/// [`MAX_SESSION_TODOS`] entries. When the map is at or above the bound and a
/// new session is requested, the least-recently-touched **empty** entry is
/// evicted — a session that still holds confirmed todo items is never silently
/// dropped. If every resident entry is non-empty the map grows temporarily;
/// once empty entries become available, the next insert reclaims them. This
/// guarantees the TUI tray cannot display a non-empty projection while the
/// corresponding daemon tool has been evicted.
#[derive(Default)]
struct SessionTodos {
    next_touch: u64,
    tools: std::collections::HashMap<String, (u64, Arc<crate::tools::todo::TodoTool>)>,
}

impl SessionTodos {
    fn get_or_insert(&mut self, session_id: &str) -> Arc<crate::tools::todo::TodoTool> {
        let touch = self.next_touch;
        self.next_touch = self.next_touch.wrapping_add(1);
        if let Some((last_touch, tool)) = self.tools.get_mut(session_id) {
            *last_touch = touch;
            return tool.clone();
        }
        // Soft bound: repeatedly evict the least-recently-touched *empty*
        // tools until we're under the bound or no empty candidate remains.
        // Non-empty entries are pinned; if all are non-empty the map can
        // grow temporarily.
        while self.tools.len() >= MAX_SESSION_TODOS {
            let candidate = self
                .tools
                .iter()
                .filter(|(_, (_, tool))| tool.is_empty())
                .min_by_key(|(_, (last_touch, _))| *last_touch)
                .map(|(id, _)| id.clone());
            match candidate {
                Some(oldest) => {
                    self.tools.remove(&oldest);
                }
                None => break,
            }
        }
        let tool: Arc<crate::tools::todo::TodoTool> = Arc::new(crate::tools::todo::TodoTool::new());
        self.tools
            .insert(session_id.to_string(), (touch, tool.clone()));
        tool
    }
}

/// Per-turn context a provider may use to decide which tools to offer.
///
/// `AgentTool::execute` itself takes no context (tools resolve cwd from the
/// process and permissions are gated separately on `AgentConfig`), so this is
/// *only* a selection hint: a provider can vary its toolset by workspace or
/// session. Built-ins ignore it. Kept deliberately small; grow it (room id,
/// selected skills) rather than inventing a parallel context type.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    /// Working directory for this turn.
    pub cwd: PathBuf,
    /// Session id, when the turn belongs to a known session.
    pub session_id: Option<String>,
    /// Hashline-edit harness capability (W1 / harness profiles). When true, the
    /// builtin `read` emits a `[path#HASH]` content tag + records a session
    /// snapshot, and a `hashline_edit` tool is offered. Off for lean surfaces
    /// (web/voice) so their tool contract is unchanged. Resolved per-turn from
    /// the daemon's `HarnessProfile`.
    pub hashline: bool,
    /// Whether the `lsp` code-intelligence tool may be offered this turn.
    /// False for voice, whose spoken replies cannot carry definitions,
    /// references, or diagnostics (TASK-26). Defaults true so every existing
    /// construction keeps today's behavior.
    pub code_intelligence: bool,
    /// Artifact-spill harness capability (W3 / harness profiles). When true,
    /// every tool's oversized text output is truncated for the model with an
    /// explicit notice and the full output is spilled to the session artifact
    /// store, readable back via `read artifact://<id>`; `read` also gains
    /// `artifact://` resolution. Enabled for TUI/ACP/CLI/web daemon turns and
    /// off for voice; direct legacy callers default off. Resolved per-turn from
    /// the daemon's effective `HarnessProfile`.
    pub artifacts: bool,
}

/// Coarse health of a capability provider, surfaced for diagnostics.
///
/// Built-ins are always [`Ready`](ProviderHealth::Ready). A remote provider
/// (e.g. an MCP server) reports [`Degraded`](ProviderHealth::Degraded) when it
/// is reachable but partial, or [`Unavailable`](ProviderHealth::Unavailable)
/// when its tools can't be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    Ready,
    Degraded,
    Unavailable,
}

/// A source of tools for the agent.
///
/// **Caching is the provider's responsibility, not the registry's.** Built-ins
/// hold their `Vec` and clone it (cheap `Arc` bumps). A remote provider must
/// cache its discovered toolset internally and refresh on its own signals, so
/// that `tools()` stays cheap enough to call once per turn and never blocks the
/// turn on a live network/process handshake.
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Stable identifier for this provider, used in diagnostics and dedup
    /// warnings (e.g. `"builtin"`, `"mcp:linear"`).
    fn id(&self) -> &str;

    /// Tools this provider offers for the given session context.
    ///
    /// Must be cheap — it is called once at the start of every turn. Do any
    /// network/process work behind an internal cache.
    async fn tools(&self, ctx: &SessionContext) -> Vec<SharedTool>;

    /// Coarse health for diagnostics. Defaults to [`ProviderHealth::Ready`].
    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }

    /// The session-scoped artifact store this provider owns, if any (W3). The
    /// [`CapabilityRegistry`] asks each provider for one so it can wrap the whole
    /// merged toolset (built-ins + MCP) in a spill decorator sharing exactly the
    /// store `read` resolves `artifact://` against. Only [`BuiltinProvider`] owns
    /// a store; every other provider returns `None` (the default) and the
    /// registry uses the first store offered.
    fn artifacts_store(&self, _session_id: &str) -> Option<SharedArtifacts> {
        None
    }
}

/// The built-in toolset, wrapped as a [`CapabilityProvider`].
///
/// Constructs the built-in tool templates once via [`default_tools`] and clones
/// them on each `tools()` call. Stateful tools are rebound to their declared
/// scope so separate sessions cannot observe one another's memory.
pub struct BuiltinProvider {
    tools: Vec<SharedTool>,
    /// Session-scoped hashline snapshot stores, keyed by session id. Lives on
    /// the provider (which the runtime `Arc`s for its whole life) so a `read`
    /// in one turn and a `hashline_edit` in a later turn share one store.
    snapshots:
        std::sync::Mutex<std::collections::HashMap<String, crate::tools::read::SharedSnapshots>>,
    /// Session-scoped artifact spill stores, keyed by session id (W3). Lives on
    /// the provider (Arc'd for the runtime's life) so a spill in one turn and a
    /// `read artifact://` in a later turn share one store — same shape as
    /// `snapshots` above.
    artifacts: std::sync::Mutex<std::collections::HashMap<String, SharedArtifacts>>,
    code_intelligence: true,
    /// Session-scoped no-op loop guards, keyed by session id. Same lifetime
    /// shape as `snapshots`: a `hashline_edit` that repeats the identical
    /// changeless patch across turns of one session accumulates against one
    /// guard and trips to a hard error.
    noop_guards: std::sync::Mutex<
        std::collections::HashMap<String, crate::tools::hashline_edit::SharedNoopGuard>,
    >,
    /// In-memory todo tools keyed by bound session. The Files tray projects the
    /// same tool effects, so retaining this handle makes cross-turn pinning real
    /// instead of displaying items the next turn cannot complete.
    todos: std::sync::Mutex<SessionTodos>,
}

impl BuiltinProvider {
    pub fn new() -> Self {
        Self {
            tools: default_tools(),
            snapshots: std::sync::Mutex::new(std::collections::HashMap::new()),
            artifacts: std::sync::Mutex::new(std::collections::HashMap::new()),
            code_intelligence: true,
            noop_guards: std::sync::Mutex::new(std::collections::HashMap::new()),
            todos: std::sync::Mutex::new(SessionTodos::default()),
        }
    }

    /// Get-or-create the snapshot store for a session (W1 hashline).
    fn snapshots_for(&self, session_id: &str) -> crate::tools::read::SharedSnapshots {
        let mut map = self.snapshots.lock().expect("snapshots map poisoned");
        map.entry(session_id.to_string())
            .or_insert_with(|| {
                // LRU bounds match OMP: ~30 paths, 4 versions each.
                std::sync::Arc::new(std::sync::Mutex::new(ocean_hashline::SnapshotStore::new(
                    30, 4,
                )))
            })
            .clone()
    }

    /// Get-or-create the artifact spill store for a session (W3).
    fn artifacts_for(&self, session_id: &str) -> SharedArtifacts {
        let mut map = self.artifacts.lock().expect("artifacts map poisoned");
        map.entry(session_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(ArtifactStore::default())))
            .clone()
    }

    /// Get-or-create the no-op loop guard for a session.
    fn noop_guard_for(&self, session_id: &str) -> crate::tools::hashline_edit::SharedNoopGuard {
        let mut map = self.noop_guards.lock().expect("noop guards map poisoned");
        map.entry(session_id.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(std::sync::Mutex::new(
                    ocean_hashline::NoopLoopGuard::default(),
                ))
            })
            .clone()
    }

    /// Get-or-create the todo tool for one bound session. Returns a typed
    /// Arc<TodoTool> so the soft-eviction scanner can call is_empty() without
    /// an extra downcast. The caller coerces to SharedTool as needed.
    fn todo_for(&self, session_id: &str) -> Arc<crate::tools::todo::TodoTool> {
        self.todos
            .lock()
            .expect("todo tools map poisoned")
            .get_or_insert(session_id)
    }
}

impl Default for BuiltinProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CapabilityProvider for BuiltinProvider {
    fn id(&self) -> &str {
        "builtin"
    }

    /// The built-in provider owns the session artifact stores (W3); the registry
    /// wraps the merged toolset in a spill decorator sharing this exact store.
    fn artifacts_store(&self, session_id: &str) -> Option<SharedArtifacts> {
        Some(self.artifacts_for(session_id))
    }

    async fn tools(&self, ctx: &SessionContext) -> Vec<SharedTool> {
        let mut tools = self.tools.clone();
        // A bound session gets one in-memory todo list across its turns so the
        // Files-sidebar pin and the executable tool share the same state.
        // Unbound/ad-hoc runs keep isolated fresh lists.
        for tool in &mut tools {
            if tool.name() == "todo" {
                *tool = ctx.session_id.as_deref().map_or_else(
                    || Arc::new(crate::tools::todo::TodoTool::new()) as SharedTool,
                    |session_id| self.todo_for(session_id) as SharedTool,
                );
            }
        }
        // Session-scoped rebinds: a few built-ins must key shared daemon state on
        // the session the daemon resolves against, so they're rebuilt per-turn
        // with this turn's authoritative session id (never a model-supplied one).
        // `default_tools()` provides unbound instances for ad-hoc/test paths.
        //
        // - OCEAN-60: `component_wait` keys its wait registry on the session the
        //   daemon resolves component events against.
        // - OCEAN-271: `slack_canvas` keys its bridge-fulfillment lookup on the
        //   session the daemon stored the fetched content under, so a `read`
        //   surfaces real content instead of `pending_bridge`.
        if let Some(session_id) = &ctx.session_id {
            for tool in tools.iter_mut() {
                match tool.name() {
                    "component_wait" => {
                        *tool = Arc::new(crate::tools::component::ComponentWaitTool::for_session(
                            Some(session_id.clone()),
                        ));
                    }
                    "slack_canvas" => {
                        *tool = Arc::new(crate::tools::slack_canvas::SlackCanvasTool::for_session(
                            Some(session_id.clone()),
                        ));
                    }
                    "bash" => {
                        *tool = Arc::new(crate::tools::bash::BashTool::for_cwd(ctx.cwd.clone()));
                    }
                    "read" => {
                        // Hashline profile: `read` tags output + records snapshots
                        // into the session store. Otherwise the classic read.
                        let mut read = if ctx.hashline {
                            crate::tools::read::ReadTool::for_cwd_with_snapshots(
                                ctx.cwd.clone(),
                                self.snapshots_for(session_id),
                            )
                        } else {
                            crate::tools::read::ReadTool::for_cwd(ctx.cwd.clone())
                        };
                        // Artifact-spill profile: bind the session store so
                        // `read artifact://<id>` resolves spilled outputs. Same
                        // store the spill decorator writes into. Independent of
                        // the hashline flag.
                        if ctx.artifacts {
                            read = read.with_artifacts(self.artifacts_for(session_id));
                        }
                        *tool = Arc::new(read);
                    }
                    "write" => {
                        *tool = Arc::new(crate::tools::write::WriteTool::for_cwd(ctx.cwd.clone()));
                    }
                    "edit" => {
                        *tool = Arc::new(crate::tools::edit::EditTool::for_cwd(ctx.cwd.clone()));
                    }
                    "ls" => {
                        *tool = Arc::new(crate::tools::ls::LsTool::for_cwd(ctx.cwd.clone()));
                    }
                    "grep" => {
                        *tool = Arc::new(crate::tools::grep::GrepTool::for_cwd(ctx.cwd.clone()));
                    }
                    "glob" => {
                        *tool =
                            Arc::new(crate::tools::glob_tool::GlobTool::for_cwd(ctx.cwd.clone()));
                    }
                    _ => {}
                }
            }
            // Hashline profile: offer the content-anchored edit tool, sharing the
            // same session snapshot store `read` records into.
            if ctx.hashline {
                tools.push(Arc::new(
                    crate::tools::hashline_edit::HashlineEditTool::new(
                        Some(ctx.cwd.clone()),
                        self.snapshots_for(session_id),
                        self.noop_guard_for(session_id),
                    ),
                ));
            }
        }
        tools
    }
}

/// Ordered set of capability providers, flattened per session.
///
/// Providers are consulted in order. The first provider to claim a tool name
/// wins; later providers offering the same name are dropped with a warning.
/// Because built-ins are registered first, they can never be shadowed by a later
/// provider (an MCP server can't override `bash`).
pub struct CapabilityRegistry {
    providers: Vec<Arc<dyn CapabilityProvider>>,
}

impl std::fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn CapabilityProvider` isn't Debug; list provider ids instead.
        f.debug_struct("CapabilityRegistry")
            .field(
                "providers",
                &self.providers.iter().map(|p| p.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl CapabilityRegistry {
    /// Build a registry from an ordered list of providers. Order is significant:
    /// earlier providers win name collisions. Put built-ins first.
    pub fn new(providers: Vec<Arc<dyn CapabilityProvider>>) -> Self {
        Self { providers }
    }

    /// Convenience constructor: built-in tools only. Equivalent to the old
    /// `default_tools()` path, for the daemon's default and for tests.
    pub fn builtin_only() -> Self {
        Self::new(vec![Arc::new(BuiltinProvider::new())])
    }

    /// The registered providers, in order.
    pub fn providers(&self) -> &[Arc<dyn CapabilityProvider>] {
        &self.providers
    }

    /// Flatten all providers' tools for this session, deduped by name
    /// (first-wins). The result is guaranteed to have unique tool names — the
    /// invariant the agent loop's dispatch map relies on.
    pub async fn tools_for_session(&self, ctx: &SessionContext) -> Vec<SharedTool> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<SharedTool> = Vec::new();
        for provider in &self.providers {
            for tool in provider.tools(ctx).await {
                let name = tool.name().to_string();
                if seen.insert(name.clone()) {
                    out.push(tool);
                } else {
                    tracing::warn!(
                        tool = %name,
                        provider = %provider.id(),
                        "duplicate tool name; keeping earlier provider's tool"
                    );
                }
            }
        }
        // W3 — output-meta + artifact spill. When the turn's artifact capability
        // is on, wrap EVERY merged tool (built-ins + MCP) in a `SpillingTool`
        // decorator that truncates oversized text output with a notice and spills
        // the full output to the session store. The store comes from the first
        // provider that owns one (the built-in provider) so it's exactly the
        // store `read` resolves `artifact://` against. Off (or no session / no
        // store) → the toolset is returned byte-for-byte unchanged.
        if ctx.artifacts {
            if let Some(session_id) = &ctx.session_id {
                if let Some(store) = self
                    .providers
                    .iter()
                    .find_map(|p| p.artifacts_store(session_id))
                {
                    out = out
                        .into_iter()
                        .map(|t| Arc::new(SpillingTool::new(t, store.clone())) as SharedTool)
                        .collect();
                }
            }
        }
        out
    }
}

/// Byte threshold above which a single tool-result text block is spilled to an
/// artifact (W3). ~24 KB — well below the transcript cap in the agent loop, so a
/// spilled result is small enough that the model reasons over the HEAD + notice
/// while the full bytes stay one `read artifact://<id>` away. Bytes ≈ characters
/// for the ASCII-heavy tool output this fires on.
pub const SPILL_THRESHOLD_BYTES: usize = 24_000;

/// Bytes of the HEAD kept in-context when a text block is spilled (W3). The cut
/// is backed up to the last line boundary so the model never sees a half-line.
pub const SPILL_HEAD_BYTES: usize = 16_000;

/// A [`SharedTool`] decorator that spills oversized text output to a session
/// artifact store (W3 output-meta + artifact spill).
///
/// It forwards every part of the tool contract (name, label, description,
/// parameters, permission, execute) to the inner tool, then post-processes the
/// result: any text block over [`SPILL_THRESHOLD_BYTES`] is replaced with its
/// HEAD (cut at a line boundary) plus a truncation notice stating the shown
/// range and the `read artifact://<id>` handle for the full output, which is
/// `put` into the store. Under-threshold blocks, non-text blocks, `details`,
/// `terminate`, and `side_effects` pass through untouched — so with the same
/// input a small result is byte-identical to the undecorated tool.
pub struct SpillingTool {
    inner: SharedTool,
    store: SharedArtifacts,
}

impl SpillingTool {
    pub fn new(inner: SharedTool, store: SharedArtifacts) -> Self {
        Self { inner, store }
    }

    /// Spill one oversized text block: `put` the full text and return the HEAD +
    /// notice. `tool` names the producing tool (recorded on the artifact).
    fn spill_text(&self, tool: &str, text: String) -> String {
        // Cut the HEAD at a char boundary at or before the byte budget, then back
        // up to the last newline so we never show a partial line.
        let mut head_end = SPILL_HEAD_BYTES.min(text.len());
        while head_end > 0 && !text.is_char_boundary(head_end) {
            head_end -= 1;
        }
        if let Some(nl) = text[..head_end].rfind('\n') {
            head_end = nl + 1; // keep the newline so the head ends cleanly
        }
        let head = &text[..head_end];
        let shown_lines = head.lines().count();
        let total_lines = text.lines().count();

        let id = match self.store.lock() {
            Ok(mut store) => store.put(tool, text.clone()),
            // Store poisoned: fall back to the raw text rather than losing it.
            Err(_) => return text,
        };

        format!(
            "{head}\n[output truncated: showing lines 1-{shown_lines} of {total_lines} · \
             full output: read artifact://{id}]"
        )
    }
}

#[async_trait]
impl AgentTool for SpillingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    fn requires_permission(&self) -> bool {
        self.inner.requires_permission()
    }
    async fn execute(&self, tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        // A `read artifact://<id>` is the model deliberately pulling spilled
        // content back. Re-spilling that output would be circular — it would
        // mint a fresh artifact of an artifact and stop the model ever
        // retrieving the full thing. So this one call bypasses the decorator and
        // returns the (windowed) artifact bytes verbatim. The agent loop's
        // transcript cap is the backstop against an unbounded full read.
        let is_artifact_read = self.inner.name() == "read"
            && args
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| p.starts_with("artifact://"))
                .unwrap_or(false);
        let mut result = self.inner.execute(tool_call_id, args).await?;
        if is_artifact_read {
            return Ok(result);
        }
        let tool = self.inner.name().to_string();
        result.content = result
            .content
            .into_iter()
            .map(|c| match c {
                Content::Text { text } if text.len() > SPILL_THRESHOLD_BYTES => Content::Text {
                    text: self.spill_text(&tool, text),
                },
                other => other,
            })
            .collect();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentToolResult;
    use serde_json::{json, Value};

    fn ctx() -> SessionContext {
        SessionContext {
            cwd: PathBuf::from("/tmp"),
            session_id: Some("test-session".into()),
            hashline: false,
            artifacts: false,
            code_intelligence: true,
        }
    }

    /// A minimal test-only tool with a configurable name and marker description.
    struct FakeTool {
        name: String,
        desc: String,
    }
    #[async_trait]
    impl AgentTool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.desc
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            Ok(AgentToolResult::text("fake"))
        }
    }

    /// A test-only provider returning a fixed list of tools.
    struct FakeProvider {
        id: String,
        tools: Vec<SharedTool>,
    }
    #[async_trait]
    impl CapabilityProvider for FakeProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
            self.tools.clone()
        }
    }

    fn names(tools: &[SharedTool]) -> Vec<String> {
        tools.iter().map(|t| t.name().to_string()).collect()
    }

    #[tokio::test]
    async fn builtin_provider_returns_all_default_tools() {
        let provider = BuiltinProvider::new();
        let got = provider.tools(&ctx()).await;
        let expected = default_tools();
        assert_eq!(names(&got), names(&expected));
    }

    #[tokio::test]
    async fn builtin_provider_keeps_todo_per_session_and_isolates_sessions() {
        let provider = BuiltinProvider::new();
        let first = provider.tools(&ctx()).await;
        let todo = first
            .iter()
            .find(|tool| tool.name() == "todo")
            .expect("todo present");
        todo.execute("add", json!({"action": "add", "text": "pinned"}))
            .await
            .expect("add succeeds");

        let same_session = provider.tools(&ctx()).await;
        let listed = same_session
            .iter()
            .find(|tool| tool.name() == "todo")
            .expect("todo present")
            .execute("list", json!({"action": "list"}))
            .await
            .expect("list succeeds");
        assert!(matches!(
            listed.content.as_slice(),
            [ocean_protocol::Content::Text { text }] if text.contains("pinned")
        ));

        let mut other = ctx();
        other.session_id = Some("other-session".into());
        let isolated = provider.tools(&other).await;
        let listed = isolated
            .iter()
            .find(|tool| tool.name() == "todo")
            .expect("todo present")
            .execute("list", json!({"action": "list"}))
            .await
            .expect("list succeeds");
        assert!(matches!(
            listed.content.as_slice(),
            [ocean_protocol::Content::Text { text }] if text == "(empty)"
        ));
    }

    #[tokio::test]
    async fn session_todos_evicts_oldest_empty_and_keeps_recently_touched() {
        let provider = BuiltinProvider::new();
        // Fill to cap with empty sessions.
        for i in 0..MAX_SESSION_TODOS {
            let mut ctx = ctx();
            ctx.session_id = Some(format!("ev-{i}"));
            provider.tools(&ctx).await;
        }
        // Re-touch ev-0 so it is no longer the oldest.
        let mut ctx = ctx();
        ctx.session_id = Some("ev-0".into());
        provider.tools(&ctx).await;
        // One more distinct session forces an eviction of the oldest empty.
        ctx.session_id = Some("ev-overflow".into());
        provider.tools(&ctx).await;

        let guard = provider.todos.lock().expect("lock");
        assert!(
            guard.tools.contains_key("ev-0"),
            "recently-touched ev-0 must survive"
        );
        assert!(
            !guard.tools.contains_key("ev-1"),
            "oldest untouched empty ev-1 must be evicted"
        );
        assert!(
            guard.tools.contains_key("ev-overflow"),
            "new session must be present"
        );
        assert!(
            guard.tools.len() <= MAX_SESSION_TODOS,
            "map len {} reclaimed toward bound",
            guard.tools.len(),
        );
    }

    #[tokio::test]
    async fn session_todos_survives_non_empty_oldest_under_pressure() {
        let provider = BuiltinProvider::new();
        // Insert a non-empty session first so it is the oldest touch.
        let mut non_empty_ctx = ctx();
        non_empty_ctx.session_id = Some("keeper".into());
        let tools = provider.tools(&non_empty_ctx).await;
        let keeper = tools
            .iter()
            .find(|t| t.name() == "todo")
            .expect("todo present")
            .clone();
        keeper
            .execute("add", json!({"action": "add", "text": "pinned"}))
            .await
            .expect("add succeeds");
        drop(keeper);
        drop(tools);

        // Fill the rest of the map with empty sessions up to the soft bound.
        for i in 0..(MAX_SESSION_TODOS - 1) {
            let mut ctx = ctx();
            ctx.session_id = Some(format!("fill-{i}"));
            provider.tools(&ctx).await;
        }

        // Now at the bound. "keeper" is non-empty; all others are empty.
        // One more distinct session: the oldest empty must be evicted, not keeper.
        let mut ctx = ctx();
        ctx.session_id = Some("overflow".into());
        provider.tools(&ctx).await;

        let guard = provider.todos.lock().expect("lock");
        assert!(
            guard.tools.contains_key("keeper"),
            "non-empty oldest must survive despite pressure"
        );
        assert!(
            guard.tools.contains_key("overflow"),
            "new session must be present"
        );
        assert!(
            guard.tools.len() <= MAX_SESSION_TODOS,
            "map len {} reclaimed toward bound",
            guard.tools.len(),
        );
    }

    #[tokio::test]
    async fn session_todos_shrinks_back_after_overgrowth_when_entries_cleared() {
        let provider = BuiltinProvider::new();
        // Create overgrowth: fill the map with 1_034 non-empty sessions
        // (cap + 10). All are non-empty so nothing can be evicted; the map
        // grows past MAX_SESSION_TODOS.
        for i in 0..(MAX_SESSION_TODOS + 10) {
            let mut ctx = ctx();
            ctx.session_id = Some(format!("keep-{i}"));
            let tools = provider.tools(&ctx).await;
            let todo = tools
                .iter()
                .find(|t| t.name() == "todo")
                .expect("todo present");
            todo.execute("add", json!({"action": "add", "text": format!("item-{i}")}))
                .await
                .expect("add succeeds");
        }
        // Verify overgrowth: map exceeds the soft bound.
        {
            let guard = provider.todos.lock().expect("lock");
            assert!(
                guard.tools.len() > MAX_SESSION_TODOS,
                "overgrowth must exceed soft bound, got {}",
                guard.tools.len(),
            );
        }

        // Now clear the first 50 entries to create empty reclaimable slots,
        // leaving non-empty keep-50..keep-1033 intact.
        for i in 0..50 {
            let mut ctx = ctx();
            ctx.session_id = Some(format!("keep-{i}"));
            let tools = provider.tools(&ctx).await;
            let todo = tools
                .iter()
                .find(|t| t.name() == "todo")
                .expect("todo present");
            todo.execute("clear", json!({"action": "clear"}))
                .await
                .expect("clear succeeds");
        }
        // Insert one new session to trigger the eviction loop.
        let mut ctx = ctx();
        ctx.session_id = Some("fresh".into());
        provider.tools(&ctx).await;

        let guard = provider.todos.lock().expect("lock");
        // After the loop drain, the map must have shrunk to <= MAX_SESSION_TODOS.
        assert!(
            guard.tools.len() <= MAX_SESSION_TODOS,
            "after clearing 50 and inserting fresh, map len {} must shrink to ≤ {MAX_SESSION_TODOS}",
            guard.tools.len(),
        );
        // fresh must be present.
        assert!(guard.tools.contains_key("fresh"));
        // A non-empty survivor from the uncleared range must still be present.
        assert!(
            guard.tools.contains_key("keep-100"),
            "non-empty keep-100 must survive the shrink"
        );
        // At least some cleared entries must be gone (we cleared 50; the loop
        // should drain the overflow + leave room for fresh).
        let still_present_cleared = (0..50)
            .filter(|i| guard.tools.contains_key(&format!("keep-{i}")))
            .count();
        assert!(
            still_present_cleared < 50,
            "some cleared entries must be evicted; {still_present_cleared} still present"
        );
    }

    #[tokio::test]
    async fn session_todos_evicts_cleared_entry() {
        let provider = BuiltinProvider::new();
        // Create a non-empty session, then clear it so it becomes empty.
        let mut clear_ctx = ctx();
        clear_ctx.session_id = Some("clear-me".into());
        let tools = provider.tools(&clear_ctx).await;
        let todo = tools
            .iter()
            .find(|t| t.name() == "todo")
            .expect("todo present")
            .clone();
        todo.execute("add", json!({"action": "add", "text": "transient"}))
            .await
            .expect("add succeeds");
        // Clear → becomes empty, making it evictable.
        todo.execute("clear", json!({"action": "clear"}))
            .await
            .expect("clear succeeds");
        // Verify the tool reports empty state.
        let listed = todo
            .execute("list", json!({"action": "list"}))
            .await
            .expect("list succeeds");
        assert!(matches!(
            listed.content.as_slice(),
            [ocean_protocol::Content::Text { text }] if text == "(empty)"
        ));
        drop(todo);
        drop(tools);

        // Fill up to the soft bound with other empty sessions.
        for i in 1..MAX_SESSION_TODOS {
            let mut ctx = ctx();
            ctx.session_id = Some(format!("fill-{i}"));
            provider.tools(&ctx).await;
        }

        // One more forces eviction of the oldest empty — "clear-me".
        let mut ctx = ctx();
        ctx.session_id = Some("overflow".into());
        provider.tools(&ctx).await;

        let guard = provider.todos.lock().expect("lock");
        assert!(
            !guard.tools.contains_key("clear-me"),
            "cleared entry must be evicted as oldest empty"
        );
        assert!(
            guard.tools.contains_key("overflow"),
            "new session must be present"
        );
    }

    #[tokio::test]
    async fn hashline_profile_offers_both_surgical_editors() {
        let provider = BuiltinProvider::new();
        let mut context = ctx();
        context.hashline = true;

        let got = names(&provider.tools(&context).await);

        assert!(got.contains(&"hashline_edit".to_string()));
        assert!(got.contains(&"edit".to_string()));
        assert!(got.contains(&"write".to_string()));
    }

    #[tokio::test]
    async fn plain_profile_keeps_legacy_editor() {
        let provider = BuiltinProvider::new();
        let got = names(&provider.tools(&ctx()).await);

        assert!(got.contains(&"edit".to_string()));
        assert!(!got.contains(&"hashline_edit".to_string()));
        assert!(got.contains(&"write".to_string()));
    }

    #[tokio::test]
    async fn registry_builtin_only_matches_default_tools() {
        let registry = CapabilityRegistry::builtin_only();
        let got = registry.tools_for_session(&ctx()).await;
        let expected = default_tools();
        // Behaviour preservation: the exact vec (names + order) that used to
        // reach run_agent still does.
        assert_eq!(names(&got), names(&expected));
    }

    #[tokio::test]
    async fn registry_merges_second_provider() {
        let fake = Arc::new(FakeProvider {
            id: "fake".into(),
            tools: vec![Arc::new(FakeTool {
                name: "fake_tool".into(),
                desc: "a fake".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), fake]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        assert!(got.contains(&"fake_tool".to_string()));
        assert_eq!(got.last().unwrap(), "fake_tool", "extra tool appended last");
        for n in names(&default_tools()) {
            assert!(got.contains(&n), "missing built-in {n}");
        }
    }

    /// OCEAN-60: the built-in provider rebinds `component_wait` to the turn's
    /// session id from SessionContext. The rebound tool runs without a
    /// model-supplied `session_id` arg (it times out rather than erroring on a
    /// missing arg). With no session in the ctx, the unbound default falls back
    /// to requiring the arg.
    #[tokio::test]
    async fn builtin_provider_injects_session_into_component_wait() {
        let provider = BuiltinProvider::new();

        // With a session bound in ctx, component_wait needs no session arg.
        let got = provider.tools(&ctx()).await;
        let wait = got
            .iter()
            .find(|t| t.name() == "component_wait")
            .expect("component_wait present");
        let err = wait
            .execute("c1", json!({ "id": "x", "timeout_ms": 1 }))
            .await
            .expect_err("a 1ms wait with no interaction times out");
        assert!(err.contains("timed out"), "expected timeout, got: {err}");

        // With no session in ctx, the unbound default still requires the arg.
        let no_sess = SessionContext {
            cwd: PathBuf::from("/tmp"),
            session_id: None,
            hashline: false,
            artifacts: false,
            code_intelligence: true,
        };
        let got = provider.tools(&no_sess).await;
        let wait = got
            .iter()
            .find(|t| t.name() == "component_wait")
            .expect("component_wait present");
        let err = wait
            .execute("c2", json!({ "id": "x", "timeout_ms": 1 }))
            .await
            .expect_err("missing session arg with no binding errors");
        assert!(
            err.contains("session_id"),
            "expected session_id error, got: {err}"
        );
    }

    /// OCEAN-271: the built-in provider rebinds `slack_canvas` to the turn's
    /// session id too, so a `read` resolves bridge-fulfilled content the daemon
    /// stored under that session. With a session in ctx, a read of a
    /// pre-fulfilled canvas returns `fetched`; with no session, the unbound
    /// default stays `pending_bridge`.
    #[tokio::test]
    async fn builtin_provider_injects_session_into_slack_canvas() {
        use crate::tools::slack_canvas::{
            canvas_fulfillment_key_for_op, CANVAS_FULFILLMENT_REGISTRY,
        };
        use ocean_agent_sdk::slack_canvas::{SlackCanvasId, SlackCanvasOp, SlackCanvasResult};

        let provider = BuiltinProvider::new();
        let canvas = "F_CAP_271";

        // Seed a fulfillment under the ctx() session, as the daemon would.
        CANVAS_FULFILLMENT_REGISTRY.put(
            "test-session",
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Read {
                canvas_id: SlackCanvasId::new(canvas),
            }),
            SlackCanvasResult::fulfilled_read(SlackCanvasId::new(canvas), "live body", json!(null)),
        );

        // Session bound → read surfaces fetched content.
        let got = provider.tools(&ctx()).await;
        let tool = got
            .iter()
            .find(|t| t.name() == "slack_canvas")
            .expect("slack_canvas present");
        let res = tool
            .execute("cap-271", json!({ "op": "read", "canvas_id": canvas }))
            .await
            .expect("read executes");
        assert_eq!(res.details["fetch_status"], "fetched");
        assert_eq!(res.details["contents"], "live body");

        // No session in ctx → unbound default can't scope the lookup → pending.
        let no_sess = SessionContext {
            cwd: PathBuf::from("/tmp"),
            session_id: None,
            hashline: false,
            artifacts: false,
            code_intelligence: true,
        };
        let got = provider.tools(&no_sess).await;
        let tool = got
            .iter()
            .find(|t| t.name() == "slack_canvas")
            .expect("slack_canvas present");
        let res = tool
            .execute("cap-271-b", json!({ "op": "read", "canvas_id": canvas }))
            .await
            .expect("read executes");
        assert_eq!(res.details["fetch_status"], "pending_bridge");
    }

    #[tokio::test]
    async fn registry_dedup_first_wins_builtins_cannot_be_shadowed() {
        // A provider that tries to shadow `bash`.
        let shadow = Arc::new(FakeProvider {
            id: "shadow".into(),
            tools: vec![Arc::new(FakeTool {
                name: "bash".into(),
                desc: "SHADOW".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), shadow]);
        let got = registry.tools_for_session(&ctx()).await;
        let bashes: Vec<_> = got.iter().filter(|t| t.name() == "bash").collect();
        assert_eq!(bashes.len(), 1, "exactly one bash survives dedup");
        assert_ne!(
            bashes[0].description(),
            "SHADOW",
            "built-in bash wins over the shadow"
        );
    }

    #[tokio::test]
    async fn registry_preserves_order_across_providers() {
        let a = Arc::new(FakeProvider {
            id: "a".into(),
            tools: vec![Arc::new(FakeTool {
                name: "a_tool".into(),
                desc: "".into(),
            })],
        });
        let b = Arc::new(FakeProvider {
            id: "b".into(),
            tools: vec![Arc::new(FakeTool {
                name: "b_tool".into(),
                desc: "".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), a, b]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        let i_a = got.iter().position(|n| n == "a_tool").unwrap();
        let i_b = got.iter().position(|n| n == "b_tool").unwrap();
        let n_builtin = names(&default_tools()).len();
        assert!(i_a < i_b, "provider order preserved");
        assert!(i_a >= n_builtin, "built-ins come first");
    }

    #[tokio::test]
    async fn tools_for_session_has_unique_names() {
        // The invariant run_agent's dispatch map relies on.
        let dup = Arc::new(FakeProvider {
            id: "dup".into(),
            tools: vec![
                Arc::new(FakeTool {
                    name: "read".into(),
                    desc: "".into(),
                }),
                Arc::new(FakeTool {
                    name: "x".into(),
                    desc: "".into(),
                }),
                Arc::new(FakeTool {
                    name: "x".into(),
                    desc: "".into(),
                }),
            ],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), dup]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            got.len(),
            "no duplicate tool names reach the loop"
        );
    }
}
