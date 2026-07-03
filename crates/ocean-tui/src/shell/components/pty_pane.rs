//! PtyComponent — hosts a live embedded terminal for a resumed Ocean session.
//! Wraps `shell::pty::TermPane` and renders it with tui-term. Empty until a
//! session is opened from the rail; then it runs that session's `ocean --resume`
//! line in a shell.

use std::path::Path;

use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tui_term::widget::PseudoTerminal;

use crate::shell::{action::Action, component::Component, pty::TermPane};

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
        let border = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(" session ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        match self.term.as_mut() {
            Some(t) => {
                t.resize(inner.height.max(2), inner.width.max(8));
                frame.render_widget(PseudoTerminal::new(t.parser.screen()), inner);
            }
            None => {
                frame.render_widget(
                    Paragraph::new("select a session on the left and press Enter to resume it here")
                        .style(Style::default().fg(Color::DarkGray)),
                    inner,
                );
            }
        }
    }
}
