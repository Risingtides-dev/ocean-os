use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{
    PromptRequest, PromptResponse, RequestId, SessionDetail, SessionId, SessionRunState,
    SessionSummary, SessionToolContext, SessionTranscriptEntry,
};
use ocean_protocol::{AssistantMessage, Content, Message, Model, StopReason, Usage};
use ocean_providers::{
    resolve_provider_config_from_env, ProviderConfig, ProviderId, ProviderReadiness,
};
use ocean_runtime::{
    run_agent_with_history, tools::default_tools, AgentConfig, AgentEvent, PermissionDecision,
    PermissionPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const APP_NAME: &str = "ocean-rs";

/// Native Ocean agent runtime.
///
/// This is the first extraction of the old `pi-rs-deepseek` bootstrap path into
/// daemon-owned Rust code. It intentionally uses the small `pi-agent`/`pi-ai`
/// crates as reference/runtime components for now, while Ocean owns config,
/// sessions, permissions, protocol mapping, and the long-running daemon shape.
#[derive(Debug, Clone)]
pub struct AgentRuntime {
    config_dir: PathBuf,
    model: Model,
    api_key: Option<String>,
    backend_name: String,
    provider_config: ProviderConfig,
}

impl AgentRuntime {
    pub fn from_env() -> anyhow::Result<Self> {
        let provider_config = resolve_provider_config_from_env()?;
        let model = model_from_provider_config(&provider_config)?;
        let api_key = provider_config
            .credential
            .as_ref()
            .map(|credential| credential.secret.expose().to_string());
        let backend_name = format!(
            "ocean-native-{}",
            provider_config.selection.provider.as_str()
        );
        let runtime = Self {
            config_dir: config_dir_from_env(),
            model,
            api_key,
            backend_name,
            provider_config,
        };
        runtime.migrate_legacy_sessions();
        Ok(runtime)
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    pub fn provider_readiness(&self) -> ProviderReadiness {
        self.provider_config.readiness()
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
            };
        }

        let result = if self.provider_config.selection.provider == ProviderId::Fake {
            self.run_fake_prompt(req.clone()).await
        } else {
            self.run_prompt(req.clone(), control).await
        };

        match result {
            Ok((session_id, stdout, stderr)) => PromptResponse {
                request_id: Some(request_id),
                ok: true,
                session_id: Some(session_id),
                code: Some(0),
                wall_ms: start.elapsed().as_millis() as u64,
                stdout,
                stderr,
                cwd,
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
    ) -> anyhow::Result<(SessionId, String, String)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        let mut session = match req.session_id {
            Some(id) => session::load(&self.config_dir, id)
                .unwrap_or_else(|_| session::Session::new_with_id(id, &self.model)),
            None => session::Session::new(&self.model),
        };
        session.bind_workspace(Path::new(&req.cwd));

        let stdout = "OCEAN_FAKE_OK\n".to_string();
        let mut messages = session.messages.clone();
        messages.push(Message::user_text(req.prompt));
        messages.push(Message::Assistant(AssistantMessage {
            content: vec![Content::text(stdout.trim_end())],
            api: self.model.api.clone(),
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: ocean_protocol::now_ms(),
        }));
        session.replace_messages(messages);
        session::save(&self.config_dir, &session)?;

        Ok((session.id, stdout, String::new()))
    }

    async fn run_prompt(
        &self,
        req: PromptRequest,
        control: PromptControl,
    ) -> anyhow::Result<(SessionId, String, String)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        let mut session = match req.session_id {
            Some(id) => session::load(&self.config_dir, id)
                .unwrap_or_else(|_| session::Session::new_with_id(id, &self.model)),
            None => session::Session::new(&self.model),
        };
        session.bind_workspace(Path::new(&req.cwd));

        let mut history = session.messages.clone();
        if self.provider_config.selection.provider == ProviderId::DeepSeek
            && self.provider_config.selection.model == "deepseek-reasoner"
        {
            strip_assistant_thinking_content(&mut history);
        }
        history.push(Message::user_text(req.prompt));

        let PromptControl {
            permission,
            cancel,
            event_sink,
        } = control;
        let mut cfg = AgentConfig::new(
            self.model.clone(),
            system_prompt::build_system_prompt(Some(&req.cwd)),
        )
        .with_tools(default_tools())
        .with_max_turns(req.max_turns.unwrap_or(32))
        .with_permission(permission);
        cfg.stream_options.api_key = self.api_key.clone();
        cfg.stream_options.base_url = Some(self.provider_config.selection.base_url.clone());
        cfg.stream_options.cancel = cancel;

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
        session.replace_messages(run.messages.clone());
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

        Ok((session.id, stdout, stderr))
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

fn model_from_provider_config(config: &ProviderConfig) -> anyhow::Result<Model> {
    let selection = &config.selection;
    match selection.provider {
        ProviderId::DeepSeek | ProviderId::OpenAiCompatible | ProviderId::Fake => {
            Ok(Model::openai_compat(
                selection.provider.as_str(),
                selection.model.clone(),
                selection.base_url.clone(),
                selection.context_window,
                selection.max_output_tokens,
            ))
        }
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
        ProviderId::Anthropic => Ok(match selection.model.as_str() {
            "claude-sonnet-4-6" => Model::anthropic_claude_sonnet_4_6(),
            "claude-opus-4-7" => Model::anthropic_claude_opus_4_7(),
            _ => {
                anyhow::bail!(
                    "unsupported anthropic model '{}' in temporary pi-ai adapter",
                    selection.model
                );
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

    /// Encode a workspace path as a filesystem-safe slug. Mirrors the
    /// Claude Code / pi-agent convention: leading slash dropped, remaining
    /// slashes turned into dashes, then prefixed with a leading dash so
    /// directory listings sort intuitively.
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
        std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub fn load(config_dir: &Path, id: SessionId) -> anyhow::Result<Session> {
        let target = format!("{id}.json");
        // Search all workspace buckets + legacy/ + top-level (for forward-compat).
        for candidate in candidate_session_paths(config_dir, &target) {
            if candidate.exists() {
                let text = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("read {}", candidate.display()))?;
                let session = serde_json::from_str(&text)?;
                return Ok(session);
            }
        }
        anyhow::bail!("session {id} not found")
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
        }
    }

    fn runtime(config_dir: PathBuf, provider_config: ProviderConfig) -> AgentRuntime {
        let model = model_from_provider_config(&provider_config).unwrap();
        let api_key = provider_config
            .credential
            .as_ref()
            .map(|credential| credential.secret.expose().to_string());
        let backend_name = format!(
            "ocean-native-{}",
            provider_config.selection.provider.as_str()
        );
        AgentRuntime {
            config_dir,
            model,
            api_key,
            backend_name,
            provider_config,
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
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
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
                    max_turns: None,
                    yolo: false,
                    cwd: ".".into(),
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
        assert!(error.to_string().contains("expected") || error.to_string().contains("key"));
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

    const BASE_SYSTEM_PROMPT: &str = r#"You are Ocean, a local-first Rust-native coding agent runtime daemon.

You have access to tools for reading and modifying files, listing directories, searching with grep and glob, running shell commands via bash, fetching URLs, and tracking todos. Use them to investigate the user's repository and make focused, correct changes.

Guidelines:
- Prefer reading files before editing them; never invent code that you have not verified.
- Make small, focused diffs. Do not introduce unrelated refactors.
- After making changes, summarize what you did briefly and accurately.
- For shell-only tasks (build, test, run), use the bash tool with sensible timeouts.
- When asked an open-ended question, prefer concise answers grounded in actual files.

You operate inside the daemon's working directory unless a future client request supplies a project directory.
"#;

    /// Build the system prompt, optionally scoped to `cwd`.
    pub fn build_system_prompt(cwd: Option<&str>) -> String {
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
            BASE_SYSTEM_PROMPT.to_string()
        } else {
            format!("{BASE_SYSTEM_PROMPT}\n----- project instructions -----\n{project}")
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
