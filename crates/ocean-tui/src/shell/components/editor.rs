//! EditorComponent — syntax-highlighted text editor. Wraps `shell::editor::EditorTab`
//! (harvested from CTRL) plus a shared syntect `Highlighter`. Opens files from
//! the tree/graph, edits in place, Ctrl-S saves.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    editor::EditorTab,
    highlight::Highlighter,
};

pub struct EditorComponent {
    hl: Highlighter,
    tabs: Vec<EditorTab>,
    active: usize,
    pub focused: bool,
    last_body_h: usize,
}

impl Default for EditorComponent {
    fn default() -> Self {
        Self {
            hl: Highlighter::new(),
            tabs: Vec::new(),
            active: 0,
            focused: false,
            last_body_h: 20,
        }
    }
}

impl EditorComponent {
    pub fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// Open `path`, focusing an existing tab if already open.
    pub fn open(&mut self, path: PathBuf) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            return;
        }
        if let Ok(tab) = EditorTab::open(path, &self.hl) {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn tab(&mut self) -> Option<&mut EditorTab> {
        self.tabs.get_mut(self.active)
    }
}

impl Component for EditorComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        let vp = self.last_body_h;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Tab switching lives above the buffer.
        if ctrl && key.code == KeyCode::Char('s') {
            if let Some(t) = self.tab() {
                let _ = t.save();
            }
            return None;
        }
        let hl = &self.hl;
        let t = self.tabs.get_mut(self.active)?;
        match key.code {
            KeyCode::Up => t.move_cursor(-1, 0, vp),
            KeyCode::Down => t.move_cursor(1, 0, vp),
            KeyCode::Left => t.move_cursor(0, -1, vp),
            KeyCode::Right => t.move_cursor(0, 1, vp),
            KeyCode::Enter => t.insert_newline(hl),
            KeyCode::Backspace => t.backspace(hl),
            KeyCode::Delete => t.delete_forward(hl),
            KeyCode::Char(c) if !ctrl => t.insert_char(c, hl),
            _ => {}
        }
        None
    }

    fn tick(&mut self) -> Option<Action> {
        // Debounced full re-highlight after typing pauses (multi-line constructs).
        let hl = &self.hl;
        let mut changed = false;
        if let Some(t) = self.tabs.get_mut(self.active) {
            changed = t.settle(hl);
        }
        changed.then_some(Action::Render)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let border = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = match self.tabs.get(self.active) {
            Some(t) => format!(" {}{} ", t.name(), if t.dirty { " ●" } else { "" }),
            None => " editor ".to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.last_body_h = inner.height as usize;

        let Some(t) = self.tabs.get(self.active) else {
            frame.render_widget(
                Paragraph::new("open a file from the tree (Tab to the files pane, Enter on a file)")
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
            return;
        };

        let rows = inner.height as usize;
        let gutter_w = 5u16;
        let mut lines: Vec<Line> = Vec::new();
        for row in t.scroll..(t.scroll + rows).min(t.lines.len()) {
            let mut spans: Vec<Span> = Vec::new();
            // line number gutter
            spans.push(Span::styled(
                format!("{:>4} ", row + 1),
                Style::default().fg(Color::DarkGray),
            ));
            // syntect-highlighted runs, else raw
            if let Some(styled) = t.highlighted.get(row).filter(|s| !s.is_empty()) {
                for (color, text) in styled {
                    spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
                }
            } else if let Some(raw) = t.lines.get(row) {
                spans.push(Span::raw(raw.clone()));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // Cursor.
        if self.focused {
            let cy = inner.y + (t.cursor_row.saturating_sub(t.scroll)) as u16;
            let cx = inner.x + gutter_w + t.cursor_col as u16;
            if cy < inner.y + inner.height && cx < inner.x + inner.width {
                frame.set_cursor_position((cx, cy));
            }
        }
        let _ = Modifier::BOLD;
    }
}
