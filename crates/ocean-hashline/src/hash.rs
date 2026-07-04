//! File-hash anchor computation.
//!
//! A hashline section tag is a 4-hex fingerprint of the whole file's
//! *normalized* text. Any read of byte-identical content mints the same tag, so
//! a follow-up edit anchored at any line validates whenever the live file still
//! hashes to it.
//!
//! Faithful port of oh-my-pi `packages/hashline/src/format.ts`
//! `computeFileHash` / `normalizeFileHashText`.

use twox_hash::XxHash32;

/// Number of hex characters in a content-derived file-hash tag.
pub const FILE_HASH_LENGTH: usize = 4;

/// Normalize text before hashing: strip trailing `[ \t\r]+` that precedes each
/// `\n` or end-of-string. Equivalent to the JS regex `/[ \t\r]+(?=\n|$)/g` → "".
///
/// This trailing-whitespace strip applies ONLY to the hash input, never to the
/// stored/applied text — so CRLF endings and display-trimmed lines do not
/// invalidate a tag.
pub(crate) fn normalize_file_hash_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut run_start: Option<usize> = None;
    let mut i = 0;
    // Track byte indices; the trailing-whitespace bytes ` `, `\t`, `\r` are all
    // ASCII so byte-wise scanning is UTF-8 safe (we only ever elide ASCII runs
    // and copy everything else verbatim).
    let mut last_flushed = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let is_trim = b == b' ' || b == b'\t' || b == b'\r';
        if is_trim {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else {
            if let Some(start) = run_start.take() {
                // A run of trim bytes ended just before index `i`. Drop it only
                // when it precedes a newline (regex lookahead `(?=\n)`).
                if b == b'\n' {
                    // flush text up to run start, skip the run
                    out.push_str(&text[last_flushed..start]);
                    last_flushed = i;
                }
                // else: not a lookahead hit — the run is ordinary content, leave
                // it in place by NOT advancing last_flushed.
            }
        }
        i += 1;
    }
    // Trailing run at end-of-string (regex lookahead `(?=$)`).
    if let Some(start) = run_start.take() {
        out.push_str(&text[last_flushed..start]);
        last_flushed = bytes.len();
    }
    out.push_str(&text[last_flushed..]);
    out
}

/// Compute the content-derived hash tag carried by a hashline section header.
///
/// `xxHash32(normalized, seed=0) & 0xFFFF`, formatted as 4 uppercase hex chars.
pub fn compute_file_hash(text: &str) -> String {
    let normalized = normalize_file_hash_text(text);
    let full = XxHash32::oneshot(0, normalized.as_bytes());
    let low16 = full & 0xFFFF;
    format!("{:0width$X}", low16, width = FILE_HASH_LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_four_upper_hex() {
        let h = compute_file_hash("hello\nworld\n");
        assert_eq!(h.len(), 4);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn trailing_whitespace_does_not_change_hash() {
        // Trailing spaces/tabs/CR before newlines are stripped for the hash input.
        let a = compute_file_hash("foo\nbar\n");
        let b = compute_file_hash("foo   \nbar\t\n");
        let c = compute_file_hash("foo\r\nbar\r\n");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn interior_whitespace_matters() {
        let a = compute_file_hash("foo bar\n");
        let b = compute_file_hash("foobar\n");
        assert_ne!(a, b);
    }

    #[test]
    fn normalize_only_strips_before_newline_or_eof() {
        // Interior run not before a newline is preserved.
        assert_eq!(normalize_file_hash_text("a  b"), "a  b");
        // Run before newline stripped.
        assert_eq!(normalize_file_hash_text("a  \nb"), "a\nb");
        // Run at EOF stripped.
        assert_eq!(normalize_file_hash_text("a  "), "a");
        // Mixed.
        assert_eq!(normalize_file_hash_text("a \t\r\nb  "), "a\nb");
    }

    #[test]
    fn empty_and_stable() {
        let a = compute_file_hash("");
        let b = compute_file_hash("");
        assert_eq!(a, b);
        assert_eq!(a.len(), 4);
    }
}
