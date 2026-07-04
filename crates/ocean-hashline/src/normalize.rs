//! Minimal text-shape normalization: line-ending detection / round-trip and BOM
//! stripping. The patcher canonicalizes text to LF before applying edits and can
//! restore the original shape on write-back.
//!
//! Faithful port of oh-my-pi `packages/hashline/src/normalize.ts`.

use serde::{Deserialize, Serialize};

/// UTF-8 byte-order-mark, as a `&str`.
const BOM: &str = "\u{FEFF}";

/// A file's original line-ending style, recorded so a caller can restore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    /// Unix `\n`.
    Lf,
    /// Windows `\r\n`.
    Crlf,
}

/// Detect the first line-ending style in `content`. Defaults to LF when neither
/// is present.
pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    match (crlf_idx, lf_idx) {
        (_, None) => LineEnding::Lf,
        (None, Some(_)) => LineEnding::Lf,
        (Some(c), Some(l)) => {
            if c < l {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            }
        }
    }
}

/// Normalize a file for the WORKING text: strip a leading BOM and convert every
/// line ending to LF. Returns the normalized text plus the detected original
/// line ending so the caller can [`restore_line_endings`] later.
pub fn normalize_to_lf(content: &str) -> (String, LineEnding) {
    let stripped = content.strip_prefix(BOM).unwrap_or(content);
    let ending = detect_line_ending(stripped);
    // Convert `\r\n` and lone `\r` to `\n`, matching the JS regex `/\r\n?/g`.
    let mut out = String::with_capacity(stripped.len());
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            out.push('\n');
            // swallow an immediately-following \n (CRLF collapses to one LF).
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            // copy this byte's full char verbatim
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&stripped[i..i + ch_len]);
            i += ch_len;
        }
    }
    (out, ending)
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte >> 5 == 0b110 {
        2
    } else if first_byte >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Re-encode LF text with the requested line ending.
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::Crlf => text.replace('\n', "\r\n"),
    }
}

/// Whether `content` begins with a UTF-8 BOM.
pub fn has_bom(content: &str) -> bool {
    content.starts_with(BOM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_lf() {
        assert_eq!(detect_line_ending("a\nb\n"), LineEnding::Lf);
    }

    #[test]
    fn detects_crlf() {
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
    }

    #[test]
    fn detects_lf_when_lf_precedes_crlf() {
        assert_eq!(detect_line_ending("a\nb\r\n"), LineEnding::Lf);
    }

    #[test]
    fn normalize_strips_bom_and_converts_crlf() {
        let (text, ending) = normalize_to_lf("\u{FEFF}a\r\nb\r\n");
        assert_eq!(text, "a\nb\n");
        assert_eq!(ending, LineEnding::Crlf);
    }

    #[test]
    fn normalize_lone_cr() {
        let (text, _) = normalize_to_lf("a\rb\rc");
        assert_eq!(text, "a\nb\nc");
    }

    #[test]
    fn round_trip_crlf() {
        let original = "a\r\nb\r\nc";
        let (lf, ending) = normalize_to_lf(original);
        assert_eq!(lf, "a\nb\nc");
        let restored = restore_line_endings(&lf, ending);
        assert_eq!(restored, original);
    }

    #[test]
    fn round_trip_lf_is_identity() {
        let (lf, ending) = normalize_to_lf("a\nb\n");
        assert_eq!(restore_line_endings(&lf, ending), "a\nb\n");
    }

    #[test]
    fn utf8_preserved() {
        let (text, _) = normalize_to_lf("café\r\nnaïve\n");
        assert_eq!(text, "café\nnaïve\n");
    }
}
