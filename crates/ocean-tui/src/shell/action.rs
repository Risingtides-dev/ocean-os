//! Actions — the single mutation channel (Elm/TEA). Components emit actions;
//! `App::update` and each component's `update` consume them. Nothing mutates
//! state outside of an action.

use std::path::PathBuf;

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};

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
    /// Transient status message (connection state, etc.).
    Status(String),
    /// Open a discovered session in the PTY: run `line` in a shell rooted at `cwd`.
    OpenSession { line: String, cwd: PathBuf },
    /// Move keyboard focus to the next pane.
    CycleFocus,
    /// Open a file in the editor (from the file tree or the graph).
    OpenFile(PathBuf),
}
