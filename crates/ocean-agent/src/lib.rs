use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{
    ImageMeta, Project, ProjectId, PromptImage, PromptRequest, PromptResponse, RequestId, RoomId,
    SessionDetail, SessionId, SessionRunState, SessionSummary, SessionToolContext,
    SessionTranscriptEntry, TokenUsage,
};
use ocean_protocol::{AssistantMessage, Content, Message, Model, StopReason, Usage};
use ocean_providers::{
    resolve_provider_config, resolve_provider_config_from_env, ProviderConfig, ProviderEnv,
    ProviderId, ProviderReadiness,
};
// Re-export the model catalogue so the daemon can serve a picker without taking
// a direct ocean-providers dependency.
pub use ocean_providers::{known_models, KnownModel};
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
pub use config::{DaemonConfig, McpSection};
/// Filesystem-first agent definitions (folder = agent). Module-qualified to
/// avoid colliding with `ocean_runtime::AgentConfig`; refer to the folder-agent
/// config as `agentdir::AgentConfig`.
pub mod agentdir;
pub use agentdir::{AgentDef, ResolveError as AgentDirResolveError};
mod project;
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

/// Room-specific operator guidance injected by the daemon before runtime turns.
pub fn room_guidance(room_id: RoomId) -> &'static str {
    match room_id {
        RoomId::Pm => {
            "PM room: operator proxy and foreground agent turns. Keep focus on the current instruction, streamed output, and command status."
        }
        RoomId::Writers => {
            "Writers Room: drafts, sources, and handoff context. Keep output oriented to writing, doc edits, and source references."
        }
        RoomId::OrchMesh => {
            "ORCH + MESH: route requests, permissions, and event state. Keep changes operational, concise, and traceable."
        }
        RoomId::Review => {
            "Review Room: review notes, validation evidence, and release proof. Focus on risks, diffs, and test results."
        }
    }
}

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
    /// Test-only override for the per-turn environment snapshot used by provider
    /// failover (OCEAN-275). Production always reads the real process env via
    /// [`AgentRuntime::turn_env`]; tests inject a deterministic [`ProviderEnv`]
    /// here so the failover policy can be exercised end-to-end through `prompt`
    /// without mutating (and racing on) the global process environment.
    #[cfg(test)]
    test_env: Option<ProviderEnv>,
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
        let config_dir = config_dir_from_env();
        let state = build_state_from_env(&config_dir)?;
        let runtime = Self {
            config_dir,
            state: Arc::new(RwLock::new(state)),
            capabilities: Arc::new(CapabilityRegistry::builtin_only()),
            session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            #[cfg(test)]
            test_env: None,
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

    pub async fn prompt(&self, req: PromptRequest, control: PromptControl) -> PromptResponse {
        let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
        let mut req = req;
        req.request_id = Some(request_id);

        let start = Instant::now();
        // Report the turn/session cwd the daemon resolved from the client request,
        // not the long-lived daemon process cwd. Returning `current_dir()` here
        // made legacy `/v1/prompt` clients look bound to wherever the daemon was
        // launched even when tool/session execution used a different cwd.
        let cwd = req.cwd.clone();

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

        // Run the turn against the effective provider; on a pre-stream
        // connect-failure with a transient/availability error, fail over once to
        // the next ready alternate (bounded — see `run_turn_with_failover`). This
        // never fails over mid-stream: the moment any output streamed, the attempt
        // is final.
        let result = self
            .run_turn_with_failover(req.clone(), control, effective, &env)
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
        req: PromptRequest,
        control: PromptControl,
        state: RuntimeState,
        env: &ProviderEnv,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        let failed_provider = state.provider_config.selection.provider.clone();
        match self
            .dispatch_turn(req.clone(), control.clone(), &state)
            .await
        {
            Ok(ok) => Ok(ok),
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
                // Single bounded retry on the alternate. Whatever it returns is
                // final (success or failure) — no further fan-out.
                self.dispatch_turn(req, control, &alt_state)
                    .await
                    .map_err(unwrap_turn_failure)
            }
        }
    }

    /// Dispatch a single turn to the fake echo path or the real agent loop,
    /// exactly as the pre-failover `prompt` did. Factored out so
    /// [`Self::run_turn_with_failover`] can invoke it for both the primary and
    /// the fallback provider without duplicating the fake-vs-real branching.
    async fn dispatch_turn(
        &self,
        req: PromptRequest,
        control: PromptControl,
        snapshot: &RuntimeState,
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
            self.run_fake_prompt(req, control, snapshot).await
        } else {
            self.run_prompt(req, control, snapshot).await
        }
    }

    pub fn list_sessions(
        &self,
        workspace_root: Option<&str>,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        session::list(&self.config_dir, workspace_root)
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
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        let session_id = req.session_id.unwrap_or_else(SessionId::new_v4);
        let lock = self.session_lock(session_id);
        let _turn_guard = lock.lock().await;

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
        messages.push(Message::user_text(req.prompt));
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
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

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

        // Hold the per-session lock across load → run → save. Without it, two
        // turns on the same session both load the same history and the last to
        // save wins, silently dropping the other's messages.
        let lock = self.session_lock(session_id);
        let _turn_guard = lock.lock().await;

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
        // was last steered from. Detect a switch (e.g. a session started in the
        // GPUI app, continued from the Chrome extension) so the agent is told,
        // then record the new surface on the session for next turn / resume.
        let prev_surface = session.client_type.clone();
        let surface_switched = match (prev_surface.as_deref(), req.client_type.as_deref()) {
            (Some(old), Some(new)) => old != new,
            _ => false,
        };
        if req.client_type.is_some() {
            session.client_type = req.client_type.clone();
        }

        let mut history = session.messages.clone();
        // OpenAI-compatible providers (DeepSeek, OpenAI o-series, xAI, etc.)
        // do not accept assistant `thinking` blocks as input on the next turn —
        // reasoning is output-only. Strip them on replay. Anthropic stores
        // thinking with a signature and is happy to receive it back.
        if matches!(
            snapshot.provider_config.selection.provider,
            ProviderId::DeepSeek
                | ProviderId::OpenAi
                | ProviderId::OpenAiCodex
                | ProviderId::OpenAiCompatible
                | ProviderId::MiniMax
                | ProviderId::Kimi
        ) {
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
                out.push_str(&format!(
                    "[surface switch: the user is now messaging you via [{flag}] (was [{from}]). \
                     Adjust your rendering and tone to this surface.]\n",
                ));
            }
            out.push_str(&format!("[{flag}] "));
            out.push_str(&req.prompt);
            out
        };
        // First user message of the turn: prompt text plus any attached images
        // as `Content::Image` blocks (OCEAN-115). No images → plain-text message,
        // identical to the prior `Message::user_text` path.
        history.push(build_user_message(user_text, req.images.as_deref()));

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
        } = control;
        // Resolve the toolset for this turn through the capability registry —
        // built-ins plus any connected MCP/skill providers, deduped first-wins.
        // This is the seam that replaced the old hardcoded `default_tools()`.
        let tool_ctx = SessionContext {
            cwd: PathBuf::from(&req.cwd),
            session_id: Some(session_id.to_string()),
        };
        let tools = self.capabilities.tools_for_session(&tool_ctx).await;
        // Folder-as-agent tool narrowing: a named agent's declared `tools` list
        // restricts this turn to those tools. Fail-safe — if the allowlist
        // matches no available tool (typo / renamed tool), keep the full set and
        // warn rather than running the agent with zero tools.
        let tools = narrow_tools(tools, tool_allowlist.as_deref());

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
        let cfg_cloned = cfg.clone();
        // The agent loop runs on its own task, so the turn's span context does NOT
        // propagate automatically — a freshly spawned task starts with no parent
        // span. Re-attach the current `runtime.prompt` span (OCEAN-274) so the
        // `agent_loop` span (and its `round`/`provider_stream`/`tool_exec`
        // children) nest under this turn instead of detaching into a rootless tree.
        let handle = tokio::spawn(
            async move { run_agent_with_history(&cfg_cloned, history, Some(tx)).await }
                .instrument(tracing::Span::current()),
        );

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
                _ => {}
            }
        }

        let run = match handle.await.context("agent task join failed")? {
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
        }
    }

    /// Narrow this turn's toolset to the named tools (folder-as-agent allowlist).
    pub fn with_tool_allowlist(mut self, tools: Vec<String>) -> Self {
        self.tool_allowlist = (!tools.is_empty()).then_some(tools);
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

    // Browser control. Chrome is launched lazily on the first turn that asks for
    // tools (see BrowserProvider). We drive **Chrome for Testing** with its own
    // dedicated profile (NOT the user's everyday Chrome): current stable Chrome
    // (137+) removed `--load-extension`, so the Ocean cockpit extension only
    // auto-loads in CfT — and a dedicated profile means we never conflict with
    // (or require quitting) the user's running Chrome. The user logs into their
    // accounts once inside Ocean's CfT; the profile persists them.
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

    let cfg = match config::DaemonConfig::load(config_dir) {
        Ok(c) => c,
        Err(e) => {
            // A malformed config shouldn't take the agent down — run with
            // built-ins and make the misconfiguration loud.
            tracing::error!(error = %e, "failed to load ocean.toml; running with built-in tools only");
            return CapabilityRegistry::new(providers);
        }
    };

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
            _ => Model::openai_compat(
                selection.provider.as_str(),
                selection.model.clone(),
                selection.base_url.clone(),
                selection.context_window,
                selection.max_output_tokens,
            ),
        }),
        ProviderId::OpenAiCodex => Ok(Model::codex(
            selection.model.clone(),
            selection.context_window,
            selection.max_output_tokens,
        )),
        ProviderId::Anthropic => Ok(match selection.model.as_str() {
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

/// Persist the operator's YOLO preference (best-effort; a write failure is
/// logged, never fatal — losing the hint just falls back to the safe default
/// of gated/off). Mirrors [`persist_last_model`].
pub fn persist_yolo_pref(config_dir: &std::path::Path, enabled: bool) {
    let path = config_dir.join(YOLO_PREF_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, if enabled { "true" } else { "false" }) {
        tracing::warn!(path = %path.display(), error = %e, "failed to persist yolo preference");
    }
}

/// Read the persisted YOLO preference, if any. `None` on first run / unreadable
/// / unrecognized content — the caller treats `None` as "no persisted default"
/// and falls through to the built-in safe default (off). Mirrors
/// [`load_last_model`]; accepts the same truthy/falsey spellings as the
/// `OCEAN_YOLO` env parse so the two sources stay consistent.
pub fn load_yolo_pref(config_dir: &std::path::Path) -> Option<bool> {
    let raw = std::fs::read_to_string(config_dir.join(YOLO_PREF_FILE)).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
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

mod session {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Session {
        pub id: SessionId,
        pub created_ms: i64,
        pub updated_ms: i64,
        pub model: String,
        pub provider: String,
        pub messages: Vec<Message>,
        /// Workspace anchor — git toplevel if the cwd is inside a repo,
        /// else the cwd itself. Used to bucket sessions per project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_root: Option<String>,
        /// The cwd this session was started in. May differ from
        /// `workspace_root` if cwd was inside a subdirectory of a repo.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        /// Git branch captured at session creation, when in a repo.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub git_branch: Option<String>,
        /// Git short-commit captured at session creation, when in a repo.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub git_commit: Option<String>,
        /// The client surface this session is currently bound to (the last
        /// `client_type` seen on a turn). Lets the runtime detect a
        /// surface switch between turns (Fix 3) and re-inject the right
        /// surface profile on resume (Fix 5). Old session files predate this
        /// field and deserialize as `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub client_type: Option<String>,
    }

    impl Session {
        /// Mint a session with a fresh random id. Only used by tests today
        /// (production always mints the id at the daemon layer and calls
        /// `new_with_id`), hence `cfg(test)` to keep the non-test build clean.
        #[cfg(test)]
        pub fn new(model: &Model) -> Self {
            Self::new_with_id(SessionId::new_v4(), model)
        }

        pub fn new_with_id(id: SessionId, model: &Model) -> Self {
            let now = ocean_protocol::now_ms();
            Self {
                id,
                created_ms: now,
                updated_ms: now,
                model: model.id.clone(),
                provider: model.provider.clone(),
                messages: Vec::new(),
                workspace_root: None,
                cwd: None,
                git_branch: None,
                git_commit: None,
                client_type: None,
            }
        }

        /// Tag this session with workspace metadata derived from the caller's cwd.
        /// First bind is unconditional. On resume, rebinds when the incoming cwd
        /// resolves to a different workspace root — so `cd /project-b && ocean
        /// --resume <id>` picks up project-b's context instead of staying stale.
        pub fn bind_workspace(&mut self, cwd: &Path) {
            let new_root = workspace_root(cwd);
            let same_root = self
                .workspace_root
                .as_deref()
                .map(|r| *r == new_root)
                .unwrap_or(false);
            self.cwd = Some(cwd.to_string_lossy().into_owned());
            if same_root {
                // Same project — keep the workspace root and git metadata, but
                // refresh the recorded cwd to the latest launch directory.
                return;
            }
            // Different project (or first bind) — write all fields unconditionally.
            self.workspace_root = Some(new_root.to_string_lossy().into_owned());
            let (branch, commit) = probe_git(cwd);
            self.git_branch = branch;
            self.git_commit = commit;
        }

        pub fn replace_messages(&mut self, messages: Vec<Message>) {
            self.messages = messages;
            self.updated_ms = ocean_protocol::now_ms();
        }
    }

    /// Resolve the workspace root for `cwd`. Tries `git rev-parse --show-toplevel`
    /// first; falls back to the cwd itself when not in a repo or git is absent.
    pub fn workspace_root(cwd: &Path) -> PathBuf {
        match std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
        {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    PathBuf::from(s)
                } else {
                    cwd.to_path_buf()
                }
            }
            _ => cwd.to_path_buf(),
        }
    }

    /// Encode a workspace path as a filesystem-safe slug. Leading slash
    /// dropped, remaining slashes turned into dashes, then prefixed with a
    /// leading dash so directory listings sort intuitively.
    pub fn workspace_slug(root: &Path) -> String {
        let s = root.to_string_lossy();
        let trimmed = s.trim_start_matches('/');
        let slug: String = trimmed
            .chars()
            .map(|c| match c {
                '/' => '-',
                '\\' => '-',
                ':' => '-',
                ' ' => '-',
                c => c,
            })
            .collect();
        format!("-{slug}")
    }

    fn probe_git(cwd: &Path) -> (Option<String>, Option<String>) {
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("rev-parse")
            .arg("--abbrev-ref")
            .arg("HEAD")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() || s == "HEAD" {
                        None
                    } else {
                        Some(s)
                    }
                } else {
                    None
                }
            });
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("rev-parse")
            .arg("--short")
            .arg("HEAD")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                } else {
                    None
                }
            });
        (branch, commit)
    }

    pub fn sessions_dir(config_dir: &Path) -> PathBuf {
        config_dir.join("sessions")
    }

    /// Per-workspace bucket: `<config>/sessions/<workspace-slug>/`.
    fn workspace_dir(config_dir: &Path, workspace_root: &str) -> PathBuf {
        sessions_dir(config_dir).join(workspace_slug(Path::new(workspace_root)))
    }

    /// Migrate any loose `sessions/<uuid>.json` files into
    /// `sessions/legacy/<uuid>.json`. Idempotent — running twice is a
    /// no-op once the loose files are gone. Failures here are best-effort;
    /// we don't crash the daemon over a stuck migration.
    pub fn migrate_legacy_sessions(config_dir: &Path) {
        let dir = sessions_dir(config_dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let legacy = dir.join("legacy");
        let mut made_dir = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if !made_dir {
                if std::fs::create_dir_all(&legacy).is_err() {
                    return;
                }
                made_dir = true;
            }
            if let Some(name) = path.file_name() {
                let _ = std::fs::rename(&path, legacy.join(name));
            }
        }
    }

    /// Env knob for the session-file TTL, in days. Default 90; `0` disables
    /// pruning entirely. Read once per GC pass.
    const SESSION_TTL_ENV: &str = "OCEAN_SESSION_TTL_DAYS";
    const DEFAULT_SESSION_TTL_DAYS: u64 = 90;

    /// Resolve the session-file TTL from `OCEAN_SESSION_TTL_DAYS` into the
    /// `Option<Duration>` that [`session_file_gc`] consumes. Read ONCE here, off
    /// the GC function itself, so the GC stays a pure function of its arguments
    /// (no process-global env reads — that keeps its tests free of env races).
    ///
    /// Semantics:
    /// - unset / unparseable → default 90 days
    /// - `0` → `None` (pruning disabled)
    /// - any other `n` → `Some(n days)`
    ///
    /// Overflow safety (the OCEAN-211 bug): `days * 86_400` can overflow `u64`
    /// for huge inputs — debug builds panicked at startup (GC runs in
    /// `from_env`), release wrapped to a tiny TTL that would delete sessions the
    /// operator meant to keep. See [`ttl_from_days`] for the overflow-safe
    /// conversion.
    pub(crate) fn ttl_from_env() -> Option<std::time::Duration> {
        ttl_from_days(ttl_days_from_env())
    }

    /// The raw day count from the env, before conversion. Split out so the
    /// conversion logic (the overflow-prone part, [`ttl_from_days`]) is
    /// unit-testable without touching the process-global env.
    fn ttl_days_from_env() -> u64 {
        match std::env::var(SESSION_TTL_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(DEFAULT_SESSION_TTL_DAYS),
            Err(_) => DEFAULT_SESSION_TTL_DAYS,
        }
    }

    /// Convert a TTL in days to a `Duration`, overflow-safely.
    ///
    /// - `0` → `None` (pruning disabled).
    /// - overflow on `days * 86_400` → SATURATE to `Duration::MAX`
    ///   ("never prune") rather than panicking (debug) or wrapping down to a
    ///   tiny TTL (release) that would delete sessions the operator meant to
    ///   keep. A TTL that large can't be exceeded by any real file age, so
    ///   nothing is pruned — the safe direction for an absurd input.
    /// - otherwise → `Some(days as a Duration)`.
    pub(crate) fn ttl_from_days(days: u64) -> Option<std::time::Duration> {
        if days == 0 {
            return None; // disabled
        }
        let ttl = days
            .checked_mul(24 * 60 * 60)
            .map_or(std::time::Duration::MAX, std::time::Duration::from_secs);
        Some(ttl)
    }

    /// Prune on-disk session files older than the configured TTL.
    ///
    /// Background: the store caps message *count* per session
    /// (`MAX_SESSION_MESSAGES`) but never deleted old session *files*, so a
    /// long-lived daemon accumulated `sessions/<workspace>/*.json` unbounded —
    /// and [`list`] deserializes every one of them on each call, so the dir's
    /// growth slows listing too. This is distinct from OCEAN-182's in-memory
    /// per-session *lock* prune (that's the registry; this is the files).
    ///
    /// Age signal: file **mtime** via `metadata()`, not the persisted
    /// `updated_ms`. mtime is cheaper (no read + JSON parse per file) and the
    /// save path rewrites the file on every turn, so mtime tracks last-touch
    /// faithfully. GC is opportunistic, but there's no reason to deserialize
    /// thousands of files when a stat suffices.
    ///
    /// Safety: a file whose mtime is older than the TTL (default 90d) cannot
    /// belong to an active session — an in-flight turn rewrites the file on
    /// save, refreshing its mtime — so the TTL gate alone protects live
    /// sessions. No need to consult the in-memory session/lock map.
    ///
    /// `ttl` is the resolved age threshold (`None` = pruning disabled / never
    /// prune); the caller reads `OCEAN_SESSION_TTL_DAYS` once via [`ttl_from_env`]
    /// and passes the result in. Keeping the env OUT of this function makes it a
    /// pure function of its arguments — its tests pass an explicit `ttl` and
    /// never mutate the process-global env, so they can't race each other
    /// (OCEAN-211).
    ///
    /// Returns the number of files pruned. Logs a single info summary when it
    /// prunes anything; silent otherwise. Best-effort: I/O errors on individual
    /// entries are skipped, never fatal — GC must never wedge startup.
    pub fn session_file_gc(config_dir: &Path, ttl: Option<std::time::Duration>) -> usize {
        let Some(ttl) = ttl else {
            return 0; // disabled
        };
        let now = std::time::SystemTime::now();
        let root = sessions_dir(config_dir);

        let mut pruned = 0usize;
        let mut oldest_age: Option<std::time::Duration> = None;

        // Walk one level of workspace buckets plus any loose top-level files.
        let mut json_files: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(bucket) = std::fs::read_dir(&path) {
                        for f in bucket.flatten() {
                            json_files.push(f.path());
                        }
                    }
                } else {
                    json_files.push(path);
                }
            }
        }

        for path in json_files {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(mtime) else {
                continue; // mtime in the future — leave it alone
            };
            if age > ttl && std::fs::remove_file(&path).is_ok() {
                pruned += 1;
                if oldest_age.map_or(true, |o| age > o) {
                    oldest_age = Some(age);
                }
            }
        }

        if pruned > 0 {
            let oldest_days = oldest_age
                .map(|d| d.as_secs() / (24 * 60 * 60))
                .unwrap_or(0);
            let ttl_days = ttl.as_secs() / (24 * 60 * 60);
            tracing::info!(
                pruned,
                oldest_age_days = oldest_days,
                ttl_days,
                "session_file_gc: pruned old session files"
            );
        }
        pruned
    }

    pub fn save(config_dir: &Path, session: &Session) -> anyhow::Result<PathBuf> {
        let dir = match session.workspace_root.as_deref() {
            Some(root) => workspace_dir(config_dir, root),
            None => sessions_dir(config_dir).join("legacy"),
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(format!("{}.json", session.id));
        let json = serde_json::to_string_pretty(session)?;
        // Atomic write: a crash mid-write must never corrupt an existing good
        // transcript. Write to a temp sibling, then rename over the target.
        let tmp = dir.join(format!(".{}.json.tmp", session.id));
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(path)
    }

    pub fn load(config_dir: &Path, id: SessionId) -> anyhow::Result<Session> {
        match load_resumable(config_dir, id)? {
            Some(session) => Ok(session),
            None => anyhow::bail!("session {id} not found"),
        }
    }

    /// Load a session for resumption, distinguishing "no session file exists"
    /// (Ok(None) — safe to start fresh) from "a session file exists but could
    /// not be read or parsed" (Err — must NOT be treated as a fresh session,
    /// or the entire prior transcript is silently discarded mid-chat).
    pub fn load_resumable(config_dir: &Path, id: SessionId) -> anyhow::Result<Option<Session>> {
        let target = format!("{id}.json");
        // Search all workspace buckets + legacy/ + top-level (for forward-compat).
        for candidate in candidate_session_paths(config_dir, &target) {
            if candidate.exists() {
                let text = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("read {}", candidate.display()))?;
                let session = serde_json::from_str(&text)
                    .with_context(|| format!("parse {}", candidate.display()))?;
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    fn candidate_session_paths(config_dir: &Path, filename: &str) -> Vec<PathBuf> {
        let root = sessions_dir(config_dir);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    out.push(p.join(filename));
                }
            }
        }
        // Final fallback: a loose file in the legacy layout we haven't migrated yet.
        out.push(root.join(filename));
        out
    }

    pub fn detail(config_dir: &Path, id: SessionId) -> anyhow::Result<SessionDetail> {
        let session = load(config_dir, id)?;
        Ok(session_detail(session))
    }

    /// List sessions, optionally scoped to a single workspace root.
    /// `workspace_root = None` returns every session across every bucket.
    pub fn list(
        config_dir: &Path,
        workspace_root: Option<&str>,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        let dir = sessions_dir(config_dir);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for bucket in std::fs::read_dir(&dir)?.flatten() {
            let bucket_path = bucket.path();
            if !bucket_path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&bucket_path)?.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(session) = serde_json::from_str::<Session>(&text) else {
                    continue;
                };
                if let Some(filter) = workspace_root {
                    if session.workspace_root.as_deref() != Some(filter) {
                        continue;
                    }
                }
                out.push(SessionSummary {
                    id: session.id,
                    model: session.model,
                    turns: session.messages.len() as u32,
                    title: first_user_text(&session.messages),
                    workspace_root: session.workspace_root,
                    git_branch: session.git_branch,
                    updated_ms: Some(session.updated_ms),
                });
            }
        }
        // Newest first by updated_ms; fall back to id ordering when missing.
        out.sort_by(|a, b| {
            b.updated_ms
                .unwrap_or(0)
                .cmp(&a.updated_ms.unwrap_or(0))
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(out)
    }

    fn first_user_text(messages: &[Message]) -> String {
        messages
            .iter()
            .find_map(|message| match message {
                Message::User { content, .. } => content
                    .iter()
                    .find_map(|content| content.as_text().map(truncate_title)),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn session_detail(session: Session) -> SessionDetail {
        let title = first_user_text(&session.messages);
        let transcript = session
            .messages
            .iter()
            .map(transcript_entry)
            .collect::<Vec<_>>();
        let tool_context = session
            .messages
            .iter()
            .flat_map(tool_context_entries)
            .collect::<Vec<_>>();
        let messages = session
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
            .collect::<Vec<_>>();

        SessionDetail {
            id: session.id,
            created_ms: session.created_ms,
            updated_ms: session.updated_ms,
            model: session.model,
            provider: session.provider,
            turns: session.messages.len() as u32,
            title,
            state: SessionRunState::Stored,
            resumable: true,
            active_requests: Vec::new(),
            pending_permissions: Vec::new(),
            transcript,
            tool_context,
            messages,
            workspace_root: session.workspace_root,
            cwd: session.cwd,
            git_branch: session.git_branch,
            git_commit: session.git_commit,
            client_type: session.client_type,
            // Resolved by the daemon's `enrich_session_detail` from
            // `workspace_root` (it owns the project store path); the agent layer
            // has no project index, so it leaves the binding unresolved here.
            owning_project: None,
        }
    }

    pub(super) fn transcript_entry(message: &Message) -> SessionTranscriptEntry {
        match message {
            Message::User { content, timestamp } => SessionTranscriptEntry {
                role: "user".into(),
                timestamp_ms: Some(*timestamp),
                text: text_from_content(content),
                images: images_from_content(content),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
            },
            Message::Assistant(assistant) => SessionTranscriptEntry {
                role: "assistant".into(),
                timestamp_ms: Some(assistant.timestamp),
                text: text_from_content(&assistant.content),
                images: images_from_content(&assistant.content),
                tool_call_id: None,
                tool_name: None,
                is_error: assistant.error_message.as_ref().map(|_| true),
            },
            Message::ToolResult(tool) => SessionTranscriptEntry {
                role: "tool".into(),
                timestamp_ms: Some(tool.timestamp),
                text: text_from_content(&tool.content),
                images: images_from_content(&tool.content),
                tool_call_id: Some(tool.tool_call_id.clone()),
                tool_name: Some(tool.tool_name.clone()),
                is_error: Some(tool.is_error),
            },
        }
    }

    fn tool_context_entries(message: &Message) -> Vec<SessionToolContext> {
        match message {
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(SessionToolContext {
                        kind: "call".into(),
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        arguments: Some(arguments.clone()),
                        is_error: None,
                        text: String::new(),
                    }),
                    _ => None,
                })
                .collect(),
            Message::ToolResult(tool) => vec![SessionToolContext {
                kind: "result".into(),
                tool_call_id: tool.tool_call_id.clone(),
                tool_name: tool.tool_name.clone(),
                arguments: None,
                is_error: Some(tool.is_error),
                text: text_from_content(&tool.content),
            }],
            Message::User { .. } => Vec::new(),
        }
    }

    fn text_from_content(content: &[Content]) -> String {
        content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                Content::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Project `Content::Image` blocks to lightweight `ImageMeta` (mime_type
    /// only — never the base64 `data`). `text_from_content` drops Image blocks,
    /// so without this an image-bearing turn would render as empty text and a
    /// replaying client would lose all evidence an image was attached
    /// (OCEAN-177). The raw bytes remain in `SessionDetail::messages`.
    fn images_from_content(content: &[Content]) -> Vec<ImageMeta> {
        content
            .iter()
            .filter_map(|content| match content {
                Content::Image { mime_type, .. } => Some(ImageMeta {
                    mime_type: mime_type.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    fn truncate_title(text: &str) -> String {
        let squashed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if squashed.chars().count() <= 70 {
            squashed
        } else {
            format!("{}…", squashed.chars().take(70).collect::<String>())
        }
    }
}

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

    // Missing plugins dir → no plugins, no error, unchanged behavior.
    #[tokio::test]
    async fn missing_plugins_dir_yields_no_providers() {
        let config_dir = temp_config_dir("plugin-missing-dir");
        // config_dir intentionally not created.
        let providers = discover_plugin_providers(&config_dir).await;
        assert!(providers.is_empty(), "no plugins dir → no providers");
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
            test_env,
        }
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
    fn room_guidance_matches_track0_rooms() {
        assert!(room_guidance(RoomId::Pm).contains("PM room"));
        assert!(room_guidance(RoomId::Writers).contains("Writers Room"));
        assert!(room_guidance(RoomId::OrchMesh).contains("ORCH + MESH"));
        assert!(room_guidance(RoomId::Review).contains("Review Room"));
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
        s.client_type = Some("surface-gpui".into());
        let id = s.id;
        session::save(&config_dir, &s).unwrap();

        let loaded = session::load_resumable(&config_dir, id).unwrap().unwrap();
        assert_eq!(
            loaded.client_type.as_deref(),
            Some("surface-gpui"),
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
            auth_file: None,
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
        // Primary deepseek has no credential here (degraded), but an Anthropic key
        // is present — the default fallback order leads with Anthropic.
        let degraded = state_from_provider_config(provider_config(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            false,
        ))
        .unwrap();
        let env = provider_env(&[("ANTHROPIC_API_KEY", "sk-ant")]);
        let out = AgentRuntime::resolve_turn_state_with_failover(degraded, &env)
            .expect("a ready alternate must be selected");
        assert_eq!(
            out.provider_config.selection.provider,
            ProviderId::Anthropic,
            "degraded primary must route to the ready anthropic alternate"
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
                content: vec![Content::ToolCall {
                    id: tool_call_id.clone(),
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
        assert_eq!(detail.tool_context.len(), 2);
        assert_eq!(detail.tool_context[0].kind, "call");
        assert_eq!(detail.tool_context[0].tool_name, "read");
        assert_eq!(detail.tool_context[1].kind, "result");
        assert_eq!(detail.tool_context[1].text, "contents");
        let _ = std::fs::remove_dir_all(config_dir);
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

mod system_prompt {
    use super::*;

    const BASE_SYSTEM_PROMPT: &str = r#"You are an Ocean agent — a local-first, Rust-native coding agent with real permissions and agency. You are NOT the daemon. `ocean-daemon` is just the small HTTP+SSE runtime that runs you on the operator's own machine and routes turns to you; it is your body, not your identity. The operator addresses you through a surface (TUI, web, native, CLI, voice) over one product API (POST /v1/agent/turns + GET /v1/agent/events) — the surface decides your role and tone, the daemon decides nothing about who you are. You act on the operator's machine on their behalf: when they tell you to do something, do it — check the git, read the files, run the commands, drive the tools. Don't ask permission for work you've been asked to do, and don't come back with "I got nothing" when you haven't actually looked.

## What ocean-os is

A Rust monorepo at github.com/Risingtides-dev/ocean-os. Crates:
- ocean-core — shared daemon/client wire types: requests, responses, events, sessions, rooms
- ocean-daemon  — the HTTP service that runs you
- ocean-runtime — the agent loop, tool execution, streaming
- ocean-protocol — provider implementations (OpenAI-compatible, Anthropic, Google)
- ocean-providers — credential + model resolution
- ocean-agent — session storage, system prompt (you're reading from here), permission policy
- ocean-agent-sdk — embedding/SDK surface for Rust clients
- ocean-tui — the F1 PM cockpit + workspace rooms
- ocean-cli — one-shot CLI

Companion repo (separate, non-Rust): github.com/Risingtides-dev/ocean-surface, the planned web + voice client.

## How you differ from Claude Code, Cursor, Aider, Codex

- You run as a long-lived daemon, not a per-invocation CLI. Multiple clients share one brain and one session store. Switch from TUI to phone mid-conversation, you're still you.
- Sessions are workspace-bound (git toplevel or cwd). `/sessions` shows just the current project unless asked for all.
- You speak any OpenAI-compatible provider (DeepSeek, OpenAI, xAI, OpenAI-compat endpoints) plus Anthropic and Google natively. Model is hot-swappable at runtime via `/model <name>` — no daemon restart.
- Reasoning models (DeepSeek reasoner + v4-pro, OpenAI o-series) surface their chain-of-thought as collapsible "thinking" blocks, not buried in logs.
- Clients stream in real time delta-by-delta with markdown rendering, inline components, collapsible thinking pills. The web surface renders rich HTML; the TUI renders markdown in the terminal.
- Local-first. Your sessions, your keys, your machine. No cloud relay.

## Tools available

read, write, edit (files); ls, glob (filesystem nav); grep (content search); bash (shell with timeout); fetch (HTTP GET); todo_write (track multi-step work).

**Browser control** — you can drive a real Chrome over the DevTools Protocol:
`browser_navigate` (open a URL), `browser_read_page` (structured read: title, URL,
visible interactive elements each with a `ref` selector, and visible text),
`browser_screenshot` (PNG of the page), `browser_click` (by `ref` from read_page,
OR by `x`/`y` pixel for canvas/video), `browser_type` (type into the focused
element), `browser_key` (press Enter/Tab/etc.), `browser_scroll`, `browser_eval_js`
(run JS in the page), `browser_console`, `browser_network`.

## Driving the browser

When the user asks you to do anything on the web — open a site, fill a form, click
through a flow, scrape a page, check something live — USE THE BROWSER TOOLS. Don't
answer web tasks from memory; actually drive Chrome and report what you see.

The loop: `browser_navigate` → `browser_read_page` to see what's on the page and
get element `ref`s → `browser_click {ref}` / `browser_type {text}` / `browser_key`
to act → `browser_read_page` again to confirm. Prefer `browser_read_page` (cheap,
precise) over screenshots. Only `browser_screenshot` when the page is visual
(canvas, video, maps) or `read_page` reports `visual_hint: true` — then click by
`x`/`y` from what you see. Chrome launches automatically on your first browser
call and persists logins across turns, so a site you logged into stays logged in.
Navigation, clicks, typing, keypresses, and eval prompt the user for permission;
reads and screenshots don't.

## How to respond

**Conversational questions** ("what is X", "how does Y work", "tell me about Z", greetings, opinions): answer directly from what you know. Do NOT reach for tools to investigate. If the answer is genuinely in this repo, you already know — that information is above. If it's a question about THE USER's project specifics, then yes, read files.

**Concrete code tasks** ("fix X", "add Y", "refactor Z", "find where ABC happens"): read first, then act. Use grep/glob to locate, read to understand, edit/write to change. Run the build or tests when the change warrants it.

**After tool calls**: ALWAYS produce a text reply summarizing what you found or did. Never end a turn with only a tool result. The user reads your text, not your tool output.

## Style

- Be direct. Skip "Great question!" and other preamble.
- Match the user's energy. If they're casual, be casual. If they're terse, be terse.
- Use markdown — the TUI renders it. Bold for emphasis, code spans for filenames/symbols, numbered lists for steps.
- Show, don't editorialize. Cite file paths with line numbers when useful (e.g. `crates/ocean-tui/src/main.rs:3905`).
- Don't apologize for taking actions you were asked to take.

## Rich web surface — render components when the client supports them

Some Ocean clients render live, interactive UI components, not just text. When the current client explicitly says it supports the web/Leptos component surface, lean on `component_render`. When the current client is GPUI, TUI, CLI, or voice, prefer medium-appropriate text and do not assume web components render. The full web kit:

- Tabular data → `table` ({columns, rows}). NEVER hand-build a markdown pipe table when `table` fits.
- Task/status boards → `kanban`. Collecting input → `form` (then `component_wait` for the submit).
- Live task → `progress` (reuse the id with replace:true to advance). Multi-step plan → `timeline` (flip steps done/active/pending; re-render to advance).
- KPIs / metrics (views, plays, saves) → `stat`. Numeric series to chart → `chart` (bar or line).
- Project structure / file listing → `file_tree`. Showing code edits → `diff`. A copy-able snippet → `code`.
- An important note or warning → `callout`. Images / screenshots / art → `gallery`.
- Yes/no before something destructive → `confirm` (then `component_wait` for the answer).
- Several at once → `dashboard`. Long prose / explanation → plain markdown text; don't over-componentize.

The `component_render` tool description carries the exact props schema for each kind. Use it; don't guess the shape. After rendering, still give a short text reply — the component complements your words, it doesn't replace them.

You operate from the user's project directory (passed per turn). Look for AGENTS.md, .ocean/AGENTS.md, CLAUDE.md, or .pi/instructions.md in the project tree — those are project-specific instructions that override or extend the above.
"#;

    /// Build the system prompt, optionally scoped to `cwd` and `client_type`.
    pub fn build_system_prompt(cwd: Option<&str>, client_type: Option<&str>) -> String {
        // Production resolves the surface profile against the real assistants
        // root (`OCEAN_ASSISTANTS_DIR`, else the Ocean config dir). Tests call
        // [`build_system_prompt_from`] with an explicit temp root for isolation.
        build_system_prompt_from(cwd, client_type, assistants_root().as_deref())
    }

    /// Inner form of [`build_system_prompt`] that resolves any file-loaded
    /// surface profile against an explicit `assistants_root` instead of the
    /// process-global one. This is the isolation seam (OCEAN-285): tests pass a
    /// temp root (or `None`) so a surface-profile lookup never reads — or
    /// depends on the contents of — the operator's real
    /// `~/.config/ocean-rs/assistants`, and never has to mutate process env.
    /// Passing `assistants_root()` reproduces production behavior exactly.
    fn build_system_prompt_from(
        cwd: Option<&str>,
        client_type: Option<&str>,
        assistants_root: Option<&Path>,
    ) -> String {
        let cwd = cwd
            .and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(s))
                }
            })
            .or_else(|| std::env::current_dir().ok());
        let project = cwd
            .as_ref()
            .map(|p| load_project_prompt(p))
            .unwrap_or_default();
        if project.is_empty() {
            append_client_type_from(BASE_SYSTEM_PROMPT, client_type, assistants_root)
        } else {
            let prompt =
                format!("{BASE_SYSTEM_PROMPT}\n----- project instructions -----\n{project}");
            append_client_type_from(&prompt, client_type, assistants_root)
        }
    }

    const WEB_SURFACE_COMPONENT_PROMPT: &str = r#"
## Ocean web surface component UX

You are speaking through Ocean Surface, which renders live Leptos components from `component_render` events. Treat components as task UI, not chat decoration.

Use components aggressively when they fit:

- **Running work** → `progress`. Reuse the same id with `replace:true` as work advances; finish with a short summary and often a `callout`.
- **Multi-step plan/status** → `timeline`. Flip steps from `pending` → `active` → `done`/`error` with `replace:true`.
- **Structured rows/columns** → `table`. Do not fake tables with markdown when `table` fits.
- **Important result/warning/error** → `callout` with `variant: info|success|warn|error`.
- **Code edits** → `diff`; copyable commands/config/source → `code`.
- **Need user input** → `form`, then `component_wait` if the turn depends on the answer.
- **Important yes/no or destructive action** → `confirm`, then `component_wait` before acting.
- **Locations/POIs/routes/search areas** → `map` with `markers` and usually `fit_markers:true`.
- **KPIs/numbers** → `stat` or `chart`.
- **Multiple panels at once** → `dashboard`.

Common patterns:

- Long-running dev task: `progress(start)` → `progress(update)` → `diff/table/callout` → concise text summary.
- Code edit: `timeline(plan)` → `progress(while editing/testing)` → `diff(show change)` → `callout(result)`.
- User decision: `callout(context)` → `confirm` → `component_wait` → act on result.
- Data-heavy answer: render `table`/`stat`/`chart`/`map` first, then explain briefly.

Never end a turn with only a component. Always include short text so non-rich clients retain context.

Reference docs in this repo:
- `docs/AGENT_RENDER_PROTOCOL.md`
- `docs/OCEAN_SURFACE_COMPONENT_PROMPT_GUIDE.md`
- `docs/PAGE_LEVEL_AGENT_SURFACE_UI_NOTE.md`
"#;

    const GPUI_SURFACE_PROMPT: &str = r#"
## Ocean GPUI surface UX

You are inside Ocean GUI, an agent-native desktop work surface.
When the user asks for canvas, board, workflow, storyboard, diagram, or spatial work, use `surface_patch`.
Do not draw ASCII diagrams in chat.
Do not tell the user to draw manually.
Use the injected canvas ledger to choose ids, coordinates, containers, and update targets.
If exact x/y is not important, omit it and let the app place the component.
Always include a short text summary after patching.

Do not use Leptos/web-only component rendering for chat UI. Use `surface_patch` for native canvas mutations. The native surface does not render Leptos components or arbitrary HTML inside chat, so avoid `component_render`, `component_wait`, and web/HTML-oriented widgets unless the user explicitly asks for a protocol test. For non-canvas output, use compact markdown, file paths, commands, and short status text.
"#;

    const TUI_SURFACE_PROMPT: &str = r#"
## Ocean TUI surface UX

You are speaking through the Ocean TUI. The user sees a terminal interface with basic markdown rendering. Keep responses concise and terminal-native.

Do not use `component_render`, `component_wait`, web widgets, Leptos component assumptions, maps, dashboards, forms, or HTML-oriented UI unless the user explicitly asks for a protocol test. Prefer short markdown, file paths, command output summaries, and state updates that fit a terminal transcript.
"#;

    const SLACK_SURFACE_PROMPT: &str = r#"
## Ocean Slack surface UX

You are an Ocean assistant living **inside** a Slack workspace. You were mentioned in a thread, DMed, or addressed in a channel, and you reply back in that same place. Slack is the room you're standing in — behave like a sharp, present teammate in that room, not a bot pasting output into it.

**Where you reply:** every turn arrives from a thread, a DM, or a channel mention. Always reply in the *same context* — a threaded message stays in its thread, a DM stays in the DM; never break a threaded conversation out into the channel root. Treat the thread as the unit of memory: one thread = one ongoing task; don't restate what's already established in it. Assume you're often read on a phone, in passing — lead with the answer.

**Style — Slack-native:** be concise. Slack is chat, not a document. A good reply is one to four short paragraphs or a tight list, not an essay with headings. Front-load the takeaway: first line is the answer or the status; caveats and next steps come after, only if they earn their place. Compose the whole reply and send it once — don't dribble out five messages. Match the room's register (relaxed in an internal channel, tighter in a client-facing one). Emoji are punctuation, not decoration — a ✅ for done, 👀 for "on it", ⚠️ for a risk, used sparingly.

**Format — Slack mrkdwn, NOT Markdown.** Slack does not render standard Markdown:
- **Bold** is `*single asterisks*`, _italic_ is `_underscores_`, strikethrough is `~tildes~`. Never use `**double asterisks**` — Slack shows the literal stars.
- No Markdown headings (`#`, `##`) — they render as literal hashes. Use a **bold lead-in line** instead.
- No Markdown tables — pipe-and-dash renders as raw text. Use a short bulleted or `key: value` list, or render a canvas for anything tabular/large.
- Lists: plain `•` or `-`, kept shallow (mobile flattens deep nesting). Inline `code` and triple-backtick fences are fine; don't dump long logs inline.
- Links: prefer `<https://url|readable label>` over naked URLs. @-mention a person only when you genuinely need their eyes; never @-here/@-channel unless explicitly asked.

When in doubt about rendering, prefer plain text with a bold lead-in over rich syntax that might leak literal characters into the channel.

**When to use a Slack Canvas:** render into a canvas (the `surface-canvas` surface) instead of a message when the content is too big or structured to read inline — a gallery, a status/queue board, a multi-row table, a long structured summary, or anything the operator will want to revisit or share. Keep it inline for direct answers, short status, confirmations, or a link or two. When you create or update a canvas, also post a short one-line message in-thread pointing at it — never drop a canvas silently. Prefer appending to an existing canvas over overwriting one someone may be mid-review on.

**Safety on Slack:** act only on inbound turns — never auto-post on startup, connect, or a schedule of your own. Confirm before anything irreversible or wide-reach (posting into a new channel, @-channel/@-here, deleting a canvas or message, anything client-visible); routine in-thread replies need no confirmation, so be fast there. Stay in your lane — use only the tools your profile grants, and say so plainly if a request needs a capability you don't have. Never paste secrets, tokens, raw credentials, or internal IDs into a channel.
"#;

    const CANVAS_SURFACE_PROMPT: &str = r#"
## Ocean canvas surface UX

You are rendering onto a **canvas** — a rich, persistent surface (a Slack Canvas or equivalent) meant to hold an *artifact*, not a conversation. The canvas is for output someone will scroll, revisit, and share; the chat thread is for the conversation around it.

**Reach for the canvas when** the content is a gallery of generated media, a status/queue board, a multi-row table, a long structured summary, or anything large or structured enough that it reads badly inline. **Keep it in the message** when it's a direct answer, a short status, a confirmation, or a link or two — don't canvas a one-liner.

**Always pair the canvas with a message.** When you create or update a canvas, post a short one-line note in the originating thread — context plus the canvas reference ("Updated the gallery canvas 👆 — 6 new clips."). Never drop or mutate a canvas silently; the thread must stay readable on its own.

**Prefer append over overwrite.** For an ongoing task, update or extend the existing canvas rather than blowing it away — someone may be mid-review on it. Append-only is the safer default; destructive rewrites need a reason and usually a confirmation.

**Structure for scanning.** Canvases tolerate more structure than a Slack message — headings, sections, and tables are appropriate here. Organize so the most important state is at the top and the artifact stays self-explanatory when revisited later out of context. Drive canvas create/update through the surface's tools, not by hand; never leak secrets or internal IDs into a shared canvas.
"#;

    const MOBILE_SURFACE_PROMPT: &str = r#"
## Ocean mobile surface UX

You are speaking through the **Ocean mobile app** — a compact, on-the-go screen. Assume the reply is read on a phone, one-handed, in passing, and possibly half-listened-to or read aloud.

**Be short and answer-first.** Lead with the answer or the status in the first line; one to three short sentences is the default. Detail, caveats, and next steps come only if they earn their place — offer to expand rather than dumping everything. No long preambles, no thinking out loud.

**Keep it readable on a small screen.** Short paragraphs and shallow bullet lists only; avoid wide tables, dense code blocks, long file paths, and anything that forces horizontal scrolling. Speak plainly — favor wording that survives being read aloud, since mobile is often a hands-busy context adjacent to voice. Don't lean on heavy visual components or rich widgets the compact surface can't show well.

**Confirm consequential actions in one line.** Real or irreversible actions still get a quick read-back before you act, but keep it tight — a single confirming sentence, not a form. Routine answers need no ceremony; be fast. Never paste secrets or internal IDs into the reply.
"#;

    fn web_surface_prompt(prompt: &str, client_label: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **{client_label}**. Responses render as HTML with rich interactive Leptos components, inline images, and live UI.\n\n{WEB_SURFACE_COMPONENT_PROMPT}\n"
        )
    }

    /// The Chrome extension side panel. Same Leptos render surface as the web
    /// PWA (so the full component kit applies), but it is **docked inside the
    /// user's real Chrome** — which changes how you should think about the
    /// browser tools.
    fn extension_surface_prompt(prompt: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\n\
You are speaking through the **Ocean cockpit — the Chrome extension side panel \
docked inside the user's own Chrome window**, not a detached web app. Responses \
render as HTML with the rich interactive Leptos components, inline images, and \
live UI described below.\n\n\
**You are attached to the browser the user is looking at.** When they say \"this \
page\", \"this video\", \"this profile\", \"here\", or ask what's on screen, they \
mean the tab currently open next to you in that same Chrome. Your browser tools \
(`browser_read_page`, `browser_screenshot`, `browser_click`, `browser_navigate`, \
etc.) act on **that live browser** — so don't answer from memory and don't assume \
you can't see it. Call `browser_read_page` to read what's actually on the tab \
before responding about it. Logins and open tabs persist across turns because it \
is the user's real, signed-in browser session.\n\n\
{WEB_SURFACE_COMPONENT_PROMPT}\n"
        )
    }

    fn gpui_surface_prompt(prompt: &str, client_label: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **{client_label}**.\n\n{GPUI_SURFACE_PROMPT}\n"
        )
    }

    fn tui_surface_prompt(prompt: &str) -> String {
        format!("{prompt}\n\n## Current client\n\n{TUI_SURFACE_PROMPT}\n")
    }

    fn cli_surface_prompt(prompt: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through the **Ocean CLI** — a one-shot terminal tool. No interactivity, just text output.\n"
        )
    }

    fn voice_surface_prompt(prompt: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **Leo (voice)** — a voice-only interface. Responses should be concise and spoken aloud. Do not use visual components.\n"
        )
    }

    /// Canonical surface flag for a `client_type` string. This is the single
    /// source of truth shared by the per-turn flag stamp (Fix 2), the
    /// surface-switch notice (Fix 3), and the per-surface profile lookup
    /// (Fix 5). It is reconciled with the `ocean-agents` surface-profile
    /// registry: every flag here maps 1:1 to an `assistants/<DIR>` profile
    /// directory via [`surface_dir`]. Unknown clients get `[?]`.
    pub fn surface_flag(client_type: Option<&str>) -> &'static str {
        match client_type {
            Some("surface-extension") => "BRWSR",
            Some("tui") => "TUI",
            Some("surface-web") => "WEB",
            Some("surface-gpui") | Some("surface-native") => "GUI",
            Some("cli") => "CLI",
            Some("leo-voice") => "VOX",
            Some("acp-zed") => "ACP",
            Some("surface-slack") => "SLACK",
            Some("surface-canvas") => "CNVS",
            Some("surface-mobile") => "MOBL",
            _ => "?",
        }
    }

    /// The `assistants/<DIR>` profile directory name for a `client_type`.
    /// Mirrors [`surface_flag`] (same labels). This is the key the file-loaded
    /// profile path resolves against in [`load_surface_profile_from`].
    ///
    /// File-loaded profiles are implemented — the runtime prefers
    /// `assistants/<surface_dir>/system.md` when present, falling back to const
    /// seeds. Author profiles in `ocean-agents/assistants/<DIR>/`; loaded at
    /// runtime, no rebuild.
    ///
    /// Still parked for John: org file-tree / namespacing so many agents can
    /// share one surface without their profiles/tools bleeding — symlink-vs-
    /// resolver for composing agent-dir CLAUDE.md + the surface profile in one
    /// `load_project_prompt` ancestor-walk.
    #[allow(dead_code)]
    pub fn surface_dir(client_type: Option<&str>) -> &'static str {
        surface_flag(client_type)
    }

    /// Root of the editable per-surface profile tree. ocean-agents owns the
    /// content (`assistants/<DIR>/system.md`); the daemon only *reads* it at
    /// turn time so a surface's role/SOPs/limits can be hot-reconfigured
    /// without a Rust rebuild. Override with `OCEAN_ASSISTANTS_DIR`; default is
    /// `assistants/` under the Ocean config dir.
    fn assistants_root() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("OCEAN_ASSISTANTS_DIR") {
            if !dir.is_empty() {
                return Some(PathBuf::from(dir));
            }
        }
        // Mirror the daemon's config-dir resolution (XDG / ~/.config/ocean-rs).
        dirs_config_dir().map(|c| c.join("assistants"))
    }

    fn dirs_config_dir() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("ocean-rs"));
            }
        }
        std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config").join("ocean-rs"))
    }

    /// Prefer an on-disk surface profile for this `client_type` over the
    /// compiled-in const, resolved against an already-resolved optional
    /// assistants root. Returns the file's contents when present and non-empty,
    /// else `None` (caller falls back to the seed const). This is the R2
    /// file-loaded seam — the consts stay as seed + fallback, but the editable
    /// file wins, enabling hot-reconfigure (ocean-agents).
    ///
    /// Production passes `assistants_root()` (real `OCEAN_ASSISTANTS_DIR` / config
    /// dir); tests pass a temp root for isolation. `None` root means "no
    /// assistants dir", so the caller takes the const fallback (OCEAN-285).
    fn load_surface_profile_opt(
        assistants_root: Option<&Path>,
        client_type: Option<&str>,
    ) -> Option<String> {
        load_surface_profile_from(assistants_root?, client_type)
    }

    /// Inner form that reads from an explicit root — keeps the file-loaded
    /// logic testable without mutating global env.
    fn load_surface_profile_from(root: &Path, client_type: Option<&str>) -> Option<String> {
        let dir = surface_dir(client_type);
        if dir == "?" {
            return None;
        }
        let path = root.join(dir).join("system.md");
        let content = std::fs::read_to_string(&path).ok()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Append the per-surface ("current client") section to a base prompt,
    /// resolving any file-loaded surface profile against an explicit optional
    /// assistants root rather than the process-global one (OCEAN-285 isolation
    /// seam — see [`build_system_prompt_from`]). Production passes
    /// `assistants_root()`; tests pass a temp root (or `None`).
    fn append_client_type_from(
        prompt: &str,
        client_type: Option<&str>,
        assistants_root: Option<&Path>,
    ) -> String {
        // File-loaded surface profile wins when present (R2 / ocean-agents
        // hot-reconfigure). Falls through to the seed consts below otherwise.
        if let Some(profile) = load_surface_profile_opt(assistants_root, client_type) {
            return format!("{prompt}\n\n## Current client\n\n{profile}\n");
        }
        match client_type {
            Some("tui") => tui_surface_prompt(prompt),
            Some("surface-web") => web_surface_prompt(prompt, "Ocean Surface (web) — a browser PWA"),
            Some("surface-extension") => extension_surface_prompt(prompt),
            Some("surface-gpui") => gpui_surface_prompt(prompt, "Ocean GUI (GPUI native desktop)"),
            Some("surface-native") => gpui_surface_prompt(prompt, "Ocean native surface"),
            Some("cli") => cli_surface_prompt(prompt),
            Some("leo-voice") => voice_surface_prompt(prompt),
            // Slack / Canvas / Mobile are first-class now (ocean-agents R3).
            // These are the daemon-side compiled fallbacks — real, surface-aware
            // profiles, not bare-label stubs. A file-loaded `assistants/<DIR>`
            // profile (resolved above) overrides them when present; this is what
            // the runtime falls back to when no on-disk profile exists. They
            // mirror the shape and intent of the authored ocean-agents profiles
            // (`assistants/SLACK/system.md` et al.).
            Some("surface-slack") => slack_surface_prompt(prompt),
            Some("surface-canvas") => canvas_surface_prompt(prompt),
            Some("surface-mobile") => mobile_surface_prompt(prompt),
            Some(other) => format!("{prompt}\n\n## Current client\n\nYou are speaking through an unknown client: `{other}`.\n"),
            None => prompt.to_string(),
        }
    }

    /// Slack surface — an Ocean assistant living *inside* a Slack workspace,
    /// replying in threads/DMs/channels. Compiled fallback mirroring the
    /// authored `assistants/SLACK/system.md` house profile (R3): concise,
    /// thread-aware, Slack-mrkdwn-aware, canvas-aware. Overridden by a
    /// file-loaded SLACK profile when one exists on disk.
    fn slack_surface_prompt(prompt: &str) -> String {
        format!("{prompt}\n\n## Current client\n\n{SLACK_SURFACE_PROMPT}\n")
    }

    /// Canvas surface — rich, persistent artifact rendering (a Slack Canvas or
    /// equivalent canvas surface) paired with an in-thread message. Compiled
    /// fallback; overridden by a file-loaded CNVS profile when present.
    fn canvas_surface_prompt(prompt: &str) -> String {
        format!("{prompt}\n\n## Current client\n\n{CANVAS_SURFACE_PROMPT}\n")
    }

    /// Mobile surface — a compact, on-the-go screen read in passing. Compiled
    /// fallback; overridden by a file-loaded MOBL profile when present.
    fn mobile_surface_prompt(prompt: &str) -> String {
        format!("{prompt}\n\n## Current client\n\n{MOBILE_SURFACE_PROMPT}\n")
    }

    fn load_project_prompt(start: &Path) -> String {
        const FILES: &[&str] = &[
            "AGENTS.md",
            ".ocean/AGENTS.md",
            "CLAUDE.md",
            ".pi/instructions.md",
        ];
        let mut found = Vec::new();
        for ancestor in start.ancestors() {
            for name in FILES {
                let path = ancestor.join(name);
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        found.push((path, trimmed.to_string()));
                    }
                }
            }
        }
        if found.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for (path, content) in found {
            out.push_str(&format!("\n\n----- {} -----\n", path.display()));
            out.push_str(&content);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::{
            build_system_prompt_from, load_surface_profile_from, surface_dir, surface_flag,
        };
        use std::path::Path;
        use tempfile::TempDir;

        /// A fresh, empty, auto-cleaned assistants root. Building a system prompt
        /// against this root resolves NO on-disk surface profile (the dir holds
        /// no `<DIR>/system.md`), so `build_system_prompt_from` takes the
        /// compiled-in const fallback — the path these tests actually assert on.
        ///
        /// This is the OCEAN-285 isolation primitive: every prompt-building test
        /// pins its own temp root instead of letting the lookup fall through to
        /// the operator's real `~/.config/ocean-rs/assistants`. No process env is
        /// read or mutated, so parallel `cargo test` threads can't race, and the
        /// result never depends on whatever profiles happen to exist on the box.
        fn empty_assistants_root() -> TempDir {
            tempfile::Builder::new()
                .prefix("ocean-assistants-empty-")
                .tempdir()
                .expect("create temp assistants root")
        }

        /// An auto-cleaned assistants root seeded with a single
        /// `<surface_dir>/system.md` for `client_type`, holding `body`. Used to
        /// exercise the file-loaded-profile-wins path in isolation.
        fn seeded_assistants_root(client_type: &str, body: &str) -> TempDir {
            let root = empty_assistants_root();
            let dir = root.path().join(surface_dir(Some(client_type)));
            std::fs::create_dir_all(&dir).expect("create surface dir");
            std::fs::write(dir.join("system.md"), body).expect("write seeded profile");
            root
        }

        #[test]
        fn file_loaded_surface_profile_wins_over_const() {
            // R2: an on-disk assistants/<DIR>/system.md must override the seed
            // const so a surface can be reconfigured without a rebuild. Isolated
            // against a temp root (auto-cleaned), never the real config.
            let root = seeded_assistants_root("surface-slack", "CUSTOM SLACK PROFILE FROM FILE");

            let loaded = load_surface_profile_from(root.path(), Some("surface-slack"));
            assert_eq!(loaded.as_deref(), Some("CUSTOM SLACK PROFILE FROM FILE"));

            // Unknown surface never resolves a file.
            assert!(load_surface_profile_from(root.path(), Some("who-knows")).is_none());
            // Missing file → None (falls back to const).
            assert!(load_surface_profile_from(root.path(), Some("tui")).is_none());

            // And the loaded file actually wins inside the full prompt build.
            let prompt = build_system_prompt_from(None, Some("surface-slack"), Some(root.path()));
            assert!(prompt.contains("CUSTOM SLACK PROFILE FROM FILE"));
        }

        #[test]
        fn missing_profile_root_falls_back_to_const() {
            let root = Path::new("/nonexistent/ocean/assistants/root");
            assert!(load_surface_profile_from(root, Some("surface-slack")).is_none());
        }

        #[test]
        fn surface_flag_taxonomy_is_canonical() {
            // Canonical map reconciled with the ocean-agents surface-profile
            // registry (addendum R1). These exact labels are load-bearing —
            // downstream keys its assistants/<DIR> tree against them, so a
            // rename here is a cross-repo break.
            assert_eq!(surface_flag(Some("surface-extension")), "BRWSR");
            assert_eq!(surface_flag(Some("tui")), "TUI");
            assert_eq!(surface_flag(Some("surface-web")), "WEB");
            assert_eq!(surface_flag(Some("surface-gpui")), "GUI");
            assert_eq!(surface_flag(Some("surface-native")), "GUI");
            assert_eq!(surface_flag(Some("cli")), "CLI");
            assert_eq!(surface_flag(Some("leo-voice")), "VOX");
            assert_eq!(surface_flag(Some("acp-zed")), "ACP");
            // Slack / Canvas / Mobile are first-class now (R3), not future.
            assert_eq!(surface_flag(Some("surface-slack")), "SLACK");
            assert_eq!(surface_flag(Some("surface-canvas")), "CNVS");
            assert_eq!(surface_flag(Some("surface-mobile")), "MOBL");
            // Unknown / absent → sentinel, never a panic.
            assert_eq!(surface_flag(Some("who-knows")), "?");
            assert_eq!(surface_flag(None), "?");
            // surface_dir mirrors surface_flag (same labels, one source).
            assert_eq!(surface_dir(Some("surface-slack")), "SLACK");
        }

        #[test]
        fn slack_and_canvas_have_real_arms_not_unknown_fallthrough() {
            // R3: the runtime must recognize these surfaces ahead of the
            // inbound path, so they don't resolve to "unknown client". Pinned to
            // an empty temp assistants root so the compiled fallback is exercised
            // (OCEAN-285) — never the operator's real ~/.config profiles.
            let root = empty_assistants_root();
            for ct in ["surface-slack", "surface-canvas", "surface-mobile"] {
                let prompt = build_system_prompt_from(None, Some(ct), Some(root.path()));
                assert!(
                    !prompt.contains("unknown client"),
                    "{ct} must have a real surface arm, not the fallthrough"
                );
                assert!(prompt.contains("## Current client"));
            }
        }

        /// OCEAN-173: slack / canvas / mobile must get *real* surface-aware
        /// profiles, not the old bare-label stub (base prompt + "You are
        /// speaking through **<label>**."). Each must carry genuine,
        /// surface-specific guidance, and must not bleed another surface's UX.
        #[test]
        fn slack_canvas_mobile_get_real_profiles_not_stub() {
            // This test asserts against the COMPILED FALLBACK profiles
            // (SLACK/CNVS/MOBL consts). `build_system_prompt` would otherwise
            // resolve an on-disk `assistants/<DIR>/system.md` first via the real
            // `assistants_root()` (OCEAN_ASSISTANTS_DIR, else
            // ~/.config/ocean-rs/assistants), and in any dev/CI box that has a
            // real SLACK/CNVS/MOBL profile that file would shadow the consts
            // under test — wrong/flaky, and a read of the operator's machine
            // state. Build against an empty temp root instead (OCEAN-285): the
            // file lookup finds nothing, the const fallback is exercised, and no
            // process env is touched (no save/restore race with sibling tests).
            let root = empty_assistants_root();

            let slack = build_system_prompt_from(None, Some("surface-slack"), Some(root.path()));
            // Slack-native: thread-aware, concise, mrkdwn-not-Markdown, canvas-aware.
            assert!(slack.contains("Slack surface UX"));
            assert!(slack.contains("thread"));
            assert!(slack.contains("Slack mrkdwn"));
            assert!(slack.contains("single asterisks"));
            assert!(slack.contains("Slack Canvas"));
            assert!(slack.contains("act only on inbound turns"));
            // Not the old stub one-liner, and not a web/HTML surface.
            assert!(!slack.contains("Responses render as HTML"));

            let canvas = build_system_prompt_from(None, Some("surface-canvas"), Some(root.path()));
            assert!(canvas.contains("canvas surface UX"));
            assert!(canvas.contains("artifact"));
            assert!(canvas.contains("append over overwrite"));
            assert!(canvas.contains("pair the canvas with a message"));

            let mobile = build_system_prompt_from(None, Some("surface-mobile"), Some(root.path()));
            assert!(mobile.contains("mobile surface UX"));
            assert!(mobile.contains("phone"));
            assert!(mobile.contains("answer-first"));
            assert!(mobile.contains("small screen"));

            // None of the three may be the bare-label stub: the stub had no
            // surface-specific "UX" section beyond the `## Current client`
            // header, so a real profile must add a dedicated guidance section.
            for (ct, p) in [
                ("surface-slack", &slack),
                ("surface-canvas", &canvas),
                ("surface-mobile", &mobile),
            ] {
                assert!(
                    p.contains("surface UX"),
                    "{ct} must carry a real surface-UX section, not a bare label"
                );
            }
        }

        #[test]
        fn web_surface_gets_leptos_component_guidance() {
            // Const fallback under test — pin an empty temp assistants root so a
            // real WEB profile on the box can't shadow it (OCEAN-285).
            let root = empty_assistants_root();
            let prompt = build_system_prompt_from(None, Some("surface-web"), Some(root.path()));

            assert!(prompt.contains("Leptos components"));
            assert!(prompt.contains("component_render"));
            assert!(prompt.contains("Responses render as HTML"));
        }

        #[test]
        fn extension_surface_knows_it_is_docked_in_chrome() {
            // Tests the BUILT-IN (const) extension prompt — the fallback when no
            // on-disk BRWSR profile exists. Call the const builder directly (it
            // never file-loads) so a real
            // ~/.config/ocean-rs/assistants/BRWSR/system.md (the intended Fix-5
            // hot-reconfigure override) doesn't shadow the source under test.
            let prompt = super::extension_surface_prompt(super::BASE_SYSTEM_PROMPT);

            // Same rich component surface as the web PWA…
            assert!(prompt.contains("Leptos components"));
            assert!(prompt.contains("component_render"));
            // …but it must know it's the in-Chrome side panel attached to the
            // user's real browser, not a detached web app.
            assert!(prompt.contains("Chrome extension side panel"));
            assert!(prompt.contains("attached to the browser the user is looking at"));
            assert!(prompt.contains("browser_read_page"));
            // It must NOT claim to be a browser PWA like surface-web does.
            assert!(!prompt.contains("a browser PWA"));
        }

        #[test]
        fn gpui_surface_avoids_web_component_guidance() {
            let root = empty_assistants_root();
            let prompt = build_system_prompt_from(None, Some("surface-gpui"), Some(root.path()));

            assert!(prompt.contains("Ocean GUI"));
            assert!(prompt.contains("GPUI"));
            // Web-only Leptos rendering is still discouraged, but now scoped to
            // chat UI rather than blanket-blocked.
            assert!(prompt.contains("does not render Leptos components"));
            assert!(prompt.contains("Use `surface_patch` for native canvas mutations"));
            assert!(!prompt.contains("Responses render as HTML"));
        }

        /// OCEAN-154 / Slice 7: the GPUI surface guidance must point the model at
        /// `surface_patch` for canvas work and must NOT carry the old prompt that
        /// blocked all surface/visual tools ("It is not a browser/WebView surface
        /// and it does not render Leptos components or arbitrary HTML inside
        /// chat" + a blanket `component_render` ban with no `surface_patch`
        /// alternative).
        #[test]
        fn gpui_surface_guides_to_surface_patch_not_ascii() {
            let root = empty_assistants_root();
            let prompt = build_system_prompt_from(None, Some("surface-gpui"), Some(root.path()));

            // The keystone: the model is told the canvas tool exists and how to
            // use it.
            assert!(prompt.contains("surface_patch"));
            assert!(prompt.contains("agent-native desktop work surface"));
            assert!(prompt.contains("Do not draw ASCII diagrams in chat"));
            assert!(prompt.contains("injected canvas ledger"));

            // The old over-broad blocking text is gone.
            assert!(!prompt.contains(
                "It is not a browser/WebView surface and it does not render Leptos components or arbitrary HTML inside chat"
            ));
        }

        #[test]
        fn legacy_native_surface_is_not_treated_as_webview() {
            let root = empty_assistants_root();
            let prompt = build_system_prompt_from(None, Some("surface-native"), Some(root.path()));

            assert!(prompt.contains("Ocean native surface"));
            assert!(prompt.contains("surface_patch"));
            assert!(!prompt.contains("Responses render as HTML"));
        }

        #[test]
        fn project_prompt_loads_ocean_agents_md_from_ancestor() {
            let assistants = empty_assistants_root();
            let project = TempDir::new().expect("create project tempdir");
            let ocean_dir = project.path().join(".ocean");
            std::fs::create_dir_all(&ocean_dir).expect("create .ocean dir");
            std::fs::write(
                ocean_dir.join("AGENTS.md"),
                "OCEAN PROJECT CONTRACT FROM DOT OCEAN",
            )
            .expect("write .ocean/AGENTS.md");
            let nested = project.path().join("crates/example/src");
            std::fs::create_dir_all(&nested).expect("create nested cwd");

            let prompt = build_system_prompt_from(
                Some(nested.to_str().expect("nested path utf8")),
                Some("tui"),
                Some(assistants.path()),
            );

            assert!(prompt.contains(".ocean/AGENTS.md"));
            assert!(prompt.contains("OCEAN PROJECT CONTRACT FROM DOT OCEAN"));
        }

        #[test]
        fn tui_surface_avoids_web_component_guidance() {
            let root = empty_assistants_root();
            let prompt = build_system_prompt_from(None, Some("tui"), Some(root.path()));

            assert!(prompt.contains("Ocean TUI"));
            assert!(prompt.contains("terminal-native"));
            assert!(prompt.contains("Do not use `component_render`"));
            assert!(!prompt.contains("Leptos components from `component_render` events"));
        }
    }
}
