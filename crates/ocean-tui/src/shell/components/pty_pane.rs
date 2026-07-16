//! PtyComponent — hosts the workbench's embedded project terminal.
//! Wraps `shell::pty::TermPane` and renders it with tui-term. Session resume is
//! native to chat; this pane is only for operator shell commands.

use std::path::Path;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, style::Style, widgets::Block, Frame};
use tui_term::widget::PseudoTerminal;

use crate::shell::{action::Action, component::Component, panel, pty::TermPane, theme};

#[derive(Default)]
pub struct PtyComponent {
    term: Option<TermPane>,
    last_area: Rect,
    pub focused: bool,
}

impl PtyComponent {
    pub fn is_active(&self) -> bool {
        self.term.is_some()
    }

    /// Spawn a shell in `cwd`, size it to the last drawn area, and type `line`.
    pub fn open(&mut self, cwd: &Path, line: &str) {
        let (rows, cols) = self.body_size();
        match TermPane::new(cwd, rows, cols) {
            Ok(mut t) => {
                t.send(line.as_bytes());
                self.term = Some(t);
            }
            Err(_) => {
                self.term = None;
            }
        }
    }

    fn body_size(&self) -> (u16, u16) {
        // Inside the border.
        let rows = self.last_area.height.saturating_sub(2).max(2);
        let cols = self.last_area.width.saturating_sub(2).max(8);
        (rows, cols)
    }
}

impl Component for PtyComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        if let Some(t) = self.term.as_mut() {
            t.send_key(key);
        }
        None
    }

    /// Bracketed paste: forward to the child terminal (bracket-aware — see
    /// `TermPane::paste`).
    fn handle_paste(&mut self, text: &str) -> Option<Action> {
        if !self.focused {
            return None;
        }
        if let Some(t) = self.term.as_mut() {
            t.paste(text);
        }
        None
    }

    fn tick(&mut self) -> Option<Action> {
        if let Some(t) = self.term.as_mut() {
            if t.pump() {
                return Some(Action::Render);
            }
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.last_area = area;
        let body = panel::draw(frame, area, "", None, self.focused);
        if body.width == 0 {
            return;
        }
        // Terminal content sits on the dark void, like CTRL's embedded shells.
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            body,
        );

        if let Some(t) = self.term.as_mut() {
            t.resize(body.height.max(2), body.width.max(8));
            frame.render_widget(PseudoTerminal::new(t.parser.screen()), body);
        }
        // Reserved footer row stays for stable panel geometry; nothing to say.
        panel::footer(frame, area, "");
    }
}
