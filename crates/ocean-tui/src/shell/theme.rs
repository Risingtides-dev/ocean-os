//! Tokyo Night palette + depth-fill system, shared across panes.
#![allow(dead_code)]
use ratatui::style::Color;

// ── base tiers ────────────────────────────────────────────────────────────────
pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26); // editor void
pub const BG_DARK: Color = Color::Rgb(0x16, 0x16, 0x1e); // gutter / void rail
pub const SLATE: Color = Color::Rgb(0x1f, 0x23, 0x35); // panel bed (invented)
pub const BG_HL: Color = Color::Rgb(0x29, 0x2e, 0x42); // selected / segment
pub const EDGE: Color = Color::Rgb(0x3b, 0x42, 0x61); // light-edge column
pub const SHADOW: Color = Color::Rgb(0x0f, 0x0f, 0x16); // faux drop-shadow column
pub const CURLINE: Color = Color::Rgb(0x1e, 0x20, 0x2e); // current line bg
pub const HOVER: Color = Color::Rgb(0x20, 0x23, 0x2f); // hover row bg

// ── accents ───────────────────────────────────────────────────────────────────
pub const FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
pub const COMMENT: Color = Color::Rgb(0x56, 0x5f, 0x89);
pub const BLUE: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
pub const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
pub const DEEPBLUE: Color = Color::Rgb(0x3d, 0x59, 0xa1); // deep blue (logo caret) — darkest of the 3 logo shades
pub const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
pub const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
pub const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
pub const MAGENTA: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
pub const ORANGE: Color = Color::Rgb(0xff, 0x9e, 0x64);

// ── program badge beds (Yazi graft, 2-char filled pill) ─────────────────────────
pub const BADGE_CLAUDE_BG: Color = Color::Rgb(0x2d, 0x24, 0x38);
pub const BADGE_CODEX_BG: Color = Color::Rgb(0x1e, 0x2b, 0x1e);
pub const BADGE_PI_BG: Color = Color::Rgb(0x1b, 0x24, 0x38);
pub const BADGE_OCEAN_BG: Color = Color::Rgb(0x16, 0x2f, 0x3d);

/// Nerd-font / fancy-glyph rendering. Flip to `false` to test the ASCII fallback.
pub const NERD: bool = true;

/// Pick a glyph or its ASCII fallback depending on [`NERD`].
pub const fn g(nerd: &'static str, ascii: &'static str) -> &'static str {
    if NERD {
        nerd
    } else {
        ascii
    }
}
