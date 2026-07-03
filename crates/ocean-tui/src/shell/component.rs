//! The `Component` trait — the unit of UI. Each panel owns its state, draws
//! itself into a `Rect`, turns raw terminal events into `Action`s, and reacts
//! to `Action`s. This is what keeps the app off the god-struct path: adding a
//! panel is a new `Component`, not a new field on one struct.

use crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};

use super::action::Action;

#[allow(unused_variables)]
pub trait Component {
    /// Translate a raw terminal event into an optional action. Default fans out
    /// to `handle_key`/`handle_mouse`.
    fn handle_event(&mut self, event: &CrosstermEvent) -> Option<Action> {
        match event {
            CrosstermEvent::Key(k) => self.handle_key(*k),
            CrosstermEvent::Mouse(m) => self.handle_mouse(*m),
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        None
    }

    /// React to an action. May emit a follow-up action.
    fn update(&mut self, action: &Action) -> Option<Action> {
        None
    }

    /// Periodic tick — e.g. drain a PTY. Return `Action::Render` to force a
    /// redraw when internal state changed off-band.
    fn tick(&mut self) -> Option<Action> {
        None
    }

    /// Draw into the given area.
    fn draw(&mut self, frame: &mut Frame, area: Rect);
}
