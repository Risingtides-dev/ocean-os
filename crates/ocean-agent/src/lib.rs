use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use async_trait::async_trait;
use ocean_core::{PromptRequest, PromptResponse, RequestId, SessionId, SessionSummary};
use pi_agent::{
    run_agent_with_history, tools::default_tools, AgentConfig, AgentEvent, PermissionDecision,
    PermissionPolicy,
};
use pi_ai::{Content, Message, Model};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

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
}

impl AgentRuntime {
    pub fn from_env() -> anyhow::Result<Self> {
        let model = model_from_env();
        let api_key = api_key_for_model(&model)?;
        let backend_name = format!("ocean-native-{}", model.provider);
        Ok(Self {
            config_dir: config_dir_from_env(),
            model,
            api_key,
            backend_name,
        })
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    pub async fn prompt(&self, req: PromptRequest) -> PromptResponse {
        let request_id = req.request_id.unwrap_or_else(RequestId::new_v4);
        let mut req = req;
        req.request_id = Some(request_id);

        let start = Instant::now();
        match self.run_prompt(req.clone()).await {
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

    async fn run_prompt(&self, req: PromptRequest) -> anyhow::Result<(SessionId, String, String)> {
        anyhow::ensure!(!req.prompt.trim().is_empty(), "prompt cannot be empty");

        let mut session = match req.session_id {
            Some(id) => session::load(&self.config_dir, id)
                .unwrap_or_else(|_| session::Session::new_with_id(id, &self.model)),
            None => session::Session::new(&self.model),
        };

        let mut history = session.messages.clone();
        history.push(Message::user_text(req.prompt));

        let mut cfg = AgentConfig::new(self.model.clone(), system_prompt::build_system_prompt())
            .with_tools(default_tools())
            .with_max_turns(req.max_turns.unwrap_or(32))
            .with_permission(Arc::new(DaemonPermission::new(req.yolo)));
        cfg.stream_options.api_key = self.api_key.clone();

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

struct DaemonPermission {
    allow_mutating: bool,
}

impl DaemonPermission {
    fn new(allow_mutating: bool) -> Self {
        Self { allow_mutating }
    }
}

#[async_trait]
impl PermissionPolicy for DaemonPermission {
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

fn model_from_env() -> Model {
    let id = std::env::var("OCEAN_MODEL")
        .or_else(|_| std::env::var("PI_MODEL"))
        .unwrap_or_else(|_| "deepseek-chat".to_string());
    match id.as_str() {
        "deepseek" | "deepseek-chat" => Model::openai_compat(
            "deepseek",
            "deepseek-chat",
            "https://api.deepseek.com/v1",
            64_000,
            8_192,
        ),
        "deepseek-reasoner" | "deepseek-r1" => Model::openai_compat(
            "deepseek",
            "deepseek-reasoner",
            "https://api.deepseek.com/v1",
            64_000,
            8_192,
        ),
        "deepseek-v4-flash" => Model::openai_compat(
            "deepseek",
            "deepseek-v4-flash",
            "https://api.deepseek.com/v1",
            64_000,
            8_192,
        ),
        "gpt-4o" => Model::openai_gpt_4o(),
        "gpt-4o-mini" => Model::openai_gpt_4o_mini(),
        "claude-sonnet-4-6" | "claude-sonnet" | "sonnet" => Model::anthropic_claude_sonnet_4_6(),
        "claude-opus-4-7" | "claude-opus" | "opus" => Model::anthropic_claude_opus_4_7(),
        other => Model::openai_compat(
            "openai-compatible",
            other,
            std::env::var("OCEAN_OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            128_000,
            16_384,
        ),
    }
}

fn api_key_for_model(model: &Model) -> anyhow::Result<Option<String>> {
    match model.provider.as_str() {
        "deepseek" => Ok(std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(read_deepseek_key_from_pi_auth)),
        _ => Ok(None),
    }
}

fn read_deepseek_key_from_pi_auth() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".pi/agent/auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    json.pointer("/deepseek/key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
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
