//! FileTreeComponent — the project explorer wearing CTRL's panel skin: slate
//! bed, plain FILES title, hairline, accent bar on the selected row, dirs in
//! blue with ▸/▾ carets. Enter expands a dir or opens a file in the editor.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    Frame,
};

use unicode_width::UnicodeWidthStr;

use crate::shell::{
    action::Action,
    component::Component,
    components::chat::sanitize_line,
    panel,
    rail::{self, RowState},
    theme::{self, g},
    tree::{Entry, Tree},
};

fn depth_prefix(depth: usize) -> String {
    (0..depth).map(|_| g("│ ", "| ")).collect()
}

fn file_row(entry: &Entry, width: usize, state: RowState) -> Line<'static> {
    let name = entry
        .path
        .file_name()
        .map(|n| sanitize_line(&n.to_string_lossy()))
        .unwrap_or_default();
    let caret = if entry.is_dir {
        if entry.expanded {
            g("▾ ", "v ")
        } else {
            g("▸ ", "> ")
        }
    } else {
        "  "
    };
    let prefix = format!("{}{caret}", depth_prefix(entry.depth));
    let prefix_w = UnicodeWidthStr::width(prefix.as_str()).min(width);
    let name_w = width.saturating_sub(prefix_w);
    let modifier = state.text_modifier();
    let guide_style = Style::default().fg(theme::BG_HL).add_modifier(modifier);
    let name_style = Style::default()
        .fg(if entry.is_dir { theme::BLUE } else { theme::FG })
        .add_modifier(modifier);

    if entry.is_dir {
        return Line::from(vec![
            Span::styled(panel::fit_cells(&prefix, width), guide_style),
            Span::styled(panel::pad_to(&name, name_w), name_style),
        ]);
    }

    // Keep the extension visible when the stem must truncate: file type is
    // more useful than a few additional stem cells in a narrow explorer.
    let extension = entry
        .path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| sanitize_line(&format!(".{ext}")))
        .unwrap_or_default();
    let ext_w = UnicodeWidthStr::width(extension.as_str());
    let (stem, extension) = if ext_w > 0 && ext_w < name_w {
        let stem = entry
            .path
            .file_stem()
            .map(|stem| sanitize_line(&stem.to_string_lossy()))
            .unwrap_or_default();
        let fitted_stem = panel::fit_cells(&stem, name_w - ext_w);
        (panel::pad_to(&fitted_stem, name_w - ext_w), extension)
    } else {
        (panel::pad_to(&name, name_w), String::new())
    };
    Line::from(vec![
        Span::styled(panel::fit_cells(&prefix, width), guide_style),
        Span::styled(stem, name_style),
        Span::styled(
            extension,
            Style::default().fg(theme::COMMENT).add_modifier(modifier),
        ),
    ])
}

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
                if !rail::contains(body, mouse.column, mouse.row) {
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
        self.body_rect = Rect::default();
        let body = panel::draw(frame, area, "FILES", None, self.focused);
        if body.width == 0 {
            return;
        }
        self.body_rect = body;

        if self.tree.entries.is_empty() {
            rail::draw_empty(frame, body, "No files in this workspace");
            panel::footer(frame, area, " ↵ open/toggle");
            return;
        }

        let view_h = body.height as usize;
        let sel = self.tree.selected;
        rail::clamp_scroll(sel, &mut self.scroll, view_h);

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
            let row_state = RowState::new(selected, self.focused);
            let line = file_row(e, body.width.saturating_sub(1) as usize, row_state);
            rail::draw_row(frame, body.x, y, body.width, row_state, line);
        }

        panel::footer(frame, area, " ↵ open/toggle");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn file_rows_sanitize_controls_and_fit_terminal_cells() {
        let safe = sanitize_line("  ▸ bad\t\u{1b}界界");
        let row = panel::pad_to(&safe, 12);
        assert!(!row.contains('\t') && !row.contains('\u{1b}'));
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 12);
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn render(component: &mut FileTreeComponent, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| component.draw(frame, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in buf.area.top()..buf.area.bottom() {
            for x in buf.area.left()..buf.area.right() {
                out.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    fn mouse(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ocean-file-rail-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn nested_file_row_has_depth_guide_and_preserves_extension() {
        let entry = Entry {
            path: PathBuf::from("/repo/src/非常长的组件名称.rs"),
            depth: 2,
            is_dir: false,
            expanded: false,
        };
        let line = file_row(&entry, 15, RowState::Focused);
        let text = row_text(&line);
        assert!(text.starts_with(g("│ │   ", "| |   ")));
        assert!(text.ends_with(".rs"), "extension lost from {text:?}");
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 15);
    }

    #[test]
    fn file_mouse_hits_are_bounded_to_body_columns() {
        let root = temp_root("mouse");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        let mut component = FileTreeComponent::new(root.clone());
        render(&mut component, 24, 8);
        let body = component.body_rect;
        assert_eq!(component.tree.selected, 0);

        // Same visible row but outside either horizontal edge must not select.
        assert!(component
            .handle_mouse(mouse(body.x.saturating_sub(1), body.y + 1))
            .is_none());
        assert!(component
            .handle_mouse(mouse(body.x + body.width, body.y + 1))
            .is_none());
        assert_eq!(component.tree.selected, 0);

        assert!(component
            .handle_mouse(mouse(body.x + 1, body.y + 1))
            .is_none());
        assert_eq!(component.tree.selected, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_unicode_and_resized_file_rails_render_safely() {
        let empty_root = temp_root("empty");
        let mut empty = FileTreeComponent::new(empty_root.clone());
        assert!(render(&mut empty, 12, 7).contains("No files"));

        let unicode_root = temp_root("unicode");
        std::fs::write(unicode_root.join("界面组件.rs"), "").unwrap();
        let mut component = FileTreeComponent::new(unicode_root.clone());
        let narrow = render(&mut component, 12, 7);
        assert!(narrow.contains(".rs"));
        let wide: String = render(&mut component, 30, 9)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(wide.contains("界面组件.rs"));

        let _ = std::fs::remove_dir_all(empty_root);
        let _ = std::fs::remove_dir_all(unicode_root);
    }
}
