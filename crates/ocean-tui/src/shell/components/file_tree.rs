//! FileTreeComponent — the project file explorer. Wraps `shell::tree::Tree`
//! (harvested from CTRL). Arrow keys move, Enter expands a dir or opens a file.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::shell::{action::Action, component::Component, tree::Tree};

pub struct FileTreeComponent {
    tree: Tree,
    pub focused: bool,
}

impl FileTreeComponent {
    pub fn new(root: PathBuf) -> Self {
        Self {
            tree: Tree::new(root),
            focused: false,
        }
    }
}

impl Component for FileTreeComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.tree.move_sel(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.tree.move_sel(1);
                None
            }
            KeyCode::Enter => self.tree.activate().map(Action::OpenFile),
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let border = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(" files ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = inner.height as usize;
        let sel = self.tree.selected;
        let start = sel.saturating_sub(rows.saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        for (i, e) in self.tree.entries.iter().enumerate().skip(start).take(rows) {
            let name = e
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let icon = if e.is_dir {
                if e.expanded { "▾ " } else { "▸ " }
            } else {
                "  "
            };
            let indent = "  ".repeat(e.depth);
            let selected = i == sel;
            let style = if selected {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else if e.is_dir {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            };
            let marker = if selected { "▎" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::styled(format!("{indent}{icon}{name}"), style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
