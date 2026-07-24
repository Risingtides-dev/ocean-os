use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{
    ImageMeta, PermissionMode, Project, ProjectId, PromptImage, PromptRequest, PromptResponse,
    RequestId, SessionDetail, SessionId, SessionRunState, SessionSummary, SessionToolContext,
    SessionTranscriptEntry, TokenUsage,
};
use ocean_protocol::{AssistantMessage, Content, Message, Model, StopReason, Usage};
use ocean_providers::{
    resolve_provider_config, resolve_provider_config_from_env, ProviderConfig, ProviderId,
    ProviderReadiness,
};
// Re-export the model catalogue (and the env snapshot readiness reads from) so
// the daemon can serve a picker without taking a direct ocean-providers
// dependency.
pub use ocean_providers::{
    known_models, known_models_with_readiness, KnownModel, ProviderEnv, ReadyModel,
};
use ocean_runtime::{
    run_agent_with_history, AgentConfig, AgentError, AgentEvent, BuiltinProvider,
    CapabilityProvider, CapabilityRegistry, PermissionDecision, PermissionPolicy, SessionContext,
    SharedTool,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod config;
pub use config::{DaemonConfig, McpSection, OffshoreSection};
/// Filesystem-first agent definitions (folder = agent). Module-qualified to
/// avoid colliding with `ocean_runtime::AgentConfig`; refer to the folder-agent
/// config as `agentdir::AgentConfig`.
pub mod agentdir;
mod durable;
mod memory_tools;
pub use memory_tools::{list_memories, MemoryView};
mod oauth_refresh;
pub use agentdir::{AgentDef, ResolveError as AgentDirResolveError};
mod project;
pub use project::{git_head_info, WorktreeInfo};
mod rooms;
pub use rooms::{RoomRecord, RoomRegistry, RoomStoreError};

const APP_NAME: &str = "ocean-rs";

/// How long to wait for a single MCP server to connect + list its tools at
/// startup. A server that exceeds this contributes no tools (non-fatal) rather
/// than wedging daemon startup.
const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Default page size for a collection list (sessions, projects) when a caller
/// does not specify a limit (OCEAN-250). The list endpoints used to return every
/// row — a daemon with thousands of historical sessions answered a multi-MB JSON
/// blob on every poll. This caps the default; callers page through with the
/// returned cursor. Mirrors `ocean_store::DEFAULT_LIST_LIMIT`.
pub const DEFAULT_LIST_LIMIT: usize = 100;

/// Hard ceiling on a single collection-list page (OCEAN-250). A caller-supplied
/// limit is clamped to this so no list request can be coerced into an unbounded
/// load + serialize. Mirrors `ocean_store::MAX_LIST_LIMIT`.
pub const MAX_LIST_LIMIT: usize = 1000;

/// Default and maximum result counts for transcript history search.
pub const DEFAULT_HISTORY_SEARCH_LIMIT: usize = 20;
pub const MAX_HISTORY_SEARCH_LIMIT: usize = 50;
/// Bound request work before any transcript files are opened.
pub const MAX_HISTORY_SEARCH_QUERY_CHARS: usize = 512;
/// Maximum cumulative size of persisted session files searched by one request.
pub const MAX_HISTORY_SEARCH_STORE_BYTES: u64 = 64 * 1024 * 1024;

/// Search-capacity error returned before raw session files are deserialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistorySearchCapacityError {
    pub observed_bytes: u64,
    pub max_bytes: u64,
}

impl std::fmt::Display for HistorySearchCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transcript history store is {} bytes; search budget is {} bytes",
            self.observed_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for HistorySearchCapacityError {}

/// Classification of a deterministic transcript-text match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryMatchKind {
    Exact,
    Lexical,
    Fuzzy,
}

/// One display-transcript match returned by daemon-owned history search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistorySearchHit {
    pub hit_id: String,
    pub session_id: SessionId,
    pub session_title: String,
    pub role: String,
    pub excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    /// Always serialized (`null` for unbound legacy sessions) so Recall clients
    /// receive one stable hit shape.
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub score: f64,
    pub match_kind: HistoryMatchKind,
}

/// Clamp a caller-supplied collection-list limit into the allowed range. `None`
/// ⇒ [`DEFAULT_LIST_LIMIT`]; any value is capped at [`MAX_LIST_LIMIT`] and
/// floored at 1 so `0` can never request an empty-yet-`has_more` page.
pub fn clamp_list_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

/// One bounded page of a collection list (OCEAN-250).
///
/// `items` holds at most the effective limit of rows, in the list's existing
/// stable order. `next_cursor` is the opaque cursor a client replays to fetch the
/// next page (here, the `id` of the last returned item); it is `Some(..)` when
/// more rows exist and `None` at the end. `has_more` is the same signal as a
/// bool. The list-endpoint analogue of `ocean_store::TranscriptPage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// This page's items, in list order, at most the effective limit.
    pub items: Vec<T>,
    /// Cursor for the next page, or `None` at the end of the list.
    pub next_cursor: Option<String>,
    /// Whether at least one more item exists beyond this page.
    pub has_more: bool,
}

/// Order projects newest-first by `(updated_ms DESC, id DESC)` (OCEAN-250).
///
/// The on-disk project index has no inherent order; a stable sort here gives the
/// list endpoint a deterministic sequence so its keyset cursor is meaningful (and
/// matches how sessions are ordered). The `id` tiebreak keeps the order total
/// when two projects share an `updated_ms`.
fn sort_projects_newest_first(projects: &mut [Project]) {
    projects.sort_by(|a, b| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// Page an already-ordered list by an opaque per-item cursor key (OCEAN-250).
///
/// `items` MUST already be in the caller's stable display order (these lists are
/// fully sorted before paging). `key_of` extracts each item's cursor string (a
/// stable, unique id). When `after` is `Some`, we resume *just past* the item
/// whose key equals it; a cursor that matches nothing (item deleted since it was
/// handed out) falls back to the start rather than erroring, so paging is
/// resilient to a stale cursor. At most `clamp_list_limit(limit)` items are
/// returned, with `next_cursor` = the last returned item's key when more remain.
///
/// This is the in-memory counterpart to the store's SQL `LIMIT + 1` sentinel:
/// the file-backed session/project stores load their (already bounded by the
/// filesystem) set and slice it here, so a single response is still capped.
fn paginate_by_id<T>(
    items: Vec<T>,
    after: Option<&str>,
    limit: Option<usize>,
    key_of: impl Fn(&T) -> String,
) -> Page<T> {
    let effective_limit = clamp_list_limit(limit);
    // Resume index: the first item *after* the cursor. Unknown cursor ⇒ 0 (start).
    let start = match after {
        Some(cursor) => items
            .iter()
            .position(|it| key_of(it) == cursor)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };
    let remaining = items.len().saturating_sub(start);
    let take = effective_limit.min(remaining);
    let has_more = remaining > take;
    let page: Vec<T> = items.into_iter().skip(start).take(take).collect();
    let next_cursor = if has_more {
        page.last().map(&key_of)
    } else {
        None
    };
    Page {
        items: page,
        next_cursor,
        has_more,
    }
}

/// Build the first user `Message` of a turn from the prompt text and any
/// attached images (OCEAN-115). The content is always `[Text, Image, Image…]`:
/// the text leads, then one `Content::Image` block per image, so the provider
/// encoders (OCEAN-99) serialize vision input alongside the instruction.
///
/// `images.is_none()`/empty reduces to the plain text path — identical to the
/// previous `Message::user_text` behaviour, keeping non-vision turns unchanged.
///
/// Each image's `data` accepts either a bare base64 string or a
/// `data:<mime>;base64,<body>` URL; the prefix is stripped so `Content::Image`
/// holds only the base64 body it expects.
/// Compose the surface-switch notice (Fix 3, TASK-65) prepended to a user turn
/// when the session's steering surface changed since the last turn. It is emitted
/// AHEAD of the per-turn `[FLAG]` so the model leads with an explicit
/// adjust-your-rendering cue. `flag`/`from` are canonical [`system_prompt::
/// surface_flag`] labels (the new and previous surfaces).
///
/// Single source of truth for the notice text. The rigid lead-in
/// `[surface switch: the user is now messaging you via [` is the exact anchor the
/// DISPLAY strip (`strip_surface_switch_notice` in `session/mod.rs`) peels off
/// transcript projections, and the trailing `]\n` is that strip's terminator —
/// the notice is a single line, so its only `]\n` is the terminator. Kept in sync
/// with the stripper's literals by `surface_switch_notice_anchors_display_strip_marker`;
/// rewording the notice fails that guard rather than silently leaking the preamble.
fn compose_surface_switch_notice(flag: &str, from: &str) -> String {
    format!(
        "[surface switch: the user is now messaging you via [{flag}] (was [{from}]). \
         Adjust your rendering and tone to this surface.]\n"
    )
}

fn build_user_message(text: String, images: Option<&[PromptImage]>) -> Message {
    let mut content = vec![Content::text(text)];
    if let Some(images) = images {
        for img in images {
            let data = strip_data_url_prefix(&img.data).to_string();
            content.push(Content::Image {
                data,
                mime_type: img.mime_type.clone(),
            });
        }
    }
    Message::User {
        content,
        timestamp: ocean_protocol::now_ms(),
    }
}

/// If `s` is a `data:<mime>;base64,<body>` URL, return just `<body>`; otherwise
/// return `s` unchanged. `Content::Image.data` is the raw base64 body.
fn strip_data_url_prefix(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("data:") {
        if let Some(idx) = rest.find("base64,") {
            return &rest[idx + "base64,".len()..];
        }
    }
    s
}

/// Subdirectory under the config dir where plugin packs live, each as its own
/// directory containing a `plugin.toml`. Overridable via `OCEAN_PLUGINS_DIR`
/// (mirrors how MCP servers are config-driven and how the config dir itself is
/// env-overridable).
const PLUGINS_DIRNAME: &str = "plugins";

/// Inner runtime state that's swappable at runtime (model, provider, key).
/// Kept behind an `Arc<RwLock<_>>` so a single AgentRuntime instance can
/// re-bind to a different model without restarting the daemon process.
#[derive(Debug, Clone)]
struct RuntimeState {
    model: Model,
    api_key: Option<String>,
    backend_name: String,
    provider_config: ProviderConfig,
}

/// Native Ocean agent runtime.
///
/// Daemon-owned wrapper around `ocean-runtime` that adds Ocean's session,
/// history, config, and permission layers on top of the underlying agent loop.
/// Opaque ownership of one session's mutation lane. The guard is deliberately
/// not exposed so callers cannot unlock early or operate on another id.
pub struct SessionOperationLease {
    id: SessionId,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SessionOperationLease {
    pub fn session_id(&self) -> SessionId {
        self.id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOperationBusy;

impl std::fmt::Display for SessionOperationBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session has an active operation")
    }
}

impl std::error::Error for SessionOperationBusy {}

#[derive(Debug, Clone)]
pub struct AgentRuntime {
    config_dir: PathBuf,
    state: Arc<RwLock<RuntimeState>>,
    /// Source of tools for every turn. Built-ins plus any MCP/skill providers,
    /// flattened per turn through `tools_for_session`. The agent loop never
    /// builds tools directly — this is the one seam that replaced the old
    /// hardcoded `default_tools()` call. Assembled once at startup.
    capabilities: Arc<CapabilityRegistry>,
    /// Per-session turn serialization. A turn against a session must hold this
    /// session's lock across load → run → save, so two concurrent turns on the
    /// same session can't both load the same history and clobber each other's
    /// transcript (lost-update corruption). Keyed by SessionId; entries are
    /// created on demand and kept for the process lifetime (one cheap Arc<Mutex>
    /// per session ever touched — negligible).
    session_locks: Arc<std::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>,
    /// Lifecycle hooks from `<config_dir>/ocean.toml` (`[[hooks.Stop]]`).
    ///
    /// This is the plugin-agnostic wake/engagement seam: at turn end the runtime
    /// fires the configured `Stop` hook subprocesses (ocean-hooks contract —
    /// `{cwd, session_id, stop_hook_active}` on stdin, optional
    /// `{"decision":"block","reason":…}` on stdout) and a blocking decision
    /// continues the turn with `reason` as the next user message. Ocean core
    /// knows nothing about any specific consumer (stitchpad, notifiers, audit
    /// loggers); those live entirely in operator config. Empty config → zero
    /// cost, zero behavior change.
    hooks: ocean_hooks::HooksConfig,
    /// Test-only override for the per-turn environment snapshot used by provider
    /// failover (OCEAN-275). Production always reads the real process env via
    /// [`AgentRuntime::turn_env`]; tests inject a deterministic [`ProviderEnv`]
    /// here so the failover policy can be exercised end-to-end through `prompt`
    /// without mutating (and racing on) the global process environment.
    #[cfg(test)]
    test_env: Option<ProviderEnv>,
    /// Test-only scripted provider for [`AgentRuntime::compact_session`]'s
    /// one-shot summarize call, mirroring the `test_env` idiom: production
    /// always dispatches through `ocean_protocol::stream_simple`.
    #[cfg(test)]
    test_compact_provider: Option<TestCompactProvider>,
}

/// Debug-opaque wrapper so the `dyn Provider` test seam doesn't break
/// `AgentRuntime`'s `#[derive(Debug)]`.
#[cfg(test)]
#[derive(Clone)]
struct TestCompactProvider(Arc<dyn ocean_protocol::Provider>);

#[cfg(test)]
impl std::fmt::Debug for TestCompactProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestCompactProvider")
    }
}

impl AgentRuntime {
    /// Build the runtime from the environment with **built-in tools only**.
    ///
    /// MCP/extension providers are connected separately via
    /// [`with_extensions`](Self::with_extensions), which is async (it spawns and
    /// handshakes child processes). Keeping `from_env` sync and built-ins-only
    /// preserves every existing caller and test; the daemon upgrades the
    /// registry with `.with_extensions().await` right after construction.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::with_config_dir(config_dir_from_env())
    }

    /// Build the runtime rooted at an explicit config dir instead of the
    /// process-global `OCEAN_CONFIG_DIR`.
    ///
    /// The config dir owns the on-disk project/session store, so injecting it
    /// lets embedders — and parallel tests — each own an isolated store without
    /// racing on shared process env (two runtimes reading a clobbered
    /// `OCEAN_CONFIG_DIR` otherwise collide on the same `projects.json`,
    /// producing atomic-rename races and cross-test 404s). Model and credential
    /// resolution still read the process env via `build_state_from_env`; only
    /// the on-disk root is injected. `from_env` is the production default and
    /// simply resolves the dir from the environment first.
    pub fn with_config_dir(config_dir: PathBuf) -> anyhow::Result<Self> {
        let state = build_state_from_env(&config_dir)?;
        // Stop hooks are fail-open at load: a malformed ocean.toml already logs
        // loudly in `build_capability_registry`; the runtime must still start
        // (matching the MCP posture), so hooks degrade to none rather than
        // taking the agent down.
        let hooks = config::DaemonConfig::load(&config_dir)
            .map(|cfg| cfg.hooks)
            .unwrap_or_default();
        let runtime = Self {
            config_dir,
            state: Arc::new(RwLock::new(state)),
            capabilities: Arc::new(CapabilityRegistry::builtin_only()),
            session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            hooks,
            #[cfg(test)]
            test_env: None,
            #[cfg(test)]
            test_compact_provider: None,
        };
        runtime.migrate_legacy_sessions();
        // Bound on-disk session growth: prune session files past the TTL once
        // at startup, off the hot turn path (OCEAN-209).
        runtime.session_file_gc();
        Ok(runtime)
    }

    /// Connect configured MCP servers and fold their tools into the capability
    /// registry, on top of the built-ins. Reads `<config_dir>/ocean.toml`;
    /// absent/empty config leaves the registry built-ins-only. Each server is
    /// connected with a timeout and non-fatally — a server that fails to start
    /// logs a warning and contributes no tools, never blocking startup.
    ///
    /// Consuming builder so the daemon can do
    /// `AgentRuntime::from_env()?.with_extensions(longhouse).await` before sharing
    /// the runtime behind an `Arc`.
    ///
    /// `longhouse` is the daemon's shared read-side topic registry. Passing the
    /// SAME `Arc` the HTTP routes serve off means a council convened via the
    /// `longhouse__convene` tool, and the topics an agent reads via
    /// `longhouse__board_read`, share one observable board with the operator
    /// surface. `None` (tests / embedders without a daemon) simply omits the
    /// Longhouse provider.
    pub async fn with_extensions(
        mut self,
        longhouse: Option<ocean_longhouse::LonghouseRegistryHandle>,
    ) -> Self {
        let registry = build_capability_registry(&self.config_dir, longhouse).await;
        self.capabilities = Arc::new(registry);
        self
    }

    /// Get (or create) the per-session turn lock for `id`.
    ///
    /// Two robustness properties (OCEAN-182):
    ///
    /// 1. **Poison recovery.** A panic inside the guarded section (load → run →
    ///    save) poisons the std `Mutex` wrapping the registry. We recover via
    ///    `into_inner()` — matching the daemon's event-bus idiom — instead of
    ///    `.expect()`, so a single panicked turn can't permanently wedge every
    ///    future turn on every session.
    ///
    /// 2. **Stale-entry pruning.** The HashMap held one `Arc<Mutex<()>>` per
    ///    session ever touched, never removed → unbounded growth on a
    ///    long-lived daemon. We opportunistically `retain` only entries that
    ///    still have an active holder before handing out the requested lock.
    ///
    ///    Strong-count reasoning: an in-flight turn holds a CLONE of the Arc, so
    ///    a live entry has `strong_count >= 2` (one in the map + one per active
    ///    turn). An entry no turn holds has `strong_count == 1` (only the map).
    ///    `retain(|_, l| Arc::strong_count(l) > 1)` therefore drops exactly the
    ///    idle entries. Crucially this runs BEFORE we `entry(id).or_insert` the
    ///    requested id: the lock we're about to hand out is either not yet in
    ///    the map (fresh insert after retain) or is an existing live entry the
    ///    caller already holds — never an idle entry that retain would prune.
    ///    The clone we return then bumps the requested id's count to >= 2 before
    ///    the registry lock is released, so a concurrent `session_lock` call
    ///    can't prune it out from under us either.
    fn session_lock(&self, id: SessionId) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.session_locks.lock().unwrap_or_else(|p| p.into_inner());
        // Drop entries no active turn holds. Safe to run before inserting `id`:
        // it only ever touches OTHER entries (an existing `id` entry is, by
        // definition, currently held by this caller's in-flight turn and so has
        // strong_count >= 2).
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
        map.entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Acquire the shared mutation lane for a durable queued internal operation.
    pub async fn session_operation(&self, id: SessionId) -> SessionOperationLease {
        SessionOperationLease {
            id,
            _guard: self.session_lock(id).lock_owned().await,
        }
    }

    /// Try to acquire the shared mutation lane without queueing. Interactive
    /// admission and compaction use this so a busy session returns conflict
    /// before any model call or lifecycle claim.
    pub fn try_session_operation(
        &self,
        id: SessionId,
    ) -> Result<SessionOperationLease, SessionOperationBusy> {
        self.session_lock(id)
            .try_lock_owned()
            .map(|guard| SessionOperationLease { id, _guard: guard })
            .map_err(|_| SessionOperationBusy)
    }

    /// Test-only view of the live per-session lock registry size, so prune
    /// tests can assert idle entries were evicted.
    #[cfg(test)]
    fn session_lock_count(&self) -> usize {
        self.session_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    fn snapshot(&self) -> RuntimeState {
        self.state.read().expect("runtime state poisoned").clone()
    }

    /// The per-turn environment snapshot driving provider failover (OCEAN-275).
    ///
    /// Production reads the live process environment. In test builds a runtime
    /// may carry an injected [`ProviderEnv`] (`test_env`) so failover behavior is
    /// deterministic without touching global process env; when unset it falls
    /// back to the process env exactly like production.
    fn turn_env(&self) -> ProviderEnv {
        #[cfg(test)]
        if let Some(env) = &self.test_env {
            return env.clone();
        }
        ProviderEnv::from_process()
    }

    pub fn backend_name(&self) -> String {
        self.snapshot().backend_name
    }

    pub fn provider_readiness(&self) -> ProviderReadiness {
        self.snapshot().provider_config.readiness()
    }

    /// The ordered list of **ready fallback providers** for the current model
    /// (OCEAN-275), as non-secret `provider/model` labels.
    ///
    /// These are the alternates a degraded primary (or a pre-stream
    /// connect-failure) would route to, highest-priority first, drawn from the
    /// same environment a turn would use. Surfaced in `/ready` so an operator can
    /// see at a glance whether failover has anywhere to go — an empty list while
    /// the primary is degraded is the "all providers degraded" condition. Carries
    /// no credentials, only provider/model identifiers.
    pub fn fallback_providers(&self) -> Vec<String> {
        let primary = self.snapshot().provider_config.selection.provider;
        let env = self.turn_env();
        ocean_providers::fallback_candidates(&env, &primary)
            .into_iter()
            .map(|cfg| {
                format!(
                    "{}/{}",
                    cfg.selection.provider.as_str(),
                    cfg.selection.model
                )
            })
            .collect()
    }

    /// Currently-bound model and provider id, for `/model` read paths.
    pub fn current_model(&self) -> (String, String) {
        let s = self.snapshot();
        (
            s.provider_config.selection.provider.as_str().to_string(),
            s.provider_config.selection.model.clone(),
        )
    }

    /// Swap the active model. Resolves a fresh provider config from the
    /// process environment with `OCEAN_MODEL` overridden, then atomically
    /// replaces the runtime state. Fails (without mutating anything) if
    /// the new selection doesn't resolve or has no credential.
    pub fn set_model(&self, model_spec: &str) -> anyhow::Result<(String, String)> {
        let mut env = ProviderEnv::from_process();
        env.vars
            .insert("OCEAN_MODEL".to_string(), model_spec.to_string());
        // If model spec doesn't have a known provider, the env-driven
        // OCEAN_PROVIDER override carries through; we don't force-clear it.
        let provider_config = resolve_provider_config(&env)
            .map_err(|e| anyhow::anyhow!("failed to resolve model `{model_spec}`: {e}"))?;
        let state = state_from_provider_config(provider_config)?;
        let label = (
            state
                .provider_config
                .selection
                .provider
                .as_str()
                .to_string(),
            state.provider_config.selection.model.clone(),
        );
        *self.state.write().expect("runtime state poisoned") = state;
        // Remember this choice so the next daemon start resumes on it instead of
        // snapping back to a hardcoded default. Last-used wins.
        persist_last_model(&self.config_dir, &label.1);
        Ok(label)
    }

    /// Resolve a fresh [`RuntimeState`] for a model alias **without** mutating
    /// the runtime's global state or persisting the choice (OCEAN-36).
    ///
    /// This is the per-turn counterpart to [`set_model`](Self::set_model): it
    /// runs the same provider-config resolution (so credentials, base_url, and
    /// the openai-vs-anthropic provider routing all match), but returns the
    /// state for the caller to use for a single turn. Two concurrent turns can
    /// therefore drive different models without racing through the shared
    /// global selection. Fails (touching nothing) if the alias doesn't resolve
    /// or has no credential.
    fn resolve_state_for_model(&self, model_spec: &str) -> anyhow::Result<RuntimeState> {
        let mut env = ProviderEnv::from_process();
        env.vars
            .insert("OCEAN_MODEL".to_string(), model_spec.to_string());
        let provider_config = resolve_provider_config(&env)
            .map_err(|e| anyhow::anyhow!("failed to resolve model `{model_spec}`: {e}"))?;
        state_from_provider_config(provider_config)
    }

    /// One-shot, single-completion call against an arbitrary model alias on a
    /// FRESH context — no session, no history, no tools, no agent loop. Resolves
    /// `model_spec` through the same provider-config machinery as a real turn
    /// ([`resolve_state_for_model`]), makes ONE `stream_simple` call with the
    /// given system + user text, and returns the collected assistant text plus
    /// the resolved model id (for attribution).
    ///
    /// This is the seam the daemon's advisor observer runs on: it deliberately
    /// does not touch the runtime's global state, so an advisor pass is a pure
    /// side call that can never perturb an operator turn. Errors (bad alias,
    /// missing credential, provider failure) are returned to the caller, which
    /// logs and drops them — the advisor is fully best-effort.
    pub async fn complete_once(
        &self,
        model_spec: &str,
        system_prompt: &str,
        user_text: &str,
    ) -> anyhow::Result<(String, String)> {
        use futures::StreamExt as _;

        let state = self.resolve_state_for_model(model_spec)?;
        let model_id = state.model.id.clone();

        let ctx = ocean_protocol::Context {
            system_prompt: Some(system_prompt.to_string()),
            messages: vec![Message::user_text(user_text)],
            tools: Vec::new(),
            dynamic_tool_declarations: Vec::new(),
            tool_choice: ocean_protocol::ToolChoice::Auto,
        };

        let mut options = ocean_protocol::StreamOptions {
            api_key: state.api_key.clone(),
            base_url: Some(state.provider_config.selection.base_url.clone()),
            auth: auth_method_for(&state.provider_config),
            ..Default::default()
        };
        if let Some(account_id) = &state.provider_config.account_id {
            options
                .headers
                .insert("chatgpt-account-id".into(), account_id.clone());
        }

        let mut stream = ocean_protocol::stream_simple(&state.model, &ctx, &options).await?;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev? {
                ocean_protocol::AssistantMessageEvent::TextDelta { delta, .. } => {
                    text.push_str(&delta);
                }
                ocean_protocol::AssistantMessageEvent::Done { message, .. } => {
                    // Fall back to the finalized message text if no deltas were
                    // observed (some providers emit only a terminal message).
                    if text.is_empty() {
                        for c in &message.content {
                            if let Content::Text { text: t } = c {
                                text.push_str(t);
                            }
                        }
                    }
                    break;
                }
                ocean_protocol::AssistantMessageEvent::Error { error, .. } => {
                    let msg = error
                        .content
                        .iter()
                        .find_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "provider error".to_string());
                    anyhow::bail!("advisor provider error: {msg}");
                }
                _ => {}
            }
        }
        Ok((text, model_id))
    }

    pub async fn prompt(&self, req: PromptRequest, control: PromptControl) -> PromptResponse {
        self.prompt_inner(req, control, None).await
    }

    /// Execute a turn under a daemon-admitted session operation lease. The
    /// caller retains ownership so it can hold the same lease through terminal
    /// lifecycle publication after this future returns.
    pub async fn prompt_with_lease(
        &self,
        req: PromptRequest,
        control: PromptControl,
        lease: &SessionOperationLease,
    ) -> PromptResponse {
        self.prompt_inner(req, control, Some(lease)).await
    }

    async fn prompt_inner(
        &self,
        req: PromptRequest,
        control: PromptControl,
        lease: Option<&SessionOperationLease>,
    ) -> PromptResponse {
        let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
        let mut req = req;
        req.request_id = Some(request_id);

        let start = Instant::now();
        // Report the turn/session cwd the daemon resolved from the client request,
        // not the long-lived daemon process cwd. Returning `current_dir()` here
        // made legacy `/v1/prompt` clients look bound to wherever the daemon was
        // launched even when tool/session execution used a different cwd.
        let cwd = req.cwd.clone();

        // Keep OAuth subscription tokens fresh BEFORE credential resolution
        // reads auth.json — an expired claude-code/codex block used to
        // hard-fail the turn as "missing credential" until the user re-ran the
        // vendor CLI's login. Cheap no-op when everything is fresh;
        // single-flight + per-block cooldown inside; never errors the turn.
        if let Some(auth_file) = self.turn_env().auth_file.clone() {
            oauth_refresh::ensure_fresh(&auth_file).await;
        }

        // Resolve the EFFECTIVE turn state before any readiness/dispatch check
        // (OCEAN-36 + Codex). When the turn pins a per-session `model_id`, the
        // override — not the runtime's global model — must drive the provider
        // preflight, the Fake-vs-real dispatch, and the run itself. Resolving it
        // here (rather than inside `run_prompt`) means a turn pinned to a ready
        // model is no longer rejected because the *global* model is degraded, and
        // an ACP turn pinned to a real model is never silently routed through a
        // global `fake-ok`. A bad alias fails the turn cleanly, touching nothing.
        let turn_state = match control.model_id.as_deref() {
            // Explicit per-request model: fail HARD on a bad alias — the operator
            // pinned it, so surface the error rather than silently substituting.
            Some(model_spec) => match self.resolve_state_for_model(model_spec) {
                Ok(state) => Some(state),
                Err(e) => {
                    return PromptResponse {
                        request_id: Some(request_id),
                        ok: false,
                        session_id: req.session_id,
                        code: None,
                        wall_ms: start.elapsed().as_millis() as u64,
                        stdout: String::new(),
                        stderr: e.to_string(),
                        cwd,
                        usage: TokenUsage::default(),
                    };
                }
            },
            // No explicit model: a folder-as-agent's declared `agent.toml` model
            // drives the turn, but fail SOFT — an unresolvable / not-yet-mapped
            // agent model (e.g. an eve-style gateway id Ocean doesn't map) falls
            // back to the global model with a warning, never breaking the turn.
            None => match control.agent_model.as_deref() {
                Some(model_spec) => match self.resolve_state_for_model(model_spec) {
                    Ok(state) => Some(state),
                    Err(e) => {
                        tracing::warn!(
                            agent_model = %model_spec,
                            error = %e,
                            "agent's declared model did not resolve; using the global model"
                        );
                        None
                    }
                },
                None => None,
            },
        };
        let global_snapshot = self.snapshot();
        let turn_snapshot: RuntimeState = turn_state.unwrap_or_else(|| global_snapshot.clone());

        // Resolve the environment ONCE for the whole failover decision (selection
        // + connect-failure), so the fallback list is computed against a single
        // consistent snapshot and the process env is read once per turn.
        let env = self.turn_env();

        // Selection-time failover (OCEAN-275). If the EFFECTIVE provider for this
        // turn is not ready (degraded / missing credential), route to a ready
        // alternate BEFORE the turn starts — the fully-safe failover point, since
        // nothing has run yet. `resolve_turn_state_with_failover` returns the
        // alternate's state (logging `primary degraded → routed to alternate`), or
        // — when no alternate is ready — a clear "all providers degraded" error
        // rather than the bare single-provider preflight message. A ready primary
        // passes straight through untouched.
        let requested_model = turn_snapshot.provider_config.selection.model.clone();
        let effective = match Self::resolve_turn_state_with_failover(turn_snapshot, &env) {
            Ok(state) => state,
            Err(stderr) => {
                return PromptResponse {
                    request_id: Some(request_id),
                    ok: false,
                    session_id: req.session_id,
                    code: None,
                    wall_ms: start.elapsed().as_millis() as u64,
                    stdout: String::new(),
                    stderr,
                    cwd,
                    usage: TokenUsage::default(),
                };
            }
        };
        // OCEAN-275 honesty: selection-time failover keeps the turn alive, but
        // hiding it from an operator who pinned a model is lying — announce the
        // reroute on the event stream BEFORE any output.
        if effective.provider_config.selection.model != requested_model {
            if let Some(sink) = control.event_sink.as_ref() {
                let _ = sink.send(AgentEvent::ModelRerouted {
                    session_id: req.session_id.map(|s| s.to_string()),
                    requested: requested_model,
                    effective: effective.provider_config.selection.model.clone(),
                    reason: "provider degraded at selection (missing credential or not ready)"
                        .into(),
                });
            }
        }

        // Run the turn against the effective provider; on a pre-stream
        // connect-failure with a transient/availability error, fail over once to
        // the next ready alternate (bounded — see `run_turn_with_failover`). This
        // never fails over mid-stream: the moment any output streamed, the attempt
        // is final.
        let result = self
            .run_turn_with_failover(req.clone(), control, effective, &env, lease)
            .await;

        match result {
            Ok((session_id, stdout, stderr, usage)) => PromptResponse {
                request_id: Some(request_id),
                ok: true,
                session_id: Some(session_id),
                code: Some(0),
                wall_ms: start.elapsed().as_millis() as u64,
                stdout,
                stderr,
                cwd,
                usage,
            },
            Err(e) => {
                // Surface a per-turn timeout distinctly so the daemon can map it
                // to HTTP 408. `run_agent_with_history` propagates the failure as
                // an `anyhow::Error`, so recover the concrete `AgentError` variant
                // by downcast; a `Timeout` sets code 408, everything else stays
                // `None` (generic failure).
                let code = e
                    .downcast_ref::<AgentError>()
                    .filter(|err| matches!(err, AgentError::Timeout { .. }))
                    .map(|_| 408);
                PromptResponse {
                    request_id: Some(request_id),
                    ok: false,
                    session_id: req.session_id,
                    code,
                    wall_ms: start.elapsed().as_millis() as u64,
                    stdout: String::new(),
                    stderr: e.to_string(),
                    cwd,
                    usage: TokenUsage::default(),
                }
            }
        }
    }

    /// Selection-time provider failover (OCEAN-275).
    ///
    /// Given the effective per-turn state, returns a state whose provider is
    /// *ready* to serve the turn:
    /// - if the requested provider is already ready, returns it unchanged;
    /// - otherwise resolves the highest-priority ready *alternate* (see
    ///   [`ocean_providers::resolve_fallback_config`]), logs the reroute, and
    ///   returns the alternate's state;
    /// - if no alternate is ready, returns `Err(message)` describing that **all**
    ///   providers are degraded — a clear failure instead of a silent hang or a
    ///   bare single-provider preflight error.
    ///
    /// This is the fully-safe failover point: it runs before any turn work, so
    /// swapping providers here can never replay output or side effects.
    ///
    /// `env` is the process-environment snapshot resolved once per turn; the
    /// fallback candidates are drawn from it so credentials line up with the
    /// primary's. Taking it as a parameter (rather than reading the process env
    /// here) keeps the policy deterministic and unit-testable.
    fn resolve_turn_state_with_failover(
        requested: RuntimeState,
        env: &ProviderEnv,
    ) -> Result<RuntimeState, String> {
        // Ready primary → use it as-is. This is the overwhelmingly common path
        // and adds only a readiness check (no env re-resolution).
        if Self::preflight_error_for(&requested).is_none() {
            return Ok(requested);
        }

        // Primary is degraded. Look for a ready alternate, resolved from the same
        // environment the primary used (so credentials line up).
        let primary = requested.provider_config.selection.provider.clone();
        let primary_model = requested.provider_config.selection.model.clone();
        let alternate = ocean_providers::resolve_fallback_config(env, &primary);

        match alternate.and_then(|cfg| state_from_provider_config(cfg).ok()) {
            Some(alt_state) => {
                tracing::warn!(
                    primary_provider = primary.as_str(),
                    primary_model = %primary_model,
                    fallback_provider =
                        alt_state.provider_config.selection.provider.as_str(),
                    fallback_model = %alt_state.provider_config.selection.model,
                    "provider degraded at selection; routing turn to ready fallback provider"
                );
                Ok(alt_state)
            }
            None => {
                // Nothing ready anywhere — surface the primary's own reason plus
                // the explicit "all providers degraded" signal.
                let primary_detail = Self::preflight_error_for(&requested)
                    .unwrap_or_else(|| "provider is not ready".to_string());
                Err(format!(
                    "all providers degraded: {primary_detail}. No ready fallback provider \
                     is configured (set credentials for an alternate, or configure \
                     {} to point at a ready one).",
                    ocean_providers::ENV_PROVIDER_FALLBACK
                ))
            }
        }
    }

    /// Run one turn against `state`, failing over once to the next ready
    /// alternate on a **pre-stream** availability failure (OCEAN-275).
    ///
    /// Bounded and safe:
    /// - the primary attempt runs first;
    /// - failover is attempted only when [`failover_eligible`] is true — i.e. the
    ///   attempt streamed *no* output (so no model output / tool side effect can
    ///   be replayed) **and** the error is transient/availability. A mid-stream
    ///   failure, a user/content error, or a cancellation is returned as-is;
    /// - at most ONE alternate is tried (the highest-priority ready provider
    ///   other than the one that just failed), so failover can never fan out or
    ///   loop. If the alternate also fails, its error is returned.
    ///
    /// The returned error is always the plain underlying `anyhow::Error` (the
    /// internal [`TurnFailure`] wrapper is unwrapped here), so `prompt`'s existing
    /// `AgentError` downcast — e.g. the 408 timeout mapping — is unchanged.
    ///
    /// `env` is the same per-turn environment snapshot used for selection-time
    /// failover, so the connect-failure fallback draws from the identical
    /// candidate list.
    async fn run_turn_with_failover(
        &self,
        mut req: PromptRequest,
        control: PromptControl,
        state: RuntimeState,
        env: &ProviderEnv,
        admitted_lease: Option<&SessionOperationLease>,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        // Pin an implicit new session once for the whole primary+fallback
        // attempt. Otherwise each dispatch mints independently, orphaning the
        // primary's durable accepted-user checkpoint on pre-stream failover.
        if req.session_id.is_none() {
            req.session_id = Some(SessionId::new_v4());
            req.create_if_missing = true;
        }

        let session_id = req
            .session_id
            .expect("run_turn_with_failover pins a session id");
        // One serialization guard owns the entire primary → optional fallback
        // transaction. Releasing it between attempts would let another turn
        // append after the primary's accepted-user checkpoint, so the fallback
        // could reuse the wrong row or fail its invariant check.
        let owned_lease;
        let _turn_lease = match admitted_lease {
            Some(lease) if lease.session_id() == session_id => lease,
            Some(_) => anyhow::bail!("session operation lease does not match prompt session"),
            None => {
                owned_lease = SessionOperationLease {
                    id: session_id,
                    _guard: self.session_lock(session_id).lock_owned().await,
                };
                &owned_lease
            }
        };

        let failed_provider = state.provider_config.selection.provider.clone();
        let (turn, effective_state) = match self
            .dispatch_turn(req.clone(), control.clone(), &state, false)
            .await
        {
            Ok(ok) => (ok, state),
            Err(e) => {
                if !failover_eligible(&e) {
                    // Mid-stream, user-error, or non-availability — final. Unwrap
                    // any TurnFailure wrapper so the caller sees the bare error.
                    return Err(unwrap_turn_failure(e));
                }
                // Pre-stream availability failure: try ONE ready alternate.
                let alternate = ocean_providers::resolve_fallback_config(env, &failed_provider)
                    .and_then(|cfg| state_from_provider_config(cfg).ok());
                let Some(alt_state) = alternate else {
                    // No alternate to try — return the original failure as-is.
                    return Err(unwrap_turn_failure(e));
                };
                tracing::warn!(
                    primary_provider = failed_provider.as_str(),
                    fallback_provider =
                        alt_state.provider_config.selection.provider.as_str(),
                    fallback_model = %alt_state.provider_config.selection.model,
                    error = %e,
                    "provider call failed before streaming; failing over to ready alternate"
                );
                // OCEAN-275 honesty: announce the reroute on the event stream —
                // this is the path a 429'd/suspended provider takes, and it used
                // to swap models with zero operator-visible signal. The reason
                // clamps so a provider's JSON error blob can't flood the wire.
                if let Some(sink) = control.event_sink.as_ref() {
                    let mut reason = format!("provider call failed: {e}");
                    if reason.chars().count() > 200 {
                        reason = reason.chars().take(199).chain(['…']).collect();
                    }
                    let _ = sink.send(AgentEvent::ModelRerouted {
                        session_id: req.session_id.map(|s| s.to_string()),
                        requested: state.provider_config.selection.model.clone(),
                        effective: alt_state.provider_config.selection.model.clone(),
                        reason,
                    });
                }
                // Single bounded retry on the alternate. Whatever it returns is
                // final (success or failure) — no further fan-out.
                // The primary already persisted the accepted user row. Reuse
                // it rather than appending the same prompt a second time.
                let ok = self
                    .dispatch_turn(req.clone(), control.clone(), &alt_state, true)
                    .await
                    .map_err(unwrap_turn_failure)?;
                (ok, alt_state)
            }
        };

        // Stop hooks (the ocean-hooks seam): the turn completed and persisted;
        // before releasing it, give configured `[[hooks.Stop]]` subprocesses a
        // chance to block the stop and continue the session with new input —
        // the generic "reply before you sleep" engagement gate external
        // channels (stitchpad pads, review queues, notifiers) plug into
        // without Ocean core knowing them. Still under the session lock, so
        // continuation turns are part of this turn's transaction. No hooks
        // configured → zero cost, identical behavior.
        Ok(self
            .run_stop_hook_continuations(turn, &req, &control, &effective_state)
            .await)
    }

    /// Fire `Stop` hooks for a just-completed turn and run bounded continuation
    /// turns while a hook blocks the stop.
    ///
    /// Contract (mirrors the de-facto local-agent Stop-hook protocol):
    /// - each hook gets `{cwd, session_id, stop_hook_active}` on stdin;
    /// - `{"decision":"block","reason":…}` continues the session with `reason`
    ///   as the next user message; anything else stops normally;
    /// - continuation turns fire the hooks again with `stop_hook_active: true`,
    ///   so a well-behaved hook (e.g. stitchpad's) self-limits to one block;
    /// - a hard iteration bound protects against a hook that always blocks;
    /// - everything is fail-open: hook warnings and continuation failures are
    ///   appended to stderr and never fail the already-completed turn.
    async fn run_stop_hook_continuations(
        &self,
        turn: (SessionId, String, String, TokenUsage),
        req: &PromptRequest,
        control: &PromptControl,
        state: &RuntimeState,
    ) -> (SessionId, String, String, TokenUsage) {
        use ocean_hooks::{HookEvent, HookOutcome};

        /// Hard upper bound on hook-driven continuation turns per operator
        /// prompt, independent of hook behavior — a misconfigured hook that
        /// blocks unconditionally must not spin the session forever.
        const MAX_STOP_HOOK_CONTINUATIONS: usize = 4;

        if self.hooks.count_for(HookEvent::Stop) == 0 {
            return turn;
        }
        let (session_id, mut stdout, mut stderr, mut usage) = turn;
        let context = ocean_hooks::HookContext::new(req.cwd.clone(), session_id.to_string());
        let mut stop_hook_active = false;
        for _ in 0..=MAX_STOP_HOOK_CONTINUATIONS {
            let result =
                ocean_hooks::run_hooks(&self.hooks, HookEvent::Stop, &context, stop_hook_active)
                    .await;
            for warning in result.warnings {
                tracing::warn!(%session_id, %warning, "stop hook warning");
                stderr.push_str(&format!("stop-hook: {warning}\n"));
            }
            let reason = match result.outcome {
                HookOutcome::Continue => break,
                HookOutcome::Block { reason } if reason.trim().is_empty() => break,
                HookOutcome::Block { reason } => reason,
            };
            tracing::info!(
                %session_id,
                reason_len = reason.len(),
                "stop hook blocked turn release; continuing session with hook reason"
            );
            let mut follow = req.clone();
            follow.prompt = reason;
            follow.session_id = Some(session_id);
            follow.create_if_missing = false;
            // Continuations fire the hooks with `stop_hook_active: true`.
            stop_hook_active = true;
            match self
                .dispatch_turn(follow, control.clone(), state, false)
                .await
            {
                Ok((_, cont_stdout, cont_stderr, cont_usage)) => {
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    stdout.push_str(&cont_stdout);
                    stderr.push_str(&cont_stderr);
                    usage.input += cont_usage.input;
                    usage.output += cont_usage.output;
                    usage.cache_read += cont_usage.cache_read;
                    usage.cache_write += cont_usage.cache_write;
                    usage.total_tokens += cont_usage.total_tokens;
                }
                Err(e) => {
                    // Fail-open: the operator's turn already completed and
                    // persisted; a failed continuation is reported, not fatal.
                    tracing::warn!(%session_id, error = %e, "stop-hook continuation turn failed");
                    stderr.push_str(&format!("stop-hook continuation failed: {e}\n"));
                    break;
                }
            }
        }
        (session_id, stdout, stderr, usage)
    }

    /// Dispatch a single turn to the fake echo path or the real agent loop,
    /// exactly as the pre-failover `prompt` did. Factored out so
    /// [`Self::run_turn_with_failover`] can invoke it for both the primary and
    /// the fallback provider without duplicating the fake-vs-real branching.
    /// Dispatch one provider attempt. The caller holds the session's turn lock
    /// across the complete primary/fallback transaction.
    async fn dispatch_turn(
        &self,
        req: PromptRequest,
        control: PromptControl,
        snapshot: &RuntimeState,
        reuse_accepted_user: bool,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        // Fake provider dispatch. The default `fake-ok` is a text-only echo that
        // never touches the runtime loop (`run_fake_prompt`). The OCEAN-130
        // `fake-tool` variant is different: it must trip the *real* permission
        // gate and run a *real* tool, so it routes through `run_prompt` like a
        // real provider — only with a deterministic `FakeToolProvider` injected
        // (no network, no key) that emits one `write` tool call.
        // The OCEAN-150 `fake-surface` variant likewise drives the real loop (it
        // emits a `surface_patch` tool call so the daemon's SurfacePatch SSE
        // bridge can be exercised end to end), so it routes through `run_prompt`.
        let is_fake = snapshot.provider_config.selection.provider == ProviderId::Fake;
        let is_fake_real_loop = is_fake
            && (snapshot.model.id == ocean_runtime::FAKE_TOOL_MODEL
                || snapshot.model.id == ocean_runtime::FAKE_SURFACE_MODEL);
        if is_fake && !is_fake_real_loop {
            self.run_fake_prompt(req, control, snapshot, reuse_accepted_user)
                .await
        } else {
            self.run_prompt(req, control, snapshot, reuse_accepted_user)
                .await
        }
    }

    pub fn list_sessions(
        &self,
        workspace_root: Option<&str>,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        session::list(&self.config_dir, workspace_root)
    }

    /// Search persisted display transcript text without provider or embedding calls.
    pub fn search_history(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<HistorySearchHit>> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "history search query cannot be empty");
        anyhow::ensure!(
            query.chars().count() <= MAX_HISTORY_SEARCH_QUERY_CHARS,
            "history search query exceeds {MAX_HISTORY_SEARCH_QUERY_CHARS} characters",
        );
        session::search_history(
            &self.config_dir,
            query,
            limit
                .unwrap_or(DEFAULT_HISTORY_SEARCH_LIMIT)
                .clamp(1, MAX_HISTORY_SEARCH_LIMIT),
        )
    }

    /// One bounded page of sessions (OCEAN-250), optionally scoped to a workspace.
    ///
    /// Sessions come back newest-first (`updated_ms DESC, id DESC`) exactly as
    /// [`Self::list_sessions`] orders them; this slices that order by the `after`
    /// cursor (a session id) and caps the page via [`clamp_list_limit`]. The
    /// returned [`Page`] carries `next_cursor` (the last returned session's id, to
    /// replay as the next `after`) and `has_more`. Page to the end by repeating
    /// with `after = next_cursor` until `has_more` is false.
    pub fn list_sessions_page(
        &self,
        workspace_root: Option<&str>,
        after: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<Page<SessionSummary>> {
        let all = session::list(&self.config_dir, workspace_root)?;
        Ok(paginate_by_id(all, after, limit, |s| s.id.to_string()))
    }

    /// Resolve the workspace root for an arbitrary cwd. Exposed so callers
    /// (daemon, TUI) can ask "what workspace would my current cwd map to?"
    /// without depending on the private session module.
    pub fn workspace_root_for(&self, cwd: &Path) -> PathBuf {
        session::workspace_root(cwd)
    }

    // ---- Projects ----------------------------------------------------------

    /// Every registered project, newest-first (`updated_ms DESC, id DESC`).
    ///
    /// The on-disk index is an unordered array; this returns it in a stable,
    /// deterministic order so clients (and the cursor in [`Self::list_projects_page`])
    /// see a consistent sequence rather than filesystem/insertion order.
    pub fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        let mut projects = project::load_all(&self.config_dir)?;
        sort_projects_newest_first(&mut projects);
        Ok(projects)
    }

    /// One bounded page of projects (OCEAN-250).
    ///
    /// Projects are ordered newest-first (`updated_ms DESC, id DESC`) — the same
    /// stable order [`Self::list_projects`] returns — then sliced by the `after`
    /// cursor (a project id) and capped via [`clamp_list_limit`]. The returned
    /// [`Page`] carries `next_cursor` (the last returned project's id) and
    /// `has_more`; page to the end by repeating with `after = next_cursor`.
    pub fn list_projects_page(
        &self,
        after: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<Page<Project>> {
        let all = self.list_projects()?;
        Ok(paginate_by_id(all, after, limit, |p| p.id.to_string()))
    }

    /// One project by id.
    pub fn find_project(&self, id: ProjectId) -> anyhow::Result<Option<Project>> {
        project::find_by_id(&self.config_dir, id)
    }

    /// The project that owns a given workspace directory, if any project claims
    /// it. This is the reverse of [`Self::find_project`] → `workspace_root`: it
    /// maps a session's bound `workspace_root` back to its owning project, so a
    /// client viewing a session can see (and link to) the project it belongs to
    /// without scanning the project list itself. `None` ⇒ the directory is not a
    /// project root (the session is project-less, which is valid).
    pub fn project_for_workspace(&self, workspace_root: &str) -> anyhow::Result<Option<Project>> {
        project::find_by_workspace(&self.config_dir, workspace_root)
    }

    /// Resolve the project that owns a session's workspace, following git
    /// worktrees back to their main checkout. First tries an exact match on the
    /// given root; if that misses, resolves the git common-dir's main worktree
    /// (`git -C <root> rev-parse --path-format=absolute --git-common-dir` → the
    /// enclosing repo root) and matches THAT. None on any git/lookup failure.
    pub fn owning_project_for_root(&self, workspace_root: &str) -> Option<Project> {
        if let Ok(Some(project)) = self.project_for_workspace(workspace_root) {
            return Some(project);
        }

        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .arg("rev-parse")
            .arg("--path-format=absolute")
            .arg("--git-common-dir")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }

        let common_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if common_dir.is_empty() {
            return None;
        }

        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            Path::new(workspace_root).join(common_dir)
        };
        let common_dir = common_dir.canonicalize().ok()?;
        let main_root = common_dir.parent()?.canonicalize().ok()?;
        let main_root = main_root.to_string_lossy();
        let main_root = main_root.trim_end_matches(std::path::MAIN_SEPARATOR);
        if main_root.is_empty() {
            return None;
        }

        self.project_for_workspace(main_root).ok().flatten()
    }

    /// A cheap owning-project index: each project's `workspace_root` → the
    /// project, first-match-wins in stored order (mirrors
    /// [`Self::project_for_workspace`]'s exact-root semantics). Built from ONE
    /// `projects.json` read so a caller resolving many sessions' owners does a
    /// single load + O(1) lookups instead of one disk read AND a `git` spawn
    /// per session — the fix for `GET /v1/agent/sessions` taking ~10s to list
    /// a few hundred sessions (the per-row `owning_project_for_root` spawned a
    /// `git rev-parse` for every session that was not an exact project root).
    /// Exact-root grouping is all the session panel needs; worktree→main-repo
    /// resolution stays in the single-session detail path.
    pub fn owning_project_index(&self) -> anyhow::Result<HashMap<String, Project>> {
        let mut index: HashMap<String, Project> = HashMap::new();
        for project in project::load_all(&self.config_dir)? {
            if project.workspace_root.is_empty() {
                continue;
            }
            index
                .entry(project.workspace_root.clone())
                .or_insert(project);
        }
        Ok(index)
    }

    /// Create or replace a project (by id), stamping `updated_ms` to `now_ms`.
    pub fn upsert_project(&self, p: Project, now_ms: i64) -> anyhow::Result<Project> {
        project::upsert(&self.config_dir, p, now_ms)
    }

    /// Remove a project. `false` if it didn't exist. Sessions are untouched.
    pub fn delete_project(&self, id: ProjectId) -> anyhow::Result<bool> {
        project::delete(&self.config_dir, id)
    }

    /// Resolve the working directory for a turn given an optional `project_id`
    /// and the client-supplied `cwd`. This is the fix for the "everything
    /// reverts to the daemon's launch dir" bug: the daemon no longer falls back
    /// to its own process cwd.
    ///
    /// Precedence:
    /// 1. non-empty `requested_cwd` always wins (client may target a sub-dir of
    ///    the project, or work project-less as before);
    /// 2. else, if a `project_id` is given, the project's `workspace_root`;
    /// 3. else — no cwd and no project — an explicit error, never the daemon's
    ///    own `current_dir()`.
    pub fn resolve_cwd_for_turn(
        &self,
        project_id: Option<ProjectId>,
        requested_cwd: &str,
    ) -> anyhow::Result<String> {
        let trimmed = requested_cwd.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        if let Some(id) = project_id {
            let proj = self
                .find_project(id)?
                .ok_or_else(|| anyhow::anyhow!("unknown project id {id}"))?;
            return Ok(proj.workspace_root);
        }
        anyhow::bail!(
            "no working directory: the turn supplied neither a cwd nor a project_id, \
             so there is nothing to bind the session to (the daemon will not guess)"
        )
    }

    /// One-shot legacy migration — safe to call repeatedly.
    pub fn migrate_legacy_sessions(&self) {
        session::migrate_legacy_sessions(&self.config_dir);
    }

    /// Prune on-disk session files older than `OCEAN_SESSION_TTL_DAYS` (default
    /// 90; `0` disables). Run opportunistically at startup, off the hot turn
    /// path, to bound disk growth on long-lived daemons (OCEAN-209). Returns
    /// the number of files pruned.
    pub fn session_file_gc(&self) -> usize {
        // Read OCEAN_SESSION_TTL_DAYS once, overflow-safely, here at the edge —
        // the GC function itself takes the resolved TTL as a parameter (OCEAN-211).
        session::session_file_gc(&self.config_dir, session::ttl_from_env())
    }

    pub fn session_detail(&self, id: SessionId) -> anyhow::Result<SessionDetail> {
        session::detail(&self.config_dir, id)
    }

    /// Read a persisted session while preserving the distinction between a
    /// missing id and an unreadable/corrupt session file. Config-style HTTP
    /// adapters need this three-way result so only a genuinely absent session
    /// becomes 404; storage and decode failures remain internal errors.
    pub fn session_detail_optional(&self, id: SessionId) -> anyhow::Result<Option<SessionDetail>> {
        session::load_resumable(&self.config_dir, id)
            .map(|session| session.map(session::session_detail))
    }

    /// Append a client-authored message to a persisted session outside the
    /// turn loop (the realtime voice agent's handoff notes, voice phases
    /// 2/3). Returns `Ok(false)` when no such session exists — the caller
    /// maps that to 404. Takes the same per-session lock as the run path so
    /// a concurrent turn's load→run→save can never drop the appended row.
    pub async fn append_session_message(
        &self,
        id: SessionId,
        text: String,
    ) -> anyhow::Result<bool> {
        let lease = SessionOperationLease {
            id,
            _guard: self.session_lock(id).lock_owned().await,
        };
        self.append_session_message_with_lease(&lease, text)
    }

    pub fn append_session_message_with_lease(
        &self,
        lease: &SessionOperationLease,
        text: String,
    ) -> anyhow::Result<bool> {
        let id = lease.id;
        let Some(mut session) = session::load_resumable(&self.config_dir, id)? else {
            return Ok(false);
        };
        session.messages.push(Message::user_text(text));
        session.updated_ms = ocean_protocol::now_ms();
        session::save(&self.config_dir, &session)?;
        Ok(true)
    }

    /// Pin a persisted session to a model + provider (session-config RPC v1,
    /// `PATCH /v1/agent/sessions/{id}/config`). Load-mutate-persist under the
    /// same per-session lock as the run path, so a concurrent turn's
    /// load→run→save can never clobber the pin (or vice versa). Returns
    /// `Ok(None)` when no such session exists — the caller maps that to 404.
    /// The caller resolves `provider` from the model catalog; this method
    /// stores the pair as given.
    pub async fn set_session_model(
        &self,
        id: SessionId,
        model: String,
        provider: String,
    ) -> anyhow::Result<Option<SessionDetail>> {
        let lease = SessionOperationLease {
            id,
            _guard: self.session_lock(id).lock_owned().await,
        };
        self.set_session_model_with_lease(&lease, model, provider)
    }

    pub fn set_session_model_with_lease(
        &self,
        lease: &SessionOperationLease,
        model: String,
        provider: String,
    ) -> anyhow::Result<Option<SessionDetail>> {
        let id = lease.id;
        let Some(mut session) = session::load_resumable(&self.config_dir, id)? else {
            return Ok(None);
        };
        session.set_model(model, provider);
        session::save(&self.config_dir, &session)?;
        Ok(Some(session::session_detail(session)))
    }

    /// Read the authoritative public session projection under the same mutation
    /// lane as turns/compact/config/message append. This is refresh-only: it
    /// performs no provider call and never changes persistence.
    pub async fn sync_session(
        &self,
        id: SessionId,
    ) -> anyhow::Result<Option<ocean_core::SessionSyncSnapshot>> {
        let lease = SessionOperationLease {
            id,
            _guard: self.session_lock(id).lock_owned().await,
        };
        self.sync_session_with_lease(&lease)
    }

    /// Check existence/readability without constructing the unbounded ordinary
    /// `SessionDetail` projection. Callers already hold the matching lane.
    pub fn session_exists_with_lease(&self, lease: &SessionOperationLease) -> anyhow::Result<bool> {
        Ok(session::load_resumable(&self.config_dir, lease.id)?.is_some())
    }

    /// Read a synchronized public snapshot while the caller retains the
    /// matching operation lease across daemon-side fence capture.
    pub fn sync_session_with_lease(
        &self,
        lease: &SessionOperationLease,
    ) -> anyhow::Result<Option<ocean_core::SessionSyncSnapshot>> {
        Ok(session::load_resumable(&self.config_dir, lease.id)?
            .as_ref()
            .map(session::session_sync_snapshot))
    }

    /// Compact a session: replace the transcript with a model-generated summary
    /// plus a protected recent window. Uses the currently configured model in a
    /// one-shot no-tools call. The session is saved atomically; if interrupted
    /// mid-compact the prior state survives.
    ///
    /// Failure posture: unknown session, provider-not-ready, provider errors,
    /// timeouts, and empty summaries all return `ok:false` WITHOUT touching the
    /// stored transcript. Corrupt session storage is an `Err` (never wiped).
    /// When the whole transcript already fits inside the protected window the
    /// call is an `ok:true` no-op and no model call is made.
    pub async fn compact_session(
        &self,
        id: SessionId,
    ) -> anyhow::Result<ocean_core::CompactResponse> {
        let guard = self.session_lock(id).lock_owned().await;
        let lease = SessionOperationLease { id, _guard: guard };
        self.compact_session_with_lease(&lease).await
    }

    /// Non-blocking compact admission. `Err(SessionOperationBusy)` means some
    /// turn/config/message/compact operation already owns this session lane.
    pub async fn try_compact_session(
        &self,
        id: SessionId,
    ) -> Result<anyhow::Result<ocean_core::CompactResponse>, SessionOperationBusy> {
        let lease = self.try_session_operation(id)?;
        Ok(self.compact_session_with_lease(&lease).await)
    }

    pub async fn compact_session_with_lease(
        &self,
        lease: &SessionOperationLease,
    ) -> anyhow::Result<ocean_core::CompactResponse> {
        use futures::StreamExt as _;
        let id = lease.id;
        use ocean_protocol::{stream_simple, AssistantMessageEvent, Context, StreamOptions};
        let start = std::time::Instant::now();
        let fail = |stderr: String, start: &std::time::Instant| ocean_core::CompactResponse {
            ok: false,
            session_id: id,
            wall_ms: start.elapsed().as_millis() as u64,
            elided_messages: 0,
            stderr,
            sync: None,
            fence: None,
        };

        let Some(session) = session::load_resumable(&self.config_dir, id)? else {
            return Ok(fail("session not found".into(), &start));
        };

        let snapshot = self.snapshot();
        let model = snapshot.model.clone();

        // Protected window FIRST: if nothing would be elided (short session),
        // compacting would only grow the transcript — return a no-op without
        // spending a model call.
        let split = compact_protected_split(&session.messages, model.context_window);
        if split == 0 {
            let sync = session::session_sync_snapshot(&session);
            return Ok(ocean_core::CompactResponse {
                ok: true,
                session_id: id,
                wall_ms: start.elapsed().as_millis() as u64,
                elided_messages: 0,
                stderr: "nothing to compact: the transcript fits in the protected window".into(),
                sync: Some(sync),
                fence: None,
            });
        }

        // Readiness preflight — fail closed with a clear reason instead of a
        // raw transport error when the provider has no usable credential.
        if let Some(reason) = Self::preflight_error_for(&snapshot) {
            return Ok(fail(reason, &start));
        }

        // Shape the transcript for the one-shot summarize call exactly like a
        // real turn would: strip stored assistant thinking on routes that must
        // not see it replayed (provider encoders drop cross-provider thinking,
        // this keeps the request minimal), then end on an explicit user
        // instruction so providers that require a trailing user message accept
        // the request.
        let mut summarize_messages = session.messages.clone();
        let selection = &snapshot.provider_config.selection;
        if should_strip_assistant_thinking(&selection.provider, &selection.model) {
            strip_assistant_thinking_content(&mut summarize_messages);
        }
        summarize_messages.push(Message::user_text(
            "Summarize the conversation above now, following the system instructions.",
        ));

        let ctx = Context {
            system_prompt: Some(
                "You are a session summarizer. Summarize this conversation concisely, \
                 preserving key facts, decisions, open tasks, and the user's goals. \
                 Output a plain text summary without markdown formatting."
                    .into(),
            ),
            messages: summarize_messages,
            tools: Vec::new(),
            dynamic_tool_declarations: Vec::new(),
            tool_choice: Default::default(),
        };

        // Full credential wiring, mirroring `complete_once`: api key, provider
        // base URL, auth method, and the codex account header when present.
        // `session_id` keeps provider prompt-cache routing stable for the
        // session being compacted.
        let mut options = StreamOptions {
            max_tokens: Some(model.max_tokens / 4),
            api_key: snapshot.api_key.clone(),
            base_url: Some(snapshot.provider_config.selection.base_url.clone()),
            auth: auth_method_for(&snapshot.provider_config),
            session_id: Some(id.to_string()),
            ..Default::default()
        };
        if let Some(account_id) = &snapshot.provider_config.account_id {
            options
                .headers
                .insert("chatgpt-account-id".into(), account_id.clone());
        }

        // One-shot, no-tools, bounded by the same wall-clock budget as a
        // provider round in a real turn. Provider trouble of any kind returns
        // `ok:false` and leaves the stored transcript untouched.
        let summarize = async {
            #[cfg(test)]
            let stream_res = match self.test_compact_provider.as_ref() {
                Some(provider) => provider.0.stream(&model, &ctx, &options).await,
                None => stream_simple(&model, &ctx, &options).await,
            };
            #[cfg(not(test))]
            let stream_res = stream_simple(&model, &ctx, &options).await;

            let mut stream = match stream_res {
                Ok(stream) => stream,
                Err(e) => return Err(format!("compact: provider dispatch failed: {e}")),
            };
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(AssistantMessageEvent::TextDelta { delta, .. }) => text.push_str(&delta),
                    Ok(AssistantMessageEvent::Done { message, .. }) => {
                        // Some providers emit only a terminal message and no
                        // deltas — fall back to the finalized text.
                        if text.is_empty() {
                            for c in &message.content {
                                if let Content::Text { text: t } = c {
                                    text.push_str(t);
                                }
                            }
                        }
                        break;
                    }
                    Ok(AssistantMessageEvent::Error { error, .. }) => {
                        return Err(error
                            .error_message
                            .unwrap_or_else(|| "compact: provider error".into()));
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("compact: provider stream failed: {e}")),
                }
            }
            Ok(text)
        };
        let summary = match tokio::time::timeout(
            std::time::Duration::from_secs(COMPACT_TIMEOUT_SECS),
            summarize,
        )
        .await
        {
            Ok(Ok(text)) => text.trim().to_string(),
            Ok(Err(stderr)) => return Ok(fail(stderr, &start)),
            Err(_elapsed) => {
                return Ok(fail(
                    format!("compact: model call timed out after {COMPACT_TIMEOUT_SECS}s"),
                    &start,
                ))
            }
        };

        if summary.is_empty() {
            return Ok(fail(
                "compact: model returned an empty summary".into(),
                &start,
            ));
        }

        let protected = &session.messages[split..];
        let elided_messages = split as u64;

        // Build the replacement transcript: summary marker + summary + protected window.
        let mut new_messages = vec![
            Message::user_text(
                "The session was compacted. Below is a summary of the \
                 conversation that was replaced to save context.",
            ),
            Message::Assistant(ocean_protocol::AssistantMessage {
                content: vec![Content::text(summary)],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: ocean_protocol::Usage::default(),
                stop_reason: ocean_protocol::StopReason::Stop,
                error_message: None,
                timestamp: ocean_protocol::now_ms(),
            }),
        ];
        new_messages.extend_from_slice(protected);

        // Atomically replace the session transcript (temp + fsync + rename in
        // `session::save`); a crash mid-save leaves the prior file intact.
        let mut updated = session;
        updated.messages = new_messages;
        updated.updated_ms = ocean_protocol::now_ms();
        session::save(&self.config_dir, &updated)?;
        let sync = session::session_sync_snapshot(&updated);

        Ok(ocean_core::CompactResponse {
            ok: true,
            session_id: id,
            wall_ms: start.elapsed().as_millis() as u64,
            elided_messages,
            stderr: String::new(),
            sync: Some(sync),
            fence: None,
        })
    }

    /// Explicitly mint a session container *before* any turn is run, per the
    /// ecosystem contract. Mirrors the implicit create-on-turn path's session
    /// setup (mint id → `bind_workspace(cwd)` → persist) but runs no agent loop
    /// and stores no messages — the session starts empty.
    ///
    /// Like the implicit path, this always mints a *fresh* session id; it does
    /// not adopt an existing session that happens to share a workspace. The
    /// surface owns the returned `session_id` and threads it onto every turn.
    ///
    /// `client_type`, when supplied, is recorded on the session so the first
    /// turn's surface profile is correct even if that turn omits it.
    pub fn create_session(
        &self,
        cwd: &str,
        client_type: Option<String>,
    ) -> anyhow::Result<(SessionId, String, Option<String>)> {
        anyhow::ensure!(
            !cwd.trim().is_empty(),
            "cannot create a session: no working directory to bind it to"
        );
        let snapshot = self.snapshot();
        // The id is freshly minted and not yet known to any client, so no turn
        // can be racing it — no per-session lock is needed here (unlike the
        // run path, which serializes load→run→save on an id a client may
        // already be steering).
        let session_id = SessionId::new_v4();
        let mut session = session::Session::new_with_id(session_id, &snapshot.model);
        session.bind_workspace(Path::new(cwd));
        if client_type.is_some() {
            session.client_type = client_type.clone();
        }
        session::save(&self.config_dir, &session)?;
        Ok((session.id, cwd.to_string(), session.client_type))
    }

    /// Readiness preflight for a *specific* resolved state, so a per-turn model
    /// override (OCEAN-36) is checked against the model that will actually run —
    /// not the global one. Returns `None` when ready, else a human-readable
    /// reason.
    fn preflight_error_for(snapshot: &RuntimeState) -> Option<String> {
        let readiness = snapshot.provider_config.readiness();
        if readiness.ok {
            return None;
        }

        let detail = readiness
            .error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "provider is not ready".to_string());
        Some(format!(
            "provider readiness failed for provider {} model {}: {detail}",
            readiness.provider.as_str(),
            readiness.model
        ))
    }

    async fn run_fake_prompt(
        &self,
        req: PromptRequest,
        control: PromptControl,
        snapshot: &RuntimeState,
        reuse_accepted_user: bool,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        let session_id = req.session_id.unwrap_or_else(SessionId::new_v4);

        let supplied = req.session_id.is_some();
        let mut session = match session::load_resumable(&self.config_dir, session_id)? {
            Some(existing) => existing,
            None if !supplied || req.create_if_missing => {
                session::Session::new_with_id(session_id, &snapshot.model)
            }
            None => anyhow::bail!("session not found: {session_id}"),
        };
        session.bind_workspace(Path::new(&req.cwd));

        let stdout = "OCEAN_FAKE_OK\n".to_string();

        // OCEAN-127: emit the assistant text as a streaming delta, exactly like
        // the real provider path does. `run_prompt` streams the model's reply
        // through `AgentEvent::TextDelta`, which the daemon bridge converts into
        // `AgentTurnEvent::AssistantTextDelta` on the scoped `?session_id=`
        // stream. The fake path used to synthesize the assistant message in
        // place and emit nothing, so a scoped subscriber saw only
        // `turn_started` + `turn_finished` and the reply never rendered. Emit a
        // single text delta here so the fake provider behaves like a real one
        // for surfaces (TUI, web) that render the transcript off the scoped
        // stream.
        if let Some(sink) = control.event_sink.as_ref() {
            // The session id is stamped on every runtime event (OCEAN-54); the
            // daemon bridge re-uses it to scope the AssistantTextDelta.
            let _ = sink.send(AgentEvent::TextDelta {
                session_id: Some(session_id.to_string()),
                delta: stdout.trim_end().to_string(),
            });
        }

        let mut messages = session.messages.clone();
        if reuse_accepted_user {
            anyhow::ensure!(
                matches!(messages.last(), Some(Message::User { .. })),
                "fallback session is missing its accepted user turn"
            );
        } else {
            messages.push(Message::user_text(req.prompt));
        }
        messages.push(Message::Assistant(AssistantMessage {
            content: vec![Content::text(stdout.trim_end())],
            api: snapshot.model.api.clone(),
            provider: snapshot.model.provider.clone(),
            model: snapshot.model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: ocean_protocol::now_ms(),
        }));
        // First user turn names the session (see `run_prompt`): adopt the
        // daemon-supplied pre-composition prompt as the label so the fake path
        // labels sessions identically to the real one.
        if !reuse_accepted_user {
            if let Some(title) = control.display_title.as_deref() {
                session.ensure_title(title);
            }
        }
        session.replace_messages(messages);
        session::save(&self.config_dir, &session)?;

        // The fake provider does no real inference; report zero usage.
        Ok((session.id, stdout, String::new(), TokenUsage::default()))
    }

    // OCEAN-274: the `runtime.prompt` span — the session/history layer's slice of
    // the turn. Sits under the daemon's `turn` root span (the daemon `.instrument`s
    // the `prompt()` call) and is the parent the spawned `agent_loop` future is
    // re-attached to below, so load → run → persist all read as one turn in the
    // logs. `skip_all` because the args carry the prompt text, tools, permission
    // policy and snapshot — none of which belong in a span field; `session_id` is
    // recorded explicitly once resolved.
    #[tracing::instrument(name = "runtime.prompt", skip_all, fields(session_id = tracing::field::Empty))]
    async fn run_prompt(
        &self,
        req: PromptRequest,
        control: PromptControl,
        snapshot: &RuntimeState,
        reuse_accepted_user: bool,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        // Session label for a first-turn (the daemon-supplied pre-composition
        // prompt). Read before `control` is destructured for the toolset below so
        // the switcher label can be adopted at the first durable save.
        let display_title = control.display_title.clone();

        // `snapshot` is already the EFFECTIVE turn state: `prompt()` resolved the
        // per-turn `model_id` override (OCEAN-36) before the readiness/dispatch
        // check and handed it down here, so every downstream read (model,
        // provider routing, api_key, base_url, the thinking-strip check) sees the
        // override consistently. The runtime's global selection is never touched.

        // Resolve the effective session id up front so we can serialize on it.
        // None means "new session" — mint the id now so the lock still covers
        // the load/run/save window.
        let session_id = req.session_id.unwrap_or_else(SessionId::new_v4);
        // Now that the id is resolved (supplied or freshly minted), stamp it onto
        // the `runtime.prompt` span so every log line in this turn slice — and the
        // child `agent_loop`/`persist` spans — is attributable to one session
        // even under concurrent turns (OCEAN-274).
        tracing::Span::current().record("session_id", tracing::field::display(session_id));

        // The outer primary/fallback transaction holds the per-session lock
        // across load → run → save. Without it, two turns on the same session
        // could load the same history and the last save would silently win.

        // Strict resume-vs-create. A supplied-but-unknown session id is an error
        // by default (so a stale client id surfaces instead of silently forking
        // a fresh transcript). Creating with a specific id requires opt-in.
        let supplied = req.session_id.is_some();
        let mut session = match session::load_resumable(&self.config_dir, session_id)? {
            Some(existing) => existing,
            None if !supplied || req.create_if_missing => {
                session::Session::new_with_id(session_id, &snapshot.model)
            }
            None => anyhow::bail!(
                "session not found: {session_id} (resume requires an existing session; \
                 pass create_if_missing to start a new one with this id)"
            ),
        };
        session.bind_workspace(Path::new(&req.cwd));

        // Surface identity (Fixes 1–3). The session remembers the surface it
        // was last steered from. Detect a switch (for example, from the desktop
        // Surface to the Chrome extension) so the agent is told, then record
        // the new surface on the session for next turn / resume.
        let prev_surface = session.client_type.clone();
        let surface_switched = match (prev_surface.as_deref(), req.client_type.as_deref()) {
            (Some(old), Some(new)) => old != new,
            _ => false,
        };
        if req.client_type.is_some() {
            session.client_type = req.client_type.clone();
        }

        let mut history = session.messages.clone();
        // Token-budgeted compaction (the "402M-token finding"): the loop trims
        // each REQUEST to the model window, but nothing bounded the cumulative
        // cost below it — a 150K-token transcript that fits was re-sent on
        // every round of every turn, forever. When stored history crosses the
        // trigger, elide OLD tool-result bodies (the token-dominant content)
        // outside a protected recent window. The elision lands in `history`,
        // flows through the run, and is PERSISTED at turn end — so the prompt
        // prefix stays byte-stable between compactions and provider prompt
        // caching keeps working (a per-turn rolling rewrite would invalidate
        // the cache every turn and could cost MORE than it saves).
        let elided = compact_history(&mut history, snapshot.model.context_window);
        if elided > 0 {
            tracing::info!(
                session_id = %session_id,
                elided,
                "compacted session history: elided old tool results past the token trigger"
            );
        }
        // Most OpenAI-compatible providers do not accept Ocean's assistant
        // `thinking` blocks on replay, so strip them before the runtime turn.
        // Anthropic replays signed thinking; exact Kimi K3 separately requires
        // its prior `reasoning_content`, which the OpenAI adapter reconstructs.
        let selection = &snapshot.provider_config.selection;
        if should_strip_assistant_thinking(&selection.provider, &selection.model) {
            // Kimi K3 is the sole Chat Completions exception: Moonshot requires
            // same-model reasoning_content replay across tool rounds. K2.x and
            // every other backend above retain the cross-provider privacy drop.
            strip_assistant_thinking_content(&mut history);
        }
        // Per-turn surface flag (Fix 2): every user turn is prefixed with a
        // canonical `[FLAG]` so the agent always knows which surface this turn
        // arrived on — robust to session reuse across surfaces (the system
        // prompt alone isn't, since a resumed session can switch surfaces).
        // On a detected surface switch (Fix 3), lead with a one-line notice.
        let user_text = {
            let flag = system_prompt::surface_flag(req.client_type.as_deref());
            let mut out = String::new();
            if surface_switched {
                let from = system_prompt::surface_flag(prev_surface.as_deref());
                out.push_str(&compose_surface_switch_notice(flag, from));
            }
            out.push_str(&format!("[{flag}] "));
            out.push_str(&req.prompt);
            out
        };
        // First user message of the turn: prompt text plus any attached images
        // as `Content::Image` blocks (OCEAN-115). No images → plain-text message,
        // identical to the prior `Message::user_text` path.
        if reuse_accepted_user {
            anyhow::ensure!(
                matches!(history.last(), Some(Message::User { .. })),
                "fallback session is missing its accepted user turn"
            );
        } else {
            history.push(build_user_message(user_text, req.images.as_deref()));
        }

        // First user turn of a fresh session names it: adopt the daemon-supplied
        // display title (the pre-composition prompt) as the session label, before
        // the first durable save so the switcher shows it immediately. Only on a
        // genuine new user turn (`reuse_accepted_user` replays an already-accepted
        // one) and only when unset, so resumes never relabel. `ensure_title` is a
        // no-op when the daemon supplied no title (direct callers), leaving the
        // read-side derivation in charge.
        if !reuse_accepted_user {
            if let Some(title) = display_title.as_deref() {
                session.ensure_title(title);
            }
        }

        // Acceptance is itself a durable boundary. Save the user turn before
        // any provider call or side-effecting tool can run, so interruption in
        // the first long browser round cannot erase what was submitted.
        {
            let accepted = cap_session_history(history.clone());
            let accept_span =
                tracing::info_span!("checkpoint", kind = "accepted", messages = accepted.len());
            let _accept = accept_span.enter();
            session.replace_messages(accepted);
            session::save(&self.config_dir, &session)?;
        }

        let PromptControl {
            permission,
            cancel,
            event_sink,
            thinking_level,
            // `model_id` and `agent_model` were already consumed above (turn_state
            // resolution); discarded here so the destructure stays exhaustive.
            model_id: _,
            agent_model: _,
            tool_allowlist,
            agent_capabilities,
            tools_disabled,
            hashline_edits,
            artifact_spill,
            // Already consumed above (session label at the first durable save);
            // named here so the destructure stays exhaustive.
            display_title: _,
        } = control;
        // Resolve the toolset for this turn through the capability registry —
        // built-ins plus any connected MCP/skill providers, deduped first-wins.
        // This is the seam that replaced the old hardcoded `default_tools()`.
        let tool_ctx = SessionContext {
            cwd: PathBuf::from(&req.cwd),
            session_id: Some(session_id.to_string()),
            hashline: hashline_edits,
            artifacts: artifact_spill,
        };
        let tools = self.capabilities.tools_for_session(&tool_ctx).await;
        // `tools_disabled` is a fail-closed authorization boundary. Unlike an
        // empty/unmatched allowlist, it clears every registered capability and
        // prevents folder-agent subprocess capabilities from being added later.
        let mut tools = if tools_disabled {
            Vec::new()
        } else {
            // Folder-as-agent tool narrowing is intentionally fail-open for bad
            // configuration and is not a security control.
            narrow_tools(tools, tool_allowlist.as_deref())
        };
        // A2 — folder-as-agent capability binding. When the turn's agent declares
        // tier-1 subprocess capabilities, launch each as an `ocean-plugin`
        // subprocess and APPEND its tools (namespaced `plugin__<name>__*`, always
        // permission-gated) to this turn's toolset. Appended AFTER narrowing so an
        // agent's own declared capability tools are never filtered out by its
        // built-in `tools` allowlist — the allowlist restricts built-ins, not the
        // capabilities the agent explicitly asked for. Fail-soft: a spec that
        // can't spawn is skipped inside the builder, so a bad capability never
        // breaks the turn. No caps → the toolset is unchanged (behavior-neutral
        // for every other turn). First-wins dedup keeps built-ins unshadowable.
        if !tools_disabled {
            if let Some((agent_root, caps)) = agent_capabilities {
                let extra = build_agent_capability_providers(&caps, &agent_root).await;
                if !extra.is_empty() {
                    let mut seen: std::collections::HashSet<String> =
                        tools.iter().map(|t| t.name().to_string()).collect();
                    for provider in &extra {
                        for tool in provider.tools(&tool_ctx).await {
                            let name = tool.name().to_string();
                            if seen.insert(name.clone()) {
                                tools.push(tool);
                            } else {
                                tracing::warn!(
                                    tool = %name,
                                    provider = %provider.id(),
                                    "agent capability tool name collides with an existing tool; keeping the existing one"
                                );
                            }
                        }
                    }
                }
            }
        }

        let mut cfg = AgentConfig::new(
            snapshot.model.clone(),
            system_prompt::build_system_prompt(Some(&req.cwd), req.client_type.as_deref()),
        )
        .with_tools(tools)
        .with_max_turns(req.max_turns.unwrap_or(32))
        .with_turn_timeout_secs(turn_timeout_secs_from_env())
        .with_permission(permission)
        // Stamp the session onto every runtime AgentEvent so the daemon SSE
        // bridge can route by session natively (OCEAN-54).
        .with_session_id(session_id.to_string());
        // Per-turn reasoning override (OCEAN-41): when the turn carries an
        // explicit `thinking_level`, apply it to *this* turn's config only. The
        // runtime's global `thinking_level` is untouched, so the next turn falls
        // back to the default unless it too overrides.
        if let Some(level) = thinking_level {
            cfg = cfg.with_thinking(level);
        }
        cfg.stream_options.api_key = snapshot.api_key.clone();
        cfg.stream_options.base_url = Some(snapshot.provider_config.selection.base_url.clone());
        cfg.stream_options.auth = auth_method_for(&snapshot.provider_config);
        cfg.stream_options.cancel = cancel;
        if let Some(account_id) = &snapshot.provider_config.account_id {
            cfg.stream_options
                .headers
                .insert("chatgpt-account-id".into(), account_id.clone());
        }

        // OCEAN-130: the keyless `fake-tool` model runs the *real* loop but with
        // a deterministic provider injected through the same seam the e2e tests
        // use. It emits one `write` tool call on the first round — tripping the
        // permission gate when gating is on — then a `done` completion. No
        // network, no key; production models never set `provider`.
        if snapshot.provider_config.selection.provider == ProviderId::Fake
            && snapshot.model.id == ocean_runtime::FAKE_TOOL_MODEL
        {
            cfg = cfg.with_provider(std::sync::Arc::new(ocean_runtime::FakeToolProvider::new()));
        }
        // OCEAN-150: the keyless `fake-surface` model scripts one `surface_patch`
        // tool call through the same seam, so the daemon SurfacePatch SSE bridge
        // can be live-tested over HTTP with no key.
        if snapshot.provider_config.selection.provider == ProviderId::Fake
            && snapshot.model.id == ocean_runtime::FAKE_SURFACE_MODEL
        {
            cfg = cfg.with_provider(std::sync::Arc::new(
                ocean_runtime::FakeToolProvider::surface(),
            ));
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Parent-side durable transcript. The runtime sends only completed-round
        // deltas; keeping the valid prefix here avoids cloning the full history
        // on every browser/tool round.
        let mut checkpoint_messages = history.clone();
        let cfg_cloned = cfg.clone();
        // The agent loop runs on its own task, so the turn's span context does NOT
        // propagate automatically — a freshly spawned task starts with no parent
        // span. Re-attach the current `runtime.prompt` span (OCEAN-274) so the
        // `agent_loop` span (and its `round`/`provider_stream`/`tool_exec`
        // children) nest under this turn instead of detaching into a rootless tree.
        // Abort the loop if this parent future is dropped (for example when a
        // synchronous HTTP client disconnects). A bare JoinHandle detaches on
        // drop, allowing ghost tools to keep running after the session lock and
        // persistence owner disappear.
        let handle = AbortOnDropJoinHandle::new(tokio::spawn(
            async move { run_agent_with_history(&cfg_cloned, history, Some(tx)).await }
                .instrument(tracing::Span::current()),
        ));

        let mut stdout = String::new();
        let mut stderr = String::new();
        // Failover safety boundary (OCEAN-275). The agent loop emits control
        // events (`AgentStart`/`TurnStart`) *before* it connects to the provider;
        // those are not observable side effects. The moment ANY content or tool
        // event flows — assistant text/thinking, a tool execution, a permission
        // outcome — the turn has begun producing real, possibly side-effecting
        // output. We flip `streamed_output` on exactly those events so the caller
        // (`prompt()`) can tell a pre-stream connect-failure (safe to fail over)
        // from a mid-stream failure (UNSAFE — failing over would re-run the model
        // and replay tool side effects). On a connect-failure the loop sees only
        // control events, so this stays `false` and failover is allowed.
        let mut streamed_output = false;
        while let Some(ev) = rx.recv().await {
            if let Some(sink) = event_sink.as_ref() {
                let _ = sink.send(ev.clone());
            }
            match ev {
                AgentEvent::TextDelta { delta, .. } => {
                    streamed_output = true;
                    stdout.push_str(&delta)
                }
                AgentEvent::ThinkingDelta { delta, .. } => {
                    streamed_output = true;
                    stderr.push_str("thinking: ");
                    stderr.push_str(&delta);
                    stderr.push('\n');
                }
                AgentEvent::AssistantMessage { .. } if !stdout.ends_with('\n') => {
                    streamed_output = true;
                    stdout.push('\n');
                }
                AgentEvent::AssistantMessage { .. } => {
                    streamed_output = true;
                }
                AgentEvent::ToolExecutionStart {
                    tool_name, args, ..
                } => {
                    streamed_output = true;
                    stderr.push_str(&format!("→ {tool_name}({args})\n"));
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    ..
                } => {
                    streamed_output = true;
                    stderr.push_str(&format!(
                        "← {tool_name} {}\n",
                        if is_error { "error" } else { "ok" }
                    ));
                }
                AgentEvent::PermissionDenied {
                    tool_name, reason, ..
                } => {
                    streamed_output = true;
                    stderr.push_str(&format!("✗ permission denied for {tool_name}: {reason}\n"));
                }
                AgentEvent::TurnCheckpoint { messages, .. } => {
                    // A checkpoint is emitted only after the runtime has paired
                    // assistant tool calls with all results in provider-valid
                    // order. Persist that valid prefix immediately, matching
                    // stock Pi's message-end durability without ever saving an
                    // orphan tool call.
                    checkpoint_messages.extend(messages);
                    let persisted = cap_session_history(checkpoint_messages.clone());
                    let checkpoint_span =
                        tracing::info_span!("checkpoint", messages = persisted.len());
                    let _checkpoint = checkpoint_span.enter();
                    session.replace_messages(persisted);
                    session::save(&self.config_dir, &session)?;
                }
                _ => {}
            }
        }

        let run = match handle.join().await.context("agent task join failed")? {
            Ok(run) => run,
            // The agent loop failed. Carry the `streamed_output` flag out so the
            // caller can decide whether failover is safe (only when nothing
            // streamed). We do NOT save the session here — a failed turn commits
            // no transcript, so a fallback retry starts from the same clean
            // history with no partial/duplicated state.
            Err(e) => {
                return Err(TurnFailure {
                    streamed_output,
                    // Box as anyhow so the failover classifier can downcast back
                    // to the concrete `AgentError`/protocol cause.
                    error: anyhow::Error::new(e),
                }
                .into());
            }
        };
        // Cap what we persist. The agent loop already trims per-send to the
        // context window, but the *stored* transcript would otherwise grow
        // forever and be reloaded in full on every future turn (the dominant
        // source of runaway input-token cost). Keep the most recent messages.
        //
        // Persist under a `persist` span (OCEAN-274) — the final leaf of the turn
        // tree (turn → runtime.prompt → agent_loop → … → persist). `save` is
        // synchronous, so a plain span guard (no await held across it) is correct
        // here. Only the retained message count is recorded; the transcript bytes
        // never enter a span field.
        let persisted = cap_session_history(run.messages.clone());
        let persist_span = tracing::info_span!("persist", messages = persisted.len());
        {
            let _persist = persist_span.enter();
            session.replace_messages(persisted);
            session::save(&self.config_dir, &session)?;
        }

        if stdout.trim().is_empty() {
            stdout = last_assistant_text(&run.messages).unwrap_or_default();
            if !stdout.ends_with('\n') && !stdout.is_empty() {
                stdout.push('\n');
            }
        }
        if run.stopped_at_turn_limit {
            stderr.push_str("stopped at max turns\n");
        }

        let usage = TokenUsage {
            input: run.usage.input,
            output: run.usage.output,
            cache_read: run.usage.cache_read,
            cache_write: run.usage.cache_write,
            total_tokens: run.usage.total_tokens,
            context_tokens: run.context_tokens,
            context_window: u64::from(snapshot.model.context_window),
        };

        Ok((session.id, stdout, stderr, usage))
    }
}

/// A failed turn, carrying whether the provider had begun streaming output when
/// it failed (OCEAN-275).
///
/// `run_prompt` wraps its underlying `anyhow::Error` in this so the failover
/// decision in [`AgentRuntime::prompt`] can recover the `streamed_output` flag by
/// downcast. The flag is the mid-stream-safety gate: failover is only ever
/// attempted when `streamed_output == false` (a pre-stream connect-failure), so a
/// turn that already emitted assistant text or ran a tool is NEVER replayed
/// against a second provider. `Display`/`source` delegate to the inner error, so
/// the user-facing message and `downcast_ref::<AgentError>()` (the existing 408
/// timeout mapping) keep working unchanged.
/// Tokio detaches a spawned task when its `JoinHandle` is dropped. Agent turns
/// cannot use that default: the parent owns the session lock and persistence,
/// so a detached child could keep executing side-effecting tools with no owner
/// left to save or finalize it. This wrapper aborts unless explicitly joined.
struct AbortOnDropJoinHandle<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortOnDropJoinHandle<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        self.handle
            .take()
            .expect("abort-on-drop handle joined at most once")
            .await
    }
}

impl<T> Drop for AbortOnDropJoinHandle<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[derive(Debug)]
struct TurnFailure {
    streamed_output: bool,
    error: anyhow::Error,
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for TurnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// Classify whether a failed turn is eligible for provider failover (OCEAN-275).
///
/// Two conditions, both required:
/// 1. **Nothing streamed.** A turn that already emitted content or a tool event
///    must not be retried on another provider (it would replay side effects).
///    A bare `anyhow::Error` that isn't a [`TurnFailure`] is treated as
///    "streamed/unknown" → not eligible, the conservative default.
/// 2. **Availability error.** The underlying failure must be transient/
///    availability (connect/timeout, 429, 5xx, missing credential, or an
///    exhausted-retry wrapping one of those) — not a user/content error that
///    would fail identically on any provider.
///
/// Returns `None` when not eligible, otherwise `Some(())` (the caller already
/// holds the context it needs to pick the alternate).
fn failover_eligible(err: &anyhow::Error) -> bool {
    // Must be a TurnFailure that did not stream — otherwise mid-stream/unknown.
    let Some(turn) = err.downcast_ref::<TurnFailure>() else {
        return false;
    };
    if turn.streamed_output {
        return false;
    }
    // The wrapped error must be an availability/transient one. The chain is
    // anyhow(TurnFailure) → anyhow(inner) → AgentError → ocean_protocol::Error.
    // Recover the protocol error: AgentError::Provider holds it; otherwise the
    // failure isn't a provider-availability problem (e.g. a join error).
    match turn.error.downcast_ref::<AgentError>() {
        Some(AgentError::Provider(perr)) => perr.is_retryable_availability(),
        // AgentError::Timeout is a per-turn deadline; a provider that hangs past
        // the deadline is an availability problem worth trying an alternate for.
        Some(AgentError::Timeout { .. }) => true,
        _ => false,
    }
}

/// Strip the internal [`TurnFailure`] wrapper, returning the underlying
/// `anyhow::Error` (the one carrying the `AgentError`/protocol cause).
///
/// `prompt`'s error handling — the 408 timeout `downcast_ref::<AgentError>()` and
/// the `Display` shown to the caller — expects the original error, not the
/// failover-bookkeeping wrapper. A non-`TurnFailure` error passes through.
fn unwrap_turn_failure(err: anyhow::Error) -> anyhow::Error {
    match err.downcast::<TurnFailure>() {
        Ok(turn) => turn.error,
        Err(other) => other,
    }
}

/// Per-turn timeout (seconds) from the environment.
///
/// Reads `OCEAN_TURN_TIMEOUT_SECS`. Returns `Some(n)` for a valid positive
/// integer, and `None` when the variable is unset, empty, unparseable, or `0`
/// — in which case [`AgentConfig`] falls back to
/// [`AgentConfig::DEFAULT_TURN_TIMEOUT_SECS`] (300s). A `0` is treated as
/// "unset" rather than "deadline of zero" so a stray value can never make every
/// turn time out instantly.
fn turn_timeout_secs_from_env() -> Option<u32> {
    std::env::var("OCEAN_TURN_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|secs| *secs > 0)
}

#[derive(Clone)]
pub struct PromptControl {
    pub permission: Arc<dyn PermissionPolicy>,
    pub cancel: Option<CancellationToken>,
    /// Optional sink that receives raw `AgentEvent`s as the run progresses.
    /// The daemon uses this to push real-time deltas onto its broadcast bus
    /// so SSE consumers (TUI/CLI) can render text as it streams.
    pub event_sink: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Per-turn reasoning effort override. When `Some`, it is applied to *this
    /// turn's* `AgentConfig` only — the runtime's global `thinking_level` is
    /// never mutated. `None` leaves the global default in force.
    pub thinking_level: Option<ocean_protocol::ThinkingLevel>,
    /// Per-turn model override (OCEAN-36). When `Some`, *this turn only* is
    /// driven by the given model alias — the runtime's global model selection
    /// is never mutated. This lets independent client windows (e.g. two
    /// Zed/ACP sessions) each pin their own model without racing each other
    /// through the global `set_model` swap. `None` uses the global model.
    pub model_id: Option<String>,
    /// Per-turn tool allowlist (folder-as-agent). When `Some` and non-empty, the
    /// turn's toolset is narrowed to tools whose `name()` is in the list — a
    /// named agent's declared `agent.toml` `tools`/`capabilities`. `None` (every
    /// non-folder turn) leaves the full registry toolset in force. Fail-safe: if
    /// the allowlist matches NO available tool (e.g. a typo), narrowing is
    /// skipped and a warning logged rather than running the agent toolless.
    pub tool_allowlist: Option<Vec<String>>,
    /// Per-turn model declared by a folder-as-agent (`agent.toml` `model`). Used
    /// ONLY when `model_id` (an explicit per-request override) is unset. Unlike
    /// `model_id` it fails SOFT — an unresolvable agent model falls back to the
    /// global model rather than failing the turn. `None` = no agent model.
    pub agent_model: Option<String>,
    /// A folder-as-agent's declared tier-1 subprocess capabilities plus the agent
    /// folder they resolve relative to (A2 capability binding). When `Some`, the
    /// turn launches each spec as an `ocean-plugin` subprocess and merges its
    /// tools into this turn's registry alongside the built-ins. `None` (every
    /// non-folder turn, and every data-only agent) leaves the process registry
    /// unchanged. Fail-soft: a spec that can't spawn is warned and skipped, never
    /// breaking the turn.
    pub agent_capabilities: Option<(PathBuf, Vec<agentdir::SubprocessCapability>)>,
    /// Fail-closed per-turn control that suppresses every tool source, including
    /// dynamically registered and folder-agent subprocess capabilities.
    pub tools_disabled: bool,
    /// Hashline-edit harness capability for this turn (W1 / harness profiles).
    /// When true the turn's `read` tags output + records snapshots and a
    /// `hashline_edit` tool is offered. Set by the daemon from the surface's
    /// daemon's effective `HarnessProfile`; `false` for web/voice and every
    /// direct legacy caller (defaults off in `PromptControl::new`).
    pub hashline_edits: bool,
    /// Artifact-spill harness capability for this turn (W3 / harness profiles).
    /// When true, oversized tool output is truncated for the model with a notice
    /// and the full output is spilled to the session artifact store (readable via
    /// `read artifact://<id>`). Set by the daemon from the surface's
    /// daemon's effective `HarnessProfile`; `false` for voice and every direct
    /// legacy caller, while TUI/ACP/CLI/web daemon turns enable it (defaults off
    /// in `PromptControl::new`).
    pub artifact_spill: bool,
    /// Human-facing session label for the first-turn case: the ORIGINAL user
    /// prompt, captured by the daemon BEFORE any prompt composition (room /
    /// operator guidance, folder-as-agent instructions, the Longhouse advisory,
    /// browser context, the surface flag). The runtime stores it as the session
    /// `title` on the first user turn so the switcher label is the user's own
    /// words, not the injected prefix — the persisted first message still carries
    /// the fully-composed prompt the model saw. `None` for direct callers that
    /// don't set it; the read side then derives and cleans the label from the
    /// first user message. Set via [`PromptControl::with_display_title`].
    pub display_title: Option<String>,
}

/// Narrow a turn's toolset to `allowlist` (folder-as-agent tool restriction).
/// Keeps the tools whose `name()` is in the list. Fail-safe: an allowlist that
/// matches NOTHING (every entry a typo / renamed tool) returns the FULL set plus
/// a warning, so a named agent is never left toolless by a bad config. `None` or
/// an empty list means no narrowing.
fn narrow_tools(tools: Vec<SharedTool>, allowlist: Option<&[String]>) -> Vec<SharedTool> {
    let Some(allow) = allowlist else {
        return tools;
    };
    if allow.is_empty() {
        return tools;
    }
    let narrowed: Vec<SharedTool> = tools
        .iter()
        .filter(|t| allow.iter().any(|a| a == t.name()))
        .cloned()
        .collect();
    if narrowed.is_empty() {
        tracing::warn!(
            ?allow,
            "agent tool allowlist matched no available tool; keeping the full toolset"
        );
        tools
    } else {
        narrowed
    }
}

impl PromptControl {
    pub fn new(permission: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            permission,
            cancel: None,
            event_sink: None,
            thinking_level: None,
            model_id: None,
            tool_allowlist: None,
            agent_model: None,
            agent_capabilities: None,
            tools_disabled: false,
            hashline_edits: false,
            artifact_spill: false,
            display_title: None,
        }
    }

    /// Disable every tool for this turn. This is the fail-closed control for
    /// contexts that must never execute capabilities; do not substitute an
    /// empty allowlist, whose semantics intentionally remain fail-open.
    pub fn without_tools(mut self) -> Self {
        self.tools_disabled = true;
        self
    }

    /// Enable the hashline-edit harness for this turn (W1). Set from the
    /// surface's effective `HarnessProfile` capabilities on the daemon side.
    pub fn with_hashline_edits(mut self, on: bool) -> Self {
        self.hashline_edits = on;
        self
    }

    /// Enable the artifact-spill harness for this turn (W3). Set from the
    /// surface's effective `HarnessProfile` capabilities on the daemon side.
    pub fn with_artifact_spill(mut self, on: bool) -> Self {
        self.artifact_spill = on;
        self
    }

    /// Narrow this turn's toolset to the named tools (folder-as-agent allowlist).
    pub fn with_tool_allowlist(mut self, tools: Vec<String>) -> Self {
        self.tool_allowlist = (!tools.is_empty()).then_some(tools);
        self
    }

    /// Bind a folder-as-agent's declared tier-1 subprocess capabilities for this
    /// turn (A2). `caps` are launched (relative commands resolved against
    /// `agent_root`) and their tools merged into the turn's registry. An empty
    /// `caps` list leaves the turn registry unchanged.
    pub fn with_agent_capabilities(
        mut self,
        agent_root: PathBuf,
        caps: Vec<agentdir::SubprocessCapability>,
    ) -> Self {
        self.agent_capabilities = (!caps.is_empty()).then_some((agent_root, caps));
        self
    }

    /// Set the folder-as-agent's declared model (fail-soft; see the field doc).
    pub fn with_agent_model(mut self, model: Option<String>) -> Self {
        self.agent_model = model.filter(|m| !m.trim().is_empty());
        self
    }

    pub fn yolo(allow_mutating: bool) -> Self {
        Self::new(Arc::new(StaticPermissionPolicy { allow_mutating }))
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_event_sink(mut self, sink: mpsc::UnboundedSender<AgentEvent>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Override the reasoning effort for this turn only (does not touch the
    /// runtime's global `thinking_level`).
    pub fn with_thinking_level(mut self, level: Option<ocean_protocol::ThinkingLevel>) -> Self {
        self.thinking_level = level;
        self
    }

    /// Pin this turn to a specific model alias without touching global state
    /// (OCEAN-36). `None` (or an empty string) leaves the global model in force.
    pub fn with_model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id.filter(|m| !m.trim().is_empty());
        self
    }

    /// Provide the human-facing session label (the pre-composition user prompt)
    /// the runtime stores as the session `title` on its first turn. `None` or a
    /// blank string leaves it unset, so the read side derives the label from the
    /// first user message instead.
    pub fn with_display_title(mut self, title: Option<String>) -> Self {
        self.display_title = title.filter(|t| !t.trim().is_empty());
        self
    }
}

struct StaticPermissionPolicy {
    allow_mutating: bool,
}

#[async_trait]
impl PermissionPolicy for StaticPermissionPolicy {
    async fn check(&self, _tool_name: &str, _args: &Value) -> PermissionDecision {
        if self.allow_mutating {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: "this static policy denies all mutating tools and has no interactive approval path; it is only reached by SDK embedders that constructed the agent with PromptControl::yolo(false). The daemon does NOT use this policy — live daemon turns gate through DaemonPermissionPolicy, which prompts the operator. To allow mutating tools here, build the embedder with PromptControl::yolo(true).".into(),
            }
        }
    }
}

/// Assemble the capability registry: built-ins first, then one provider per
/// configured MCP server. Built-ins-first ordering means an MCP server can never
/// shadow a built-in tool name (the registry dedups first-wins).
///
/// Secrets are resolved by env-var name from the daemon's process environment
/// (`std::env::var`), which is loaded from `tools.env`. Names only ever leave
/// this layer; values are injected straight into the child by `ocean-mcp` and
/// are never logged.
async fn build_capability_registry(
    config_dir: &Path,
    longhouse: Option<ocean_longhouse::LonghouseRegistryHandle>,
) -> CapabilityRegistry {
    let mut providers: Vec<Arc<dyn CapabilityProvider>> = vec![Arc::new(BuiltinProvider::new())];

    // Browser control. With the default-off `legacy-chromium` feature, Chrome
    // is launched lazily on the first turn that asks for tools (see
    // BrowserProvider); without it the provider registers the same 19 tool
    // schemas but every execute returns `browser_host_unavailable` and no
    // Chromium dependency exists in the build. In legacy mode we drive
    // **Chrome for Testing** with its own dedicated profile (NOT the user's
    // everyday Chrome): current stable Chrome (137+) removed `--load-extension`,
    // so the Ocean cockpit extension only auto-loads in CfT — and a dedicated
    // profile means we never conflict with (or require quitting) the user's
    // running Chrome. The user logs into their accounts once inside Ocean's
    // CfT; the profile persists them.
    let chrome_exe = resolve_chrome_for_testing(config_dir);
    let browser_profile = config_dir.join("chrome-profile");
    let browser_ext = {
        let p = config_dir.join("chrome-extension");
        p.exists().then_some(p)
    };
    providers.push(Arc::new(
        ocean_runtime::tools::browser::BrowserProvider::new(
            browser_profile,
            None,
            browser_ext,
            chrome_exe,
        ),
    ));

    // Code intelligence (W5 of the OMP port). Cheap to register: the provider
    // offers the `lsp` tool only for workspaces where a known language server's
    // root marker AND binary are both present (rust-analyzer, tsserver,
    // pyright, gopls); servers spawn lazily on first use and are shared
    // process-wide per (server, workspace-root).
    providers.push(Arc::new(ocean_lsp::LspProvider));

    // Memory verbs (port-map "cheapest win"): `retain`/`recall` over the typed
    // SQLite store at <config>/memory.sqlite. Fail-soft — a store that can't
    // open logs and is skipped; the turn runs without memory tools.
    match memory_tools::MemoryToolsProvider::open(&config_dir.join("memory.sqlite")) {
        Ok(p) => providers.push(Arc::new(p)),
        Err(e) => {
            tracing::warn!(error = %e, "memory store unavailable; retain/recall tools disabled");
        }
    }

    let cfg = match config::DaemonConfig::load(config_dir) {
        Ok(c) => c,
        Err(e) => {
            // A malformed config shouldn't take the agent down — run with
            // built-ins and make the misconfiguration loud.
            tracing::error!(error = %e, "failed to load ocean.toml; running with built-in tools only");
            return CapabilityRegistry::new(providers);
        }
    };

    // Offshore dispatch: agent work on the remote (tailnet) Ocean daemon inside
    // per-job git worktrees. Registered only when `[offshore]` is present AND
    // enabled — otherwise nothing changes. Construction is pure (the tools hold
    // config; nothing dials out until one executes), mirroring BrowserProvider's
    // lazy posture.
    if let Some(offshore) = cfg.offshore.as_ref().filter(|o| o.enabled) {
        providers.push(Arc::new(
            ocean_runtime::tools::offshore::OffshoreProvider::new(
                ocean_runtime::tools::offshore::OffshoreConfig {
                    remote_url: offshore.remote_url.clone(),
                    ssh_host: offshore.ssh_host.clone(),
                    ssh_bin: offshore.ssh_bin().to_string(),
                    remote_root: offshore.remote_root().to_string(),
                    turn_timeout_secs: offshore.turn_timeout_secs(),
                },
            ),
        ));
    }

    // Load tools.env once. Process env takes precedence (an explicitly exported
    // var overrides the file), so the closure falls back to the file only when
    // the var isn't already set. We log only how many keys were loaded — never
    // names or values.
    let file_env = load_tools_env(config_dir);

    for server in &cfg.mcp.server {
        let lookup = |name: &str| {
            std::env::var(name)
                .ok()
                .or_else(|| file_env.get(name).cloned())
        };
        match ocean_mcp::McpProvider::connect(server, lookup, MCP_CONNECT_TIMEOUT).await {
            Ok(provider) => providers.push(Arc::new(provider)),
            Err(e) => {
                // Connect only errors for an unusable *config* (e.g. stdio with
                // no command); server-side failures are already folded into an
                // empty provider inside connect().
                tracing::error!(server = %server.name, error = %e, "skipping misconfigured MCP server");
            }
        }
    }

    // Discover + register subprocess plugins, the same way MCP servers are
    // discovered from config and folded in as providers. Each plugin's tools
    // require permission (PluginProvider tools report requires_permission ==
    // true), so they're gated by the daemon's PermissionPolicy exactly like
    // bash/write/edit and MCP tools — plugins never bypass the gate.
    for provider in discover_plugin_providers(config_dir).await {
        providers.push(provider);
    }

    // Longhouse council ops as tools (OCEAN-118): convene a council /
    // read the board, namespaced `longhouse__*`. The provider holds the SAME
    // registry handle the daemon serves its `/v1/longhouse/topics*` routes off,
    // so tool-driven and operator-driven councils share one observable board.
    // Its tools report `requires_permission == true`, so they're gated by the
    // daemon's PermissionPolicy exactly like bash/write/edit and MCP/plugin
    // tools — agents never bypass the gate (post-OCEAN-54). When no handle is
    // supplied (tests / non-daemon embedders) the provider is simply omitted.
    if let Some(registry) = longhouse {
        providers.push(Arc::new(ocean_longhouse::LonghouseProvider::new(registry)));
    }

    CapabilityRegistry::new(providers)
}

/// Resolve the plugins directory: `OCEAN_PLUGINS_DIR` if set, else
/// `<config_dir>/plugins`. Mirrors `config_dir_from_env`'s env-override posture.
fn plugins_dir(config_dir: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("OCEAN_PLUGINS_DIR") {
        return PathBuf::from(path);
    }
    config_dir.join(PLUGINS_DIRNAME)
}

/// Scan the plugins directory, parse each `plugin.toml`, launch its subprocess,
/// and wrap it in a [`PluginProvider`] for the registry.
///
/// Fail-soft throughout, mirroring MCP discovery: a missing or empty plugins
/// directory yields no providers and no error (unchanged behavior); a plugin
/// that fails to parse, launch, or list its tools is logged at warn and skipped
/// — it can never break registry construction or daemon startup. The plugin's
/// own `list_tools` failure is already absorbed into an empty,
/// `Unavailable`-health provider by `PluginProvider::connect`; we only skip when
/// the manifest can't be read or the subprocess can't be spawned at all.
async fn discover_plugin_providers(config_dir: &Path) -> Vec<Arc<dyn CapabilityProvider>> {
    let dir = plugins_dir(config_dir);

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // No plugins directory is the normal case: no plugins, no error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "could not read plugins dir; no plugins loaded");
            return Vec::new();
        }
    };

    let mut providers: Vec<Arc<dyn CapabilityProvider>> = Vec::new();
    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let manifest_path = plugin_dir.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }

        let manifest = match ocean_plugin::PluginManifest::from_path(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %manifest_path.display(), error = %e, "skipping plugin: manifest parse failed");
                continue;
            }
        };

        let plugin = match ocean_plugin::SubprocessPlugin::launch(&manifest, &plugin_dir) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(plugin = %manifest.name, error = %e, "skipping plugin: failed to launch");
                continue;
            }
        };

        // connect() never errors on plugin-side failure: a plugin that can't
        // list tools becomes an empty, Unavailable provider rather than aborting
        // discovery.
        let provider = ocean_plugin::PluginProvider::connect(
            Arc::new(plugin) as Arc<dyn ocean_plugin::Plugin>
        )
        .await;
        providers.push(Arc::new(provider));
    }

    providers
}

/// Bind a folder-as-agent's declared tier-1 **subprocess capabilities** into
/// runtime providers for a turn (A2 capability binding).
///
/// For each `[[subprocess_capability]]` in the agent's `agent.toml`, launch its
/// `command` as an `ocean-plugin` [`SubprocessPlugin`](ocean_plugin::SubprocessPlugin)
/// (relative commands/cwd resolved against `agent_root`) and adapt it with
/// [`PluginProvider::connect`](ocean_plugin::PluginProvider::connect), so the
/// agent's declared tools become callable alongside the built-ins — namespaced
/// `plugin__<name>__<tool>` and permission-gated exactly like every other plugin
/// tool. Reuses `ocean-plugin`'s subprocess JSON-RPC wholesale; no reimplementation.
///
/// **Fail-soft, mirroring MCP/plugin discovery and the agent model-honoring path
/// in `prompt()`:** a capability with an empty command, or one whose command
/// can't spawn, is logged at warn and skipped — it can NEVER kill the turn. The
/// plugin's own `list_tools` failure is already absorbed into an empty,
/// `Unavailable`-health provider by `connect`. An agent with no subprocess
/// capabilities yields an empty vec (behavior-neutral for every data-only agent).
async fn build_agent_capability_providers(
    caps: &[agentdir::SubprocessCapability],
    agent_root: &Path,
) -> Vec<Arc<dyn CapabilityProvider>> {
    let canonical_agent_root = match std::fs::canonicalize(agent_root) {
        Ok(root) if root.is_dir() => root,
        Ok(root) => {
            tracing::warn!(agent_root = %root.display(), "skipping agent subprocess capabilities: agent root is not a directory");
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!(agent_root = %agent_root.display(), %error, "skipping agent subprocess capabilities: could not canonicalize agent root");
            return Vec::new();
        }
    };

    let mut providers: Vec<Arc<dyn CapabilityProvider>> = Vec::new();
    for cap in caps {
        let name = cap.effective_name();
        if cap.command.trim().is_empty() {
            tracing::warn!(
                capability = %name,
                "skipping agent subprocess capability: empty command"
            );
            continue;
        }

        // Resolve a relative command against the agent folder; an absolute path is
        // used as-is. Mirrors `SubprocessPlugin::launch`'s base-dir resolution for
        // a manifest `entry`, so a capability command is located exactly like a
        // plugin pack's `entry` (relative to the folder that declares it).
        let command = {
            let p = Path::new(&cap.command);
            if p.is_absolute() {
                cap.command.clone()
            } else {
                canonical_agent_root.join(p).to_string_lossy().into_owned()
            }
        };
        let requested_cwd = match cap.cwd.as_deref() {
            Some(cwd) if cwd.trim().is_empty() => {
                tracing::warn!(capability = %name, "skipping agent subprocess capability: empty cwd");
                continue;
            }
            Some(cwd) => {
                let path = Path::new(cwd);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    canonical_agent_root.join(path)
                }
            }
            None => canonical_agent_root.clone(),
        };
        let current_dir = match std::fs::canonicalize(&requested_cwd) {
            Ok(path) if path.is_dir() => path,
            Ok(path) => {
                tracing::warn!(capability = %name, cwd = %path.display(), "skipping agent subprocess capability: cwd is not a directory");
                continue;
            }
            Err(error) => {
                tracing::warn!(capability = %name, cwd = %requested_cwd.display(), %error, "skipping agent subprocess capability: invalid cwd");
                continue;
            }
        };
        let env: Vec<(String, String)> = cap
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let options = ocean_plugin::LaunchOptions::new(current_dir).with_env(&env);

        let plugin = match ocean_plugin::SubprocessPlugin::launch_command_with_options(
            &name, "0.0.0", &command, &cap.args, &options,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    capability = %name,
                    command = %command,
                    error = %e,
                    "skipping agent subprocess capability: failed to launch"
                );
                continue;
            }
        };
        // connect() never errors on a plugin-side failure: a plugin that can't
        // list tools becomes an empty, Unavailable provider rather than aborting.
        let provider = ocean_plugin::PluginProvider::connect(
            Arc::new(plugin) as Arc<dyn ocean_plugin::Plugin>
        )
        .await;
        providers.push(Arc::new(provider));
    }
    providers
}

/// Locate the Chrome for Testing binary the browser tools should drive. Current
/// stable Chrome removed `--load-extension`, so we need CfT for the cockpit
/// extension to auto-load. Resolution order:
///   1. OCEAN_CHROME_EXECUTABLE env (explicit override)
///   2. `<config_dir>/chrome-for-testing/.../Google Chrome for Testing` (staged)
///   3. None — chromiumoxide falls back to auto-detecting system Chrome (the
///      extension won't auto-load there, but navigation/screenshots still work)
fn resolve_chrome_for_testing(config_dir: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCEAN_CHROME_EXECUTABLE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let staged = config_dir
        .join("chrome-for-testing")
        .join("Google Chrome for Testing.app")
        .join("Contents")
        .join("MacOS")
        .join("Google Chrome for Testing");
    if staged.exists() {
        return Some(staged);
    }
    None
}

/// Parse `<config_dir>/tools.env` into a name→value map. Best-effort: a missing
/// file is normal (returns empty). Supports `KEY=VALUE` lines, `#` comments,
/// blank lines, an optional `export ` prefix, and surrounding quotes on the
/// value. Values are never logged — only the count of keys loaded.
fn load_tools_env(config_dir: &Path) -> HashMap<String, String> {
    let path = config_dir.join("tools.env");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read tools.env");
            return HashMap::new();
        }
    };
    let mut map = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        // Strip matching surrounding quotes from the value.
        let val = val.trim();
        let val = val
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| val.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(val);
        map.insert(key.to_string(), val.to_string());
    }
    if !map.is_empty() {
        tracing::info!(path = %path.display(), keys = map.len(), "loaded tools.env");
    }
    map
}

/// Resolve a fresh runtime state at startup. Model precedence:
///   1. explicit `OCEAN_MODEL` in the env (operator/CI override — always wins),
///   2. the last model the operator selected via `set_model` (persisted),
///   3. the provider layer's own fallback (true first run only).
///
/// This is why the daemon resumes on whatever you last used instead of snapping
/// back to a hardcoded model on every restart.
fn build_state_from_env(config_dir: &std::path::Path) -> anyhow::Result<RuntimeState> {
    // If OCEAN_MODEL is explicitly set, honor it untouched. Otherwise, if we
    // have a persisted last-used model, inject it as OCEAN_MODEL so the normal
    // resolution path picks it up. Only with neither do we hit the provider
    // default.
    if std::env::var_os("OCEAN_MODEL").is_none() {
        if let Some(last) = load_last_model(config_dir) {
            let mut env = ProviderEnv::from_process();
            env.vars.insert("OCEAN_MODEL".to_string(), last);
            let provider_config = resolve_provider_config(&env)?;
            return state_from_provider_config(provider_config);
        }
    }
    let provider_config = resolve_provider_config_from_env()?;
    state_from_provider_config(provider_config)
}

/// Build a runtime state from an already-resolved provider config.
fn state_from_provider_config(provider_config: ProviderConfig) -> anyhow::Result<RuntimeState> {
    let model = model_from_provider_config(&provider_config)?;
    let api_key = provider_config
        .credential
        .as_ref()
        .map(|credential| credential.secret.expose().to_string());
    // `provider/model` (e.g. "deepseek/deepseek-v4-pro"). Shows the model, not
    // just the provider, so a within-provider swap (deepseek-chat →
    // deepseek-v4-pro) is visible in the readout. Drops the legacy
    // "ocean-native-" prefix — a relic of the pre-daemon monolith.
    let backend_name = format!(
        "{}/{}",
        provider_config.selection.provider.as_str(),
        provider_config.selection.model,
    );
    Ok(RuntimeState {
        model,
        api_key,
        backend_name,
        provider_config,
    })
}

fn model_from_provider_config(config: &ProviderConfig) -> anyhow::Result<Model> {
    let selection = &config.selection;
    match selection.provider {
        ProviderId::DeepSeek
        | ProviderId::OpenAiCompatible
        | ProviderId::MiniMax
        | ProviderId::Kimi
        | ProviderId::Glm
        | ProviderId::Fake => Ok(Model::openai_compat(
            selection.provider.as_str(),
            selection.model.clone(),
            selection.base_url.clone(),
            selection.context_window,
            selection.max_output_tokens,
        )),
        ProviderId::OpenAi => Ok(match selection.model.as_str() {
            "gpt-4o" => Model::openai_gpt_4o(),
            "gpt-4o-mini" => Model::openai_gpt_4o_mini(),
            _ => {
                // Genuine OpenAI endpoint, just an id outside the named
                // constructors (gpt-4.1 / gpt-5 / o3 …) — all current OpenAI
                // chat models take image parts, so keep vision on even though
                // the compat constructor defaults it off for third-party
                // backends (OCEAN-386).
                let mut m = Model::openai_compat(
                    selection.provider.as_str(),
                    selection.model.clone(),
                    selection.base_url.clone(),
                    selection.context_window,
                    selection.max_output_tokens,
                );
                m.supports_images = true;
                m
            }
        }),
        ProviderId::OpenAiCodex => Ok(Model::codex(
            selection.model.clone(),
            selection.context_window,
            selection.max_output_tokens,
        )),
        ProviderId::Anthropic => Ok(match selection.model.as_str() {
            "claude-sonnet-5" => Model::anthropic_claude_sonnet_5(),
            "claude-opus-4-8" => Model::anthropic_claude_opus_4_8(),
            "claude-haiku-4-5" => Model::anthropic_claude_haiku_4_5(),
            "claude-fable-5" => Model::anthropic_claude_fable_5(),
            // Legacy ids — pinned sessions from before the 2026-07 refresh.
            "claude-sonnet-4-6" => Model::anthropic_claude_sonnet_4_6(),
            "claude-opus-4-7" => Model::anthropic_claude_opus_4_7(),
            _ => {
                anyhow::bail!("unsupported anthropic model '{}'", selection.model);
            }
        }),
        ProviderId::Google => Ok(match selection.model.as_str() {
            "gemini-2.0-flash" => Model::gemini_2_0_flash(),
            _ => {
                anyhow::bail!("unsupported google model '{}'", selection.model);
            }
        }),
        ProviderId::ClaudeCode => Ok(match selection.model.as_str() {
            // The claude-code alias maps to the REAL Anthropic API model id on
            // the wire — "claude-code-sonnet-5" is never sent to the API.
            "claude-code-fable-5" => Model::anthropic_claude_fable_5(),
            "claude-code-opus-4-8" | "claude-opus-4-8" => Model::anthropic_claude_opus_4_8(),
            "claude-code-sonnet-5" | "claude-sonnet-5" => Model::anthropic_claude_sonnet_5(),
            "claude-code-haiku-4-5" | "claude-haiku-4-5" => Model::anthropic_claude_haiku_4_5(),
            // Legacy ids — pinned sessions from before the 2026-07 refresh.
            "claude-code-sonnet-4-6" | "claude-sonnet-4-6" => Model::anthropic_claude_sonnet_4_6(),
            "claude-code-opus-4-7" | "claude-opus-4-7" => Model::anthropic_claude_opus_4_7(),
            _ => {
                anyhow::bail!("unsupported claude-code model '{}'", selection.model);
            }
        }),
    }
}

/// Map a resolved provider credential's kind onto the protocol wire auth
/// method so the request presents the token correctly. OAuth providers
/// (openai-codex, claude-code) resolve a bearer-kind credential and must
/// authenticate with `authorization: Bearer`; every other provider — including
/// a missing credential (e.g. the keyless fake provider) — keeps the default
/// `x-api-key`-style API-key auth so existing callers are unchanged. Carries
/// no secret: the token still arrives via `StreamOptions::api_key`.
fn auth_method_for(config: &ProviderConfig) -> ocean_protocol::types::AuthMethod {
    match config
        .credential
        .as_ref()
        .map(|credential| &credential.kind)
    {
        Some(ocean_providers::CredentialKind::OAuthBearer) => {
            ocean_protocol::types::AuthMethod::Bearer
        }
        _ => ocean_protocol::types::AuthMethod::ApiKey,
    }
}

/// File under `config_dir` that remembers the last model the operator selected,
/// so the daemon resumes on it across restarts instead of snapping back to a
/// hardcoded default.
const LAST_MODEL_FILE: &str = "last_model";

/// Persist the operator's current model choice (best-effort; a write failure is
/// logged, never fatal — losing the hint just falls back to the default).
fn persist_last_model(config_dir: &std::path::Path, model: &str) {
    let path = config_dir.join(LAST_MODEL_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, model.trim()) {
        tracing::warn!(path = %path.display(), error = %e, "failed to persist last model");
    }
}

/// Read the last persisted model choice, if any. `None` on first run / unreadable.
fn load_last_model(config_dir: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(config_dir.join(LAST_MODEL_FILE)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// File under `config_dir` that remembers the operator's persisted YOLO default
/// (OCEAN-YOLO), so the daemon resumes the chosen permission-gating posture
/// across restarts instead of always falling back to the safe default. Mirrors
/// [`LAST_MODEL_FILE`]: a tiny plaintext file holding `true`/`false`.
const YOLO_PREF_FILE: &str = "yolo_pref";

fn write_pref_atomic(path: &std::path::Path, value: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = durable::durable_rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

fn load_legacy_yolo_pref(config_dir: &std::path::Path) -> Option<bool> {
    let raw = std::fs::read_to_string(config_dir.join(YOLO_PREF_FILE)).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Three-state successor to [`YOLO_PREF_FILE`]. The old boolean file remains a
/// best-effort downgrade mirror; current binaries treat this file as authority.
const PERMISSION_MODE_PREF_FILE: &str = "permission_mode_pref";

fn load_permission_mode_file(config_dir: &std::path::Path) -> Option<PermissionMode> {
    let raw = std::fs::read_to_string(config_dir.join(PERMISSION_MODE_PREF_FILE)).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "manual" | "always_ask" | "always-ask" => Some(PermissionMode::Manual),
        "automatic" | "auto" | "write" => Some(PermissionMode::Automatic),
        "skip_all" | "skip-all" | "yolo" => Some(PermissionMode::SkipAll),
        _ => None,
    }
}

/// Persist the legacy boolean preference for compatibility callers.
pub fn persist_yolo_pref(config_dir: &std::path::Path, enabled: bool) {
    let path = config_dir.join(YOLO_PREF_FILE);
    if let Err(error) = write_pref_atomic(&path, if enabled { "true" } else { "false" }) {
        tracing::warn!(path = %path.display(), %error, "failed to persist yolo preference");
    }
}

/// Read the boolean compatibility view. Once a three-state preference exists,
/// derive the bool from that authoritative file so a failed/stale downgrade
/// mirror can never contradict the live daemon setting.
pub fn load_yolo_pref(config_dir: &std::path::Path) -> Option<bool> {
    load_permission_mode_file(config_dir)
        .map(|mode| mode == PermissionMode::SkipAll)
        .or_else(|| load_legacy_yolo_pref(config_dir))
}

/// Persist the daemon's authoritative global approval mode atomically. A write
/// error is returned to the settings endpoint instead of being reported as a
/// successful save. The legacy boolean file is a best-effort downgrade mirror;
/// current readers derive their boolean view from this authoritative mode file.
pub fn persist_permission_mode(
    config_dir: &std::path::Path,
    mode: PermissionMode,
) -> std::io::Result<()> {
    let path = config_dir.join(PERMISSION_MODE_PREF_FILE);
    write_pref_atomic(&path, mode.as_str())?;
    persist_yolo_pref(config_dir, mode == PermissionMode::SkipAll);
    Ok(())
}

/// Read the saved three-state approval mode. A pre-upgrade `yolo_pref` is
/// migrated logically on read without rewriting it: `true` becomes `SkipAll`,
/// `false` becomes `Automatic`. No files means no explicit saved choice.
pub fn load_permission_mode(config_dir: &std::path::Path) -> Option<PermissionMode> {
    load_permission_mode_file(config_dir).or_else(|| {
        load_legacy_yolo_pref(config_dir).map(|enabled| {
            if enabled {
                PermissionMode::SkipAll
            } else {
                PermissionMode::Automatic
            }
        })
    })
}

/// The directory the daemon uses for its on-disk state (sessions, projects, and
/// the persistent-rooms SQLite DB). Resolved from `OCEAN_CONFIG_DIR`, then
/// `XDG_CONFIG_HOME/ocean-rs`, then `~/.config/ocean-rs`, falling back to
/// `./.ocean-rs`. Exposed so other crates (e.g. the daemon wiring `ocean-store`)
/// place their files alongside the agent's, under one config dir.
pub fn config_dir_from_env() -> PathBuf {
    if let Some(path) = std::env::var_os("OCEAN_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join(APP_NAME);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join(APP_NAME);
    }
    PathBuf::from(".ocean-rs")
}

/// A language server relevant to a workspace, for the TUI `/lsp` surface: its
/// name, the extensions it owns, whether the project has its root marker, and
/// whether its binary is installed (`ready` = both, so a turn's `lsp` tool can
/// actually use it). Cheap + synchronous — pure filesystem + `$PATH` checks,
/// NO server spawn (live diagnostics stay the agent's `lsp` tool, which manages
/// the stateful server processes).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspServerView {
    pub name: String,
    pub command: String,
    pub extensions: Vec<String>,
    /// The detected project root for this server, if its marker was found.
    pub root: Option<String>,
    /// The server's binary resolves on `$PATH`.
    pub binary_available: bool,
    /// Root marker present AND binary installed — usable this session.
    pub ready: bool,
}

/// List the language servers whose root markers are present in `cwd`'s tree,
/// with their install/ready state. Only servers relevant to the project (root
/// marker found) are returned, so `/lsp` shows "rust-analyzer: ready" or
/// "pyright: install pyright-langserver" rather than the whole builtin table.
pub fn lsp_servers(cwd: &std::path::Path) -> Vec<LspServerView> {
    ocean_lsp::SERVERS
        .iter()
        .filter_map(|def| {
            let root = ocean_lsp::servers::find_root(cwd, def.root_markers);
            // Skip servers whose markers aren't in this project at all.
            let root_str = root.as_ref().map(|p| p.display().to_string());
            root_str.as_ref()?;
            let binary_available = ocean_lsp::servers::binary_on_path(def.command);
            Some(LspServerView {
                name: def.name.to_string(),
                command: def.command.to_string(),
                extensions: def.extensions.iter().map(|s| s.to_string()).collect(),
                ready: binary_available,
                binary_available,
                root: root_str,
            })
        })
        .collect()
}

fn should_strip_assistant_thinking(provider: &ProviderId, model: &str) -> bool {
    // OpenAiCodex is deliberately NOT in this list: the codex provider stores
    // encrypted Responses `reasoning` items in thinking_signature and MUST get
    // them back to replay them — stripping here is what degenerated gpt-5.x
    // into malformed tool calls across tool rounds. The codex encoder itself
    // drops any thinking block that isn't its own marked reasoning item, so the
    // cross-provider privacy drop still holds on that route.
    matches!(
        provider,
        ProviderId::DeepSeek
            | ProviderId::OpenAi
            | ProviderId::OpenAiCompatible
            | ProviderId::MiniMax
    ) || (*provider == ProviderId::Kimi && model != "kimi-k3")
}

fn strip_assistant_thinking_content(messages: &mut [Message]) {
    for message in messages {
        if let Message::Assistant(assistant) = message {
            assistant
                .content
                .retain(|content| !matches!(content, Content::Thinking { .. }));
        }
    }
}

/// Max messages we persist per session. The agent loop trims per-send to the
/// model's context window, but the stored transcript is reloaded in full at the
/// start of every future turn — so without a stored cap a long-lived session
/// reloads an ever-growing history and re-pays for it on every turn. Keep the
/// most recent messages; this is a hard bound on session file size and reload
/// cost, well above what any single coherent task needs.
const MAX_SESSION_MESSAGES: usize = 200;

/// Wall-clock bound on the operator-invoked `compact_session` model call — the
/// same budget as one provider round in a real turn
/// (`ocean_runtime::types::AgentConfig::DEFAULT_TURN_TIMEOUT_SECS`).
const COMPACT_TIMEOUT_SECS: u64 = 300;

/// Operator compaction keeps at most this many most-recent messages verbatim.
const COMPACT_PROTECT_MAX_MESSAGES: usize = 20;

/// Index where operator compaction (`compact_session`) splits the transcript:
/// `messages[..split]` is replaced by the model summary, `messages[split..]`
/// is the protected recent window kept verbatim. `0` means nothing would be
/// elided (the whole transcript fits the protected window — compaction is a
/// no-op).
///
/// The window is bounded by BOTH [`COMPACT_PROTECT_MAX_MESSAGES`] and 20% of
/// the model context window (by the shared [`estimate_tokens`] heuristic),
/// whichever is tighter — except the newest message, which is always
/// protected (mirroring `trim_to_context_window`'s always-keep-last rule).
/// The window never BEGINS on a `ToolResult`: a result whose originating
/// assistant tool call was elided into the summary would be provider-invalid
/// (Anthropic/OpenAI both 400 on an orphan result), so leading orphans are
/// pushed out of the window and into the summarized region — the same
/// direction `trim_to_context_window` resolves the split-pair hazard.
fn compact_protected_split(messages: &[Message], context_window: u32) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let protect_tokens = (context_window as usize) / 5;
    let mut kept_tokens = 0usize;
    let mut start = messages.len();
    for (idx, msg) in messages.iter().enumerate().rev() {
        if messages.len() - idx > COMPACT_PROTECT_MAX_MESSAGES {
            break;
        }
        let cost = estimate_tokens(msg);
        let is_last = idx == messages.len() - 1;
        if !is_last && kept_tokens + cost > protect_tokens {
            break;
        }
        kept_tokens += cost;
        start = idx;
    }
    // Never start the protected window on an orphan tool result.
    while start < messages.len() && matches!(messages[start], Message::ToolResult(_)) {
        start += 1;
    }
    start
}

/// Marker replacing an elided tool-result body during compaction. Tells the
/// model exactly what happened and how to recover, instead of silently
/// vanishing content it may reference.
const COMPACTION_MARKER: &str =
    "[old tool output elided to keep this session inside its token budget; \
     re-run the tool if you need it]";

/// Rough token estimate: serialized JSON length / 4 — the same heuristic the
/// runtime's request trim uses, so the two layers agree about sizes.
fn estimate_tokens<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0) / 4
}

/// Threshold-based, cache-stable compaction of stored session history.
///
/// Trigger: estimated history tokens exceed 50% of the model's context window.
/// Action: walk newest → oldest reserving a PROTECTED window (20% of the model
/// window) of most-recent messages kept verbatim; every `ToolResult` OLDER than
/// the protected window has its content replaced by [`COMPACTION_MARKER`].
/// Tool-call/result pairing is untouched (the result message stays, only its
/// body shrinks), so the transcript remains provider-valid by construction.
///
/// Why elide-only (no summarize, no drop): tool results dominate transcript
/// tokens by an order of magnitude, eliding is deterministic (no LLM call on
/// the turn's critical path), and idempotent — an already-elided result is
/// tiny and skipped, so re-running compaction as the protected window slides
/// forward only touches results that newly aged out. The messages BEFORE the
/// newly-elided region are byte-identical across turns, which is what keeps
/// the provider prompt-cache prefix warm. The runtime's per-request
/// `trim_to_context_window` remains the hard floor beneath this.
///
/// Returns how many tool results were elided this pass.
fn compact_history(messages: &mut [Message], context_window: u32) -> usize {
    let window = context_window as usize;
    let trigger = window / 2;
    let protect = window / 5;

    let total: usize = messages.iter().map(estimate_tokens).sum();
    if total <= trigger {
        return 0;
    }

    // Find the protection boundary: the oldest index whose suffix (newest
    // messages) still fits the protected budget. Everything before it is
    // eligible for elision.
    let mut kept = 0usize;
    let mut boundary = 0usize;
    for (idx, msg) in messages.iter().enumerate().rev() {
        kept += estimate_tokens(msg);
        if kept > protect {
            boundary = idx + 1;
            break;
        }
    }

    let mut elided = 0usize;
    for msg in &mut messages[..boundary] {
        if let Message::ToolResult(tr) = msg {
            // Skip results that are already tiny (incl. previously-elided
            // ones) — rewriting them would churn bytes for no savings.
            let body: usize = tr
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .map(str::len)
                .sum();
            let has_non_text = tr.content.iter().any(|c| c.as_text().is_none());
            if body > COMPACTION_MARKER.len() * 2 || has_non_text {
                tr.content = vec![Content::text(COMPACTION_MARKER)];
                elided += 1;
            }
        }
    }
    elided
}

fn cap_session_history(mut messages: Vec<Message>) -> Vec<Message> {
    if messages.len() <= MAX_SESSION_MESSAGES {
        return messages;
    }
    let drop = messages.len() - MAX_SESSION_MESSAGES;
    messages.drain(0..drop);
    // Don't let the kept history begin with an orphan tool-result whose
    // originating assistant ToolCall was just dropped — a provider would reject
    // it on the next turn's replay.
    while matches!(messages.first(), Some(Message::ToolResult(_))) {
        messages.remove(0);
    }
    messages
}

fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::Assistant(assistant) => {
            let text = assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

mod session;
#[cfg(test)]
mod tests {
    use super::*;

    struct NamedTool(&'static str);
    #[async_trait]
    impl ocean_runtime::AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _id: &str,
            _args: Value,
        ) -> std::result::Result<ocean_runtime::AgentToolResult, String> {
            Ok(ocean_runtime::AgentToolResult::text(""))
        }
    }
    fn tool(name: &'static str) -> SharedTool {
        Arc::new(NamedTool(name))
    }

    /// TASK-65 cross-crate guard: ocean-agent's display strip
    /// (`strip_surface_switch_notice` in `session/mod.rs`) anchors on this exact
    /// lead-in and `]\n` terminator to peel the Fix-3 surface-switch notice out of
    /// transcript projections. Keep the two literals in sync — if the notice text
    /// changes, the display strip silently stops matching and the whole preamble
    /// leaks into the user bubble again (the original operator-reported defect).
    /// Mirrors `folder_agent_block_anchors_display_strip_marker` in ocean-daemon.
    #[test]
    fn surface_switch_notice_anchors_display_strip_marker() {
        let notice = compose_surface_switch_notice("WEB", "TUI");
        // The exact rigid lead-in the stripper anchors on, up to the variable flag.
        assert!(notice.starts_with("[surface switch: the user is now messaging you via ["));
        // Single-line notice bounded by the stripper's `]\n` terminator.
        assert!(notice.ends_with("]\n"));
        // The only `]\n` is the terminator, so the first one cleanly bounds the
        // notice regardless of the {flag}/{from} pair — the internal `[WEB]`/`[TUI]`
        // brackets are followed by a space or `(`, never a newline.
        assert_eq!(notice.matches("]\n").count(), 1);
    }

    #[test]
    fn with_agent_model_filters_empty_and_defers_to_explicit() {
        let p = |perm| PromptControl::new(perm);
        let perm: Arc<dyn PermissionPolicy> = Arc::new(StaticPermissionPolicy {
            allow_mutating: false,
        });
        // A real model is kept; an empty/whitespace one is dropped to None.
        assert_eq!(
            p(perm.clone())
                .with_agent_model(Some("claude-opus-4-7".into()))
                .agent_model
                .as_deref(),
            Some("claude-opus-4-7")
        );
        assert!(p(perm.clone())
            .with_agent_model(Some("  ".into()))
            .agent_model
            .is_none());
        assert!(p(perm.clone()).with_agent_model(None).agent_model.is_none());
        // agent_model is independent of model_id (the explicit override); the
        // turn path prefers model_id and only falls to agent_model when it's None.
        let ctl = p(perm)
            .with_model_id(Some("explicit".into()))
            .with_agent_model(Some("agentmodel".into()));
        assert_eq!(ctl.model_id.as_deref(), Some("explicit"));
        assert_eq!(ctl.agent_model.as_deref(), Some("agentmodel"));
    }

    #[test]
    fn prompt_control_defaults_to_tools_enabled_and_without_tools_is_explicit() {
        let default = PromptControl::yolo(false);
        assert!(!default.tools_disabled);

        let disabled = PromptControl::yolo(true)
            .with_tool_allowlist(vec!["read".into()])
            .without_tools();
        assert!(disabled.tools_disabled);
        assert_eq!(
            disabled.tool_allowlist.as_deref(),
            Some(["read".into()].as_slice())
        );
    }

    #[test]
    fn prompt_control_harness_gates_default_off_and_apply_independently() {
        let default = PromptControl::yolo(false);
        assert!(!default.hashline_edits);
        assert!(!default.artifact_spill);

        let profiled = PromptControl::yolo(false)
            .with_hashline_edits(true)
            .with_artifact_spill(true);
        assert!(profiled.hashline_edits);
        assert!(profiled.artifact_spill);

        let web_like = PromptControl::yolo(false)
            .with_hashline_edits(false)
            .with_artifact_spill(true);
        assert!(!web_like.hashline_edits);
        assert!(web_like.artifact_spill);
    }

    #[test]
    fn narrow_tools_filters_and_is_fail_safe() {
        let all = || vec![tool("read"), tool("write"), tool("bash")];
        let names = |v: Vec<SharedTool>| -> Vec<String> {
            v.iter().map(|t| t.name().to_string()).collect()
        };

        // None / empty allowlist => no narrowing.
        assert_eq!(narrow_tools(all(), None).len(), 3);
        assert_eq!(narrow_tools(all(), Some(&[])).len(), 3);

        // A real allowlist narrows to the named tools.
        assert_eq!(
            names(narrow_tools(all(), Some(&["read".into(), "bash".into()]))),
            vec!["read", "bash"]
        );

        // FAIL-SAFE: an allowlist that matches NOTHING keeps the full set rather
        // than running the agent toolless.
        assert_eq!(
            narrow_tools(all(), Some(&["nonexistent".into()])).len(),
            3,
            "no match must fail safe to the full toolset"
        );
    }

    fn temp_config_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ocean-agent-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    /// Build a `Some(ttl)` from a day count for the GC tests. The GC takes its
    /// TTL as a parameter now (OCEAN-211), so tests pass it explicitly and never
    /// touch the process-global env — no env races, no EnvVarGuard needed.
    fn ttl_days(days: u64) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(days * 24 * 60 * 60))
    }

    /// Backdate a file's mtime by `days` so the GC sees it as aged.
    fn backdate(path: &Path, days: u64) {
        let when =
            std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    #[test]
    fn session_file_gc_prunes_aged_files_keeps_recent() {
        let config_dir = temp_config_dir("session-gc-prune");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();

        // Two aged sessions, one fresh — all in real workspace buckets.
        let mut old_a = session::Session::new(&model);
        old_a.bind_workspace(Path::new("."));
        let old_a_path = session::save(&config_dir, &old_a).unwrap();

        let mut old_b = session::Session::new(&model);
        old_b.bind_workspace(Path::new("."));
        let old_b_path = session::save(&config_dir, &old_b).unwrap();

        let mut recent = session::Session::new(&model);
        recent.bind_workspace(Path::new("."));
        let recent_path = session::save(&config_dir, &recent).unwrap();

        // Backdate the two "old" files well past a 30-day TTL.
        backdate(&old_a_path, 100);
        backdate(&old_b_path, 45);

        let pruned = session::session_file_gc(&config_dir, ttl_days(30));

        assert_eq!(pruned, 2, "both aged files pruned");
        assert!(!old_a_path.exists(), "100-day file deleted");
        assert!(!old_b_path.exists(), "45-day file deleted");
        assert!(recent_path.exists(), "fresh file kept");

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn session_file_gc_disabled_prunes_nothing() {
        let config_dir = temp_config_dir("session-gc-disabled");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();

        let mut ancient = session::Session::new(&model);
        ancient.bind_workspace(Path::new("."));
        let ancient_path = session::save(&config_dir, &ancient).unwrap();
        backdate(&ancient_path, 10_000); // way past any default

        // None = disabled (the `0` days case, resolved at the env edge).
        let pruned = session::session_file_gc(&config_dir, None);

        assert_eq!(pruned, 0, "TTL=None disables pruning");
        assert!(ancient_path.exists(), "nothing deleted when disabled");

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn ttl_from_days_zero_is_disabled() {
        assert_eq!(session::ttl_from_days(0), None, "0 days disables pruning");
    }

    #[test]
    fn ttl_from_days_normal_value() {
        assert_eq!(
            session::ttl_from_days(30),
            Some(std::time::Duration::from_secs(30 * 24 * 60 * 60)),
            "30 days resolves to 30 * 86_400 seconds"
        );
    }

    /// OCEAN-211 bug 1: a huge OCEAN_SESSION_TTL_DAYS overflows `days * 86_400`.
    /// Before the fix that panicked in debug (GC runs in from_env) / wrapped to a
    /// tiny TTL in release (deleting sessions the operator wanted kept). The
    /// conversion must now SATURATE to a never-prune TTL instead of panicking or
    /// wrapping.
    #[test]
    fn ttl_from_days_overflow_saturates_to_never_prune() {
        let ttl = session::ttl_from_days(u64::MAX);
        assert_eq!(
            ttl,
            Some(std::time::Duration::MAX),
            "overflow saturates to Duration::MAX, not a wrapped tiny value"
        );
    }

    /// End-to-end of the overflow path: a never-prune TTL must leave even an
    /// aged file untouched — i.e. the saturated value really does behave as
    /// "never prune", never deleting wanted sessions.
    #[test]
    fn session_file_gc_overflow_ttl_keeps_everything() {
        let config_dir = temp_config_dir("session-gc-overflow");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();

        let mut aged = session::Session::new(&model);
        aged.bind_workspace(Path::new("."));
        let aged_path = session::save(&config_dir, &aged).unwrap();
        backdate(&aged_path, 10_000); // way past any sane TTL

        // Feed the overflow-resolved TTL (u64::MAX days) into the GC.
        let ttl = session::ttl_from_days(u64::MAX);
        let pruned = session::session_file_gc(&config_dir, ttl);

        assert_eq!(pruned, 0, "never-prune TTL deletes nothing");
        assert!(aged_path.exists(), "aged file survives an overflow TTL");

        let _ = std::fs::remove_dir_all(config_dir);
    }

    // OCEAN-115: an image attached to a turn must reach the FIRST user message
    // as a `Content::Image` block, alongside the prompt text, so the provider
    // encoders (OCEAN-99) serialize vision input. This tests the exact seam that
    // `run_prompt` uses to build that message.
    #[test]
    fn build_user_message_attaches_images_as_content_blocks() {
        let images = vec![
            PromptImage {
                mime_type: "image/png".into(),
                data: "AAAA".into(),
            },
            // A data-URL payload: the prefix must be stripped to the base64 body.
            PromptImage {
                mime_type: "image/jpeg".into(),
                data: "data:image/jpeg;base64,BBBB".into(),
            },
        ];
        let msg = build_user_message("look at this".into(), Some(&images));

        let Message::User { content, .. } = msg else {
            panic!("expected a user message");
        };
        // [Text, Image, Image] — text leads, one Image block per attachment.
        assert_eq!(content.len(), 3, "text + 2 images");
        assert!(matches!(&content[0], Content::Text { text } if text == "look at this"));

        match &content[1] {
            Content::Image { data, mime_type } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, "AAAA");
            }
            other => panic!("expected Content::Image, got {other:?}"),
        }
        match &content[2] {
            Content::Image { data, mime_type } => {
                assert_eq!(mime_type, "image/jpeg");
                // data-URL prefix stripped; only the base64 body remains.
                assert_eq!(data, "BBBB");
            }
            other => panic!("expected Content::Image, got {other:?}"),
        }
    }

    // OCEAN-177: a turn carrying a `Content::Image` block must surface in the
    // display projection. `text_from_content` keeps only Text/Thinking, so the
    // transcript entry's `text` is empty for an image-only block — but `images`
    // must still record the attachment (mime_type only, no base64) so a
    // replaying client sees evidence an image was attached.
    #[test]
    fn transcript_entry_reflects_image_turns() {
        let content = vec![
            Content::Text {
                text: "look at this".into(),
            },
            Content::Image {
                data: "AAAAbase64payload".into(),
                mime_type: "image/png".into(),
            },
        ];
        let message = Message::User {
            content,
            timestamp: 1_700_000_000,
        };

        let entry = session::transcript_entry(&message);

        assert_eq!(entry.role, "user");
        assert_eq!(entry.text, "look at this", "text keeps the prompt text");
        assert_eq!(entry.images.len(), 1, "the image block is recorded");
        assert_eq!(entry.images[0].mime_type, "image/png");
        // The base64 payload must NEVER be inlined into the display projection.
        let serialized = serde_json::to_string(&entry).unwrap();
        assert!(
            !serialized.contains("AAAAbase64payload"),
            "image bytes must not leak into the transcript entry"
        );

        // An image-ONLY block still yields a non-empty projection via `images`
        // even though `text` is empty.
        let image_only = Message::User {
            content: vec![Content::Image {
                data: "BBBB".into(),
                mime_type: "image/jpeg".into(),
            }],
            timestamp: 1_700_000_001,
        };
        let entry = session::transcript_entry(&image_only);
        assert!(entry.text.is_empty(), "no Text/Thinking → empty text");
        assert_eq!(entry.images.len(), 1);
        assert_eq!(entry.images[0].mime_type, "image/jpeg");
    }

    // A turn with no images produces an empty `images` vec, which serde elides
    // from the wire (`skip_serializing_if = "Vec::is_empty"`) — additive and
    // backwards-compatible for existing clients.
    #[test]
    fn transcript_entry_without_images_omits_images_field() {
        let message = Message::user_text("just text");
        let entry = session::transcript_entry(&message);
        assert!(entry.images.is_empty());
        let serialized = serde_json::to_string(&entry).unwrap();
        assert!(
            !serialized.contains("images"),
            "empty images vec must not serialize"
        );
    }

    // No images → plain-text message, identical to the prior behaviour.
    #[test]
    fn build_user_message_without_images_is_text_only() {
        let none = build_user_message("hi".into(), None);
        let Message::User { content, .. } = none else {
            panic!("expected a user message");
        };
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], Content::Text { text } if text == "hi"));

        // An empty image slice is also text-only.
        let empty = build_user_message("hi".into(), Some(&[]));
        let Message::User { content, .. } = empty else {
            panic!("expected a user message");
        };
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn cap_session_history_is_noop_under_limit() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| Message::user_text(format!("m{i}")))
            .collect();
        assert_eq!(cap_session_history(msgs).len(), 10);
    }

    /// One assistant tool_call + its big tool_result pair, ~`kb` KB of result.
    fn tool_round(id: &str, kb: usize) -> [Message; 2] {
        let call = Message::Assistant(ocean_protocol::AssistantMessage {
            content: vec![ocean_protocol::Content::ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Default::default(),
            stop_reason: ocean_protocol::StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        });
        let result = Message::ToolResult(ocean_protocol::ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![ocean_protocol::Content::text("x".repeat(kb * 1024))],
            is_error: false,
            timestamp: 0,
        });
        [call, result]
    }

    #[test]
    fn compact_history_is_noop_under_trigger() {
        let mut msgs: Vec<Message> = tool_round("a", 4).into_iter().collect();
        // 4KB ≈ 1K tokens against a 200K window — far under the 50% trigger.
        assert_eq!(compact_history(&mut msgs, 200_000), 0);
        let Message::ToolResult(tr) = &msgs[1] else {
            panic!()
        };
        assert!(tr.content[0].as_text().unwrap().len() > 4000, "untouched");
    }

    /// Over the trigger: OLD tool results are elided to the marker, results in
    /// the protected recent window keep their bodies, and pairing is intact.
    /// A second pass elides nothing (byte-stable prefix = warm prompt cache).
    #[test]
    fn compact_history_elides_old_tool_results_and_is_idempotent() {
        // Window 40K tokens → trigger 20K, protect 8K. Ten 20KB (≈5K-token)
        // rounds ≈ 50K tokens total → well over trigger; protect covers ~the
        // newest 1-2 rounds.
        let mut msgs: Vec<Message> = (0..10)
            .flat_map(|i| tool_round(&format!("c{i}"), 20))
            .collect();
        let elided = compact_history(&mut msgs, 40_000);
        assert!(elided >= 5, "old results must be elided, got {elided}");

        // Newest result keeps its body (inside the protected window).
        let Message::ToolResult(newest) = &msgs[19] else {
            panic!()
        };
        assert!(
            newest.content[0].as_text().unwrap().len() > 10_000,
            "the newest tool result must stay verbatim"
        );
        // Oldest result is the marker.
        let Message::ToolResult(oldest) = &msgs[1] else {
            panic!()
        };
        assert_eq!(oldest.content[0].as_text(), Some(COMPACTION_MARKER));
        // Pairing intact: every tool_call id still has a result message.
        for i in (0..20).step_by(2) {
            assert!(matches!(&msgs[i], Message::Assistant(_)));
            assert!(matches!(&msgs[i + 1], Message::ToolResult(_)));
        }

        // Idempotent while nothing new ages out: an immediate second pass
        // rewrites zero messages, so the prefix stays byte-identical.
        let again = compact_history(&mut msgs, 40_000);
        assert_eq!(again, 0, "second pass must not churn bytes");
    }

    // --- operator compaction: compact_protected_split ------------------------

    fn big_user(chars: usize) -> Message {
        Message::user_text("x".repeat(chars))
    }

    /// Window bounds: empty and short transcripts are fully protected (split
    /// 0); a lone oversized message is still protected (always-keep-last); the
    /// token bound tightens the window; the 20-message count cap binds when
    /// tokens are plentiful.
    #[test]
    fn compact_protected_split_bounds_by_count_tokens_and_keeps_last() {
        // Empty and small-session transcripts: nothing to elide.
        assert_eq!(compact_protected_split(&[], 1_000), 0);
        let small: Vec<Message> = (0..3).map(|_| big_user(10)).collect();
        assert_eq!(compact_protected_split(&small, 1_000), 0);

        // A single message larger than the whole budget is still protected.
        let huge = vec![big_user(10_000)];
        assert_eq!(compact_protected_split(&huge, 1_000), 0);

        // Token bound: 6 × 600-char messages against a 1K window (200-token
        // protect budget) protects only a newest suffix; the split is > 0 and
        // the protected side stays within the budget plus the always-kept last.
        let long: Vec<Message> = (0..6).map(|_| big_user(600)).collect();
        let split = compact_protected_split(&long, 1_000);
        assert!(split > 0, "a long transcript must have an elidable prefix");
        assert!(split < long.len(), "the newest message must stay protected");

        // Count cap: with an enormous token budget the 20-message cap binds.
        let many: Vec<Message> = (0..30).map(|_| big_user(10)).collect();
        assert_eq!(
            compact_protected_split(&many, 1_000_000),
            10,
            "20-message cap must bound the protected window"
        );
    }

    /// The protected window must never BEGIN on a tool result whose
    /// originating assistant call was elided — that orphan pair split is
    /// provider-invalid. Leading orphans are pushed into the summarized side.
    #[test]
    fn compact_protected_split_never_starts_on_an_orphan_tool_result() {
        // Big user + BIG assistant tool-call + result + small tail. The token
        // walk breaks inside the pair so the naive boundary lands on the
        // ToolResult; the split must advance past it.
        let call = Message::Assistant(ocean_protocol::AssistantMessage {
            content: vec![ocean_protocol::Content::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "pad": "y".repeat(600) }),
            }],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Default::default(),
            stop_reason: ocean_protocol::StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        });
        let result = Message::ToolResult(ocean_protocol::ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: vec![ocean_protocol::Content::text("z".repeat(300))],
            is_error: false,
            timestamp: 0,
        });
        let msgs = vec![big_user(600), call, result, Message::user_text("tail")];

        let split = compact_protected_split(&msgs, 1_000);
        assert!(
            !matches!(msgs[split], Message::ToolResult(_)),
            "protected window must not begin on an orphan tool result (split {split})"
        );
        assert_eq!(split, 3, "the orphan result joins the summarized region");
    }

    // --- operator compaction: compact_session end-to-end ----------------------

    /// Scripted no-tools provider for `compact_session`: replays a fixed event
    /// script and counts invocations (so no-op paths can prove the model was
    /// never called).
    struct ScriptedCompactProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        script: Vec<ocean_protocol::AssistantMessageEvent>,
    }
    #[async_trait]
    impl ocean_protocol::Provider for ScriptedCompactProvider {
        async fn stream(
            &self,
            _model: &ocean_protocol::Model,
            _ctx: &ocean_protocol::Context,
            _options: &ocean_protocol::StreamOptions,
        ) -> ocean_protocol::Result<ocean_protocol::AssistantMessageEventStream> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events: Vec<ocean_protocol::Result<ocean_protocol::AssistantMessageEvent>> =
                self.script.clone().into_iter().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn scripted_assistant(text: &str) -> ocean_protocol::AssistantMessage {
        ocean_protocol::AssistantMessage {
            content: vec![Content::text(text)],
            api: "test".into(),
            provider: "test".into(),
            model: "scripted".into(),
            usage: Default::default(),
            stop_reason: ocean_protocol::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    /// Runtime with a ready fake-provider config plus a scripted compact
    /// provider; returns the call counter for no-op assertions.
    fn compact_runtime(
        name: &str,
        script: Vec<ocean_protocol::AssistantMessageEvent>,
    ) -> (AgentRuntime, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut rt = runtime(
            temp_config_dir(name),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        rt.test_compact_provider = Some(TestCompactProvider(Arc::new(ScriptedCompactProvider {
            calls: calls.clone(),
            script,
        })));
        (rt, calls)
    }

    /// Persist a session with the given transcript under the runtime's config
    /// dir, returning its id and on-disk path.
    fn seeded_session(rt: &AgentRuntime, messages: Vec<Message>) -> (SessionId, PathBuf) {
        let model = rt.snapshot().model.clone();
        let mut session = session::Session::new(&model);
        session.bind_workspace(Path::new("."));
        session.messages = messages;
        let path = session::save(&rt.config_dir, &session).expect("seed session");
        (session.id, path)
    }

    #[tokio::test]
    async fn compact_session_replaces_transcript_with_summary_and_protected_window() {
        let (rt, calls) = compact_runtime(
            "compact-happy",
            vec![
                ocean_protocol::AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "SUMMARY OF THE OLD CONVERSATION".into(),
                },
                ocean_protocol::AssistantMessageEvent::Done {
                    reason: ocean_protocol::StopReason::Stop,
                    message: scripted_assistant(""),
                },
            ],
        );
        let original: Vec<Message> = (0..6).map(|_| big_user(600)).collect();
        let expected_split = compact_protected_split(&original, rt.snapshot().model.context_window);
        assert!(expected_split > 0, "fixture must actually need compaction");
        let (id, _path) = seeded_session(&rt, original.clone());

        let res = rt.compact_session(id).await.expect("compact runs");
        assert!(res.ok, "compact must succeed: {}", res.stderr);
        assert_eq!(res.elided_messages, expected_split as u64);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Reload FROM DISK: the atomic save is the observable contract.
        let saved = session::load(&rt.config_dir, id).expect("reload compacted session");
        assert_eq!(
            saved.messages.len(),
            2 + (original.len() - expected_split),
            "marker + summary + protected window"
        );
        let Message::User { content, .. } = &saved.messages[0] else {
            panic!("first message must be the compaction marker");
        };
        assert!(content[0]
            .as_text()
            .unwrap()
            .contains("The session was compacted"));
        let Message::Assistant(summary) = &saved.messages[1] else {
            panic!("second message must carry the summary");
        };
        assert_eq!(
            summary.content[0].as_text(),
            Some("SUMMARY OF THE OLD CONVERSATION")
        );
        // The protected tail is the original suffix, verbatim.
        let tail = &saved.messages[2..];
        for (kept, orig) in tail.iter().zip(&original[expected_split..]) {
            assert_eq!(
                serde_json::to_string(kept).unwrap(),
                serde_json::to_string(orig).unwrap(),
                "protected messages must be byte-identical"
            );
        }
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[tokio::test]
    async fn compact_session_unknown_session_is_a_clean_error() {
        let (rt, calls) = compact_runtime("compact-unknown", Vec::new());
        let res = rt
            .compact_session(SessionId::new_v4())
            .await
            .expect("unknown session is a response, not an Err");
        assert!(!res.ok);
        assert_eq!(res.stderr, "session not found");
        assert_eq!(res.elided_messages, 0);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no model call for an unknown session"
        );
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[tokio::test]
    async fn compact_session_corrupt_session_errors_without_wiping() {
        let (rt, calls) = compact_runtime("compact-corrupt", Vec::new());
        let (id, path) = seeded_session(&rt, vec![big_user(600)]);
        std::fs::write(&path, "{ not json").expect("corrupt the session file");

        let err = rt
            .compact_session(id)
            .await
            .expect_err("corrupt storage must be an Err, never a fresh session");
        assert!(!err.to_string().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).expect("file still present"),
            "{ not json",
            "a corrupt session file must never be wiped or rewritten"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[tokio::test]
    async fn compact_session_short_session_is_a_noop_without_a_model_call() {
        let (rt, calls) = compact_runtime("compact-short", Vec::new());
        let original = vec![Message::user_text("hi"), Message::user_text("there")];
        let (id, _path) = seeded_session(&rt, original.clone());

        let res = rt.compact_session(id).await.expect("noop compact runs");
        assert!(res.ok);
        assert_eq!(res.elided_messages, 0);
        assert!(res.stderr.contains("nothing to compact"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a fully-protected transcript must not spend a model call"
        );
        let saved = session::load(&rt.config_dir, id).expect("reload");
        assert_eq!(saved.messages.len(), original.len(), "transcript untouched");
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[tokio::test]
    async fn try_compact_rejects_busy_session_but_not_a_different_session() {
        let (rt, _calls) = compact_runtime("compact-busy", Vec::new());
        let (busy_id, _) = seeded_session(&rt, vec![Message::user_text("busy")]);
        let (other_id, _) = seeded_session(&rt, vec![Message::user_text("other")]);
        let lease = rt.try_session_operation(busy_id).expect("first lease");

        assert_eq!(
            rt.try_compact_session(busy_id).await.unwrap_err(),
            SessionOperationBusy
        );
        let other = rt
            .try_compact_session(other_id)
            .await
            .expect("different session is independent")
            .expect("other compact runs");
        assert!(other.ok);
        drop(lease);
        let retry = rt
            .try_compact_session(busy_id)
            .await
            .expect("released session admits")
            .expect("retry runs");
        assert!(retry.ok);
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[tokio::test]
    async fn compact_session_provider_error_leaves_transcript_untouched() {
        let mut error = scripted_assistant("");
        error.stop_reason = ocean_protocol::StopReason::Error;
        error.error_message = Some("boom".into());
        let (rt, _calls) = compact_runtime(
            "compact-provider-error",
            vec![ocean_protocol::AssistantMessageEvent::Error {
                reason: ocean_protocol::StopReason::Error,
                error,
            }],
        );
        let original: Vec<Message> = (0..6).map(|_| big_user(600)).collect();
        let (id, _path) = seeded_session(&rt, original.clone());

        let res = rt.compact_session(id).await.expect("error is a response");
        assert!(!res.ok);
        assert_eq!(res.stderr, "boom");
        assert_eq!(res.elided_messages, 0);
        let saved = session::load(&rt.config_dir, id).expect("reload");
        assert_eq!(
            saved.messages.len(),
            original.len(),
            "a failed compact must leave the transcript untouched"
        );
        let _ = std::fs::remove_dir_all(&rt.config_dir);
    }

    #[cfg(unix)]
    #[test]
    fn agent_capability_process_probe_child() {
        use std::io::{BufRead, Write};

        if std::env::var_os("OCEAN_AGENT_PROCESS_PROBE").is_none() {
            return;
        }

        let observation_path =
            std::env::var_os("OCEAN_AGENT_PROCESS_OBSERVATION").expect("probe observation path");
        let observation = serde_json::json!({
            "pwd": std::env::var("PWD").ok(),
            "cwd": std::env::current_dir()
                .expect("probe current dir")
                .to_string_lossy(),
            "explicit_env": std::env::var("OCEAN_AGENT_EXPLICIT_ENV").ok(),
            "ambient_home": std::env::var("HOME").ok(),
        });
        std::fs::write(observation_path, serde_json::to_vec(&observation).unwrap()).unwrap();

        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "tools": [] }
        });
        let mut stdout = std::io::stdout();
        writeln!(stdout, "\n{response}").unwrap();
        stdout.flush().unwrap();
    }

    #[cfg(unix)]
    fn process_probe_capability(
        observation: &Path,
        cwd: Option<&str>,
    ) -> agentdir::SubprocessCapability {
        let mut env = std::collections::BTreeMap::new();
        env.insert("OCEAN_AGENT_PROCESS_PROBE".to_string(), "1".to_string());
        env.insert(
            "OCEAN_AGENT_PROCESS_OBSERVATION".to_string(),
            observation.to_string_lossy().into_owned(),
        );
        env.insert(
            "OCEAN_AGENT_EXPLICIT_ENV".to_string(),
            "explicit-value".to_string(),
        );
        agentdir::SubprocessCapability {
            name: Some("cwd-probe".to_string()),
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".to_string(),
                "tests::agent_capability_process_probe_child".to_string(),
                "--nocapture".to_string(),
                "--test-threads=1".to_string(),
            ],
            cwd: cwd.map(str::to_string),
            env,
        }
    }

    #[cfg(unix)]
    async fn assert_agent_process_context(agent_root: &Path, cwd: Option<&str>, expected: &Path) {
        assert!(
            std::env::var_os("HOME").is_some(),
            "test requires an ambient parent variable"
        );
        let observation = agent_root.join("observation.json");
        let cap = process_probe_capability(&observation, cwd);

        let providers = build_agent_capability_providers(&[cap], agent_root).await;
        assert_eq!(providers.len(), 1, "probe capability must bind");
        let observed: Value =
            serde_json::from_slice(&std::fs::read(&observation).unwrap()).unwrap();
        let expected = std::fs::canonicalize(expected).unwrap();
        assert_eq!(observed["pwd"], expected.to_string_lossy().as_ref());
        assert_eq!(observed["cwd"], expected.to_string_lossy().as_ref());
        assert_eq!(observed["explicit_env"], "explicit-value");
        assert!(
            observed["ambient_home"].is_null(),
            "arbitrary ambient parent environment must not reach the child"
        );
        drop(providers);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_subprocess_omitted_cwd_uses_canonical_agent_root() {
        let agent_root = temp_config_dir("agent-cap-default-cwd");
        std::fs::create_dir_all(&agent_root).unwrap();
        assert_agent_process_context(&agent_root, None, &agent_root).await;
        let _ = std::fs::remove_dir_all(agent_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_subprocess_relative_cwd_changes_real_cwd_and_pwd() {
        let agent_root = temp_config_dir("agent-cap-relative-cwd");
        let declared_cwd = agent_root.join("declared-cwd");
        std::fs::create_dir_all(&declared_cwd).unwrap();
        assert_agent_process_context(&agent_root, Some("declared-cwd"), &declared_cwd).await;
        let _ = std::fs::remove_dir_all(agent_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_subprocess_invalid_cwd_is_skipped_fail_soft() {
        let agent_root = temp_config_dir("agent-cap-invalid-cwd");
        std::fs::create_dir_all(&agent_root).unwrap();
        let observation = agent_root.join("observation.json");
        let cap = process_probe_capability(&observation, Some("missing"));

        let providers = build_agent_capability_providers(&[cap], &agent_root).await;
        assert!(providers.is_empty());
        assert!(
            !observation.exists(),
            "invalid cwd must not launch the child"
        );
        let _ = std::fs::remove_dir_all(agent_root);
    }

    /// Write a tiny stdio "plugin" — a shell script that speaks the same
    /// JSON-RPC wire as `ocean-plugin`'s echo_plugin: it answers `list_tools`
    /// with one `echo` tool and `invoke_tool` by echoing the args back. Returns
    /// the script path (made executable). Used so the discovery test exercises a
    /// real subprocess plugin without depending on another crate's test binary.
    fn write_echo_plugin_script(dir: &Path) -> PathBuf {
        let script = dir.join("echo-plugin.sh");
        let body = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  [ -z "$line" ] && continue
  id=$(printf '%s' "$line" | sed -n 's/.*"id":[[:space:]]*\([0-9]*\).*/\1/p')
  case "$line" in
    *list_tools*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the args back","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *invoke_tool*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"echoed":true}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"method not found"}}\n' "$id"
      ;;
  esac
done
"#;
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    // OCEAN-110: a plugin discovered under <config_dir>/plugins must have its
    // declared tool surface into the capability registry, namespaced
    // plugin__<name>__<tool>, alongside the built-ins. Proves the daemon now
    // actually loads plugins + contributes their tools.
    #[tokio::test]
    async fn discovers_plugin_and_its_tool_appears_in_registry() {
        let config_dir = temp_config_dir("plugin-discovery");
        let plugin_dir = config_dir.join(PLUGINS_DIRNAME).join("echo-pack");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let script = write_echo_plugin_script(&plugin_dir);
        // Manifest points entry at the script (relative to the plugin dir).
        let manifest = format!(
            "name = \"echo-pack\"\nversion = \"0.1.0\"\nentry = \"{}\"\n\n[[tool]]\nname = \"echo\"\ndescription = \"Echo the args back\"\n",
            script.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();

        let providers = discover_plugin_providers(&config_dir).await;
        assert_eq!(providers.len(), 1, "exactly one plugin discovered");

        // Compose with built-ins and assert the namespaced tool is present and
        // gated (plugin tools require permission).
        let mut all: Vec<Arc<dyn CapabilityProvider>> = vec![Arc::new(BuiltinProvider::new())];
        all.extend(providers);
        let registry = CapabilityRegistry::new(all);
        let tools = registry.tools_for_session(&SessionContext::default()).await;
        let echo = tools
            .iter()
            .find(|t| t.name() == "plugin__echo-pack__echo")
            .expect("plugin tool namespaced into registry");
        assert!(
            echo.requires_permission(),
            "plugin tools must be permission-gated"
        );
        assert!(
            tools.iter().any(|t| t.name() == "bash"),
            "built-ins still present"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    // A2 — folder-as-agent capability binding. A fixture agent folder that
    // declares a tier-1 `[[subprocess_capability]]` must have its tool folded
    // into the resolved registry, namespaced `plugin__<name>__<tool>` and
    // permission-gated, alongside the built-ins — the whole point of A2. Resolves
    // the agent through `agentdir` for real so the parse + bind path is exercised
    // end-to-end, then merges the agent providers on top of the built-ins exactly
    // as `run_prompt` does.
    #[tokio::test]
    async fn agent_subprocess_capability_tool_appears_in_registry() {
        let agents_root = temp_config_dir("agent-cap-bind");
        let agent_dir = agents_root.join("scraper");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("instructions.md"), "You scrape.\n").unwrap();
        let script = write_echo_plugin_script(&agent_dir);
        // The capability command is RELATIVE to the agent folder — the binder must
        // resolve it against `def.root` for the spawn to succeed.
        let agent_toml = format!(
            "description = \"scraper\"\n\n[[subprocess_capability]]\nname = \"scrape\"\ncommand = \"{}\"\n",
            script.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(agent_dir.join("agent.toml"), agent_toml).unwrap();

        let def = agentdir::resolve(&agents_root, "scraper").expect("agent resolves");
        assert_eq!(def.config.subprocess_capabilities.len(), 1);

        let providers =
            build_agent_capability_providers(&def.config.subprocess_capabilities, &def.root).await;
        assert_eq!(providers.len(), 1, "exactly one capability bound");

        // Layer the agent's providers on top of the built-ins, as run_prompt does.
        let mut all: Vec<Arc<dyn CapabilityProvider>> = vec![Arc::new(BuiltinProvider::new())];
        all.extend(providers);
        let registry = CapabilityRegistry::new(all);
        let tools = registry.tools_for_session(&SessionContext::default()).await;

        let echo = tools
            .iter()
            .find(|t| t.name() == "plugin__scrape__echo")
            .expect("agent capability tool namespaced into registry");
        assert!(
            echo.requires_permission(),
            "capability tools must be permission-gated"
        );
        assert!(
            tools.iter().any(|t| t.name() == "bash"),
            "built-ins still present alongside the capability"
        );

        let _ = std::fs::remove_dir_all(agents_root);
    }

    // A2 fail-soft — a capability whose command can't spawn is logged and skipped;
    // the turn still resolves with the FULL built-in toolset intact. This is the
    // guarantee that a broken agent.toml capability never kills a turn. Mirrors the
    // model-honoring fail-soft posture (a bad agent model falls back, never fails).
    #[tokio::test]
    async fn broken_agent_capability_is_skipped_turn_still_resolves() {
        let agents_root = temp_config_dir("agent-cap-broken");
        let agent_dir = agents_root.join("broken");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("instructions.md"), "You do stuff.\n").unwrap();
        // A command that does not exist anywhere → spawn fails at launch.
        std::fs::write(
            agent_dir.join("agent.toml"),
            "description = \"broken\"\n\n[[subprocess_capability]]\nname = \"nope\"\ncommand = \"./definitely-not-a-real-binary-xyz\"\n",
        )
        .unwrap();

        let def = agentdir::resolve(&agents_root, "broken").expect("agent resolves");
        let providers =
            build_agent_capability_providers(&def.config.subprocess_capabilities, &def.root).await;
        assert!(
            providers.is_empty(),
            "a capability that can't spawn is skipped, not bound"
        );

        // The turn's toolset is unaffected: built-ins remain the full set.
        let mut all: Vec<Arc<dyn CapabilityProvider>> = vec![Arc::new(BuiltinProvider::new())];
        all.extend(providers);
        let registry = CapabilityRegistry::new(all);
        let tools = registry.tools_for_session(&SessionContext::default()).await;
        assert!(
            tools.iter().any(|t| t.name() == "bash"),
            "built-ins intact after a broken capability is skipped"
        );

        let _ = std::fs::remove_dir_all(agents_root);
    }

    // Missing plugins dir → no plugins, no error, unchanged behavior.
    #[tokio::test]
    async fn missing_plugins_dir_yields_no_providers() {
        let config_dir = temp_config_dir("plugin-missing-dir");
        // config_dir intentionally not created.
        let providers = discover_plugin_providers(&config_dir).await;
        assert!(providers.is_empty(), "no plugins dir → no providers");
    }

    // An enabled `[offshore]` table registers the offshore provider and its ten
    // tools alongside the built-ins.
    #[tokio::test]
    async fn offshore_provider_registered_when_configured() {
        let config_dir = temp_config_dir("offshore-enabled");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "http://100.90.205.60:4780"
            ssh_host = "smathdaddy@100.90.205.60"
            "#,
        )
        .unwrap();

        let registry = build_capability_registry(&config_dir, None).await;
        assert!(
            registry.providers().iter().any(|p| p.id() == "offshore"),
            "offshore provider registered"
        );
        let tools = registry.tools_for_session(&SessionContext::default()).await;
        let offshore: Vec<_> = tools
            .iter()
            .filter(|t| t.name().starts_with("offshore_"))
            .collect();
        assert_eq!(offshore.len(), 10, "the full offshore family is offered");
        assert!(
            tools.iter().any(|t| t.name() == "offshore_dispatch"),
            "dispatch present"
        );
        assert!(
            tools.iter().any(|t| t.name() == "bash"),
            "built-ins still present"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    // No `[offshore]` table — or a disabled one — registers nothing: zero
    // behavior change for unconfigured daemons.
    #[tokio::test]
    async fn offshore_provider_absent_without_config_or_when_disabled() {
        let no_table = temp_config_dir("offshore-absent");
        std::fs::create_dir_all(&no_table).unwrap();
        let registry = build_capability_registry(&no_table, None).await;
        assert!(
            !registry.providers().iter().any(|p| p.id() == "offshore"),
            "no table → no offshore provider"
        );
        let _ = std::fs::remove_dir_all(no_table);

        let disabled = temp_config_dir("offshore-disabled");
        std::fs::create_dir_all(&disabled).unwrap();
        std::fs::write(
            disabled.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "http://100.90.205.60:4780"
            ssh_host = "smathdaddy@100.90.205.60"
            enabled = false
            "#,
        )
        .unwrap();
        let registry = build_capability_registry(&disabled, None).await;
        assert!(
            !registry.providers().iter().any(|p| p.id() == "offshore"),
            "enabled = false → no offshore provider"
        );
        let _ = std::fs::remove_dir_all(disabled);
    }

    // A plugin whose manifest can't parse is skipped, never breaking discovery
    // of the valid plugin beside it.
    #[tokio::test]
    async fn bad_manifest_is_skipped_not_fatal() {
        let config_dir = temp_config_dir("plugin-bad-manifest");
        let plugins = config_dir.join(PLUGINS_DIRNAME);

        // A broken plugin: manifest is not valid TOML / missing required fields.
        let broken = plugins.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("plugin.toml"), "this is not = valid [ toml").unwrap();

        // A good plugin alongside it.
        let good = plugins.join("echo-pack");
        std::fs::create_dir_all(&good).unwrap();
        let script = write_echo_plugin_script(&good);
        let manifest = format!(
            "name = \"echo-pack\"\nversion = \"0.1.0\"\nentry = \"{}\"\n\n[[tool]]\nname = \"echo\"\n",
            script.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(good.join("plugin.toml"), manifest).unwrap();

        let providers = discover_plugin_providers(&config_dir).await;
        assert_eq!(providers.len(), 1, "broken plugin skipped, good one loaded");

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn cap_session_history_keeps_most_recent_within_limit() {
        let msgs: Vec<Message> = (0..MAX_SESSION_MESSAGES + 50)
            .map(|i| Message::user_text(format!("m{i}")))
            .collect();
        let capped = cap_session_history(msgs);
        assert!(capped.len() <= MAX_SESSION_MESSAGES);
        // Oldest dropped, newest retained.
        assert!(matches!(
            capped.last(),
            Some(Message::Assistant(_)) | Some(Message::User { .. })
        ));
        if let Some(Message::User { content, .. }) = capped.last() {
            let want = format!("m{}", MAX_SESSION_MESSAGES + 50 - 1);
            assert!(content.iter().any(|c| c.as_text() == Some(want.as_str())));
        }
    }

    #[test]
    fn cap_session_history_drops_leading_orphan_tool_results() {
        use ocean_protocol::ToolResultMessage;
        // Build an over-limit history where the trim boundary lands on a
        // tool-result; the kept slice must not begin with one.
        let mut msgs: Vec<Message> = Vec::new();
        for _ in 0..MAX_SESSION_MESSAGES + 5 {
            msgs.push(Message::user_text("u"));
        }
        // Force the message right after the drop boundary to be a tool result.
        let boundary = msgs.len() - MAX_SESSION_MESSAGES;
        msgs[boundary] = Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            content: vec![Content::text("orphan")],
            is_error: false,
            timestamp: 0,
        });
        let capped = cap_session_history(msgs);
        assert!(!matches!(capped.first(), Some(Message::ToolResult(_))));
    }

    // OCEAN-85: the orphan-trim drops a *run* of leading tool-results, not just
    // one. If a turn ended mid-tool-call and the trim boundary lands inside a
    // back-to-back batch of tool-results whose originating ToolCall was dropped,
    // every one of them is an orphan a provider would reject on replay — the
    // `while` loop must peel them all.
    #[test]
    fn cap_session_history_drops_a_run_of_leading_orphan_tool_results() {
        use ocean_protocol::ToolResultMessage;
        let orphan = |id: &str| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                content: vec![Content::text("orphan")],
                is_error: false,
                timestamp: 0,
            })
        };

        let mut msgs: Vec<Message> = Vec::new();
        for _ in 0..MAX_SESSION_MESSAGES + 6 {
            msgs.push(Message::user_text("u"));
        }
        // Three consecutive tool-results straddling the boundary, then real
        // content. After trimming to MAX, the kept head begins with the run.
        let boundary = msgs.len() - MAX_SESSION_MESSAGES;
        msgs[boundary] = orphan("c1");
        msgs[boundary + 1] = orphan("c2");
        msgs[boundary + 2] = orphan("c3");

        let capped = cap_session_history(msgs);
        // The whole run is peeled — the head is NOT a tool-result...
        assert!(
            !matches!(capped.first(), Some(Message::ToolResult(_))),
            "leading orphan run not fully dropped"
        );
        // ...and no orphan tool-result remains as the very first message.
        assert!(matches!(capped.first(), Some(Message::User { .. })));
    }

    // OCEAN-85: trimming must not over-drop. A clean conversation that lands
    // exactly on the cap, or just over it with no orphan at the boundary, keeps
    // a non-tool-result head and stays within the bound.
    #[test]
    fn cap_session_history_keeps_clean_head_when_boundary_is_not_an_orphan() {
        use ocean_protocol::{AssistantMessage, StopReason, Usage};
        let assistant = || {
            Message::Assistant(AssistantMessage {
                content: vec![Content::text("ok")],
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-ok".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            })
        };
        // Alternating user/assistant, a few over the cap. No tool-results at all,
        // so the boundary message is a normal turn — nothing should be peeled
        // beyond the plain drain.
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..MAX_SESSION_MESSAGES + 8 {
            if i % 2 == 0 {
                msgs.push(Message::user_text(format!("u{i}")));
            } else {
                msgs.push(assistant());
            }
        }
        let capped = cap_session_history(msgs);
        assert_eq!(
            capped.len(),
            MAX_SESSION_MESSAGES,
            "clean history should trim to exactly the cap, no over-drop"
        );
        assert!(
            !matches!(capped.first(), Some(Message::ToolResult(_))),
            "clean head must not be a tool-result"
        );
    }

    fn provider_config(provider: ProviderId, model: &str, credential: bool) -> ProviderConfig {
        ProviderConfig {
            selection: ocean_providers::ModelSelection {
                provider,
                model: model.to_string(),
                base_url: "fake://local".to_string(),
                context_window: 1_000,
                max_output_tokens: 1_000,
            },
            credential: credential.then(|| ocean_providers::ResolvedCredential {
                secret: ocean_providers::SecretString::new("test-secret").unwrap(),
                source: ocean_providers::CredentialSource::Env {
                    name: "OCEAN_TEST_API_KEY".into(),
                },
                kind: ocean_providers::CredentialKind::ApiKey,
            }),
            account_id: None,
        }
    }

    fn runtime(config_dir: PathBuf, provider_config: ProviderConfig) -> AgentRuntime {
        runtime_with_env(config_dir, provider_config, None)
    }

    /// Like [`runtime`] but injects a deterministic [`ProviderEnv`] used by the
    /// failover decision (OCEAN-275), so `prompt`-level failover can be tested
    /// without touching the global process environment. `None` ⇒ falls back to
    /// the real process env (same as production).
    fn runtime_with_env(
        config_dir: PathBuf,
        provider_config: ProviderConfig,
        test_env: Option<ProviderEnv>,
    ) -> AgentRuntime {
        let state = state_from_provider_config(provider_config).unwrap();
        AgentRuntime {
            config_dir,
            state: std::sync::Arc::new(std::sync::RwLock::new(state)),
            capabilities: Arc::new(CapabilityRegistry::builtin_only()),
            session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            hooks: ocean_hooks::HooksConfig::default(),
            test_env,
            test_compact_provider: None,
        }
    }

    #[test]
    fn glm_provider_config_maps_to_openai_compat_model() {
        // GLM (Zhipu) is an OpenAI-compatible chat-completions endpoint: the
        // resolved Model must carry provider "glm", the openai-completions api,
        // the selected model id, and preserve the selection's base URL and
        // token limits verbatim.
        let config = provider_config(ProviderId::Glm, "glm-4.6", true);
        let model = model_from_provider_config(&config).expect("glm model resolves");

        assert_eq!(model.provider, "glm");
        assert_eq!(model.api, "openai-completions");
        assert_eq!(model.id, "glm-4.6");
        assert_eq!(model.base_url, "fake://local");
        assert_eq!(model.context_window, 1_000);
        assert_eq!(model.max_tokens, 1_000);
    }

    #[test]
    fn claude_code_sonnet_alias_maps_to_anthropic_messages_model() {
        // Claude Code plan OAuth speaks the Anthropic Messages wire protocol;
        // the public picker alias must resolve to the real Anthropic model id
        // so ocean-agent never sends "claude-code-sonnet-4-6" on the wire.
        let config = provider_config(ProviderId::ClaudeCode, "claude-code-sonnet-4-6", true);
        let model = model_from_provider_config(&config).expect("claude-code sonnet alias resolves");

        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.api, "anthropic-messages");
        assert_eq!(model.id, "claude-sonnet-4-6");
    }

    #[test]
    fn claude_code_opus_alias_maps_to_anthropic_messages_model() {
        let config = provider_config(ProviderId::ClaudeCode, "claude-code-opus-4-7", true);
        let model = model_from_provider_config(&config).expect("claude-code opus alias resolves");

        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.api, "anthropic-messages");
        assert_eq!(model.id, "claude-opus-4-7");
    }

    #[test]
    fn auth_method_follows_resolved_credential_kind() {
        use ocean_protocol::types::AuthMethod;

        // OAuth bearer credential (Codex / Claude Code) → Bearer.
        let bearer = ProviderConfig {
            selection: ocean_providers::ModelSelection {
                provider: ProviderId::OpenAiCodex,
                model: "gpt-5.4".into(),
                base_url: "fake://local".into(),
                context_window: 1_000,
                max_output_tokens: 1_000,
            },
            credential: Some(ocean_providers::ResolvedCredential {
                secret: ocean_providers::SecretString::new("oauth-access-token").unwrap(),
                source: ocean_providers::CredentialSource::OceanAuthFile {
                    path: "auth.json".into(),
                },
                kind: ocean_providers::CredentialKind::OAuthBearer,
            }),
            account_id: None,
        };
        assert_eq!(auth_method_for(&bearer), AuthMethod::Bearer);

        // Regular env API-key credential → ApiKey (default wire shape).
        let api_key = provider_config(ProviderId::DeepSeek, "deepseek-chat", true);
        assert_eq!(auth_method_for(&api_key), AuthMethod::ApiKey);

        // No credential at all (keyless fake provider) → ApiKey default, never
        // Bearer: a missing token must not flip the wire convention.
        let none = provider_config(ProviderId::Fake, "fake-ok", false);
        assert_eq!(auth_method_for(&none), AuthMethod::ApiKey);
    }

    #[test]
    fn create_session_mints_empty_workspace_bound_session_without_a_prompt() {
        let config_dir = temp_config_dir("create-session");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        // No prompt is supplied — the container is created on its own.
        let (id, cwd, client_type) = runtime
            .create_session(".", Some("surface-web".into()))
            .unwrap();
        assert_eq!(cwd, ".");
        assert_eq!(client_type.as_deref(), Some("surface-web"));

        // It is persisted and resumable, with zero turns and the surface tagged.
        let detail = runtime.session_detail(id).unwrap();
        assert_eq!(detail.turns, 0);
        assert!(detail.resumable);
        assert_eq!(runtime.list_sessions(None).unwrap().len(), 1);

        // Each create mints a fresh id (matches the implicit create-on-turn path).
        let (id2, _, _) = runtime.create_session(".", None).unwrap();
        assert_ne!(id, id2);
        assert_eq!(runtime.list_sessions(None).unwrap().len(), 2);

        // An empty cwd has nothing to bind to and is rejected.
        assert!(runtime.create_session("   ", None).is_err());

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn strips_assistant_thinking_for_deepseek_reasoner_history_replay() {
        let mut messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "private chain of thought".into(),
                    thinking_signature: None,
                },
                Content::text("visible answer"),
            ],
            api: "chat".into(),
            provider: "deepseek".into(),
            model: "deepseek-reasoner".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 1,
        })];

        strip_assistant_thinking_content(&mut messages);

        let Message::Assistant(assistant) = &messages[0] else {
            panic!("expected assistant message");
        };
        assert_eq!(assistant.content, vec![Content::text("visible answer")]);
    }

    #[test]
    fn history_shaping_preserves_thinking_only_for_exact_kimi_k3_route() {
        assert!(!should_strip_assistant_thinking(
            &ProviderId::Kimi,
            "kimi-k3"
        ));
        assert!(should_strip_assistant_thinking(
            &ProviderId::Kimi,
            "kimi-k2.6"
        ));
        assert!(should_strip_assistant_thinking(
            &ProviderId::OpenAi,
            "kimi-k3"
        ));
        assert!(should_strip_assistant_thinking(
            &ProviderId::OpenAiCompatible,
            "kimi-k3"
        ));
    }

    #[tokio::test]
    async fn dropping_parent_owner_aborts_spawned_agent_task() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let owner = AbortOnDropJoinHandle::new(tokio::spawn(async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        }));
        started_rx.await.expect("child reached its running state");

        drop(owner);
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("aborted child dropped promptly")
            .expect("drop signal delivered");
    }

    fn lock_runtime(name: &str) -> AgentRuntime {
        runtime(
            temp_config_dir(name),
            provider_config(ProviderId::Fake, "fake-ok", false),
        )
    }

    // OCEAN-182 (Fix 1): a panic inside a guarded section poisons the std Mutex
    // wrapping the registry. `session_lock` must recover via `into_inner()`
    // instead of `.expect()` — otherwise one panicked turn wedges EVERY future
    // turn on EVERY session for the life of the process.
    #[test]
    fn session_lock_survives_a_poisoned_registry() {
        let rt = lock_runtime("lock-poison");

        // Poison the std Mutex: panic while holding its guard, in a scoped
        // thread so the panic doesn't unwind the test itself.
        let locks = rt.session_locks.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = locks.lock().unwrap();
            panic!("poison the registry while holding the lock");
        }));
        assert!(res.is_err(), "the scoped panic should have been caught");
        assert!(
            rt.session_locks.lock().is_err(),
            "registry mutex must now be poisoned"
        );

        // Despite the poison, handing out a session lock still works.
        let id = SessionId::new_v4();
        let lock = rt.session_lock(id);
        assert_eq!(
            Arc::strong_count(&lock),
            2,
            "the returned clone (1) + the map entry (1) == 2"
        );
    }

    // Voice phases 2/3: the handoff append must persist through a real
    // save→load round-trip and must report a missing session as `false`
    // (the daemon maps that to 404) instead of minting a phantom file.
    #[tokio::test]
    async fn append_session_message_persists_and_404s_unknown_ids() {
        let rt = lock_runtime("handoff-append");
        let cwd = temp_config_dir("handoff-append-ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let (id, _, _) = rt
            .create_session(cwd.to_str().unwrap(), None)
            .expect("create session");

        assert!(rt
            .append_session_message(id, "[voice handoff] fix the header".into())
            .await
            .expect("append should succeed"));
        let detail = rt.session_detail(id).expect("reload session");
        let last = detail.transcript.last().expect("one transcript entry");
        assert_eq!(last.role, "user");
        assert_eq!(last.text, "[voice handoff] fix the header");

        assert!(
            !rt.append_session_message(SessionId::new_v4(), "ghost".into())
                .await
                .expect("unknown id is Ok(false), not an error"),
            "an unknown session must not be created by an append"
        );
    }

    // OCEAN-182 (Fix 2): idle entries (no in-flight turn holding a clone) must
    // be pruned so the registry doesn't grow unbounded on a long-lived daemon —
    // while an ACTIVELY-held lock is never pruned out from under its turn.
    #[test]
    fn session_lock_prunes_idle_entries_but_keeps_held_ones() {
        let rt = lock_runtime("lock-prune");

        // Hold several sessions' locks SIMULTANEOUSLY (clones kept alive), so
        // every entry is live (strong_count >= 2) across the touches and none
        // is pruned yet.
        let mut held: Vec<Arc<tokio::sync::Mutex<()>>> = Vec::new();
        let mut idle_ids = Vec::new();
        for _ in 0..5 {
            let id = SessionId::new_v4();
            idle_ids.push(id);
            held.push(rt.session_lock(id));
        }
        assert_eq!(
            rt.session_lock_count(),
            5,
            "all 5 entries are live while their clones are held"
        );

        // Drop every clone: now all 5 entries are idle (strong_count == 1).
        drop(held);

        // An actively-held session: keep its clone alive across the prune.
        let held_id = SessionId::new_v4();
        let _held = rt.session_lock(held_id); // strong_count == 2 (map + this clone)

        // Requesting a brand-new id triggers the prune. The 5 now-idle entries
        // are dropped; the held one survives; the new one is inserted.
        let new_id = SessionId::new_v4();
        let _new = rt.session_lock(new_id);

        for id in &idle_ids {
            assert!(
                !rt.session_locks
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(id),
                "idle session {id} must have been pruned"
            );
        }
        assert_eq!(
            rt.session_lock_count(),
            2,
            "only the actively-held session and the freshly-requested one remain"
        );
        assert!(
            rt.session_locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&held_id),
            "an actively-held lock must NOT be pruned"
        );
        assert!(
            rt.session_locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&new_id),
            "the just-requested lock must be present"
        );
    }

    #[test]
    fn load_resumable_returns_none_for_unknown_session() {
        let config_dir = temp_config_dir("resumable-none");
        let result = session::load_resumable(&config_dir, SessionId::new_v4()).unwrap();
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn load_resumable_errors_on_corrupt_session_instead_of_wiping_it() {
        let config_dir = temp_config_dir("resumable-corrupt");
        let id = SessionId::new_v4();
        let bucket = session::sessions_dir(&config_dir).join("legacy");
        std::fs::create_dir_all(&bucket).unwrap();
        // Simulate a partial/corrupt write of an existing transcript.
        std::fs::write(bucket.join(format!("{id}.json")), b"{ not valid json").unwrap();

        let result = session::load_resumable(&config_dir, id);
        assert!(
            result.is_err(),
            "corrupt session must error, not silently resolve to an empty session"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn save_then_load_resumable_roundtrips_messages() {
        let config_dir = temp_config_dir("resumable-roundtrip");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let mut s = session::Session::new(&model);
        s.bind_workspace(Path::new("."));
        s.replace_messages(vec![Message::user_text("remember me")]);
        let id = s.id;
        session::save(&config_dir, &s).unwrap();

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 1);
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn session_persists_and_roundtrips_client_type() {
        let config_dir = temp_config_dir("client-type-roundtrip");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let mut s = session::Session::new(&model);
        s.bind_workspace(Path::new("."));
        s.client_type = Some("surface-tauri".into());
        let id = s.id;
        session::save(&config_dir, &s).unwrap();

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(
            loaded.client_type.as_deref(),
            Some("surface-tauri"),
            "the session must remember which surface it was bound to so a \
             surface switch can be detected on the next turn"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn legacy_session_without_client_type_loads_as_none() {
        // A session file written before the client_type field existed must
        // still deserialize (serde default), not error.
        let config_dir = temp_config_dir("client-type-legacy");
        let id = SessionId::new_v4();
        let bucket = session::sessions_dir(&config_dir).join("legacy");
        std::fs::create_dir_all(&bucket).unwrap();
        let legacy = format!(
            r#"{{"id":"{id}","created_ms":0,"updated_ms":0,"model":"m","provider":"p","messages":[]}}"#
        );
        std::fs::write(bucket.join(format!("{id}.json")), legacy).unwrap();

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert!(loaded.client_type.is_none());
        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// Test helper: every on-disk `<id>.json` across all workspace buckets
    /// plus the top-level sessions dir. Mirrors the loader's candidate walk so
    /// tests can assert the "one id == one file" invariant directly.
    fn session_files_for(config_dir: &Path, id: SessionId) -> Vec<PathBuf> {
        let name = format!("{id}.json");
        let root = session::sessions_dir(config_dir);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for e in entries.flatten() {
                let p = e.path().join(&name);
                if p.exists() {
                    out.push(p);
                }
            }
        }
        let top = root.join(&name);
        if top.exists() {
            out.push(top);
        }
        out
    }

    /// Split-brain repro: one session id persisted as two files in different
    /// workspace buckets (an empty older stub + the real newer transcript).
    /// Loading by id must deterministically return the message-bearing copy
    /// and self-heal down to a single file. Under the old loader this was
    /// nondeterministic — `read_dir` order decided whether the empty stub or
    /// the real history won, flapping between "fresh session" and the truth
    /// mid-conversation with no daemon restart.
    #[test]
    fn load_resumable_resolves_split_brain_to_the_newest_message_bearing_file() {
        let config_dir = temp_config_dir("split-brain-load");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let id = SessionId::new_v4();

        // Empty stub in bucket A — older.
        let bucket_a = session::sessions_dir(&config_dir)
            .join(session::workspace_slug(Path::new("/fake/ws-split-a")));
        std::fs::create_dir_all(&bucket_a).unwrap();
        let mut stub = session::Session::new_with_id(id, &model);
        stub.workspace_root = Some("/fake/ws-split-a".into());
        stub.updated_ms = 1000;
        std::fs::write(
            bucket_a.join(format!("{id}.json")),
            serde_json::to_string(&stub).unwrap(),
        )
        .unwrap();

        // Real transcript in bucket B — newer, one message.
        let bucket_b = session::sessions_dir(&config_dir)
            .join(session::workspace_slug(Path::new("/fake/ws-split-b")));
        std::fs::create_dir_all(&bucket_b).unwrap();
        let mut real = session::Session::new_with_id(id, &model);
        real.workspace_root = Some("/fake/ws-split-b".into());
        real.replace_messages(vec![Message::user_text("the real first turn")]);
        real.updated_ms = 5000;
        std::fs::write(
            bucket_b.join(format!("{id}.json")),
            serde_json::to_string(&real).unwrap(),
        )
        .unwrap();

        assert_eq!(
            session_files_for(&config_dir, id).len(),
            2,
            "test setup: two files for one id must exist"
        );

        let loaded = session::load_resumable(&config_dir, id)
            .expect("load must not error")
            .expect("session must be found");
        assert_eq!(
            loaded.messages.len(),
            1,
            "split-brain load must return the message-bearing copy, not the empty stub"
        );
        assert_eq!(loaded.updated_ms, 5000);

        // Self-heal: the loser must be gone so the next load is deterministic.
        let remaining = session_files_for(&config_dir, id);
        assert_eq!(
            remaining.len(),
            1,
            "self-heal must collapse duplicate files to one; got {remaining:?}"
        );
        assert_eq!(
            remaining[0],
            bucket_b.join(format!("{id}.json")),
            "the newest, message-bearing copy must be the survivor"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// save() must write the canonical bucket AND purge any stale `<id>.json`
    /// left in another bucket — the orphan from a pre-fix rebind. One id must
    /// resolve to exactly one loadable file after every save.
    #[test]
    fn save_removes_orphaned_duplicate_session_files_across_buckets() {
        let config_dir = temp_config_dir("split-brain-save-collapses");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let id = SessionId::new_v4();

        // Seed an orphan in bucket A (as a prior rebind would have left).
        let bucket_a = session::sessions_dir(&config_dir)
            .join(session::workspace_slug(Path::new("/fake/ws-save-a")));
        let bucket_b = session::sessions_dir(&config_dir)
            .join(session::workspace_slug(Path::new("/fake/ws-save-b")));
        std::fs::create_dir_all(&bucket_a).unwrap();
        std::fs::create_dir_all(&bucket_b).unwrap();
        let mut stale = session::Session::new_with_id(id, &model);
        stale.workspace_root = Some("/fake/ws-save-a".into());
        stale.updated_ms = 1000;
        std::fs::write(
            bucket_a.join(format!("{id}.json")),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();

        // Save the canonical session bound to bucket B.
        let mut canonical = session::Session::new_with_id(id, &model);
        canonical.workspace_root = Some("/fake/ws-save-b".into());
        canonical.replace_messages(vec![Message::user_text("canonical history")]);
        let written = session::save(&config_dir, &canonical).unwrap();
        assert_eq!(written, bucket_b.join(format!("{id}.json")));

        let remaining = session_files_for(&config_dir, id);
        assert_eq!(
            remaining.len(),
            1,
            "save must leave exactly one file for the id; got {remaining:?}"
        );

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(
            loaded.messages.len(),
            1,
            "the surviving file must be the canonical one"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// The original split-brain mechanism: `bind_workspace` rebinds to a new
    /// workspace_root, then save() wrote the new bucket but left the OLD
    /// bucket's file behind. Verify rebind + save moves the file: old bucket
    /// empty, new bucket holds it, full history intact.
    #[test]
    fn bind_workspace_rebind_moves_session_file_without_leaving_orphan() {
        let config_dir = temp_config_dir("rebind-moves-file");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let id = SessionId::new_v4();

        // First bind + save in workspace A.
        let mut s = session::Session::new_with_id(id, &model);
        s.workspace_root = Some("/fake/ws-rebind-a".into());
        s.replace_messages(vec![Message::user_text("turn one")]);
        session::save(&config_dir, &s).unwrap();
        let bucket_a = session::sessions_dir(&config_dir)
            .join(session::workspace_slug(Path::new("/fake/ws-rebind-a")));
        assert!(
            bucket_a.join(format!("{id}.json")).exists(),
            "pre-rebind file must exist in bucket A"
        );

        // Rebind to workspace B (mimics `cd /project-b && ocean --resume <id>`).
        s.workspace_root = Some("/fake/ws-rebind-b".into());
        let mut msgs = s.messages.clone();
        msgs.push(Message::user_text("turn two"));
        s.replace_messages(msgs);
        session::save(&config_dir, &s).unwrap();

        assert!(
            !bucket_a.join(format!("{id}.json")).exists(),
            "rebind must not leave an orphan in the old bucket"
        );

        let remaining = session_files_for(&config_dir, id);
        assert_eq!(
            remaining.len(),
            1,
            "exactly one file after rebind; got {remaining:?}"
        );

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(
            loaded.messages.len(),
            2,
            "history must survive the rebind move intact"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// Happy path untouched: repeated saves to the same workspace keep exactly
    /// one file and load the latest content. Guards against the cleanup logic
    /// accidentally nuking the in-bucket file on a normal save.
    #[test]
    fn save_in_a_single_bucket_is_idempotent_and_leaves_one_file() {
        let config_dir = temp_config_dir("single-bucket-untouched");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let id = SessionId::new_v4();

        let mut s = session::Session::new_with_id(id, &model);
        s.workspace_root = Some("/fake/ws-single".into());
        s.replace_messages(vec![Message::user_text("first")]);
        session::save(&config_dir, &s).unwrap();
        assert_eq!(session_files_for(&config_dir, id).len(), 1);

        let mut msgs = s.messages.clone();
        msgs.push(Message::user_text("second"));
        s.replace_messages(msgs);
        session::save(&config_dir, &s).unwrap();
        assert_eq!(
            session_files_for(&config_dir, id).len(),
            1,
            "same-bucket re-save must not create a duplicate"
        );

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn missing_credential_preflight_names_ocean_provider_and_model() {
        let config_dir = temp_config_dir("missing-credential");
        // Inject an EMPTY environment so provider failover (OCEAN-275) finds no
        // ready alternate — the deterministic all-degraded path. Without this the
        // turn could reroute to whatever real provider credential happens to be in
        // the test process env, making the assertion non-hermetic.
        let runtime = runtime_with_env(
            config_dir.clone(),
            provider_config(ProviderId::DeepSeek, "deepseek-v4-pro", false),
            Some(ProviderEnv::default()),
        );

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "hello".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;

        // The turn fails clearly, and the message still names the degraded
        // provider+model (the original contract) while now also flagging the
        // all-degraded condition (no ready fallback).
        assert!(!res.ok);
        assert!(res.stderr.contains("all providers degraded"));
        assert!(res.stderr.contains("provider deepseek"));
        assert!(res.stderr.contains("deepseek-v4-pro"));
        assert!(!res.stderr.contains("provider openai"));
        assert!(runtime.list_sessions(None).unwrap().is_empty());
        let missing = runtime.session_detail(SessionId::new_v4()).unwrap_err();
        assert!(missing.to_string().contains("not found"));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    // OCEAN-275 end-to-end: a turn whose PRIMARY provider is degraded is routed
    // through `prompt()` to a ready alternate and SUCCEEDS, rather than failing.
    /// Stop hooks (the ocean-hooks seam): a configured `[[hooks.Stop]]` command
    /// that blocks the stop must continue the session with the hook's reason as
    /// the next user message, and a hook honoring `stop_hook_active: true`
    /// (blocking only the first firing) must yield exactly ONE continuation —
    /// proving the flag is passed on re-entry and the loop terminates without
    /// hitting the hard bound. Runs on the no-network fake provider.
    #[tokio::test]
    async fn stop_hook_block_runs_one_continuation_turn_then_stops() {
        let config_dir = temp_config_dir("stop-hook-continuation");
        std::fs::create_dir_all(&config_dir).unwrap();

        // A hook that blocks the FIRST stop and honors stop_hook_active on the
        // continuation's stop (the stitchpad-style self-limit).
        let hook_path = config_dir.join("stop-hook-test.sh");
        std::fs::write(
            &hook_path,
            "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n  *'\"stop_hook_active\":true'*) exit 0 ;;\nesac\nprintf '{\"decision\":\"block\",\"reason\":\"hook continuation ping\"}\\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        runtime.hooks = ocean_hooks::HooksConfig {
            stop: vec![ocean_hooks::HookCommand {
                command: hook_path.display().to_string(),
                args: vec![],
                timeout_secs: 10,
                enabled: true,
            }],
        };

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "hello".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;

        assert!(res.ok, "turn should succeed, got stderr: {}", res.stderr);
        // Exactly two fake echoes: the operator turn + ONE hook continuation.
        assert_eq!(
            res.stdout.matches("OCEAN_FAKE_OK").count(),
            2,
            "expected exactly one stop-hook continuation, stdout: {}",
            res.stdout
        );
        // The continuation's user message is the hook's reason, persisted on the
        // same session transcript.
        let session_id = res.session_id.expect("session id");
        let session = session::load_resumable(&config_dir, session_id)
            .unwrap()
            .expect("session persisted");
        let transcript = serde_json::to_string(&session.messages).unwrap();
        assert!(
            transcript.contains("hook continuation ping"),
            "hook reason must land on the transcript, got: {transcript}"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// TASK-40 (prompt path / `POST /v1/prompt` + `POST /v1/requests`): the daemon
    /// prepends the Longhouse advisory to `req.prompt` before the runtime persists
    /// the turn, but threads the ORIGINAL prompt through `PromptControl` as the
    /// display title. The persisted session `title` must be the original — not the
    /// injected boilerplate that made every session share one label — while the
    /// stored first message still carries the composed prompt the model saw.
    #[tokio::test]
    async fn session_title_derives_from_display_title_not_composed_prompt() {
        let config_dir = temp_config_dir("title-from-display-title");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        // Shape of the daemon-composed prompt: the Longhouse advisory block, a
        // blank line, then the user's actual words.
        let composed = "Longhouse consult (advisory — relevant skills/SOPs for this turn):\n\
                        - some-skill\n\nfix the flaky parser test";
        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: composed.into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false)
                    .with_display_title(Some("fix the flaky parser test".into())),
            )
            .await;
        assert!(res.ok, "fake turn should succeed: {}", res.stderr);
        let session_id = res.session_id.expect("session id");
        let session = session::load_resumable(&config_dir, session_id)
            .unwrap()
            .expect("session persisted");
        assert_eq!(
            session.title.as_deref(),
            Some("fix the flaky parser test"),
            "title must derive from the display title, not the composed prompt"
        );
        // History is untouched: the model still saw the composed prompt.
        let transcript = serde_json::to_string(&session.messages).unwrap();
        assert!(
            transcript.contains("Longhouse consult (advisory"),
            "persisted first message must keep the composed prompt"
        );
        // The read-side label (what the switcher renders) matches the title.
        let detail = runtime.session_detail(session_id).unwrap();
        assert_eq!(detail.title, "fix the flaky parser test");
        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// TASK-40 (agent_turn path / `POST /v1/agent/turns`): the agent-turn handler
    /// composes room/operator guidance AND the Longhouse advisory into the
    /// persisted prompt, yet threads the ORIGINAL prompt as the display title
    /// (`with_display_title(Some(prompt.clone()))`, captured before any layer).
    /// Same runtime seam as the prompt path — this proves the title survives ANY
    /// stacked prepend layer, so guidance layering can't pollute it either.
    #[tokio::test]
    async fn session_title_survives_guidance_and_longhouse_layers() {
        let config_dir = temp_config_dir("title-survives-layers");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let composed = "Room guidance: be terse.\n\n\
                        Longhouse consult (advisory — skills):\n- some-skill\n\nland the migration";
        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: composed.into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: Some("tui".into()),
                    decision_token: None,
                },
                PromptControl::yolo(false).with_display_title(Some("land the migration".into())),
            )
            .await;
        assert!(res.ok, "fake turn should succeed: {}", res.stderr);
        let session_id = res.session_id.expect("session id");
        let session = session::load_resumable(&config_dir, session_id)
            .unwrap()
            .expect("session persisted");
        assert_eq!(
            session.title.as_deref(),
            Some("land the migration"),
            "title must survive guidance + Longhouse layering"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    // The injected env makes `fake-ok` the configured fallback — it's ready with
    // no credential and runs the no-network fake path, so this exercises the full
    // selection-time failover wiring deterministically.
    #[tokio::test]
    async fn prompt_fails_over_degraded_primary_to_ready_alternate_and_succeeds() {
        let config_dir = temp_config_dir("failover-success");
        let env = provider_env(&[("OCEAN_PROVIDER_FALLBACK", "fake-ok")]);
        let runtime = runtime_with_env(
            config_dir.clone(),
            // Primary deepseek with NO credential → degraded at selection.
            provider_config(ProviderId::DeepSeek, "deepseek-v4-pro", false),
            Some(env),
        );

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "hello".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;

        // Rerouted to the ready fake-ok alternate → the turn succeeds.
        assert!(
            res.ok,
            "degraded primary should fail over to the ready alternate, got stderr: {}",
            res.stderr
        );
        assert!(res.stdout.contains("OCEAN_FAKE_OK"));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    // OCEAN-36 / Codex: the per-turn readiness preflight must evaluate the
    // EFFECTIVE turn state, not a fixed global one. This is the unit the fix
    // hinges on — `prompt()` now resolves the override first and feeds the
    // resulting state to `preflight_error_for`, so a turn pinned to a ready
    // model is no longer rejected because some *other* (global) model is
    // degraded, and a degraded override is still caught.
    #[test]
    fn preflight_error_for_evaluates_the_given_state_not_a_global_one() {
        // A ready state (credential present) preflights clean...
        let ready = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            true,
        ))
        .unwrap();
        assert!(
            AgentRuntime::preflight_error_for(&ready).is_none(),
            "a credentialed state must pass preflight"
        );

        // ...while a degraded state (no credential) is reported against ITS own
        // provider/model, regardless of any global selection.
        let degraded = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            false,
        ))
        .unwrap();
        let err = AgentRuntime::preflight_error_for(&degraded)
            .expect("an uncredentialed state must fail preflight");
        assert!(err.contains("deepseek"));
        assert!(err.contains("deepseek-v4-pro"));
    }

    // ---- Provider failover wiring (OCEAN-275) -----------------------------

    /// Build a `ProviderEnv` from key/value pairs for deterministic failover
    /// tests (no process-env mutation).
    fn provider_env(vars: &[(&str, &str)]) -> ProviderEnv {
        ProviderEnv {
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    // A READY primary passes straight through selection-time failover untouched —
    // failover must never perturb the happy path.
    #[test]
    fn selection_failover_passes_a_ready_primary_through_unchanged() {
        let ready = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            true,
        ))
        .unwrap();
        let env = provider_env(&[("ANTHROPIC_API_KEY", "sk-ant")]);
        let out = AgentRuntime::resolve_turn_state_with_failover(ready.clone(), &env)
            .expect("ready primary must resolve");
        assert_eq!(
            out.provider_config.selection.provider,
            ProviderId::DeepSeek,
            "a ready primary must not be rerouted"
        );
        assert_eq!(out.provider_config.selection.model, "deepseek-v4-pro");
    }

    // A DEGRADED primary (no credential) with a ready alternate in the env routes
    // the turn to that alternate. This is the core OCEAN-275 behavior at the
    // agent-wiring level: degraded → routed to a ready provider, not failed.
    #[test]
    fn selection_failover_routes_degraded_primary_to_ready_alternate() {
        // Primary deepseek has no credential here (degraded), but a Claude Code
        // OAuth bearer is present — the default fallback order leads with
        // claude-sonnet-5 (now ProviderId::ClaudeCode).
        let degraded = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            false,
        ))
        .unwrap();
        let env = provider_env(&[("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer")]);
        let out = AgentRuntime::resolve_turn_state_with_failover(degraded, &env)
            .expect("a ready alternate must be selected");
        assert_eq!(
            out.provider_config.selection.provider,
            ProviderId::ClaudeCode,
            "degraded primary must route to the ready claude-code alternate"
        );
        assert!(
            AgentRuntime::preflight_error_for(&out).is_none(),
            "the chosen alternate must itself be ready"
        );
    }

    // A degraded primary with NO ready alternate anywhere yields a clear
    // "all providers degraded" error — never a silent hang or a bare
    // single-provider message.
    #[test]
    fn selection_failover_errors_clearly_when_all_providers_degraded() {
        let degraded = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            false,
        ))
        .unwrap();
        // No credentials for any alternate.
        let env = provider_env(&[]);
        let err = AgentRuntime::resolve_turn_state_with_failover(degraded, &env)
            .expect_err("no ready provider anywhere must be an error");
        assert!(
            err.contains("all providers degraded"),
            "error must name the all-degraded condition, got: {err}"
        );
        // It also names the override knob so the operator knows the lever.
        assert!(err.contains(ocean_providers::ENV_PROVIDER_FALLBACK));
    }

    // The env override steers WHICH alternate selection-time failover picks.
    #[test]
    fn selection_failover_honors_env_override_order() {
        let degraded = state_from_provider_config(provider_config(
            ProviderId::Google,
            "gemini-2.0-flash",
            false,
        ))
        .unwrap();
        // Both deepseek and anthropic are credentialed; override puts deepseek
        // first, so it must win over the default anthropic-first order.
        let env = provider_env(&[
            (
                "OCEAN_PROVIDER_FALLBACK",
                "deepseek-v4-pro, claude-opus-4-7",
            ),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("OCEAN_DEEPSEEK_API_KEY", "ds-secret"),
        ]);
        let out = AgentRuntime::resolve_turn_state_with_failover(degraded, &env).unwrap();
        assert_eq!(out.provider_config.selection.provider, ProviderId::DeepSeek);
    }

    // `failover_eligible`: a pre-stream availability failure (nothing streamed +
    // transient cause) IS eligible.
    #[test]
    fn failover_eligible_for_prestream_availability_error() {
        let err: anyhow::Error = TurnFailure {
            streamed_output: false,
            error: anyhow::Error::new(AgentError::Provider(ocean_protocol::Error::ProviderError {
                status: 503,
                body: "overloaded".into(),
            })),
        }
        .into();
        assert!(failover_eligible(&err));
    }

    // `failover_eligible`: once output streamed, the SAME availability error is
    // NOT eligible — this is the mid-stream safety gate that prevents replaying
    // tool side effects against a second provider.
    #[test]
    fn no_failover_after_output_streamed_even_on_availability_error() {
        let err: anyhow::Error = TurnFailure {
            streamed_output: true,
            error: anyhow::Error::new(AgentError::Provider(ocean_protocol::Error::ProviderError {
                status: 503,
                body: "overloaded".into(),
            })),
        }
        .into();
        assert!(
            !failover_eligible(&err),
            "a turn that already streamed must never fail over"
        );
    }

    // `failover_eligible`: a user/content error (4xx other than 429) is NOT
    // eligible even pre-stream — it would fail identically on any provider.
    #[test]
    fn no_failover_on_user_error() {
        let err: anyhow::Error = TurnFailure {
            streamed_output: false,
            error: anyhow::Error::new(AgentError::Provider(ocean_protocol::Error::ProviderError {
                status: 400,
                body: "bad request".into(),
            })),
        }
        .into();
        assert!(!failover_eligible(&err));
    }

    // `failover_eligible`: a plain error that isn't a `TurnFailure` (e.g. a
    // pre-stream session/config error) is conservatively NOT eligible.
    #[test]
    fn no_failover_for_non_turnfailure_errors() {
        let err = anyhow::anyhow!("session not found");
        assert!(!failover_eligible(&err));
    }

    // `unwrap_turn_failure` strips the wrapper so the existing AgentError
    // downcast (e.g. the 408 timeout mapping) keeps working.
    #[test]
    fn unwrap_turn_failure_recovers_the_inner_agent_error() {
        let wrapped: anyhow::Error = TurnFailure {
            streamed_output: false,
            error: anyhow::Error::new(AgentError::Timeout { secs: 300 }),
        }
        .into();
        let inner = unwrap_turn_failure(wrapped);
        assert!(
            matches!(
                inner.downcast_ref::<AgentError>(),
                Some(AgentError::Timeout { secs: 300 })
            ),
            "the inner AgentError::Timeout must survive unwrapping"
        );
        // A non-TurnFailure passes through untouched.
        let plain = anyhow::anyhow!("plain");
        assert_eq!(unwrap_turn_failure(plain).to_string(), "plain");
    }

    // The Fake dispatch must also key off the EFFECTIVE state. A Fake global
    // model with no credential still runs the fake path (no remote call), which
    // confirms the dispatch reads the resolved snapshot `prompt()` hands down.
    #[tokio::test]
    async fn fake_global_without_override_still_takes_fake_path() {
        let config_dir = temp_config_dir("fake-effective-dispatch");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "hi".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(res.ok, "fake provider should run without credentials");
        assert!(res.stdout.contains("OCEAN_FAKE_OK"));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn fallback_dispatch_reuses_durable_accepted_user_without_duplication() {
        let config_dir = temp_config_dir("fallback-accepted-user");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let snapshot = runtime.state.read().unwrap().clone();
        let session_id = SessionId::new_v4();
        let mut stored = session::Session::new_with_id(session_id, &snapshot.model);
        stored.bind_workspace(Path::new("."));
        stored.replace_messages(vec![Message::user_text("hello")]);
        session::save(&config_dir, &stored).expect("save accepted user checkpoint");

        runtime
            .run_fake_prompt(
                PromptRequest {
                    prompt: "hello".into(),
                    images: None,
                    request_id: None,
                    session_id: Some(session_id),
                    create_if_missing: false,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
                &snapshot,
                true,
            )
            .await
            .expect("fallback dispatch succeeds");

        let detail = runtime.session_detail(session_id).expect("session detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(
            detail
                .transcript
                .iter()
                .filter(|entry| entry.role == "user")
                .count(),
            1,
            "fallback must not append the accepted prompt twice"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn fake_provider_bypasses_remote_streaming_without_api_key() {
        let config_dir = temp_config_dir("fake-provider");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "Reply exactly: OCEAN_OK".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;

        assert!(res.ok);
        assert_eq!(res.stdout, "OCEAN_FAKE_OK\n");
        assert!(res.stderr.is_empty());
        assert_eq!(runtime.list_sessions(None).unwrap().len(), 1);

        let detail = runtime.session_detail(res.session_id.unwrap()).unwrap();
        assert_eq!(detail.state, SessionRunState::Stored);
        assert!(detail.resumable);
        assert_eq!(detail.turns, 2);
        assert_eq!(detail.transcript[0].role, "user");
        assert!(detail.transcript[0].text.contains("OCEAN_OK"));
        assert_eq!(detail.transcript[1].role, "assistant");
        assert_eq!(detail.transcript[1].text, "OCEAN_FAKE_OK");
        assert_eq!(detail.messages.len(), 2);
        let _ = std::fs::remove_dir_all(config_dir);
    }

    /// OCEAN-127: the fake provider must stream its assistant reply through the
    /// `event_sink` as an `AgentEvent::TextDelta`, the same channel the real
    /// provider uses. The daemon bridge turns this into
    /// `AssistantTextDelta` on the scoped `?session_id=` SSE stream, which is
    /// the only source the TUI/web transcript renders from. Before the fix the
    /// fake path emitted nothing, so a scoped subscriber saw only
    /// `turn_started` + `turn_finished` and the reply never rendered.
    #[tokio::test]
    async fn fake_provider_streams_assistant_text_delta_on_event_sink() {
        let config_dir = temp_config_dir("fake-delta");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let control = PromptControl::yolo(false).with_event_sink(tx);

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "Reply exactly: OCEAN_OK".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                control,
            )
            .await;

        assert!(res.ok);
        let session_id = res.session_id.unwrap();

        // Drain the sink and find the streamed assistant text delta.
        let mut deltas: Vec<(Option<String>, String)> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AgentEvent::TextDelta { session_id, delta } = ev {
                deltas.push((session_id, delta));
            }
        }

        assert_eq!(
            deltas.len(),
            1,
            "fake provider should emit exactly one assistant text delta"
        );
        assert_eq!(deltas[0].1, "OCEAN_FAKE_OK");
        // The delta must carry the turn's session id so the daemon bridge can
        // scope the AssistantTextDelta to the `?session_id=` subscriber.
        assert_eq!(
            deltas[0].0.as_deref(),
            Some(session_id.to_string().as_str())
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn unknown_session_id_errors_instead_of_silently_creating() {
        // Strict resume: a supplied-but-unknown session id with create_if_missing
        // = false must fail, and must NOT leave a fresh transcript behind under
        // that id. This is the daemon-side fix for stale-client-id bugs.
        let config_dir = temp_config_dir("strict-resume");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let ghost = SessionId::new_v4();
        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "resume a session that doesn't exist".into(),
                    images: None,
                    request_id: None,
                    session_id: Some(ghost),
                    create_if_missing: false,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(!res.ok, "strict resume of unknown session must fail");
        assert!(
            res.stderr.contains("session not found"),
            "stderr: {}",
            res.stderr
        );
        // And no session was created under the ghost id.
        assert!(runtime.session_detail(ghost).is_err());
        assert!(runtime.list_sessions(None).unwrap().is_empty());

        // But with create_if_missing: true, the same id is accepted.
        let ok = runtime
            .prompt(
                PromptRequest {
                    prompt: "now create it".into(),
                    images: None,
                    request_id: None,
                    session_id: Some(ghost),
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(
            ok.ok,
            "create_if_missing should accept a new id: {}",
            ok.stderr
        );
        assert_eq!(ok.session_id, Some(ghost));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn concurrent_turns_on_same_session_serialize_without_lost_updates() {
        // Two turns fired at the same session concurrently must not clobber
        // each other: both user prompts (and both assistant replies) must
        // survive in the final transcript. Without the per-session lock, the
        // last save wins and one turn's messages vanish.
        let config_dir = temp_config_dir("concurrent-turns");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        // First turn creates the session; capture its id.
        let first = runtime
            .prompt(
                PromptRequest {
                    prompt: "first".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(first.ok);
        let sid = first.session_id.unwrap();

        // Now fire two more turns at that same session at once.
        let mk = |p: &str| PromptRequest {
            prompt: p.into(),
            images: None,
            request_id: None,
            session_id: Some(sid),
            create_if_missing: false,
            max_turns: None,
            yolo: false,
            cwd: ".".into(),
            project_id: None,
            client_type: None,
            decision_token: None,
        };
        let (a, b) = tokio::join!(
            runtime.prompt(mk("alpha"), PromptControl::yolo(false)),
            runtime.prompt(mk("bravo"), PromptControl::yolo(false)),
        );
        assert!(a.ok && b.ok);

        // Final transcript must contain all three user prompts.
        let detail = runtime.session_detail(sid).unwrap();
        let user_texts: Vec<String> = detail
            .transcript
            .iter()
            .filter(|t| t.role == "user")
            .map(|t| t.text.clone())
            .collect();
        assert!(
            user_texts.iter().any(|t| t.contains("first")),
            "lost 'first': {user_texts:?}"
        );
        assert!(
            user_texts.iter().any(|t| t.contains("alpha")),
            "lost 'alpha': {user_texts:?}"
        );
        assert!(
            user_texts.iter().any(|t| t.contains("bravo")),
            "lost 'bravo': {user_texts:?}"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[tokio::test]
    async fn many_concurrent_turns_serialize_with_exact_transcript_integrity() {
        // Harder version of the two-turn test: fire N turns at the SAME session
        // all at once and prove the per-session lock serializes load→save with
        // (a) no lost update — every distinct prompt survives, (b) no
        // duplication — each survives exactly once, (c) no corruption — the
        // final transcript is a clean user→assistant alternation of the exact
        // expected length. Without serialization, concurrent load/append/save
        // races drop messages and/or desync the user/assistant pairing.
        const N: usize = 12;
        let config_dir = temp_config_dir("many-concurrent-turns");
        let runtime = Arc::new(runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        ));

        // Seed the session so all N concurrent turns target a known id.
        let seed = runtime
            .prompt(
                PromptRequest {
                    prompt: "seed".into(),
                    images: None,
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    project_id: None,
                    client_type: None,
                    decision_token: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(seed.ok);
        let sid = seed.session_id.unwrap();

        // Each task carries a uniquely-numbered prompt so we can verify the
        // exact set survives (no lost update, no duplicate).
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let rt = Arc::clone(&runtime);
            handles.push(tokio::spawn(async move {
                rt.prompt(
                    PromptRequest {
                        prompt: format!("concurrent-prompt-{i}"),
                        images: None,
                        request_id: None,
                        session_id: Some(sid),
                        create_if_missing: false,
                        max_turns: None,
                        yolo: false,
                        cwd: ".".into(),
                        project_id: None,
                        client_type: None,
                        decision_token: None,
                    },
                    PromptControl::yolo(false),
                )
                .await
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert!(res.ok, "a concurrent turn failed: {}", res.stderr);
        }

        let detail = runtime.session_detail(sid).unwrap();

        // (a) + (b): every numbered prompt is present exactly once. The fake
        // provider prefixes a surface flag, so match on the unique suffix.
        let user_texts: Vec<&str> = detail
            .transcript
            .iter()
            .filter(|t| t.role == "user")
            .map(|t| t.text.as_str())
            .collect();
        // Match on the exact trailing token (the stored text is
        // `[FLAG] concurrent-prompt-N`), so `…-1` doesn't substring-collide with
        // `…-10`/`…-11`.
        for i in 0..N {
            let needle = format!("concurrent-prompt-{i}");
            let hits = user_texts.iter().filter(|t| t.ends_with(&needle)).count();
            assert_eq!(
                hits, 1,
                "prompt {i} should appear exactly once, found {hits} in {user_texts:?}"
            );
        }
        // Plus the seed: N concurrent + 1 seed distinct user turns.
        assert!(user_texts.iter().any(|t| t.contains("seed")));

        // (c) no corruption: transcript is a strict user→assistant alternation
        // of exactly 2*(N+1) entries — every saved user turn kept its paired
        // assistant reply, none interleaved or clobbered.
        assert_eq!(
            detail.transcript.len(),
            2 * (N + 1),
            "expected {} entries, got {}",
            2 * (N + 1),
            detail.transcript.len()
        );
        for (idx, entry) in detail.transcript.iter().enumerate() {
            let expected = if idx % 2 == 0 { "user" } else { "assistant" };
            assert_eq!(
                entry.role, expected,
                "entry {idx} should be {expected}, transcript desynced"
            );
        }
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn session_detail_reports_corrupt_session_file() {
        let config_dir = temp_config_dir("corrupt-session");
        let id = SessionId::new_v4();
        let dir = session::sessions_dir(&config_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.json")), "{not-json").unwrap();

        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let error = runtime.session_detail(id).unwrap_err();
        assert!(error
            .chain()
            .all(|cause| cause.downcast_ref::<std::io::Error>().is_none()));
        let chain = error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chain.contains("expected") || chain.contains("key"),
            "corrupt session error should surface a parse failure, got: {chain}"
        );
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn session_detail_includes_tool_context_when_persisted() {
        let config_dir = temp_config_dir("tool-context");
        let model =
            model_from_provider_config(&provider_config(ProviderId::Fake, "fake-ok", false))
                .unwrap();
        let mut session = session::Session::new(&model);
        let tool_call_id = "call-1".to_string();
        session.replace_messages(vec![
            Message::user_text("inspect workspace"),
            Message::Assistant(AssistantMessage {
                content: vec![
                    Content::text("visible answer"),
                    Content::Thinking {
                        thinking: "private reasoning sentinel".into(),
                        thinking_signature: None,
                    },
                    Content::ToolCall {
                        id: tool_call_id.clone(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "README.md"}),
                    },
                ],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: ocean_protocol::now_ms(),
            }),
            Message::ToolResult(ocean_protocol::ToolResultMessage {
                tool_call_id,
                tool_name: "read".into(),
                content: vec![Content::text("contents")],
                is_error: false,
                timestamp: ocean_protocol::now_ms(),
            }),
        ]);
        session::save(&config_dir, &session).unwrap();

        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let detail = runtime.session_detail(session.id).unwrap();
        assert_eq!(detail.transcript[1].text, "visible answer");
        assert!(!detail.transcript[1].text.contains("private reasoning"));
        assert_eq!(detail.tool_context.len(), 2);
        assert_eq!(detail.tool_context[0].kind, "call");
        assert_eq!(detail.tool_context[0].tool_name, "read");
        assert_eq!(detail.tool_context[1].kind, "result");
        assert_eq!(detail.tool_context[1].text, "contents");
        let sync = session::session_sync_snapshot(&session);
        assert_eq!(sync.transcript.len(), 2);
        assert!(sync.transcript.iter().all(|entry| entry.role != "tool"));
        assert!(sync.transcript.iter().all(|entry| entry.images.is_empty()));
        assert!(sync
            .transcript
            .iter()
            .all(|entry| !entry.text.contains("private reasoning") && entry.text != "contents"));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn session_sync_snapshot_bounds_rows_and_text_without_full_detail_projection() {
        let model =
            model_from_provider_config(&provider_config(ProviderId::Fake, "fake-ok", false))
                .unwrap();
        let mut session = session::Session::new(&model);
        let mut messages = vec![Message::ToolResult(ocean_protocol::ToolResultMessage {
            tool_call_id: "secret-call".into(),
            tool_name: "read".into(),
            content: vec![Content::text("secret tool output")],
            is_error: false,
            timestamp: ocean_protocol::now_ms(),
        })];
        messages.extend((0..514).map(|index| Message::user_text(format!("visible-{index}"))));
        session.replace_messages(messages);
        let sync = session::session_sync_snapshot(&session);
        assert_eq!(
            sync.transcript.len(),
            ocean_core::SESSION_SYNC_MAX_VISIBLE_MESSAGES
        );
        assert_eq!(sync.truncated_messages, 2);
        assert!(sync.transcript.iter().all(|entry| entry.role == "user"));

        let mut tool_only = vec![Message::user_text("visible row survives")];
        tool_only.extend((0..513).map(|index| {
            Message::Assistant(AssistantMessage {
                content: vec![Content::ToolCall {
                    id: format!("call-{index}"),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "README.md"}),
                }],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: ocean_protocol::now_ms(),
            })
        }));
        session.replace_messages(tool_only);
        let sync = session::session_sync_snapshot(&session);
        assert_eq!(sync.transcript.len(), 1);
        assert_eq!(sync.transcript[0].text, "visible row survives");
        assert_eq!(sync.truncated_messages, 0);

        session.replace_messages(vec![Message::user_text(
            "é".repeat((ocean_core::SESSION_SYNC_MAX_VISIBLE_TEXT_BYTES / 2) + 16),
        )]);
        let sync = session::session_sync_snapshot(&session);
        assert_eq!(sync.transcript.len(), 1);
        assert!(sync.transcript[0].text.len() <= ocean_core::SESSION_SYNC_MAX_VISIBLE_TEXT_BYTES);
        assert!(sync.truncated_text_bytes > 0);
    }

    // ---- OCEAN-250: collection-list pagination -----------------------------

    #[test]
    fn clamp_list_limit_defaults_and_clamps() {
        assert_eq!(clamp_list_limit(None), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_list_limit(Some(usize::MAX)), MAX_LIST_LIMIT);
        // 0 floors to 1 so it can never request an empty-yet-has_more page.
        assert_eq!(clamp_list_limit(Some(0)), 1);
        assert_eq!(clamp_list_limit(Some(50)), 50);
    }

    #[test]
    fn paginate_by_id_caps_and_returns_cursor() {
        let items: Vec<u32> = (0..10).collect();
        let page = paginate_by_id(items, None, Some(4), |n| n.to_string());
        assert_eq!(page.items, vec![0, 1, 2, 3]);
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("3"));
    }

    #[test]
    fn paginate_by_id_resumes_after_cursor_and_walks_to_end() {
        // Page the whole list in steps of 3 via the cursor; reconstruct it all.
        let total = 17usize;
        let all: Vec<usize> = (0..total).collect();
        let mut collected: Vec<usize> = Vec::new();
        let mut after: Option<String> = None;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard <= total + 2, "paging must terminate");
            let page = paginate_by_id(all.clone(), after.as_deref(), Some(3), |n| n.to_string());
            collected.extend(page.items.iter().copied());
            match page.next_cursor {
                Some(c) => {
                    assert!(page.has_more);
                    after = Some(c);
                }
                None => {
                    assert!(!page.has_more, "no cursor ⇒ no more pages");
                    break;
                }
            }
        }
        assert_eq!(collected, all, "every item once, in order");
    }

    #[test]
    fn paginate_by_id_full_final_page_has_no_cursor() {
        let items: Vec<u32> = (0..5).collect();
        let page = paginate_by_id(items, None, Some(5), |n| n.to_string());
        assert_eq!(page.items.len(), 5);
        assert!(!page.has_more, "a full final page is not has_more");
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn paginate_by_id_unknown_cursor_falls_back_to_start() {
        // A cursor matching nothing (item removed since) resumes from the top
        // rather than erroring — resilient to a stale cursor.
        let items: Vec<u32> = (0..4).collect();
        let page = paginate_by_id(items, Some("nope"), Some(2), |n| n.to_string());
        assert_eq!(page.items, vec![0, 1]);
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("1"));
    }

    #[test]
    fn paginate_by_id_default_cap_bounds_a_large_list() {
        // More items than the default cap ⇒ exactly the cap, with more pages.
        let big: Vec<usize> = (0..(DEFAULT_LIST_LIMIT + 25)).collect();
        let page = paginate_by_id(big, None, None, |n| n.to_string());
        assert_eq!(page.items.len(), DEFAULT_LIST_LIMIT);
        assert!(page.has_more, "rows beyond the cap mean more pages");
    }

    #[test]
    fn sort_projects_newest_first_orders_by_updated_then_id() {
        use ocean_core::ProjectConfig;
        let mk = |updated: i64| Project {
            id: uuid::Uuid::new_v4(),
            name: "p".into(),
            workspace_root: "/x".into(),
            config: ProjectConfig::default(),
            created_ms: 0,
            updated_ms: updated,
        };
        let mut projects = vec![mk(100), mk(300), mk(200)];
        sort_projects_newest_first(&mut projects);
        assert_eq!(projects[0].updated_ms, 300);
        assert_eq!(projects[1].updated_ms, 200);
        assert_eq!(projects[2].updated_ms, 100);
    }

    #[test]
    fn owning_project_for_root_resolves_registered_main_repo_for_linked_worktree() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .map(|status| !status.success())
            .unwrap_or(true)
        {
            return;
        }

        let assert_command = |command: &mut std::process::Command, label: &str| {
            let status = command.status().expect(label);
            assert!(status.success(), "{label} failed with status {status}");
        };

        let config_dir = temp_config_dir("worktree-project-owner");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );
        let repos = tempfile::tempdir().expect("tempdir");
        let main_root = repos.path().join("main");
        assert_command(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&main_root),
            "git init",
        );

        std::fs::write(main_root.join("README.md"), "ocean\n").expect("write seed file");
        assert_command(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&main_root)
                .args(["add", "README.md"]),
            "git add",
        );
        assert_command(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&main_root)
                .args([
                    "-c",
                    "user.name=Ocean Test",
                    "-c",
                    "user.email=ocean-test@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "initial",
                ]),
            "git commit",
        );

        let worktree_root = repos.path().join("linked");
        assert_command(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&main_root)
                .args(["worktree", "add", "-q", "-b", "linked-worktree-test"])
                .arg(&worktree_root),
            "git worktree add",
        );

        let main_root = main_root.canonicalize().expect("canonical main root");
        let main_root_str = main_root.to_string_lossy().into_owned();
        let project_id = uuid::Uuid::new_v4();
        let stored = runtime
            .upsert_project(
                Project {
                    id: project_id,
                    name: "main repo".into(),
                    workspace_root: main_root_str,
                    config: ocean_core::ProjectConfig::default(),
                    created_ms: 10,
                    updated_ms: 10,
                },
                20,
            )
            .unwrap();

        let worktree_root = worktree_root
            .canonicalize()
            .expect("canonical worktree root");
        let worktree_root_str = worktree_root.to_string_lossy();
        let owner = runtime
            .owning_project_for_root(&worktree_root_str)
            .expect("linked worktree should resolve to the registered main repo project");
        assert_eq!(owner.id, stored.id);
        assert_eq!(owner.workspace_root, stored.workspace_root);

        let unrelated = tempfile::tempdir().expect("unrelated tempdir");
        let unrelated = unrelated
            .path()
            .canonicalize()
            .expect("canonical unrelated dir");
        let unrelated_str = unrelated.to_string_lossy();
        assert!(
            runtime.owning_project_for_root(&unrelated_str).is_none(),
            "an unrelated directory must not claim the registered repo project"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn owning_project_index_maps_each_distinct_root_to_its_project() {
        // Two projects at distinct roots → the index holds both, each root keyed
        // to its own project by id. No git is involved: owning_project_index
        // reads projects.json once (the perf fix for the sessions endpoint).
        let config_dir = temp_config_dir("owning-project-index-distinct");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        let mk = |id: uuid::Uuid, name: &str, root: &str, ts: i64| Project {
            id,
            name: name.into(),
            workspace_root: root.into(),
            config: ocean_core::ProjectConfig::default(),
            created_ms: ts,
            updated_ms: ts,
        };

        let alpha_id = uuid::Uuid::new_v4();
        let beta_id = uuid::Uuid::new_v4();
        // upsert appends distinct-id projects in insertion order.
        runtime
            .upsert_project(mk(alpha_id, "alpha", "/srv/alpha", 100), 100)
            .unwrap();
        runtime
            .upsert_project(mk(beta_id, "beta", "/srv/beta", 200), 200)
            .unwrap();

        let index = runtime.owning_project_index().unwrap();
        assert_eq!(index.len(), 2, "both distinct-root projects are present");
        assert_eq!(
            index.get("/srv/alpha").map(|p| p.id),
            Some(alpha_id),
            "alpha's root maps to alpha"
        );
        assert_eq!(
            index.get("/srv/beta").map(|p| p.id),
            Some(beta_id),
            "beta's root maps to beta"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn owning_project_index_keeps_first_stored_project_for_a_duplicate_root() {
        // Two projects claim the SAME workspace_root. owning_project_index walks
        // projects in stored order and is first-match-wins (entry().or_insert),
        // exactly mirroring project_for_workspace / find_by_workspace
        // (load_all().into_iter().find). The batch path and the per-session
        // detail path MUST resolve a duplicate root to the same owner, so we
        // assert agreement between the two here.
        let config_dir = temp_config_dir("owning-project-index-duplicate-root");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        let mk = |id: uuid::Uuid, name: &str, root: &str, ts: i64| Project {
            id,
            name: name.into(),
            workspace_root: root.into(),
            config: ocean_core::ProjectConfig::default(),
            created_ms: ts,
            updated_ms: ts,
        };

        let shared_root = "/srv/shared";
        let first_id = uuid::Uuid::new_v4();
        let second_id = uuid::Uuid::new_v4();
        // upsert appends distinct-id projects in stored order, so `first` leads.
        runtime
            .upsert_project(mk(first_id, "first", shared_root, 100), 100)
            .unwrap();
        runtime
            .upsert_project(mk(second_id, "second", shared_root, 200), 200)
            .unwrap();

        let index = runtime.owning_project_index().unwrap();
        assert_eq!(
            index.len(),
            1,
            "a duplicate root collapses to a single entry"
        );
        let indexed = index
            .get(shared_root)
            .expect("the shared root is present once");
        assert_eq!(
            indexed.id, first_id,
            "the first-stored project wins for a duplicate root"
        );

        // Consistency: the single-root lookup the detail path uses agrees.
        let lookup = runtime
            .project_for_workspace(shared_root)
            .unwrap()
            .expect("project_for_workspace resolves the shared root");
        assert_eq!(
            lookup.id, first_id,
            "project_for_workspace and owning_project_index agree on duplicate roots"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn owning_project_index_skips_empty_root_and_misses_unregistered_roots() {
        // A project whose workspace_root is the empty string is skipped (never
        // keyed), and a root no project claims is simply absent from the map.
        let config_dir = temp_config_dir("owning-project-index-empty-and-miss");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        let claimed_id = uuid::Uuid::new_v4();
        runtime
            .upsert_project(
                Project {
                    id: claimed_id,
                    name: "claimed".into(),
                    workspace_root: "/srv/claimed".into(),
                    config: ocean_core::ProjectConfig::default(),
                    created_ms: 100,
                    updated_ms: 100,
                },
                100,
            )
            .unwrap();
        runtime
            .upsert_project(
                Project {
                    id: uuid::Uuid::new_v4(),
                    name: "rootless".into(),
                    workspace_root: String::new(),
                    config: ocean_core::ProjectConfig::default(),
                    created_ms: 200,
                    updated_ms: 200,
                },
                200,
            )
            .unwrap();

        let index = runtime.owning_project_index().unwrap();
        assert_eq!(
            index.len(),
            1,
            "only the project with a non-empty root is indexed"
        );
        assert!(
            !index.contains_key(""),
            "an empty workspace_root is never a key"
        );
        assert_eq!(
            index.get("/srv/claimed").map(|p| p.id),
            Some(claimed_id),
            "the claimed root maps to its project"
        );
        assert!(
            !index.contains_key("/srv/nobody-claims-this"),
            "an unregistered root is absent"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn list_projects_page_is_bounded_and_pageable() {
        let config_dir = temp_config_dir("list-projects-page");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        // 5 projects, each newer than the last so the order is deterministic.
        for i in 0..5i64 {
            let p = Project {
                id: uuid::Uuid::new_v4(),
                name: format!("proj-{i}"),
                workspace_root: format!("/dev/p{i}"),
                config: ocean_core::ProjectConfig::default(),
                created_ms: 1000 + i,
                updated_ms: 1000 + i,
            };
            runtime.upsert_project(p, 1000 + i).unwrap();
        }

        // First page of 2 (newest-first: proj-4, proj-3).
        let page1 = runtime.list_projects_page(None, Some(2)).unwrap();
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].name, "proj-4");
        assert_eq!(page1.items[1].name, "proj-3");
        assert!(page1.has_more);
        let cursor = page1.next_cursor.clone().expect("more pages ⇒ cursor");
        assert_eq!(cursor, page1.items[1].id.to_string());

        // Walk to the end with the cursor; collect everything once.
        let mut names: Vec<String> = page1.items.iter().map(|p| p.name.clone()).collect();
        let mut after = Some(cursor);
        loop {
            let page = runtime
                .list_projects_page(after.as_deref(), Some(2))
                .unwrap();
            names.extend(page.items.iter().map(|p| p.name.clone()));
            match page.next_cursor {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        assert_eq!(
            names,
            vec!["proj-4", "proj-3", "proj-2", "proj-1", "proj-0"],
            "paging reconstructs the full newest-first order exactly once"
        );

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn list_sessions_page_is_bounded_and_pageable() {
        let config_dir = temp_config_dir("list-sessions-page");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::Fake, "fake-ok", false),
        );

        // Three sessions in the same workspace (the default cwd ".").
        let mut ids = Vec::new();
        for _ in 0..3 {
            let (id, _, _) = runtime.create_session(".", None).unwrap();
            ids.push(id);
        }

        // Page of 2 then the rest via the cursor; union is all three, no dupes.
        // (scope = None ⇒ all workspaces.)
        let page1 = runtime.list_sessions_page(None, None, Some(2)).unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        let cursor = page1.next_cursor.clone().expect("more ⇒ cursor");

        let page2 = runtime
            .list_sessions_page(None, Some(&cursor), Some(2))
            .unwrap();
        assert_eq!(
            page2.items.len(),
            1,
            "one session left after the first page"
        );
        assert!(!page2.has_more);
        assert_eq!(page2.next_cursor, None);

        let mut seen: Vec<SessionId> = page1.items.iter().map(|s| s.id).collect();
        seen.extend(page2.items.iter().map(|s| s.id));
        seen.sort();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(seen, expected, "paging covers every session exactly once");

        // Default cap is bounded (not unbounded): with 3 sessions and no limit,
        // the first default page holds all three and reports no more.
        let page_default = runtime.list_sessions_page(None, None, None).unwrap();
        assert_eq!(page_default.items.len(), 3);
        assert!(!page_default.has_more);

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn list_sessions_groups_workspace_root_before_recency() {
        let config_dir = temp_config_dir("list-sessions-workspace-root-order");
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();

        let mut a_new = session::Session::new(&model);
        a_new.workspace_root = Some("/tmp/project-a".into());
        a_new.cwd = Some("/tmp/project-a/app".into());
        a_new.updated_ms = 300;

        let mut b_newest = session::Session::new(&model);
        b_newest.workspace_root = Some("/tmp/project-b".into());
        b_newest.cwd = Some("/tmp/project-b/app".into());
        b_newest.updated_ms = 400;

        let mut a_old = session::Session::new(&model);
        a_old.workspace_root = Some("/tmp/project-a".into());
        a_old.cwd = Some("/tmp/project-a/worker".into());
        a_old.updated_ms = 200;

        let mut b_old = session::Session::new(&model);
        b_old.workspace_root = Some("/tmp/project-b".into());
        b_old.cwd = Some("/tmp/project-b/worker".into());
        b_old.updated_ms = 100;

        for session in [&b_newest, &a_new, &a_old, &b_old] {
            session::save(&config_dir, session).unwrap();
        }

        let listed = session::list(&config_dir, None).unwrap();
        let roots: Vec<&str> = listed
            .iter()
            .map(|session| session.workspace_root.as_deref().unwrap_or(""))
            .collect();
        let updated: Vec<i64> = listed
            .iter()
            .map(|session| session.updated_ms.unwrap_or_default())
            .collect();

        assert_eq!(
            roots,
            vec![
                "/tmp/project-a",
                "/tmp/project-a",
                "/tmp/project-b",
                "/tmp/project-b",
            ],
            "workspace roots should cluster together before recency ordering"
        );
        assert_eq!(updated, vec![300, 200, 400, 100]);

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn bind_workspace_rebinds_on_different_project() {
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let mut s = session::Session::new(&model);

        // First bind — unconditional.
        let tmp = std::env::temp_dir();
        s.bind_workspace(&tmp);
        let first_root = s.workspace_root.clone().unwrap();

        // Second bind to a different directory — must overwrite.
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        s.bind_workspace(&home);
        let second_root = s.workspace_root.clone().unwrap();

        // If home and tmp resolve to different workspace roots, we must have rebound.
        // (They always will on a normal system; this guards the invariant.)
        if first_root != second_root {
            assert_eq!(
                s.cwd.as_deref(),
                Some(home.to_string_lossy().as_ref()),
                "cwd must update to the new project directory"
            );
        }

        // Bind to the same directory again — must NOT change cwd.
        let cwd_before = s.cwd.clone();
        let root_before = s.workspace_root.clone();
        s.bind_workspace(&home);
        assert_eq!(s.cwd, cwd_before, "same-project bind must be a no-op");
        assert_eq!(
            s.workspace_root, root_before,
            "same-project bind must be a no-op"
        );
    }

    #[test]
    fn bind_workspace_refreshes_cwd_within_same_workspace() {
        let model = ocean_protocol::Model::anthropic_claude_sonnet_4_6();
        let mut s = session::Session::new(&model);

        let repo = tempfile::tempdir().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .expect("git init");

        let sub_a = repo.path().join("a");
        let sub_b = repo.path().join("b");
        std::fs::create_dir_all(&sub_a).expect("create subdir a");
        std::fs::create_dir_all(&sub_b).expect("create subdir b");

        s.bind_workspace(&sub_a);
        let workspace_root = s.workspace_root.clone();

        s.bind_workspace(&sub_b);
        assert_eq!(
            s.workspace_root, workspace_root,
            "same-workspace bind must keep the workspace root"
        );
        assert_eq!(
            s.cwd.as_deref(),
            Some(sub_b.to_string_lossy().as_ref()),
            "same-workspace bind must refresh cwd to the latest launch directory"
        );
    }
}

mod system_prompt;
