//! Streaming markdown renderer with **prefix-freeze** — the W2 "TUI streaming
//! lock-in" from docs/specs/2026-07-03-omp-port-map.md, adapted from oh-my-pi's
//! `markdown.ts`.
//!
//! The trick: the source is split into blank-line-bounded **blocks** (fence
//! aware — a blank line inside an open ```code fence is *not* a boundary). Every
//! block *before* the still-growing tail is FROZEN: rendered once to owned
//! `Line`s and cached by content hash. As a streaming reply grows, only the tail
//! block re-renders each frame; the settled head is served from cache. The
//! invariant we protect is `render("A\n\nB")` then `render("A\n\nBC")` never
//! re-renders block `A` — see [`CacheStats`] and the unit tests.
//!
//! Per-block rendering: ATX headings (blue bold), fenced code blocks
//! syntax-highlighted via [`super::highlight::Highlighter`] on the dark bed
//! (unclosed fences mid-stream degrade gracefully — the tail renders as code
//! until the closing fence arrives), bullet / numbered lists with cyan markers,
//! blockquotes with a dim rail, and inline `code` / **bold** / *italic* runs.
//! `_` is deliberately NOT an italic delimiter so `snake_case` identifiers (the
//! common case in a coding agent's transcript) survive intact.

use std::ops::Deref;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::highlight::{Highlighter, StyledLine};
use super::theme::{self, g};

/// Cache-hit accounting, exposed for tests to assert prefix stability.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Frozen blocks served from cache (no re-render).
    pub hits: usize,
    /// Frozen blocks rendered fresh (cache miss → cache fill).
    pub misses: usize,
}

/// One rendered Markdown link, addressed by its line/span in [`MarkdownRender`].
/// The chat uses that stable identity to project exact wrapped mouse geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownLink {
    pub line: usize,
    pub span: usize,
    pub target: String,
}
/// One rendered image reference, addressed by its caption line. Clients decide
/// whether the source resolves to a displayable local image before reserving
/// any graphics rows, so remote/missing images keep the compact text fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownImage {
    pub line: usize,
    pub path: String,
}

/// Height of an inline image preview in terminal cells.
pub(crate) const INLINE_IMAGE_ROWS: u16 = 8;


/// Styled Markdown plus interaction metadata. Keeping metadata beside the
/// rendered spans lets clients add links and graphics without changing source.
pub(crate) struct MarkdownRender {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<MarkdownLink>,
    pub images: Vec<MarkdownImage>,
}

impl Deref for MarkdownRender {
    type Target = [Line<'static>];

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

impl<'a> IntoIterator for &'a MarkdownRender {
    type Item = &'a Line<'static>;
    type IntoIter = std::slice::Iter<'a, Line<'static>>;

    fn into_iter(self) -> Self::IntoIter {
        self.lines.iter()
    }
}

#[derive(Clone)]
struct RenderedBlock {
    lines: Vec<Line<'static>>,
    links: Vec<MarkdownLink>,
    images: Vec<MarkdownImage>,
}

/// Streaming markdown renderer. Owns a lazily-constructed [`Highlighter`] (the
/// syntect load is expensive — build once, share across every code fence) and a
/// content-addressed cache of rendered frozen blocks.
#[derive(Default)]
pub struct Markdown {
    hl: Option<Highlighter>,
    cache: HashMap<u64, RenderedBlock>,
    stats: CacheStats,
}

impl Markdown {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache-hit accounting since construction / last [`clear`](Self::clear).
    #[cfg(test)]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Drop the block cache (on `/clear` or a history swap) so it can't grow
    /// unbounded across sessions.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.stats = CacheStats::default();
    }

    /// Render `src` to styled lines. Frozen (pre-tail) blocks are cached by
    /// content hash and reused; the tail block re-renders every call.
    pub fn render(&mut self, src: &str) -> MarkdownRender {
        let hl = self.hl.get_or_insert_with(Highlighter::new);
        let (frozen, tail) = split_blocks(src);

        let mut rendered = MarkdownRender {
            lines: Vec::new(),
            links: Vec::new(),
            images: Vec::new(),
        };
        let mut first = true;
        for block in &frozen {
            if !first {
                rendered.lines.push(Line::from(""));
            }
            first = false;
            let key = hash_block(block);
            let block = if let Some(cached) = self.cache.get(&key) {
                self.stats.hits += 1;
                cached.clone()
            } else {
                self.stats.misses += 1;
                let block = render_block(block, hl);
                self.cache.insert(key, block.clone());
                block
            };
            append_block(&mut rendered, block);
        }
        if !tail.trim().is_empty() {
            if !first {
                rendered.lines.push(Line::from(""));
            }
            append_block(&mut rendered, render_block(&tail, hl));
        }
        rendered
    }
}

fn append_block(rendered: &mut MarkdownRender, mut block: RenderedBlock) {
    let line_base = rendered.lines.len();
    for link in &mut block.links {
        link.line += line_base;
    }
    for image in &mut block.images {
        image.line += line_base;
    }
    rendered.lines.append(&mut block.lines);
    rendered.links.append(&mut block.links);
    rendered.images.append(&mut block.images);
}

fn hash_block(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Split `src` into frozen block sources + the still-growing tail. A block
/// boundary is a blank line that is NOT inside an open code fence; every group
/// except the last is frozen. When `src` ends on a blank line the last real
/// group is frozen too and the tail is empty (the "settled head" ratchet).
fn split_blocks(src: &str) -> (Vec<String>, String) {
    let mut frozen: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for line in src.split('\n') {
        if !in_fence && line.trim().is_empty() {
            if !cur.is_empty() {
                frozen.push(cur.join("\n"));
                cur.clear();
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        cur.push(line);
    }
    (frozen, cur.join("\n"))
}

/// Render one block (no interior blank lines except inside a fence) to lines.
fn render_block(src: &str, hl: &Highlighter) -> RenderedBlock {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut images: Vec<MarkdownImage> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Fenced code block — highlight the interior on the dark bed. Tolerates
        // an unclosed fence mid-stream (renders to end, no closing marker).
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`').trim();
            let ext = lang_ext(lang);
            out.push(fence_marker(line));
            i += 1;
            let mut code = String::new();
            let mut closed = false;
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            if !code.is_empty() {
                for styled in hl.highlight(&code, ext) {
                    out.push(code_line(styled));
                }
            }
            if closed {
                out.push(fence_marker(lines[i]));
                i += 1;
            }
            continue;
        }

        // Markdown table: a `|`-delimited row whose NEXT line is the
        // `|---|---|` separator. Rendered as padded columns with box-drawing
        // dividers instead of raw pipe soup; inline styles inside cells
        // (`code`, **bold**) keep working.
        if is_table_row(trimmed)
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1].trim_start())
        {
            let mut rows: Vec<Vec<String>> = vec![table_cells(trimmed)];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j].trim_start()) {
                rows.push(table_cells(lines[j].trim_start()));
                j += 1;
            }
            render_table(&rows, &mut out);
            i = j;
            continue;
        }

        if let Some((alt, path)) = parse_image_ref(trimmed) {
            // The visible caption always survives. Chat/editor clients may add a
            // bounded kitty pixel bed after it when this resolves to a local PNG.
            let label = if alt.is_empty() {
                path.clone()
            } else {
                format!("{alt}  ·  {path}")
            };
            let image_line = out.len();
            out.push(Line::from(vec![
                Span::styled("  [img] ", Style::default().fg(theme::CYAN)),
                Span::styled(
                    label,
                    Style::default()
                        .fg(theme::CYAN)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            images.push(MarkdownImage {
                line: image_line,
                path,
            });
        } else if let Some(h) = heading(line) {
            out.push(h);
        } else if is_horizontal_rule(trimmed) {
            out.push(Line::from(Span::styled(
                g("─", "-").repeat(40),
                Style::default().fg(theme::EDGE),
            )));
        } else if trimmed.starts_with("> ") || trimmed == ">" {
            out.push(blockquote(line));
        } else if let Some(l) = list_item(line) {
            out.push(l);
        } else {
            out.push(Line::from(inline_spans(line, base_style())));
        }
        i += 1;
    }
    let mut links = Vec::new();
    for (line, rendered) in out.iter().enumerate() {
        for (span, styled) in rendered.spans.iter().enumerate() {
            if !styled.style.add_modifier.contains(Modifier::UNDERLINED) {
                continue;
            }
            let Some(note) = rendered.spans.get(span + 1) else {
                continue;
            };
            let Some(target) = note
                .content
                .strip_prefix(" (")
                .and_then(|s| s.strip_suffix(')'))
            else {
                continue;
            };
            links.push(MarkdownLink {
                line,
                span,
                target: target.to_string(),
            });
        }
    }
    RenderedBlock {
        lines: out,
        links,
        images,
    }
}

/// Parse an image reference `![alt](path)` occupying (most of) a line, into
/// `(alt, path)`. Shared by the markdown renderer (draws the card) and the chat
/// (collects paths for the `/image` viewer) so both agree on what an image is.
/// Only a leading `![...](...)` is recognized — inline images mid-sentence are
/// left as prose (rare from an agent, and a card mid-line would read oddly).
/// A `title` after the url (`](path "title")`) is tolerated and dropped.
pub(crate) fn parse_image_ref(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix("![")?;
    let alt_end = rest.find(']')?;
    let alt = rest[..alt_end].to_string();
    let after = rest[alt_end + 1..].strip_prefix('(')?;
    let url_end = after.find(')')?;
    let inner = &after[..url_end];
    // Drop an optional `"title"` after the url.
    let path = inner
        .split_once(char::is_whitespace)
        .map(|(p, _)| p)
        .unwrap_or(inner)
        .trim();
    if path.is_empty() {
        return None;
    }
    Some((alt, path.to_string()))
}

/// `---` / `***` / `___` on their own line (3+ of one marker, optionally
/// spaced) — a thematic break, rendered as a dim rule instead of raw dashes.
fn is_horizontal_rule(t: &str) -> bool {
    let t = t.trim_end();
    for marker in ['-', '*', '_'] {
        let stripped: String = t.chars().filter(|c| *c != ' ').collect();
        if stripped.len() >= 3 && stripped.chars().all(|c| c == marker) {
            return true;
        }
    }
    false
}

/// A table row starts and (after trailing-space trim) ends with `|` and has at
/// least one interior cell divider.
fn is_table_row(t: &str) -> bool {
    let t = t.trim_end();
    t.len() >= 2 && t.starts_with('|') && t.ends_with('|') && t.matches('|').count() >= 2
}

/// The header/body separator: a table row whose cells contain only `-`, `:`
/// and spaces (`|---|:--:|`).
fn is_table_separator(t: &str) -> bool {
    is_table_row(t)
        && t.trim_end()
            .trim_matches('|')
            .chars()
            .all(|c| matches!(c, '-' | ':' | '|' | ' '))
        && t.contains('-')
}

/// Split one `| a | b |` row into trimmed cell strings.
fn table_cells(t: &str) -> Vec<String> {
    let inner = t.trim_end().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Render parsed rows (first row = header) as padded columns joined by `│`,
/// with a `─┼─` rule under the header. Column width = the widest cell's
/// styled-stripped display width, capped so one long cell can't blow out the
/// whole table; cells run through the inline styler so `code` spans render.
fn render_table(rows: &[Vec<String>], out: &mut Vec<Line<'static>>) {
    const CELL_CAP: usize = 48;
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let mut w = vec![0usize; cols];
    for row in rows {
        for (ci, cell) in row.iter().enumerate() {
            w[ci] = w[ci].max(inline_plain_len(cell).min(CELL_CAP));
        }
    }
    for (ri, row) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        for (ci, cw) in w.iter().enumerate() {
            if ci > 0 {
                spans.push(Span::styled(
                    format!(" {} ", g("│", "|")),
                    Style::default().fg(theme::EDGE),
                ));
            }
            let cell = row.get(ci).map(String::as_str).unwrap_or("");
            let base = if ri == 0 {
                base_style().add_modifier(Modifier::BOLD)
            } else {
                base_style()
            };
            let len = inline_plain_len(cell);
            spans.extend(inline_spans(cell, base));
            if len < *cw {
                spans.push(Span::raw(" ".repeat(cw - len)));
            }
        }
        out.push(Line::from(spans));
        if ri == 0 {
            // Header underline: ─┼─ junctions matching the column widths.
            let mut rule = String::from("  ");
            for (ci, cw) in w.iter().enumerate() {
                if ci > 0 {
                    rule.push_str(g("─┼─", "-+-"));
                }
                rule.push_str(&g("─", "-").repeat(*cw));
            }
            out.push(Line::from(Span::styled(
                rule,
                Style::default().fg(theme::EDGE),
            )));
        }
    }
}

/// Display length of a cell after inline markers (`` ` ``, `**`, `*`) are
/// consumed by the styler — used for column-width math so padding lines up
/// with what actually renders.
fn inline_plain_len(text: &str) -> usize {
    inline_spans(text, base_style())
        .iter()
        .map(|s| s.content.chars().count())
        .sum()
}

fn base_style() -> Style {
    Style::default().fg(theme::FG)
}

fn fence_marker(line: &str) -> Line<'static> {
    Line::from(Span::styled(
        line.to_string(),
        Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
    ))
}

fn code_line(styled: StyledLine) -> Line<'static> {
    if styled.is_empty() {
        return Line::from(Span::styled(
            String::new(),
            Style::default().bg(theme::BG_DARK),
        ));
    }
    let spans: Vec<Span<'static>> = styled
        .into_iter()
        .map(|(c, t)| Span::styled(t, Style::default().fg(c).bg(theme::BG_DARK)))
        .collect();
    Line::from(spans)
}

/// ATX heading → blue bold, hash markers stripped. `sizes flat` (no
/// double-height H1). A `#` not followed by a space is not a heading (so
/// `#hashtag` / `#1` stay prose).
fn heading(line: &str) -> Option<Line<'static>> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = &t[hashes..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    let text = after.trim_start();
    Some(Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(theme::BLUE)
            .add_modifier(Modifier::BOLD),
    )))
}

fn blockquote(line: &str) -> Line<'static> {
    let t = line.trim_start();
    let content = t
        .strip_prefix("> ")
        .or_else(|| t.strip_prefix('>'))
        .unwrap_or(t);
    let mut spans = vec![Span::styled(
        format!("{} ", g("▎", "|")),
        Style::default().fg(theme::COMMENT),
    )];
    spans.extend(inline_spans(content, Style::default().fg(theme::COMMENT)));
    Line::from(spans)
}

/// Bullet (`- `/`* `/`+ `) or numbered (`1. `/`1) `) list item → cyan marker +
/// inline-styled body, indentation preserved.
fn list_item(line: &str) -> Option<Line<'static>> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let t = &line[indent_len..];

    if let Some(rest) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        // Task-list items: `- [x] done` / `- [ ] todo` get a checkbox glyph
        // (green when checked) instead of the raw bracket triplet.
        if let Some(body) = rest
            .strip_prefix("[x] ")
            .or_else(|| rest.strip_prefix("[X] "))
        {
            let mut spans = vec![
                Span::styled(indent.to_string(), base_style()),
                Span::styled(
                    format!("{} ", g("☑", "[x]")),
                    Style::default().fg(theme::GREEN),
                ),
            ];
            spans.extend(inline_spans(body, base_style()));
            return Some(Line::from(spans));
        }
        if let Some(body) = rest.strip_prefix("[ ] ") {
            let mut spans = vec![
                Span::styled(indent.to_string(), base_style()),
                Span::styled(
                    format!("{} ", g("☐", "[ ]")),
                    Style::default().fg(theme::COMMENT),
                ),
            ];
            spans.extend(inline_spans(body, base_style()));
            return Some(Line::from(spans));
        }
        let mut spans = vec![
            Span::styled(indent.to_string(), base_style()),
            Span::styled(
                format!("{} ", g("•", "-")),
                Style::default().fg(theme::CYAN),
            ),
        ];
        spans.extend(inline_spans(rest, base_style()));
        return Some(Line::from(spans));
    }

    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &t[digits.len()..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            let mut spans = vec![
                Span::styled(indent.to_string(), base_style()),
                Span::styled(format!("{digits}. "), Style::default().fg(theme::CYAN)),
            ];
            spans.extend(inline_spans(rest, base_style()));
            return Some(Line::from(spans));
        }
    }
    None
}

/// Split a text run into inline spans: `code` on the dark bed, **bold** and
/// *italic* via `Modifier`. Unbalanced markers render literally; `_` is never an
/// italic delimiter (protects `snake_case`).
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                push_buf(&mut spans, &mut buf, base);
                let code: String = chars[i + 1..close].iter().collect();
                spans.push(Span::styled(code, base.fg(theme::CYAN).bg(theme::BG_DARK)));
                i = close + 1;
                continue;
            }
        }

        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(close) = find_double_star(&chars, i + 2) {
                push_buf(&mut spans, &mut buf, base);
                let inner: String = chars[i + 2..close].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
                i = close + 2;
                continue;
            }
        }

        if c == '*' {
            if let Some(close) = find_char(&chars, i + 1, '*') {
                push_buf(&mut spans, &mut buf, base);
                let inner: String = chars[i + 1..close].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                i = close + 1;
                continue;
            }
        }

        // ~~strikethrough~~
        if c == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
            if let Some(close) = find_double(&chars, i + 2, '~') {
                push_buf(&mut spans, &mut buf, base);
                let inner: String = chars[i + 2..close].iter().collect();
                spans.push(Span::styled(
                    inner,
                    base.add_modifier(Modifier::CROSSED_OUT),
                ));
                i = close + 2;
                continue;
            }
        }

        // [text](url) — text in cyan-underline, url as a dim trailing note so
        // it stays copyable (terminals don't click ratatui spans).
        if c == '[' {
            if let Some((text_end, url_end)) = find_link(&chars, i) {
                push_buf(&mut spans, &mut buf, base);
                let label: String = chars[i + 1..text_end].iter().collect();
                let url: String = chars[text_end + 2..url_end].iter().collect();
                spans.push(Span::styled(
                    label,
                    base.fg(theme::CYAN).add_modifier(Modifier::UNDERLINED),
                ));
                if !url.is_empty() {
                    spans.push(Span::styled(format!(" ({url})"), base.fg(theme::COMMENT)));
                }
                i = url_end + 1;
                continue;
            }
        }

        buf.push(c);
        i += 1;
    }
    push_buf(&mut spans, &mut buf, base);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn push_buf(spans: &mut Vec<Span<'static>>, buf: &mut String, base: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), base));
    }
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == target)
}

/// Find a doubled `target` (`~~`, etc.) starting at `from`; returns the index
/// of the FIRST of the pair.
fn find_double(chars: &[char], from: usize, target: char) -> Option<usize> {
    let mut k = from;
    while k + 1 < chars.len() {
        if chars[k] == target && chars[k + 1] == target {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// Match `[text](url)` at `open` (the `[`). Returns (index of `]`, index of
/// the closing `)`), with `](` required to be adjacent. Single-line only.
fn find_link(chars: &[char], open: usize) -> Option<(usize, usize)> {
    let text_end = find_char(chars, open + 1, ']')?;
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_end = find_char(chars, text_end + 2, ')')?;
    Some((text_end, url_end))
}

fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    let mut k = from;
    while k + 1 < chars.len() {
        if chars[k] == '*' && chars[k + 1] == '*' {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// Map a fence language token to a syntect file extension. Unknown languages
/// pass through (syntect falls back to plain text).
fn lang_ext(lang: &str) -> &str {
    match lang {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "javascript" | "js" | "jsx" => "js",
        "typescript" | "ts" | "tsx" => "ts",
        "bash" | "sh" | "shell" | "zsh" => "sh",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" => "html",
        "css" => "css",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "go" => "go",
        "md" | "markdown" => "md",
        "" => "txt",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn links_expose_label_span_and_target_metadata() {
        let mut md = Markdown::new();
        let rendered = md.render("See [architecture](docs/ARCHITECTURE.md#runtime) now.");
        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].line, 0);
        assert_eq!(rendered.links[0].span, 1);
        assert_eq!(rendered.links[0].target, "docs/ARCHITECTURE.md#runtime");
        assert_eq!(rendered.lines[0].spans[1].content, "architecture");
    }

    #[test]
    fn frozen_blocks_preserve_link_metadata_offsets() {
        let mut md = Markdown::new();
        let rendered = md.render("[one](one.md)\n\nplain\n\n[two](two.md)");
        assert_eq!(rendered.links.len(), 2);
        assert_eq!(rendered.links[0].line, 0);
        assert_eq!(rendered.links[1].line, 4);
        assert_eq!(rendered.links[1].target, "two.md");
    }

    #[test]
    fn table_renders_aligned_columns_not_pipe_soup() {
        let mut md = Markdown::new();
        let src = "| Provider | Auth |\n|---|---|\n| Anthropic | `KEY` (API key) |\n| ClaudeCode | OAuth |";
        let lines = md.render(src);
        let texts: Vec<String> = lines.iter().map(plain).collect();
        // Header + rule + 2 body rows.
        assert_eq!(texts.len(), 4, "got: {texts:?}");
        assert!(texts[0].contains("Provider") && texts[0].contains("│"));
        assert!(texts[1].contains("┼"), "header rule: {:?}", texts[1]);
        // Raw markdown pipes/dashes must be gone.
        assert!(!texts.iter().any(|t| t.contains("|---")));
        // Columns align: the divider sits at the same char index in every
        // content row (header padded to the widest cell).
        let idx: Vec<usize> = [0, 2, 3]
            .iter()
            .map(|i| texts[*i].find('│').unwrap())
            .collect();
        assert!(idx.windows(2).all(|w| w[0] == w[1]), "dividers: {idx:?}");
        // Inline code inside a cell still styles (backticks consumed).
        assert!(texts[2].contains("KEY") && !texts[2].contains('`'));
    }

    #[test]
    fn links_strikethrough_and_task_lists_render() {
        let mut md = Markdown::new();
        let lines = md.render(
            "see [the docs](https://x.dev) for ~~old~~ new info\n\n- [x] shipped\n- [ ] next",
        );
        let texts: Vec<String> = lines.iter().map(plain).collect();
        let all = texts.join("\n");
        // Link: label + dim (url), raw []() consumed.
        assert!(all.contains("the docs") && all.contains("(https://x.dev)"));
        assert!(!all.contains("[the docs]"));
        // Strikethrough markers consumed.
        assert!(all.contains("old") && !all.contains("~~"));
        // Task list checkboxes replace the bracket triplets.
        assert!(all.contains("☑ shipped") && all.contains("☐ next"), "{all}");
    }

    #[test]
    fn image_ref_parses_and_renders_as_card() {
        assert_eq!(
            parse_image_ref("![a chart](/tmp/chart.png)"),
            Some(("a chart".into(), "/tmp/chart.png".into()))
        );
        // Empty alt, and a title after the url is dropped.
        assert_eq!(
            parse_image_ref("![](./shot.png \"caption\")"),
            Some((String::new(), "./shot.png".into()))
        );
        // Not an image ref.
        assert!(parse_image_ref("just a [link](url)").is_none());
        assert!(parse_image_ref("plain text").is_none());

        // Renders as a card, not raw markdown.
        let mut md = Markdown::new();
        let lines = md.render("![diagram](/tmp/d.png)");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("diagram") && text.contains("/tmp/d.png"));
        assert!(!text.contains("!["), "raw markdown consumed");
    }

    #[test]
    fn image_refs_expose_caption_line_and_path_metadata() {
        let mut md = Markdown::new();
        let rendered = md.render("intro\n\n![diagram](/tmp/d.png)\n\ntail");

        assert_eq!(rendered.images.len(), 1, "one image ref recorded");
        let image = &rendered.images[0];
        assert_eq!(image.path, "/tmp/d.png");
        // The recorded line is the caption row itself — a graphics-capable
        // client reserves its pixel bed directly beneath this index.
        let caption: String = rendered.lines[image.line]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            caption.contains("diagram") && caption.contains("/tmp/d.png"),
            "images[].line points at the caption, got {caption:?}"
        );
    }

    #[test]
    fn image_metadata_survives_prefix_freeze() {
        let mut md = Markdown::new();
        md.render("![a](/tmp/a.png)\n\nbody");
        // Growing the tail serves the frozen image block from cache; its
        // metadata must still be reported with correct absolute line offsets.
        let rendered = md.render("![a](/tmp/a.png)\n\nbody more");
        assert_eq!(rendered.images.len(), 1, "cached block keeps its image");
        let caption: String = rendered.lines[rendered.images[0].line]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(caption.contains("/tmp/a.png"), "got {caption:?}");
    }

    #[test]
    fn horizontal_rule_renders_as_dim_line() {
        let mut md = Markdown::new();
        let lines = md.render("above\n\n---\n\nbelow");
        let texts: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            texts.iter().any(|t| t.contains("────")),
            "expected a rule, got {texts:?}"
        );
        assert!(!texts.iter().any(|t| t.trim() == "---"));
    }

    #[test]
    fn prefix_freeze_reuses_frozen_block() {
        let mut md = Markdown::new();
        md.render("A\n\nB");
        assert_eq!(md.stats().misses, 1, "block A rendered once");
        assert_eq!(md.stats().hits, 0);

        md.render("A\n\nBC");
        assert_eq!(
            md.stats().misses,
            1,
            "growing the tail must not re-render block A"
        );
        assert_eq!(md.stats().hits, 1, "block A served from cache");
    }

    #[test]
    fn appending_a_new_block_freezes_the_previous_tail() {
        let mut md = Markdown::new();
        md.render("A\n\nB"); // A frozen (miss), B is tail
        md.render("A\n\nB\n\nC"); // now B freezes too
                                  // A hit again, B newly frozen (miss). Total misses: A + B = 2.
        assert_eq!(md.stats().misses, 2);
        assert!(md.stats().hits >= 1);
    }

    #[test]
    fn split_frozen_and_tail() {
        let (frozen, tail) = split_blocks("A\n\nB");
        assert_eq!(frozen, vec!["A".to_string()]);
        assert_eq!(tail, "B");
    }

    #[test]
    fn trailing_blank_freezes_last_block() {
        let (frozen, tail) = split_blocks("```\ncode\n```\n\n");
        assert_eq!(frozen.len(), 1, "closed fence + blank line freezes");
        assert!(tail.is_empty());
    }

    #[test]
    fn unclosed_fence_keeps_blank_line_in_tail() {
        // A blank line INSIDE an open fence is not a block boundary.
        let (frozen, tail) = split_blocks("intro\n\n```rust\nlet x = 1;\n\nlet y = 2;");
        assert_eq!(frozen, vec!["intro".to_string()]);
        assert!(tail.starts_with("```rust"));
        assert!(
            tail.contains("let y = 2;"),
            "blank line inside the fence stays in the tail block"
        );
    }

    #[test]
    fn unclosed_fence_renders_as_code() {
        let mut md = Markdown::new();
        let lines = md.render("```rust\nlet x = 1;");
        // opening marker + one highlighted code line, no closing marker.
        assert!(lines.len() >= 2);
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.bg == Some(theme::BG_DARK))));
    }

    #[test]
    fn heading_strips_hashes_and_is_blue_bold() {
        let mut md = Markdown::new();
        let lines = md.render("# Title");
        assert_eq!(lines.len(), 1);
        let txt: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "Title");
        assert!(lines[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn hashtag_is_not_a_heading() {
        assert!(heading("#nospace").is_none());
        assert!(heading("# real").is_some());
    }

    #[test]
    fn fenced_code_is_highlighted_on_dark_bed() {
        let mut md = Markdown::new();
        let lines = md.render("```rust\nlet x = 1;\n```\n\ntail");
        assert!(lines.len() >= 3);
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.bg == Some(theme::BG_DARK))));
    }

    #[test]
    fn bullet_list_gets_cyan_marker() {
        let mut md = Markdown::new();
        let lines = md.render("- one\n- two");
        assert_eq!(lines.len(), 2);
        assert!(lines[0]
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme::CYAN)));
    }

    #[test]
    fn numbered_list_is_recognised() {
        assert!(list_item("1. first").is_some());
        assert!(list_item("2) second").is_some());
        assert!(list_item("plain").is_none());
    }

    #[test]
    fn inline_code_and_bold() {
        let spans = inline_spans("use `foo` and **bar**", base_style());
        assert!(spans
            .iter()
            .any(|s| s.style.bg == Some(theme::BG_DARK) && s.content.as_ref() == "foo"));
        assert!(spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD) && s.content.as_ref() == "bar"));
    }

    #[test]
    fn underscores_stay_literal_not_italic() {
        let spans = inline_spans("call some_snake_case() now", base_style());
        let txt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "call some_snake_case() now");
        assert!(spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn italic_star_still_works() {
        let spans = inline_spans("an *emphasised* word", base_style());
        assert!(spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC)
                && s.content.as_ref() == "emphasised"));
    }

    #[test]
    fn clear_resets_cache_and_stats() {
        let mut md = Markdown::new();
        md.render("A\n\nB");
        assert_eq!(md.stats().misses, 1);
        md.clear();
        assert_eq!(md.stats(), CacheStats::default());
        md.render("A\n\nB");
        assert_eq!(md.stats().misses, 1, "block re-rendered after clear");
    }
}
