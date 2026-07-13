//! Actions — the single mutation channel (Elm/TEA). Components emit actions;
//! `App::update` and each component's `update` consume them. Nothing mutates
//! state outside of an action.

use std::path::PathBuf;

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, ThinkingLevel};

/// A workbench navigation target — a pane or center surface. Emitted by the `/`
/// palette so chat never reaches into the app's private `Focus`/`Center`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Sessions,
    Files,
    Graph,
    Terminal,
}

/// Provider login target for `/login`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTarget {
    /// Claude Code / Claude subscription OAuth.
    Claude,
    /// Codex / ChatGPT subscription OAuth.
    Codex,
}

/// A typed health source — the daemon liveness probe and the SSE transport
/// are tracked independently so a recovery clears only its own source
/// (`status::Health`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthSource {
    /// The periodic `GET /health` probe (plus autostart outcomes).
    Daemon,
    /// The scoped `/v1/agent/events` SSE stream.
    Sse,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Redraw requested (coalesced with the render tick).
    Render,
    /// Quit the app.
    Quit,
    /// A streamed agent event arrived from the daemon SSE. Boxed — it's much
    /// larger than the other variants and rides a hot channel.
    AgentEvent(Box<AgentTurnEvent>),
    /// Session was minted/adopted; scope the stream to it.
    SessionBound(AgentSessionId),
    /// Submit the composer's current text as a new turn.
    SubmitPrompt(String),
    /// A non-fatal error to surface in the status line.
    Error(String),
    /// A turn (or its session mint) could not be sent even after the daemon-blip
    /// retry window. The chat unwinds its busy state, surfaces the error in the
    /// transcript, and restores `prompt` to the composer so nothing typed is lost.
    TurnSendFailed { prompt: String, err: String },
    /// The turn POST was connected but its final response was lost/invalid.
    /// The daemon may already be executing it, so do NOT restore the prompt or
    /// offer an automatic retry that could duplicate side effects.
    TurnOutcomeUnknown { err: String },
    /// Transient status message. COMPATIBILITY PATH for slash-command and
    /// notice producers — health transitions use the typed
    /// [`Action::HealthDegraded`]/[`Action::HealthRecovered`] variants instead
    /// so recovery/failure/acknowledgement can no longer overwrite one another.
    Status(String),
    /// A health source became degraded with a terse condition. Persists until
    /// the SAME source recovers; unrelated notices never clear it.
    HealthDegraded {
        source: HealthSource,
        condition: String,
    },
    /// A health source recovered. Clears only that source; success is never
    /// rendered as text.
    HealthRecovered(HealthSource),
    /// Navigate the workbench to a pane/center surface — emitted by the `/`
    /// palette so chat never reaches into the app's private Focus/Center.
    Navigate(Nav),
    /// `/new` — drop the bound session so the next turn mints a fresh one
    /// (stays in the current active project).
    NewSession,
    /// `+ new` on a project header in the rail: start a fresh session AND
    /// re-root the workbench (cwd for turns, file tree, graph) to `cwd`.
    NewSessionInProject { cwd: PathBuf },
    /// `/copy` — put the given text (the last reply) on the system clipboard.
    CopyToClipboard(String),
    /// `/model <id>` — override the model for subsequent turns this session.
    SetModel(String),
    /// `/thinking <level>` — override the thinking level for subsequent turns
    /// this session (`None` = daemon default; rides `AgentTurnRequest`).
    SetThinking(Option<ThinkingLevel>),
    /// `/login [claude|codex]` — open a browser login flow for provider OAuth.
    Login(LoginTarget),
    /// Terminal status of an async `/login` OAuth flow (begin → browser → token
    /// exchange). Lands the final success/failure message in the status line and
    /// clears the `login_in_flight` guard so a fresh login can start.
    LoginDone(String),
    /// `/settings` — open the app's settings overlay (panel toggles, dock
    /// height, live session info).
    OpenSettings,
    /// `/providers` (or bare `/login`) — open the provider auth popup. Lists
    /// every provider with its live auth status; Enter triggers OAuth login
    /// (Claude/Codex) or inline API-key entry (GLM, DeepSeek, Kimi, …).
    OpenProviders,
    /// `/models` (or bare `/model`) — open the model picker overlay. The list
    /// is fetched fresh from the daemon so it reflects the REAL registry and
    /// per-model readiness, not a hardcoded menu.
    OpenModels,
    /// `/advisor` — open the per-session advisor picker (an "off" row + the
    /// ready models). The pick rides subsequent turns as a per-turn advisor
    /// override, toggling the second-opinion reviewer without editing
    /// `ocean.toml`.
    OpenAdvisor,
    /// `/memory` — open the retained-memory browser (fetched from the daemon's
    /// long-term store). Read/search view of what the agent has remembered.
    OpenMemory,
    /// The async `GET /v1/memory` fetch for the browser came back.
    MemoryLoaded {
        entries: Vec<crate::shell::client::MemoryEntry>,
    },
    /// `/lsp` — open the language-server panel for the active workspace
    /// (detected servers + install/ready state).
    OpenLsp,
    /// The async `GET /v1/lsp` fetch came back.
    LspLoaded {
        servers: Vec<crate::shell::client::LspServer>,
    },
    /// `/image [path]` — open the full-screen image viewer on `path` (bare =
    /// the newest image referenced in the chat). The raw path is resolved
    /// against the workspace by the app; the pixels render via kitty graphics.
    ViewImage(String),
    /// The async `GET /v1/models` fetch for the picker came back.
    ModelsLoaded {
        current: String,
        entries: Vec<crate::shell::client::ModelEntry>,
    },
    /// Open a file in the editor (from the file tree or the graph).
    OpenFile(PathBuf),
    /// Resume a session natively in the chat: load its transcript from `path`,
    /// bind future turns to `id`, and re-root the workbench to `cwd` (the dir
    /// the session ran in) so files/graph/turns follow the session.
    ResumeSession {
        id: AgentSessionId,
        path: PathBuf,
        cwd: PathBuf,
    },
    /// A global `/v1/events` envelope (permission requests/decisions ride here).
    OceanEvent(Box<ocean_core::EventEnvelope>),
    /// Operator decided a pending permission (⌃Y allow / ⌃N deny). The app
    /// replays the turn's decision token on the POST (OCEAN-185).
    PermissionDecided {
        permission_id: ocean_core::PermissionId,
        allow: bool,
    },
}
