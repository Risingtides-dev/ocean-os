//! SessionRailComponent — the left rail listing Ocean sessions for the current
//! project root. Arrow keys move the selection; Enter opens the highlighted
//! session in the PTY (runs its `ocean --resume` line). Data comes from
//! `shell::sessions` (pure disk discovery, no daemon round-trip).

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::shell::{
    action::Action,
    component::Component,
    sessions::{ago, discover, Session, Sort},
};

pub struct SessionRailComponent {
    root: PathBuf,
    sessions: Vec<Session>,
    selected: usize,
    pub focused: bool,
}

impl SessionRailComponent {
    pub fn new(root: PathBuf) -> Self {
        let sessions = discover(&root, Sort::Date);
        Self {
            root,
            sessions,
            selected: 0,
            focused: true,
        }
    }

    pub fn refresh(&mut self) {
        self.sessions = discover(&self.root, Sort::Date);
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    fn open_selected(&self) -> Option<Action> {
        let s = self.sessions.get(self.selected)?;
        let (cmd, args) = s.resume_command();
        // Match CTRL: cd into the session's cwd if it differs from the project
        // root, then run the resume line.
        let mut line = format!("{} {}", cmd, args.join(" "));
        if s.cwd != self.root {
            line = format!("cd {} && {}", s.cwd.display(), line);
        }
        line.push('\n');
        Some(Action::OpenSession {
            line,
            cwd: s.cwd.clone(),
        })
    }
}

impl Component for SessionRailComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('r') => {
                self.refresh();
                None
            }
            KeyCode::Enter => self.open_selected(),
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(" sessions ({}) ", self.sessions.len());
        let border = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.sessions.is_empty() {
            frame.render_widget(
                Paragraph::new("no ocean sessions\nfor this project")
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
            return;
        }

        // Simple vertical window around the selection.
        let rows = inner.height as usize;
        let start = self.selected.saturating_sub(rows.saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        for (i, s) in self.sessions.iter().enumerate().skip(start).take(rows) {
            let selected = i == self.selected;
            let marker = if selected { "▎" } else { " " };
            let name_style = if selected {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let badge = Span::styled(
                " OC ",
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
            );
            let title_w = (inner.width as usize).saturating_sub(10);
            let name = truncate(&s.title, title_w);
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                badge,
                Span::raw(" "),
                Span::styled(name, name_style),
                Span::raw(" "),
                Span::styled(ago(s.mtime), Style::default().fg(Color::DarkGray)),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}
