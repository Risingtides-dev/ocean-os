use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{
    PromptRequest, PromptResponse, RequestId, RoomId, SessionDetail, SessionId, SessionRunState,
    TokenUsage,
    SessionSummary, SessionToolContext, SessionTranscriptEntry,
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
    run_agent_with_history, AgentConfig, AgentEvent, BuiltinProvider, CapabilityProvider,
    CapabilityRegistry, PermissionDecision, PermissionPolicy, SessionContext,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod config;
pub use config::{DaemonConfig, McpSection};

const APP_NAME: &str = "ocean-rs";

/// How long to wait for a single MCP server to connect + list its tools at
/// startup. A server that exceeds this contributes no tools (non-fatal) rather
/// than wedging daemon startup.
const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        let state = build_state_from_env()?;
        let runtime = Self {
            config_dir: config_dir_from_env(),
            state: Arc::new(RwLock::new(state)),
            capabilities: Arc::new(CapabilityRegistry::builtin_only()),
            session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        runtime.migrate_legacy_sessions();
        Ok(runtime)
    }

    /// Connect configured MCP servers and fold their tools into the capability
    /// registry, on top of the built-ins. Reads `<config_dir>/ocean.toml`;
    /// absent/empty config leaves the registry built-ins-only. Each server is
    /// connected with a timeout and non-fatally — a server that fails to start
    /// logs a warning and contributes no tools, never blocking startup.
    ///
    /// Consuming builder so the daemon can do
    /// `AgentRuntime::from_env()?.with_extensions().await` before sharing the
    /// runtime behind an `Arc`.
    pub async fn with_extensions(mut self) -> Self {
        let registry = build_capability_registry(&self.config_dir).await;
        self.capabilities = Arc::new(registry);
        self
    }

    /// Get (or create) the per-session turn lock for `id`.
    fn session_lock(&self, id: SessionId) -> Arc<tokio::sync::Mutex<()>> {
        self.session_locks
            .lock()
            .expect("session_locks poisoned")
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn snapshot(&self) -> RuntimeState {
        self.state.read().expect("runtime state poisoned").clone()
    }

    pub fn backend_name(&self) -> String {
        self.snapshot().backend_name
    }

    pub fn provider_readiness(&self) -> ProviderReadiness {
        self.snapshot().provider_config.readiness()
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
        Ok(label)
    }

    pub async fn prompt(&self, req: PromptRequest, control: PromptControl) -> PromptResponse {
        let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
        let mut req = req;
        req.request_id = Some(request_id);

        let start = Instant::now();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        if let Some(stderr) = self.provider_preflight_error() {
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

        let snapshot = self.snapshot();
        let result = if snapshot.provider_config.selection.provider == ProviderId::Fake {
            self.run_fake_prompt(req.clone(), &snapshot).await
        } else {
            self.run_prompt(req.clone(), control, &snapshot).await
        };

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
            Err(e) => PromptResponse {
                request_id: Some(request_id),
                ok: false,
                session_id: req.session_id,
                code: None,
                wall_ms: start.elapsed().as_millis() as u64,
                stdout: String::new(),
                stderr: e.to_string(),
                cwd,
                usage: TokenUsage::default(),
            },
        }
    }

    pub fn list_sessions(
        &self,
        workspace_root: Option<&str>,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        session::list(&self.config_dir, workspace_root)
    }

    /// Resolve the workspace root for an arbitrary cwd. Exposed so callers
    /// (daemon, TUI) can ask "what workspace would my current cwd map to?"
    /// without depending on the private session module.
    pub fn workspace_root_for(&self, cwd: &Path) -> PathBuf {
        session::workspace_root(cwd)
    }

    /// One-shot legacy migration — safe to call repeatedly.
    pub fn migrate_legacy_sessions(&self) {
        session::migrate_legacy_sessions(&self.config_dir);
    }

    pub fn session_detail(&self, id: SessionId) -> anyhow::Result<SessionDetail> {
        session::detail(&self.config_dir, id)
    }

    fn provider_preflight_error(&self) -> Option<String> {
        let readiness = self.provider_readiness();
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

    async fn run_prompt(
        &self,
        req: PromptRequest,
        control: PromptControl,
        snapshot: &RuntimeState,
    ) -> anyhow::Result<(SessionId, String, String, TokenUsage)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        // Resolve the effective session id up front so we can serialize on it.
        // None means "new session" — mint the id now so the lock still covers
        // the load/run/save window.
        let session_id = req.session_id.unwrap_or_else(SessionId::new_v4);

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
        history.push(Message::user_text(req.prompt));

        let PromptControl {
            permission,
            cancel,
            event_sink,
        } = control;
        // Resolve the toolset for this turn through the capability registry —
        // built-ins plus any connected MCP/skill providers, deduped first-wins.
        // This is the seam that replaced the old hardcoded `default_tools()`.
        let tool_ctx = SessionContext {
            cwd: PathBuf::from(&req.cwd),
            session_id: Some(session_id.to_string()),
        };
        let tools = self.capabilities.tools_for_session(&tool_ctx).await;

        let mut cfg = AgentConfig::new(
            snapshot.model.clone(),
            system_prompt::build_system_prompt(Some(&req.cwd), req.client_type.as_deref()),
        )
        .with_tools(tools)
        .with_max_turns(req.max_turns.unwrap_or(32))
        .with_permission(permission);
        cfg.stream_options.api_key = snapshot.api_key.clone();
        cfg.stream_options.base_url = Some(snapshot.provider_config.selection.base_url.clone());
        cfg.stream_options.cancel = cancel;
        if let Some(account_id) = &snapshot.provider_config.account_id {
            cfg.stream_options
                .headers
                .insert("chatgpt-account-id".into(), account_id.clone());
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cfg_cloned = cfg.clone();
        let handle =
            tokio::spawn(
                async move { run_agent_with_history(&cfg_cloned, history, Some(tx)).await },
            );

        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some(ev) = rx.recv().await {
            if let Some(sink) = event_sink.as_ref() {
                let _ = sink.send(ev.clone());
            }
            match ev {
                AgentEvent::TextDelta { delta } => stdout.push_str(&delta),
                AgentEvent::ThinkingDelta { delta } => {
                    stderr.push_str("thinking: ");
                    stderr.push_str(&delta);
                    stderr.push('\n');
                }
                AgentEvent::AssistantMessage { .. } if !stdout.ends_with('\n') => {
                    stdout.push('\n');
                }
                AgentEvent::AssistantMessage { .. } => {}
                AgentEvent::ToolExecutionStart {
                    tool_name, args, ..
                } => {
                    stderr.push_str(&format!("→ {tool_name}({args})\n"));
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    ..
                } => {
                    stderr.push_str(&format!(
                        "← {tool_name} {}\n",
                        if is_error { "error" } else { "ok" }
                    ));
                }
                AgentEvent::PermissionDenied { tool_name, reason } => {
                    stderr.push_str(&format!("✗ permission denied for {tool_name}: {reason}\n"));
                }
                _ => {}
            }
        }

        let run = handle.await.context("agent task join failed")??;
        // Cap what we persist. The agent loop already trims per-send to the
        // context window, but the *stored* transcript would otherwise grow
        // forever and be reloaded in full on every future turn (the dominant
        // source of runaway input-token cost). Keep the most recent messages.
        session.replace_messages(cap_session_history(run.messages.clone()));
        session::save(&self.config_dir, &session)?;

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

#[derive(Clone)]
pub struct PromptControl {
    pub permission: Arc<dyn PermissionPolicy>,
    pub cancel: Option<CancellationToken>,
    /// Optional sink that receives raw `AgentEvent`s as the run progresses.
    /// The daemon uses this to push real-time deltas onto its broadcast bus
    /// so SSE consumers (TUI/CLI) can render text as it streams.
    pub event_sink: Option<mpsc::UnboundedSender<AgentEvent>>,
}

impl PromptControl {
    pub fn new(permission: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            permission,
            cancel: None,
            event_sink: None,
        }
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
                reason: "daemon approval flow is not implemented yet; retry with yolo only if you intend to allow mutating tools".into(),
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
async fn build_capability_registry(config_dir: &Path) -> CapabilityRegistry {
    let mut providers: Vec<Arc<dyn CapabilityProvider>> = vec![Arc::new(BuiltinProvider::new())];

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

    CapabilityRegistry::new(providers)
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

/// Resolve a fresh runtime state from the current process env.
fn build_state_from_env() -> anyhow::Result<RuntimeState> {
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
    let backend_name = format!(
        "ocean-native-{}",
        provider_config.selection.provider.as_str()
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
    }
}

fn config_dir_from_env() -> PathBuf {
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
            }
        }

        /// Tag this session with workspace metadata derived from the
        /// caller's cwd. Idempotent — pre-existing values win so that
        /// resuming a session in a different cwd doesn't rewrite its
        /// original workspace.
        pub fn bind_workspace(&mut self, cwd: &Path) {
            if self.cwd.is_none() {
                self.cwd = Some(cwd.to_string_lossy().into_owned());
            }
            if self.workspace_root.is_none() {
                self.workspace_root = Some(workspace_root(cwd).to_string_lossy().into_owned());
            }
            if self.git_branch.is_none() && self.git_commit.is_none() {
                let (branch, commit) = probe_git(cwd);
                self.git_branch = branch;
                self.git_commit = commit;
            }
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
        }
    }

    fn transcript_entry(message: &Message) -> SessionTranscriptEntry {
        match message {
            Message::User { content, timestamp } => SessionTranscriptEntry {
                role: "user".into(),
                timestamp_ms: Some(*timestamp),
                text: text_from_content(content),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
            },
            Message::Assistant(assistant) => SessionTranscriptEntry {
                role: "assistant".into(),
                timestamp_ms: Some(assistant.timestamp),
                text: text_from_content(&assistant.content),
                tool_call_id: None,
                tool_name: None,
                is_error: assistant.error_message.as_ref().map(|_| true),
            },
            Message::ToolResult(tool) => SessionTranscriptEntry {
                role: "tool".into(),
                timestamp_ms: Some(tool.timestamp),
                text: text_from_content(&tool.content),
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

    fn temp_config_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ocean-agent-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn cap_session_history_is_noop_under_limit() {
        let msgs: Vec<Message> = (0..10).map(|i| Message::user_text(format!("m{i}"))).collect();
        assert_eq!(cap_session_history(msgs).len(), 10);
    }

    #[test]
    fn cap_session_history_keeps_most_recent_within_limit() {
        let msgs: Vec<Message> = (0..MAX_SESSION_MESSAGES + 50)
            .map(|i| Message::user_text(format!("m{i}")))
            .collect();
        let capped = cap_session_history(msgs);
        assert!(capped.len() <= MAX_SESSION_MESSAGES);
        // Oldest dropped, newest retained.
        assert!(matches!(capped.last(), Some(Message::Assistant(_)) | Some(Message::User { .. })));
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
        let state = state_from_provider_config(provider_config).unwrap();
        AgentRuntime {
            config_dir,
            state: std::sync::Arc::new(std::sync::RwLock::new(state)),
            capabilities: Arc::new(CapabilityRegistry::builtin_only()),
            session_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
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

    #[tokio::test]
    async fn missing_credential_preflight_names_ocean_provider_and_model() {
        let config_dir = temp_config_dir("missing-credential");
        let runtime = runtime(
            config_dir.clone(),
            provider_config(ProviderId::DeepSeek, "deepseek-v4-pro", false),
        );

        let res = runtime
            .prompt(
                PromptRequest {
                    prompt: "hello".into(),
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    client_type: None,
                },
                PromptControl::yolo(false),
            )
            .await;

        assert!(!res.ok);
        assert!(res.stderr.contains("provider deepseek"));
        assert!(res.stderr.contains("deepseek-v4-pro"));
        assert!(!res.stderr.contains("provider openai"));
        assert!(runtime.list_sessions(None).unwrap().is_empty());
        let missing = runtime.session_detail(SessionId::new_v4()).unwrap_err();
        assert!(missing.to_string().contains("not found"));
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
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    client_type: None,
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
                    request_id: None,
                    session_id: Some(ghost),
                    create_if_missing: false,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    client_type: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(!res.ok, "strict resume of unknown session must fail");
        assert!(res.stderr.contains("session not found"), "stderr: {}", res.stderr);
        // And no session was created under the ghost id.
        assert!(runtime.session_detail(ghost).is_err());
        assert!(runtime.list_sessions(None).unwrap().is_empty());

        // But with create_if_missing: true, the same id is accepted.
        let ok = runtime
            .prompt(
                PromptRequest {
                    prompt: "now create it".into(),
                    request_id: None,
                    session_id: Some(ghost),
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    client_type: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(ok.ok, "create_if_missing should accept a new id: {}", ok.stderr);
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
                    request_id: None,
                    session_id: None,
                    create_if_missing: true,
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
                    client_type: None,
                },
                PromptControl::yolo(false),
            )
            .await;
        assert!(first.ok);
        let sid = first.session_id.unwrap();

        // Now fire two more turns at that same session at once.
        let mk = |p: &str| PromptRequest {
            prompt: p.into(),
            request_id: None,
            session_id: Some(sid),
            create_if_missing: false,
            max_turns: None,
            yolo: false,
            cwd: ".".into(),
            client_type: None,
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
        assert!(user_texts.iter().any(|t| t.contains("first")), "lost 'first': {user_texts:?}");
        assert!(user_texts.iter().any(|t| t.contains("alpha")), "lost 'alpha': {user_texts:?}");
        assert!(user_texts.iter().any(|t| t.contains("bravo")), "lost 'bravo': {user_texts:?}");
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
}

mod system_prompt {
    use super::*;

    const BASE_SYSTEM_PROMPT: &str = r#"You are Ocean — a local-first, Rust-native coding agent. You are the brain that lives inside ocean-daemon, a small HTTP+SSE service the user runs on their own machine. Several clients (TUI, web, native, CLI, voice) all talk to you over the same product API: POST /v1/agent/turns + GET /v1/agent/events.

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

You operate from the user's project directory (passed per turn). Look for AGENTS.md, CLAUDE.md, or .pi/instructions.md in the project tree — those are project-specific instructions that override or extend the above.
"#;

    /// Build the system prompt, optionally scoped to `cwd` and `client_type`.
    pub fn build_system_prompt(cwd: Option<&str>, client_type: Option<&str>) -> String {
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
            append_client_type(BASE_SYSTEM_PROMPT, client_type)
        } else {
            let prompt = format!("{BASE_SYSTEM_PROMPT}\n----- project instructions -----\n{project}");
            append_client_type(&prompt, client_type)
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

You are speaking through Ocean GUI, a native GPUI desktop surface. It is not a browser/WebView surface and it does not render Leptos components or arbitrary HTML inside chat.

Use compact markdown, clear step summaries, file paths, commands, and short status text. Do not use `component_render`, `component_wait`, web widgets, Leptos component assumptions, maps, dashboards, forms, or HTML-oriented UI unless the user explicitly asks for a protocol test. If work has structured output, present it as concise markdown or describe the native surface/state that should be updated.
"#;

    const TUI_SURFACE_PROMPT: &str = r#"
## Ocean TUI surface UX

You are speaking through the Ocean TUI. The user sees a terminal interface with basic markdown rendering. Keep responses concise and terminal-native.

Do not use `component_render`, `component_wait`, web widgets, Leptos component assumptions, maps, dashboards, forms, or HTML-oriented UI unless the user explicitly asks for a protocol test. Prefer short markdown, file paths, command output summaries, and state updates that fit a terminal transcript.
"#;

    fn web_surface_prompt(prompt: &str, client_label: &str) -> String {
        format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **{client_label}**. Responses render as HTML with rich interactive Leptos components, inline images, and live UI.\n\n{WEB_SURFACE_COMPONENT_PROMPT}\n"
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

    fn append_client_type(prompt: &str, client_type: Option<&str>) -> String {
        match client_type {
            Some("tui") => tui_surface_prompt(prompt),
            Some("surface-web") => web_surface_prompt(prompt, "Ocean Surface (web) — a browser PWA"),
            Some("surface-gpui") => gpui_surface_prompt(prompt, "Ocean GUI (GPUI native desktop)"),
            Some("surface-native") => gpui_surface_prompt(prompt, "Ocean native surface"),
            Some("cli") => cli_surface_prompt(prompt),
            Some("leo-voice") => voice_surface_prompt(prompt),
            Some(other) => format!("{prompt}\n\n## Current client\n\nYou are speaking through an unknown client: `{other}`.\n"),
            None => prompt.to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::build_system_prompt;

        #[test]
        fn web_surface_gets_leptos_component_guidance() {
            let prompt = build_system_prompt(None, Some("surface-web"));

            assert!(prompt.contains("Leptos components"));
            assert!(prompt.contains("component_render"));
            assert!(prompt.contains("Responses render as HTML"));
        }

        #[test]
        fn gpui_surface_avoids_web_component_guidance() {
            let prompt = build_system_prompt(None, Some("surface-gpui"));

            assert!(prompt.contains("Ocean GUI"));
            assert!(prompt.contains("GPUI"));
            assert!(prompt.contains("does not render Leptos components"));
            assert!(prompt.contains("Do not use `component_render`"));
            assert!(!prompt.contains("Responses render as HTML"));
        }

        #[test]
        fn legacy_native_surface_is_not_treated_as_webview() {
            let prompt = build_system_prompt(None, Some("surface-native"));

            assert!(prompt.contains("Ocean native surface"));
            assert!(prompt.contains("Do not use `component_render`"));
            assert!(!prompt.contains("Responses render as HTML"));
        }

        #[test]
        fn tui_surface_avoids_web_component_guidance() {
            let prompt = build_system_prompt(None, Some("tui"));

            assert!(prompt.contains("Ocean TUI"));
            assert!(prompt.contains("terminal-native"));
            assert!(prompt.contains("Do not use `component_render`"));
            assert!(!prompt.contains("Leptos components from `component_render` events"));
        }
    }

    fn load_project_prompt(start: &Path) -> String {
        const FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".pi/instructions.md"];
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
}
