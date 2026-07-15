//! Rail-row styling primitives shared by the SESSIONS and FILES panels.
//! Each rail is a vertical scrolling list with a focused/blurred selection
//! state, cyan accent bar, slate bed, and depth-guided tree structure.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use unicode_width::UnicodeWidthStr;

use super::{
    panel,
    theme::{self, g},
};

// ── selection state ───────────────────────────────────────────────────────────

/// Selection appearance for one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    /// Selected + rail has keyboard focus → cyan bar, raised bg, bold text.
    Focused,
    /// Selected but rail is blurred → muted bar, softer bg, normal weight.
    Blurred,
    /// Not selected → slate bg, no bar.
    Normal,
}

impl RowState {
    pub fn new(selected: bool, focused: bool) -> Self {
        match (selected, focused) {
            (true, true) => Self::Focused,
            (true, false) => Self::Blurred,
            (false, _) => Self::Normal,
        }
    }

    fn bar(&self) -> &'static str {
        match self {
            Self::Focused => g("▎", "|"),
            Self::Blurred => g("▏", "|"),
            Self::Normal => " ",
        }
    }

    fn bar_fg(&self) -> Color {
        match self {
            Self::Focused => theme::CYAN,
            Self::Blurred => theme::BG_HL,
            Self::Normal => theme::SLATE,
        }
    }

    fn bg(&self) -> Color {
        match self {
            Self::Focused => theme::BG_HL,
            Self::Blurred => theme::HOVER,
            Self::Normal => theme::SLATE,
        }
    }

    pub fn text_modifier(&self) -> Modifier {
        if *self == Self::Focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }
    }
}

// ── row drawing ────────────────────────────────────────────────────────────────

/// Draw one rail row: bar column at `x`, text in `x+1 .. x+width-1`.
/// The row background spans the full width.
pub fn draw_row(frame: &mut Frame, x: u16, y: u16, width: u16, state: RowState, line: Line) {
    if width == 0 {
        return;
    }
    let bg = state.bg();
    // Full-width background fill so non-text whitespace isn't transparent.
    frame.render_widget(
        Paragraph::new(Span::styled(
            " ".repeat(width as usize),
            Style::default().bg(bg),
        )),
        Rect::new(x, y, width, 1),
    );
    // Bar.
    if state != RowState::Normal {
        frame.render_widget(
            Paragraph::new(Span::styled(
                state.bar(),
                Style::default().fg(state.bar_fg()).bg(bg),
            )),
            Rect::new(x, y, 1, 1),
        );
    }
    // Content area.
    frame.render_widget(
        Paragraph::new(vec![line]).style(Style::default().bg(bg)),
        Rect::new(x + 1, y, width.saturating_sub(1), 1),
    );
}

// ── scroll ─────────────────────────────────────────────────────────────────────

pub fn contains(body: Rect, column: u16, row: u16) -> bool {
    body.width > 0
        && body.height > 0
        && column >= body.x
        && column < body.x.saturating_add(body.width)
        && row >= body.y
        && row < body.y.saturating_add(body.height)
}

/// Clamp `scroll` so the selected row is visible in a view of height `view_h`.
pub fn clamp_scroll(selected: usize, scroll: &mut usize, view_h: usize) {
    if view_h == 0 {
        return;
    }
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= *scroll + view_h {
        *scroll = selected + 1 - view_h;
    }
}

// ── empty state ────────────────────────────────────────────────────────────────

/// Centered dim message when a rail has no content to show.
pub fn draw_empty(frame: &mut Frame, body: Rect, msg: &str) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    let fitted = panel::fit_cells(msg, body.width as usize);
    let msg_w = UnicodeWidthStr::width(fitted.as_str()) as u16;
    let y = body.y + body.height / 2;
    let x = body.x + body.width.saturating_sub(msg_w) / 2;
    frame.render_widget(
        Paragraph::new(Span::styled(fitted, Style::default().fg(theme::COMMENT)))
            .style(Style::default().bg(theme::SLATE)),
        Rect::new(x, y, msg_w, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_state_distinguishes_focus_from_blurred_selection() {
        assert_eq!(RowState::new(false, true), RowState::Normal);
        assert_eq!(RowState::new(true, true), RowState::Focused);
        assert_eq!(RowState::new(true, false), RowState::Blurred);
        assert_eq!(RowState::Focused.text_modifier(), Modifier::BOLD);
        assert_eq!(RowState::Blurred.text_modifier(), Modifier::empty());
    }

    #[test]
    fn clamp_scroll_keeps_selection_inside_view() {
        let mut scroll = 5;
        clamp_scroll(2, &mut scroll, 4);
        assert_eq!(scroll, 2);
        clamp_scroll(8, &mut scroll, 4);
        assert_eq!(scroll, 5);
        clamp_scroll(7, &mut scroll, 4);
        assert_eq!(scroll, 5);
    }

    #[test]
    fn body_contains_uses_half_open_bounds() {
        let body = Rect::new(10, 20, 5, 3);
        assert!(contains(body, 10, 20));
        assert!(contains(body, 14, 22));
        assert!(!contains(body, 9, 20));
        assert!(!contains(body, 15, 20));
        assert!(!contains(body, 10, 19));
        assert!(!contains(body, 10, 23));
    }

    #[test]
    fn focused_and_blurred_rows_render_distinct_styles() {
        let backend = ratatui::backend::TestBackend::new(8, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_row(
                    frame,
                    0,
                    0,
                    8,
                    RowState::Focused,
                    Line::from(Span::raw("focused")),
                );
                draw_row(
                    frame,
                    0,
                    1,
                    8,
                    RowState::Blurred,
                    Line::from(Span::raw("blurred")),
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((0, 0)).unwrap().fg, theme::CYAN);
        assert_eq!(buf.cell((1, 0)).unwrap().bg, theme::BG_HL);
        assert_eq!(buf.cell((0, 1)).unwrap().fg, theme::BG_HL);
        assert_eq!(buf.cell((1, 1)).unwrap().bg, theme::HOVER);
    }
}
