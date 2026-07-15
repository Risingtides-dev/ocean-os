//! Ocean's terminal workbench.
//!
//! The component architecture (Elm actions, async daemon client, and focused
//! panes) is the sole TUI implementation. The former Track-0 room cockpit was
//! removed after native session resume reached parity.

mod action;
mod app;
mod client;
mod component;
mod components;
mod daemon_boot;
mod diff;
mod editor;
mod errfmt;
mod event;
mod git;
mod graph;
mod herdr;
mod highlight;
mod history;
mod kitty;
mod markdown;
mod mentions;
mod offshore;
mod panel;
mod pty;
mod rail;
mod sessions;
mod slash;
mod spatial;
mod status;
mod theme;
mod tree;
mod tui;

use app::App;
use client::DaemonClient;

/// Entry point for the new shell. Blocks on a tokio runtime, runs the app loop,
/// and always restores the terminal on the way out.
pub fn run(
    url: &str,
    workspace_root: String,
    requested_session: Option<&str>,
) -> anyhow::Result<()> {
    let requested_session = requested_session
        .map(sessions::resolve)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let client = DaemonClient::new(url)?;
        let mut app = App::new(client, workspace_root);
        if let Some(session) = requested_session {
            app.resume_initial_session(session)?;
        }
        let mut terminal = tui::init()?;
        // The OCEAN splash: hold, then slide-and-fade. Runs before the event
        // pump spawns, so its direct crossterm polling can't race the app loop.
        crate::splash::play(&mut terminal)?;
        app.run(&mut terminal).await
    })
}
