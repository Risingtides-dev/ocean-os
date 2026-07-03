//! CTRL's panel chrome, extracted as a reusable frame so every pane in the
//! shell wears the same skin: a slate bed, a left light-edge column, a right
//! faux drop-shadow column, a `◆ TITLE` row (with an optional right-aligned
//! pill), and a hairline underline. Returns the inner body rect.
//!
//! This is the depth-fill framing from CTRL's panel_sessions.rs, generalized.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

use super::theme::{self, g};

/// Draw the chrome for one panel and return the inner body area (below the
/// title + hairline, inside the edge columns). `focused` lights the title and
/// accent; `pill` renders right-aligned on the title row (e.g. a sort mode or
/// model tag).
pub fn draw(f: &mut Frame, area: Rect, title: &str, pill: Option<&str>, focused: bool) -> Rect {
    if area.width < 6 || area.height < 4 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    // panel bed
    f.render_widget(
        Block::default().style(Style::default().bg(theme::SLATE)),
        area,
    );
    // left light-edge + right shadow columns (the depth-fill)
    paint_col(f, area.x, area.y, area.height, theme::EDGE, theme::SLATE);
    paint_col(
        f,
        area.x + area.width - 1,
        area.y,
        area.height,
        theme::SLATE,
        theme::SHADOW,
    );

    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );

    // ── title row: "◆ TITLE" left, pill right ────────────────────────────────
    let title_fg = if focused { theme::BLUE } else { theme::COMMENT };
    let title_txt = format!("{} {}", g("◆", "*"), title);
    let pill_txt = pill.unwrap_or("");
    let pill_w = if pill_txt.is_empty() {
        0
    } else {
        pill_txt.chars().count() + 2
    };
    let pad = (inner.width as usize).saturating_sub(title_txt.chars().count() + 1 + pill_w);
    let mut spans = vec![Span::styled(
        format!(" {title_txt}"),
        Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(" ".repeat(pad)));
    if !pill_txt.is_empty() {
        spans.push(Span::styled(
            format!(" {pill_txt} "),
            Style::default().fg(theme::CYAN).bg(theme::BG_HL),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::SLATE)),
        Rect::new(inner.x, area.y, inner.width, 1),
    );
    // hairline underline
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(theme::BG_HL),
        )))
        .style(Style::default().bg(theme::SLATE)),
        Rect::new(inner.x, area.y + 1, inner.width, 1),
    );

    Rect::new(
        inner.x,
        area.y + 2,
        inner.width,
        area.height.saturating_sub(3), // leave a footer row inside the bed
    )
}

/// Bottom footer row inside a panel drawn with [`draw`] — dim text on the bed.
pub fn footer(f: &mut Frame, area: Rect, text: &str) {
    if area.width < 6 || area.height < 4 {
        return;
    }
    let inner_w = area.width.saturating_sub(2);
    f.render_widget(
        Paragraph::new(Span::styled(
            pad_to(text, inner_w as usize),
            Style::default().fg(theme::COMMENT),
        ))
        .style(Style::default().bg(theme::SLATE)),
        Rect::new(area.x + 1, area.y + area.height - 1, inner_w, 1),
    );
}

pub fn pad_to(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.chars().take(w).collect()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

fn paint_col(
    f: &mut Frame,
    x: u16,
    y: u16,
    h: u16,
    fg: ratatui::style::Color,
    bg: ratatui::style::Color,
) {
    for k in 0..h {
        f.render_widget(
            Paragraph::new(Span::styled("▏", Style::default().fg(fg)))
                .style(Style::default().bg(bg)),
            Rect::new(x, y + k, 1, 1),
        );
    }
}
