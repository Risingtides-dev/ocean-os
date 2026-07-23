//! EditorComponent — syntax-highlighted source editor plus a read-only, styled
//! Markdown preview. Opens files from the tree/graph, edits source in place,
//! Ctrl-S saves, and Ctrl-P flips Markdown tabs between source and preview.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{block::Padding, Block, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::shell::{
    action::Action,
    component::Component,
    components::chat::sanitize_line,
    editor::EditorTab,
    git::Mark,
    highlight::Highlighter,
    markdown::Markdown,
    panel,
    theme::{self, g},
};

const GUTTER_W: u16 = 6; // 1 git mark + "{:>4} " line number
const WHEEL_ROWS: isize = 3;
const PREVIEW_PAD_X: u16 = 2;

pub struct EditorComponent {
    hl: Highlighter,
    root: PathBuf,
    tabs: Vec<EditorTab>,
    markdown: Markdown,
    active: usize,
    pub focused: bool,
    last_body_h: usize,
    last_text_w: usize,
    follow_cursor: bool,
    selection_body: Rect,
    selection_top: usize,
}

impl EditorComponent {
    pub fn new(root: PathBuf) -> Self {
        Self {
            hl: Highlighter::new(),
            root,
            tabs: Vec::new(),
            markdown: Markdown::default(),
            active: 0,
            focused: false,
            last_body_h: 20,
            last_text_w: 80,
            follow_cursor: true,
            selection_body: Rect::default(),
            selection_top: 0,
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
            if self.tabs[i].markdown_preview {
                self.markdown.clear();
            }
            return;
        }
        if let Ok(mut tab) = EditorTab::open(path, &self.hl) {
            tab.markdown_preview = is_markdown(&tab.ext());
            if tab.markdown_preview {
                self.markdown.clear();
            }
            tab.load_git(&self.root);
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn tab(&mut self) -> Option<&mut EditorTab> {
        self.tabs.get_mut(self.active)
    }

    /// Stable visual row under a painted editor body cell. Code rows map to
    /// document lines; prose rows map to the width-dependent wrapped-row stream.
    pub fn selection_row_for_screen(&self, screen_row: u16) -> Option<usize> {
        (self.has_tabs()
            && screen_row >= self.selection_body.y
            && screen_row < self.selection_body.bottom())
        .then(|| self.selection_top + usize::from(screen_row - self.selection_body.y))
    }

    pub fn selection_columns(&self) -> Option<(u16, u16)> {
        let gutter = self
            .tabs
            .get(self.active)
            .filter(|t| t.markdown_preview && is_markdown(&t.ext()))
            .map_or(GUTTER_W, |_| 0);
        (self.has_tabs() && self.selection_body.width > gutter).then(|| {
            (
                self.selection_body.x + gutter,
                self.selection_body.right().saturating_sub(1),
            )
        })
    }

    /// Saturate editor chrome/composer-edge drags to the nearest painted content
    /// row while retaining the same stable row coordinate used across scrolling.
    pub fn nearest_selection_row(&self, screen_row: u16) -> Option<usize> {
        (self.has_tabs() && self.selection_body.height > 0).then(|| {
            let row = screen_row.clamp(
                self.selection_body.y,
                self.selection_body.bottom().saturating_sub(1),
            );
            self.selection_top + usize::from(row - self.selection_body.y)
        })
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
        if t.markdown_preview && is_markdown(&t.ext()) {
            return None;
        }
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
        let vp = self.last_body_h.max(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('p') {
            let t = self.tabs.get_mut(self.active)?;
            if is_markdown(&t.ext()) {
                t.markdown_preview = !t.markdown_preview;
                self.markdown.clear();
                self.follow_cursor = !t.markdown_preview;
                return Some(Action::Render);
            }
            return None;
        }
        if ctrl && key.code == KeyCode::Char('s') {
            if let Some(t) = self.tab() {
                let _ = t.save();
            }
            return None;
        }
        let preview = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.markdown_preview && is_markdown(&t.ext()));
        if preview {
            let t = self.tabs.get_mut(self.active)?;
            match key.code {
                KeyCode::Up => t.preview_scroll = t.preview_scroll.saturating_sub(1),
                KeyCode::Down => t.preview_scroll = t.preview_scroll.saturating_add(1),
                KeyCode::PageUp => t.preview_scroll = t.preview_scroll.saturating_sub(vp),
                KeyCode::PageDown => t.preview_scroll = t.preview_scroll.saturating_add(vp),
                KeyCode::Home => t.preview_scroll = 0,
                KeyCode::End => t.preview_scroll = usize::MAX,
                _ => {}
            }
            return Some(Action::Render);
        }
        self.follow_cursor = true;
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

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let (prose, preview) = self
            .tabs
            .get(self.active)
            .map(|t| {
                (
                    is_prose(&t.ext()),
                    t.markdown_preview && is_markdown(&t.ext()),
                )
            })
            .unwrap_or_default();
        let height = self.last_body_h;
        self.follow_cursor = false;
        let t = self.tabs.get_mut(self.active)?;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if preview {
                    t.preview_scroll = t.preview_scroll.saturating_sub(WHEEL_ROWS as usize);
                } else if prose {
                    t.visual_scroll = t.visual_scroll.saturating_sub(WHEEL_ROWS as usize);
                } else {
                    t.scroll_lines(-WHEEL_ROWS, height);
                }
            }
            MouseEventKind::ScrollDown => {
                if preview {
                    t.preview_scroll = t.preview_scroll.saturating_add(WHEEL_ROWS as usize);
                } else if prose {
                    t.visual_scroll = t.visual_scroll.saturating_add(WHEEL_ROWS as usize);
                } else {
                    t.scroll_lines(WHEEL_ROWS, height);
                }
            }
            _ => return None,
        }
        Some(Action::Render)
    }

    fn tick(&mut self) -> Option<Action> {
        let hl = &self.hl;
        let mut changed = false;
        if let Some(t) = self.tabs.get_mut(self.active) {
            changed = t.settle(hl);
        }
        changed.then_some(Action::Render)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let (title, dirty, preview) = match self.tabs.get(self.active) {
            Some(t) => (
                t.name().to_uppercase(),
                t.dirty,
                t.markdown_preview && is_markdown(&t.ext()),
            ),
            None => ("EDITOR".to_string(), false, false),
        };
        let state = match (preview, dirty) {
            (true, true) => Some("preview · unsaved"),
            (true, false) => Some("preview"),
            (false, true) => Some("unsaved"),
            (false, false) => None,
        };
        let body = panel::draw(frame, area, &title, state, self.focused);
        self.selection_body = body;
        if body.width == 0 {
            self.selection_top = 0;
            return;
        }
        frame.render_widget(Block::default().style(Style::default().bg(theme::BG)), body);
        self.last_body_h = body.height as usize;

        let Some(t) = self.tabs.get_mut(self.active) else {
            self.selection_top = 0;
            panel::footer(frame, area, " no file open");
            return;
        };

        if preview {
            self.last_text_w = body.width as usize;
            let source = t
                .lines
                .iter()
                .map(|line| sanitize_line(line))
                .collect::<Vec<_>>()
                .join("\n");
            let rendered = self.markdown.render(&source);
            let paragraph = Paragraph::new(rendered.lines)
                .style(Style::default().bg(theme::BG))
                .block(Block::default().padding(Padding::horizontal(PREVIEW_PAD_X)))
                .wrap(Wrap { trim: false });
            let content_width = body.width.saturating_sub(PREVIEW_PAD_X.saturating_mul(2));
            let wrapped_rows = paragraph.line_count(content_width);
            let max_scroll = wrapped_rows
                .saturating_sub(body.height as usize)
                .min(u16::MAX as usize);
            t.preview_scroll = t.preview_scroll.min(max_scroll);
            self.selection_top = t.preview_scroll;
            frame.render_widget(paragraph.scroll((t.preview_scroll as u16, 0)), body);
            panel::footer(frame, area, " preview · ^P source · ↑↓ scroll");
            return;
        }

        self.last_text_w = body.width.saturating_sub(GUTTER_W) as usize;
        let text_w = self.last_text_w.max(1);
        let prose = is_prose(&t.ext());
        let (lines, cursor) = if prose {
            let result = prose_view(t, body.height as usize, text_w, self.follow_cursor);
            self.selection_top = t.visual_scroll;
            result
        } else {
            let result = code_view(t, body.height as usize, text_w, self.follow_cursor);
            self.selection_top = t.scroll;
            result
        };
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme::BG)),
            body,
        );

        if self.focused {
            if let Some((x, y)) = cursor {
                let cx = body.x + GUTTER_W + x as u16;
                let cy = body.y + y as u16;
                if cx < body.right() && cy < body.bottom() {
                    frame.set_cursor_position((cx, cy));
                }
            }
        }
        let footer = if is_markdown(&t.ext()) {
            format!(
                " {}:{} · source · ^P preview",
                t.cursor_row + 1,
                t.cursor_col + 1
            )
        } else {
            let mode = if prose { "wrap" } else { "scroll" };
            format!(" {}:{} · {mode}", t.cursor_row + 1, t.cursor_col + 1)
        };
        panel::footer(frame, area, &footer);
        let _ = Modifier::BOLD;
    }
}

fn is_markdown(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx")
}

fn is_prose(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "md" | "markdown" | "mdx" | "txt" | "text" | "rst" | "adoc" | "org"
    )
}

fn gutter(t: &EditorTab, row: usize, continuation: bool) -> Vec<Span<'static>> {
    if continuation {
        return vec![Span::styled(
            "      ",
            Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
        )];
    }
    let (mark_ch, mark_color) = match t.git_lines.get(&row) {
        Some(Mark::Added) => (g("▎", "+"), theme::GREEN),
        Some(Mark::Modified) => (g("▎", "~"), theme::YELLOW),
        Some(Mark::Deleted) => (g("▁", "-"), theme::RED),
        None => (" ", theme::BG),
    };
    vec![
        Span::styled(mark_ch, Style::default().fg(mark_color).bg(theme::BG_DARK)),
        Span::styled(
            format!("{:>4} ", row + 1),
            Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
        ),
    ]
}

fn display_col(s: &str, char_col: usize) -> usize {
    let prefix = s.chars().take(char_col).collect::<String>();
    UnicodeWidthStr::width(sanitize_line(&prefix).as_str())
}

fn sanitized_line_with_cursor(s: &str, char_col: usize) -> (String, usize) {
    let prefix = s.chars().take(char_col).collect::<String>();
    (sanitize_line(s), sanitize_line(&prefix).chars().count())
}

/// Character ranges for terminal-cell soft wrapping. Prefer the last whitespace
/// before the edge, but hard-wrap a single oversized token so it can never clip.
fn wrap_ranges(s: &str, width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut cells = 0;
        let mut end = start;
        let mut whitespace_break = None;
        while end < chars.len() {
            let cw = chars[end].width().unwrap_or(0);
            if end > start && cells + cw > width {
                break;
            }
            cells += cw;
            end += 1;
            if chars[end - 1].is_whitespace() {
                whitespace_break = Some(end);
            }
            if cells >= width {
                break;
            }
        }
        if end < chars.len() {
            if let Some(at) = whitespace_break.filter(|at| *at > start) {
                end = at;
            }
        }
        if end == start {
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}

fn prose_view(
    t: &mut EditorTab,
    height: usize,
    width: usize,
    follow_cursor: bool,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut visual = Vec::new();
    let mut cursor_visual = 0;
    let mut cursor_x = 0;
    for (row, raw) in t.lines.iter().enumerate() {
        let (clean, clean_cursor_col) = sanitized_line_with_cursor(raw, t.cursor_col);
        let ranges = wrap_ranges(&clean, width);
        for (part, (start, end)) in ranges.iter().copied().enumerate() {
            if row == t.cursor_row && clean_cursor_col >= start && clean_cursor_col <= end {
                cursor_visual = visual.len();
                cursor_x = display_col(
                    &clean
                        .chars()
                        .skip(start)
                        .take(clean_cursor_col - start)
                        .collect::<String>(),
                    clean_cursor_col - start,
                );
            }
            let mut spans = gutter(t, row, part > 0);
            let text: String = clean.chars().skip(start).take(end - start).collect();
            spans.push(Span::styled(text, Style::default().fg(theme::FG)));
            visual.push(Line::from(spans));
        }
    }
    if follow_cursor {
        if cursor_visual < t.visual_scroll {
            t.visual_scroll = cursor_visual;
        } else if height > 0 && cursor_visual >= t.visual_scroll + height {
            t.visual_scroll = cursor_visual + 1 - height;
        }
    }
    let max_scroll = visual.len().saturating_sub(height.max(1));
    t.visual_scroll = t.visual_scroll.min(max_scroll);
    let cursor = (cursor_visual >= t.visual_scroll
        && cursor_visual < t.visual_scroll.saturating_add(height))
    .then(|| {
        (
            cursor_x.min(width.saturating_sub(1)),
            cursor_visual - t.visual_scroll,
        )
    });
    let shown = visual
        .into_iter()
        .skip(t.visual_scroll)
        .take(height)
        .collect();
    (shown, cursor)
}

fn code_view(
    t: &mut EditorTab,
    height: usize,
    width: usize,
    follow_cursor: bool,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let cursor_cells = t
        .lines
        .get(t.cursor_row)
        .map(|line| display_col(line, t.cursor_col))
        .unwrap_or(0);
    if follow_cursor {
        if t.cursor_row < t.scroll {
            t.scroll = t.cursor_row;
        } else if height > 0 && t.cursor_row >= t.scroll + height {
            t.scroll = t.cursor_row + 1 - height;
        }
        if cursor_cells < t.horizontal_scroll {
            t.horizontal_scroll = cursor_cells;
        } else if cursor_cells >= t.horizontal_scroll + width {
            t.horizontal_scroll = cursor_cells + 1 - width;
        }
    }

    let mut lines = Vec::new();
    for row in t.scroll..(t.scroll + height).min(t.lines.len()) {
        let mut spans = gutter(t, row, false);
        let raw = sanitize_line(&t.lines[row]);
        let visible = cell_slice(&raw, t.horizontal_scroll, width);
        // Horizontal viewport correctness takes precedence over retaining token
        // run boundaries; syntax color remains for the common zero-offset view.
        if t.horizontal_scroll == 0 {
            if let Some(styled) = t.highlighted.get(row).filter(|s| !s.is_empty()) {
                let mut remaining = width;
                for (color, text) in styled {
                    if remaining == 0 {
                        break;
                    }
                    let clean = sanitize_line(text);
                    let visible = cell_slice(&clean, 0, remaining);
                    remaining = remaining.saturating_sub(UnicodeWidthStr::width(visible.as_str()));
                    spans.push(Span::styled(visible, Style::default().fg(*color)));
                }
            } else {
                spans.push(Span::styled(visible, Style::default().fg(theme::FG)));
            }
        } else {
            spans.push(Span::styled(visible, Style::default().fg(theme::FG)));
        }
        lines.push(Line::from(spans));
    }
    let cursor = (t.cursor_row >= t.scroll && t.cursor_row < t.scroll + height).then(|| {
        (
            cursor_cells.saturating_sub(t.horizontal_scroll),
            t.cursor_row - t.scroll,
        )
    });
    (lines, cursor)
}

fn cell_slice(s: &str, offset: usize, width: usize) -> String {
    let mut out = String::new();
    let mut cell = 0;
    let end = offset.saturating_add(width);
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if cell + cw <= offset {
            cell += cw;
            continue;
        }
        if cell < offset {
            let clipped = (cell + cw - offset).min(width);
            out.push_str(&" ".repeat(clipped));
            cell += cw;
            continue;
        }
        if cell + cw > end {
            out.push_str(&" ".repeat(end.saturating_sub(cell)));
            break;
        }
        out.push(ch);
        cell += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_extensions_wrap_but_source_extensions_scroll() {
        assert!(is_prose("md"));
        assert!(is_prose("TXT"));
        assert!(!is_prose("rs"));
        assert!(!is_prose("ts"));
    }

    #[test]
    fn wrapping_prefers_whitespace_and_never_loses_text() {
        let text = "alpha beta gamma";
        let ranges = wrap_ranges(text, 7);
        let chars: Vec<char> = text.chars().collect();
        let rebuilt: String = ranges
            .iter()
            .flat_map(|(a, b)| chars[*a..*b].iter())
            .collect();
        assert_eq!(rebuilt, text);
        assert!(ranges.len() > 1);
    }

    #[test]
    fn cell_slice_obeys_wide_character_boundaries() {
        assert_eq!(cell_slice("ab界cd", 0, 4), "ab界");
        assert_eq!(cell_slice("ab界cd", 3, 2), " c");
        assert_eq!(cell_slice("ab界cd", 4, 2), "cd");
    }

    #[test]
    fn rendered_text_and_cursor_share_terminal_sanitization() {
        assert_eq!(sanitize_line("a\tb\u{1b}c"), "a    bc");
        assert_eq!(display_col("\tab", 1), 4);
        let (clean, cursor) = sanitized_line_with_cursor("a\u{1b}\tb", 3);
        assert_eq!(clean, "a    b");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn code_view_preserves_manual_scroll_until_keyboard_input() {
        let hl = Highlighter::new();
        let mut tab = EditorTab::open(PathBuf::from("missing.rs"), &hl).unwrap();
        tab.lines = (0..12).map(|i| format!("line {i}")).collect();
        tab.highlighted.resize(tab.lines.len(), Vec::new());
        tab.scroll = 6;

        let _ = code_view(&mut tab, 3, 20, false);
        assert_eq!(tab.scroll, 6);

        let _ = code_view(&mut tab, 3, 20, true);
        assert_eq!(tab.scroll, 0);
    }

    #[test]
    fn prose_view_preserves_manual_scroll_until_keyboard_input() {
        let hl = Highlighter::new();
        let mut tab = EditorTab::open(PathBuf::from("missing.md"), &hl).unwrap();
        tab.lines = (0..12).map(|i| format!("line {i}")).collect();
        tab.highlighted.resize(tab.lines.len(), Vec::new());
        tab.visual_scroll = 6;

        let _ = prose_view(&mut tab, 3, 20, false);
        assert_eq!(tab.visual_scroll, 6);

        let _ = prose_view(&mut tab, 3, 20, true);
        assert_eq!(tab.visual_scroll, 0);
    }

    fn focused_editor(path: &str) -> EditorComponent {
        let mut editor = EditorComponent::new(PathBuf::from("."));
        editor.focused = true;
        editor.open(PathBuf::from(path));
        editor
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn markdown_preview_is_read_only_and_preserves_source_state() {
        let mut editor = focused_editor("missing.md");
        let tab = &mut editor.tabs[0];
        tab.lines = vec!["# Draft".into(), "body".into()];
        tab.cursor_row = 1;
        tab.cursor_col = 4;
        tab.dirty = true;
        tab.visual_scroll = 7;

        let before = editor.tabs[0].lines.clone();
        editor.handle_key(press(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.tabs[0].lines, before);
        assert_eq!(editor.tabs[0].cursor_row, 1);
        assert_eq!(editor.tabs[0].cursor_col, 4);
        assert!(editor.tabs[0].dirty);
        assert_eq!(editor.tabs[0].visual_scroll, 7);

        editor.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.tabs[0].preview_scroll, 1);
        editor.handle_key(press(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(!editor.tabs[0].markdown_preview);
        editor.handle_key(press(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.tabs[0].lines[1], "bodyx");
    }

    #[test]
    fn preview_toggle_is_markdown_only() {
        let mut editor = focused_editor("missing.rs");
        assert!(editor
            .handle_key(press(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .is_none());
        assert!(!editor.tabs[0].markdown_preview);
    }

    #[test]
    fn preview_renders_unsaved_markdown_without_control_sequences() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut editor = focused_editor("missing.md");
        editor.tabs[0].lines = vec![
            "# Ocean Preview".into(),
            String::new(),
            "A **bold**\tview\u{1b} from the unsaved buffer.".into(),
            String::new(),
            "- [x] styled lists".into(),
        ];
        editor.tabs[0].dirty = true;

        let backend = TestBackend::new(72, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| editor.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("Ocean Preview"), "{screen}");
        assert!(!screen.contains("# Ocean Preview"), "{screen}");
        assert!(
            screen.contains("A bold    view from the unsaved buffer."),
            "{screen}"
        );
        assert!(screen.contains("☑ styled lists") || screen.contains("[x] styled lists"));
        assert!(screen.contains("preview · unsaved"), "{screen}");
        assert!(screen.contains("^P source"), "{screen}");
        assert!(!screen.contains('\u{1b}'), "{screen:?}");
    }

    #[test]
    fn markdown_extensions_are_distinct_from_other_prose() {
        assert!(is_markdown("md"));
        assert!(is_markdown("MARKDOWN"));
        assert!(is_markdown("mdx"));
        assert!(!is_markdown("txt"));
        assert!(!is_markdown("rs"));
    }
}
