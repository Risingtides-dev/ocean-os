//! Actions — the single mutation channel (Elm/TEA). Components emit actions;
//! `App::update` and each component's `update` consume them. Nothing mutates
//! state outside of an action.

use std::path::PathBuf;

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};

/// A workbench navigation target — a pane or center surface. Emitted by the `/`
/// palette so chat never reaches into the app's private `Focus`/`Center`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Sessions,
    Files,
    Chat,
    Editor,
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
    /// Transient status message (connection state, etc.).
    Status(String),
    /// Open a discovered session in the PTY: run `line` in a shell rooted at `cwd`.
    OpenSession { line: String, cwd: PathBuf },
    /// Move keyboard focus to the next pane.
    CycleFocus,
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
    /// `/login [claude|codex]` — open a browser login flow for provider OAuth.
    Login(LoginTarget),
    /// Terminal status of an async `/login` OAuth flow (begin → browser → token
    /// exchange). Lands the final success/failure message in the status line and
    /// clears the `login_in_flight` guard so a fresh login can start.
    LoginDone(String),
    /// `/settings` — open the app's settings overlay (panel toggles, dock
    /// height, live session info).
    OpenSettings,
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
