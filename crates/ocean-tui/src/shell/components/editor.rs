//! EditorComponent — syntax-highlighted text editor. Wraps `shell::editor::EditorTab`
//! (harvested from CTRL) plus a shared syntect `Highlighter`. Opens files from
//! the tree/graph, edits in place, Ctrl-S saves.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

use crate::shell::{
    action::Action,
    component::Component,
    editor::EditorTab,
    git::Mark,
    highlight::Highlighter,
    panel,
    theme::{self, g},
};

pub struct EditorComponent {
    hl: Highlighter,
    root: PathBuf,
    tabs: Vec<EditorTab>,
    active: usize,
    pub focused: bool,
    last_body_h: usize,
}

impl EditorComponent {
    pub fn new(root: PathBuf) -> Self {
        Self {
            hl: Highlighter::new(),
            root,
            tabs: Vec::new(),
            active: 0,
            focused: false,
            last_body_h: 20,
        }
    }

    pub fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// Breadcrumb text: the open file's path relative to the project root.
    pub fn crumb(&self) -> String {
        match self.tabs.get(self.active) {
            Some(t) => t
                .path
                .strip_prefix(&self.root)
                .unwrap_or(&t.path)
                .display()
                .to_string(),
            None => "no file".to_string(),
        }
    }

    /// Open `path`, focusing an existing tab if already open.
    pub fn open(&mut self, path: PathBuf) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            return;
        }
        if let Ok(mut tab) = EditorTab::open(path, &self.hl) {
            tab.load_git(&self.root);
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn tab(&mut self) -> Option<&mut EditorTab> {
        self.tabs.get_mut(self.active)
    }
}

impl Component for EditorComponent {
    /// Bracketed paste: insert into the buffer at the cursor, newline-aware.
    /// Tabs are kept verbatim (file content fidelity); CRs fold into the
    /// following newline; other control bytes drop.
    fn handle_paste(&mut self, text: &str) -> Option<Action> {
        if !self.focused {
            return None;
        }
        let hl = &self.hl;
        let t = self.tabs.get_mut(self.active)?;
        for c in text.chars() {
            match c {
                '\n' => t.insert_newline(hl),
                '\r' => {}
                '\t' => t.insert_char('\t', hl),
                c if c.is_control() => {}
                c => t.insert_char(c, hl),
            }
        }
        None
    }

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
        let (title, dirty) = match self.tabs.get(self.active) {
            Some(t) => (t.name().to_uppercase(), t.dirty),
            None => ("EDITOR".to_string(), false),
        };
        let pill = dirty.then(|| format!("{} unsaved", g("●", "*")));
        let body = panel::draw(frame, area, &title, pill.as_deref(), self.focused);
        if body.width == 0 {
            return;
        }
        // Editor void bed over the slate (CTRL's editor sits on BG, not SLATE).
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG)),
            body,
        );
        self.last_body_h = body.height as usize;

        let Some(t) = self.tabs.get(self.active) else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " open a file: ⌃⌥2 files, Enter on a file",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::BG)),
                body,
            );
            panel::footer(frame, area, " no file open");
            return;
        };

        let rows = body.height as usize;
        let gutter_w = 6u16; // 1 git mark + "{:>4} " line number
        let mut lines: Vec<Line> = Vec::new();
        for row in t.scroll..(t.scroll + rows).min(t.lines.len()) {
            let mut spans: Vec<Span> = Vec::new();
            // git gutter mark (1 col): ▎ colored by change kind.
            let (mark_ch, mark_color) = match t.git_lines.get(&row) {
                Some(Mark::Added) => (g("▎", "+"), theme::GREEN),
                Some(Mark::Modified) => (g("▎", "~"), theme::YELLOW),
                Some(Mark::Deleted) => (g("▁", "-"), theme::RED),
                None => (" ", theme::BG),
            };
            spans.push(Span::styled(
                mark_ch,
                Style::default().fg(mark_color).bg(theme::BG_DARK),
            ));
            // line number gutter on the dark void rail
            spans.push(Span::styled(
                format!("{:>4} ", row + 1),
                Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
            ));
            // syntect-highlighted runs, else raw
            if let Some(styled) = t.highlighted.get(row).filter(|s| !s.is_empty()) {
                for (color, text) in styled {
                    spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
                }
            } else if let Some(raw) = t.lines.get(row) {
                spans.push(Span::styled(raw.clone(), Style::default().fg(theme::FG)));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme::BG)),
            body,
        );

        // Cursor.
        if self.focused {
            let cy = body.y + (t.cursor_row.saturating_sub(t.scroll)) as u16;
            let cx = body.x + gutter_w + t.cursor_col as u16;
            if cy < body.y + body.height && cx < body.x + body.width {
                frame.set_cursor_position((cx, cy));
            }
        }
        panel::footer(
            frame,
            area,
            &format!(" {}:{}  ·  ⌃S save", t.cursor_row + 1, t.cursor_col + 1),
        );
        let _ = Modifier::BOLD;
    }
}
