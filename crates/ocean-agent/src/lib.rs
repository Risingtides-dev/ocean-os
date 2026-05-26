use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{PromptRequest, PromptResponse, RequestId, SessionId, SessionSummary};
use ocean_providers::{
    resolve_provider_config_from_env, ProviderConfig, ProviderId, ProviderReadiness,
};
use pi_agent::{
    run_agent_with_history, tools::default_tools, AgentConfig, AgentEvent, PermissionDecision,
    PermissionPolicy,
};
use pi_ai::{Content, Message, Model};
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
        Ok(Self {
            config_dir: config_dir_from_env(),
            model,
            api_key,
            backend_name,
            provider_config,
        })
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
        match self.run_prompt(req.clone(), control).await {
            Ok((session_id, stdout, stderr)) => PromptResponse {
                request_id: Some(request_id),
                ok: true,
                session_id: Some(session_id),
                code: Some(0),
                wall_ms: start.elapsed().as_millis(),
                stdout,
                stderr,
            },
            Err(e) => PromptResponse {
                request_id: Some(request_id),
                ok: false,
                session_id: req.session_id,
                code: None,
                wall_ms: start.elapsed().as_millis(),
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<SessionSummary>> {
        session::list(&self.config_dir)
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

        let mut history = session.messages.clone();
        history.push(Message::user_text(req.prompt));

        let PromptControl { permission, cancel } = control;
        let mut cfg = AgentConfig::new(self.model.clone(), system_prompt::build_system_prompt())
            .with_tools(default_tools())
            .with_max_turns(req.max_turns.unwrap_or(32))
            .with_permission(permission);
        cfg.stream_options.api_key = self.api_key.clone();
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
}

impl PromptControl {
    pub fn new(permission: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            permission,
            cancel: None,
        }
    }

    pub fn yolo(allow_mutating: bool) -> Self {
        Self::new(Arc::new(StaticPermissionPolicy { allow_mutating }))
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
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
    }

    impl Session {
        pub fn new(model: &Model) -> Self {
            Self::new_with_id(SessionId::new_v4(), model)
        }

        pub fn new_with_id(id: SessionId, model: &Model) -> Self {
            let now = pi_ai::now_ms();
            Self {
                id,
                created_ms: now,
                updated_ms: now,
                model: model.id.clone(),
                provider: model.provider.clone(),
                messages: Vec::new(),
            }
        }

        pub fn replace_messages(&mut self, messages: Vec<Message>) {
            self.messages = messages;
            self.updated_ms = pi_ai::now_ms();
        }
    }

    pub fn sessions_dir(config_dir: &Path) -> PathBuf {
        config_dir.join("sessions")
    }

    pub fn save(config_dir: &Path, session: &Session) -> anyhow::Result<PathBuf> {
        let dir = sessions_dir(config_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(format!("{}.json", session.id));
        let json = serde_json::to_string_pretty(session)?;
        std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub fn load(config_dir: &Path, id: SessionId) -> anyhow::Result<Session> {
        let path = sessions_dir(config_dir).join(format!("{id}.json"));
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let session = serde_json::from_str(&text)?;
        Ok(session)
    }

    pub fn list(config_dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
        let dir = sessions_dir(config_dir);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let session: Session = match serde_json::from_str(&text) {
                Ok(session) => session,
                Err(_) => continue,
            };
            out.push(SessionSummary {
                id: session.id,
                model: session.model,
                turns: session.messages.len() as u32,
                title: first_user_text(&session.messages),
            });
        }
        out.sort_by_key(|session| std::cmp::Reverse(session.id));
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

    fn truncate_title(text: &str) -> String {
        let squashed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if squashed.chars().count() <= 70 {
            squashed
        } else {
            format!("{}…", squashed.chars().take(70).collect::<String>())
        }
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

    pub fn build_system_prompt() -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project = load_project_prompt(&cwd);
        if project.is_empty() {
            BASE_SYSTEM_PROMPT.to_string()
        } else {
            format!("{BASE_SYSTEM_PROMPT}\n----- project instructions -----{project}")
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
