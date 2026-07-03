//! Event source for the shell: crossterm input + tick/render timers, merged
//! onto one async channel. The app loop selects on `next()`.
//!
//! Two cadences, deliberately decoupled (ratatui perf guidance): `Tick` drives
//! logic/animation at a modest rate, `Render` drives redraws. Input events
//! arrive as they happen via crossterm's async `EventStream`.

use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;

/// Everything the app loop reacts to.
#[derive(Debug, Clone)]
pub enum Event {
    /// Logic/animation tick.
    Tick,
    /// Redraw request.
    Render,
    /// A key/mouse/resize event from the terminal.
    Crossterm(CrosstermEvent),
}

/// Spawns a task that pumps crossterm input + timers onto a channel.
pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl EventHandler {
    /// `tick_hz` drives logic, `render_hz` drives redraws. Both are capped to
    /// sane rates by the caller (e.g. 8/60).
    pub fn new(tick_hz: f64, render_hz: f64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tick_period = Duration::from_secs_f64(1.0 / tick_hz.max(1.0));
        let render_period = Duration::from_secs_f64(1.0 / render_hz.max(1.0));
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            let mut tick = tokio::time::interval(tick_period);
            let mut render = tokio::time::interval(render_period);
            loop {
                let crossterm_next = reader.next().fuse();
                tokio::select! {
                    _ = tick.tick() => { let _ = tx.send(Event::Tick); }
                    _ = render.tick() => { let _ = tx.send(Event::Render); }
                    maybe_event = crossterm_next => {
                        match maybe_event {
                            Some(Ok(evt)) => { let _ = tx.send(Event::Crossterm(evt)); }
                            Some(Err(_)) => {} // ponytail: drop a bad read; next poll recovers
                            None => break,     // stream closed → stop pumping
                        }
                    }
                }
            }
        });
        Self { rx }
    }

    /// Await the next event. `None` once the source task has stopped.
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
