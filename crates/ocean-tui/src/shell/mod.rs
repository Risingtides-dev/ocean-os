//! Session-first shell — the Phase 1 spine of the ocean-tui rebuild.
//!
//! Component architecture (Component trait + tokio + event/action channels)
//! replacing the monolithic daemon UI in `main.rs`. Launched behind `--next`
//! until it reaches feature parity; the old surface stays the default.
//!
//! See docs/specs/2026-07-03-ocean-tui-shell-rebuild-design.md.

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
mod highlight;
mod history;
mod kitty;
mod markdown;
mod mentions;
mod panel;
mod pty;
mod sessions;
mod slash;
mod status;
mod theme;
mod tree;
mod tui;

use app::App;
use client::DaemonClient;

/// Entry point for the new shell. Blocks on a tokio runtime, runs the app loop,
/// and always restores the terminal on the way out.
pub fn run(url: &str, workspace_root: String) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let client = DaemonClient::new(url)?;
        let mut terminal = tui::init()?;
        // The OCEAN splash: hold, then slide-and-fade. Runs before the event
        // pump spawns, so its direct crossterm polling can't race the app loop.
        crate::splash::play(&mut terminal)?;
        let result = App::new(client, workspace_root).run(&mut terminal).await;
        tui::restore()?;
        result
    })
}
