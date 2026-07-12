//! FileTreeComponent — the project explorer wearing CTRL's panel skin: slate
//! bed, plain FILES title, hairline, accent bar on the selected row, dirs in
//! blue with ▸/▾ carets. Enter expands a dir or opens a file in the editor.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::shell::{
    action::Action,
    component::Component,
    panel,
    theme::{self, g},
    tree::Tree,
};

pub struct FileTreeComponent {
    tree: Tree,
    scroll: usize,
    pub focused: bool,
    body_rect: Rect,
}

impl FileTreeComponent {
    pub fn new(root: PathBuf) -> Self {
        Self {
            tree: Tree::new(root),
            scroll: 0,
            focused: false,
            body_rect: Rect::default(),
        }
    }

    /// Live-reflect on-disk changes (new files the agent/terminal created)
    /// while preserving expansion + selection. Called on a throttled tick.
    pub fn rescan(&mut self) {
        self.tree.rescan();
    }

    /// Re-root the explorer at a new project directory (switching worktrees /
    /// starting a session in another project). Resets expansion + selection.
    pub fn set_root(&mut self, root: PathBuf) {
        if self.tree.root != root {
            self.tree = Tree::new(root);
            self.scroll = 0;
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

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.tree.move_sel(-1);
                None
            }
            MouseEventKind::ScrollDown => {
                self.tree.move_sel(1);
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let body = self.body_rect;
                if body.width == 0 || mouse.row < body.y || mouse.row >= body.y + body.height {
                    return None;
                }
                let i = self.scroll + (mouse.row - body.y) as usize;
                if i >= self.tree.entries.len() {
                    return None;
                }
                if i == self.tree.selected {
                    // click on the selected row toggles/opens (CTRL behavior)
                    self.tree.activate().map(Action::OpenFile)
                } else {
                    self.tree.selected = i;
                    None
                }
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let body = panel::draw(frame, area, "FILES", None, self.focused);
        if body.width == 0 {
            return;
        }
        self.body_rect = body;

        let view_h = body.height as usize;
        let sel = self.tree.selected;
        if sel < self.scroll {
            self.scroll = sel;
        } else if view_h > 0 && sel >= self.scroll + view_h {
            self.scroll = sel + 1 - view_h;
        }

        let bottom = body.y + body.height;
        for (i, e) in self
            .tree
            .entries
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(view_h)
        {
            let y = body.y + (i - self.scroll) as u16;
            if y >= bottom {
                break;
            }
            let selected = i == sel;
            let row_bg = if selected { theme::BG_HL } else { theme::SLATE };
            let name = e
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let caret = if e.is_dir {
                if e.expanded {
                    g("▾ ", "v ")
                } else {
                    g("▸ ", "> ")
                }
            } else {
                "  "
            };
            let fg = if e.is_dir { theme::BLUE } else { theme::FG };
            let modif = if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };

            let bar = if selected { g("▎", "|") } else { " " };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    bar,
                    Style::default().fg(theme::CYAN).bg(row_bg),
                )),
                Rect::new(body.x, y, 1, 1),
            );
            let txt = format!("{}{caret}{name}", "  ".repeat(e.depth));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    panel::pad_to(&txt, body.width.saturating_sub(1) as usize),
                    Style::default().fg(fg).add_modifier(modif),
                )))
                .style(Style::default().bg(row_bg)),
                Rect::new(body.x + 1, y, body.width.saturating_sub(1), 1),
            );
        }

        // Reserved footer row stays for stable panel geometry; nothing to say.
        panel::footer(frame, area, "");
    }
}
