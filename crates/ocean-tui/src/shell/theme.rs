//! Ocean palette — aligned to the `ocean-surface` product tokens
//! (`ocean-surface/styles/tokens.css`): true-black grounds + the aqua "ocean
//! ramp" accent + shared semantics, so the terminal reads as the same product
//! as the web surface. (Was CTRL's Tokyo Night.) The editor's syntect code
//! theme is deliberately left on its own scheme — code colors aren't UI chrome.
#![allow(dead_code)]
use ratatui::style::Color;

// ── base tiers (ocean-surface bg ramp: #060606 → #23252B) ───────────────────────
pub const BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a); // editor void — bg-raised
pub const BG_DARK: Color = Color::Rgb(0x06, 0x06, 0x06); // deepest void / gutter — bg
pub const SLATE: Color = Color::Rgb(0x14, 0x14, 0x14); // panel bed — bg-elevated
pub const BG_HL: Color = Color::Rgb(0x23, 0x25, 0x2b); // selected / segment — bg-well
pub const EDGE: Color = Color::Rgb(0x2e, 0x32, 0x3c); // light-edge column — card line
pub const SHADOW: Color = Color::Rgb(0x00, 0x00, 0x00); // faux drop-shadow column
pub const CURLINE: Color = Color::Rgb(0x12, 0x13, 0x17); // current line bg (subtle raise)
pub const HOVER: Color = Color::Rgb(0x1b, 0x1c, 0x21); // hover row bg — bg-hover

// ── accents (ocean ramp + surface semantics) ────────────────────────────────────
pub const FG: Color = Color::Rgb(0xfa, 0xfc, 0xff); // fg — near-white
pub const COMMENT: Color = Color::Rgb(0x90, 0x90, 0x98); // fg-3 — muted label
pub const BLUE: Color = Color::Rgb(0x6a, 0xa6, 0xff); // info — dirs/headers (readable blue, distinct from aqua)
pub const CYAN: Color = Color::Rgb(0x00, 0xd7, 0xd7); // ocean-6 — PRIMARY aqua accent (bars, prompts, titles)
pub const DEEPBLUE: Color = Color::Rgb(0x00, 0x5f, 0xaf); // ocean-3 — deep logo shade
pub const GREEN: Color = Color::Rgb(0x1e, 0xd7, 0x60); // ok
pub const YELLOW: Color = Color::Rgb(0xff, 0xb2, 0x24); // warn
pub const RED: Color = Color::Rgb(0xff, 0x4d, 0x67); // err
pub const MAGENTA: Color = Color::Rgb(0xb7, 0x94, 0xf6); // soft violet — thinking (no surface token; harmonized)
pub const ORANGE: Color = Color::Rgb(0xff, 0x9e, 0x64); // orange — distinct from warn amber

// ── program badge beds (2-char filled pill) — dark tints on true black ──────────
pub const BADGE_CLAUDE_BG: Color = Color::Rgb(0x24, 0x18, 0x30);
pub const BADGE_CODEX_BG: Color = Color::Rgb(0x14, 0x21, 0x0f);
pub const BADGE_PI_BG: Color = Color::Rgb(0x0a, 0x1a, 0x2a);
pub const BADGE_OCEAN_BG: Color = Color::Rgb(0x04, 0x25, 0x2b); // deep aqua bed

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
