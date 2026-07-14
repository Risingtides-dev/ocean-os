//! CTRL's panel chrome, extracted as a reusable frame so every pane in the
//! shell wears the same skin: a slate bed, a plain TITLE row (with optional
//! right-aligned plain state text, e.g. the editor's `unsaved`), and a
//! hairline underline. Returns the inner body rect.
//!
//! This is the depth-fill framing from CTRL's panel_sessions.rs, generalized —
//! minus the decorative title glyph and pill styling (house rules: no
//! decorative icons, plain undecorated labels).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme;

/// Draw the chrome for one panel and return the inner body area (below the
/// title + hairline, inside the edge columns). `focused` lights the title;
/// `state` renders right-aligned on the title row as plain contextual text
/// (exceptional state only, e.g. the editor's `unsaved`).
pub fn draw(f: &mut Frame, area: Rect, title: &str, state: Option<&str>, focused: bool) -> Rect {
    if area.width < 6 || area.height < 4 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    // panel bed
    f.render_widget(
        Block::default().style(Style::default().bg(theme::SLATE)),
        area,
    );
    // NOTE: the panels no longer paint their own left light-edge / right shadow
    // columns. Adjacent panes are separated by the dedicated `splitter()` rule
    // drawn between them (app::draw); painting a per-panel edge PLUS a shadow
    // PLUS the splitter stacked 2–3 vertical bars (▏▏▏) at every seam. The
    // splitter is now the single, clean, theme::EDGE divider. The 1-col inner
    // inset is kept as breathing room so pane content never abuts the seam.

    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );

    // ── title row: plain "TITLE" left, plain state text right ────────────────
    // An empty title drops the label but keeps the row (state stays
    // right-aligned, hairline stays) — used by the chat pane, where the app
    // title bar + breadcrumb already say where you are and a third "OCEAN"
    // was pure redundancy.
    let title_fg = if focused { theme::BLUE } else { theme::COMMENT };
    let state_txt = state.unwrap_or("");
    let state_w = state_txt.chars().count();
    let pad = (inner.width as usize).saturating_sub(title.chars().count() + 1 + state_w);
    let mut spans = vec![Span::styled(
        format!(" {title}"),
        Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(" ".repeat(pad)));
    if !state_txt.is_empty() {
        spans.push(Span::styled(
            state_txt.to_string(),
            Style::default().fg(theme::COMMENT),
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

pub fn fit_cells(s: &str, width: usize) -> String {
    let mut out = String::new();
    for grapheme in s.graphemes(true) {
        let mut candidate = out.clone();
        candidate.push_str(grapheme);
        if UnicodeWidthStr::width(candidate.as_str()) > width {
            break;
        }
        out = candidate;
    }
    out
}

pub fn pad_to(s: &str, w: usize) -> String {
    let fitted = fit_cells(s, w);
    let n = UnicodeWidthStr::width(fitted.as_str());
    format!("{fitted}{}", " ".repeat(w.saturating_sub(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cells_keeps_contextual_emoji_whole() {
        assert_eq!(fit_cells(" 👩‍💻 tail", 4), " 👩‍💻 ");
        assert!(UnicodeWidthStr::width(fit_cells("界界", 3).as_str()) <= 3);
    }
}
