//! Syntax highlighting via syntect, converted to owned ratatui-friendly spans.
//! Harvested from CTRL's `highlight.rs`.
use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// One highlighted line: a list of (foreground color, text) runs.
pub type StyledLine = Vec<(Color, String)>;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
        }
    }

    /// Highlight an entire file. `ext` is the file extension (no dot).
    pub fn highlight(&self, text: &str, ext: &str) -> Vec<StyledLine> {
        let theme = &self.themes.themes["base16-ocean.dark"];
        let syntax = self
            .syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| self.syntaxes.find_syntax_by_first_line(text))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out = Vec::new();
        for line in LinesWithEndings::from(text) {
            let ranges: Vec<(SynStyle, &str)> =
                hl.highlight_line(line, &self.syntaxes).unwrap_or_default();
            let styled: StyledLine = ranges
                .into_iter()
                .map(|(s, t)| {
                    let c = s.foreground;
                    (
                        Color::Rgb(c.r, c.g, c.b),
                        t.trim_end_matches('\n').to_string(),
                    )
                })
                .collect();
            out.push(styled);
        }
        if out.is_empty() {
            out.push(vec![]);
        }
        out
    }
}
