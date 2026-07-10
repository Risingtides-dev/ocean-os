//! Minimal kitty graphics protocol — just enough to display a PNG inline in a
//! terminal cell box. Hand-rolled (no `ratatui-image`) because that crate's
//! current release needs a newer rustc than our MSRV and drags in the whole
//! `image` decode tree; kitty transmits a PNG from a file path natively, so we
//! need zero decoding for the common screenshot case.
//!
//! Scope: PNG only, via file-path transmission (`t=f,f=100`). kitty scales the
//! image to fit a `cols`×`rows` cell box preserving aspect ratio. Other formats
//! return `None` (the caller shows a text note). GIF animation is out of scope.
//!
//! The escapes are written to the terminal OUT OF BAND (directly to stdout,
//! after ratatui's frame paints) because kitty images float in a layer above
//! ratatui's cell buffer — see `app`'s post-draw emission.

use std::path::Path;

use base64::Engine as _;

/// Is this a kitty-graphics-capable terminal? Conservative: kitty proper,
/// detected by its env markers (both are set inside a kitty window). Ghostty /
/// WezTerm also speak the protocol but aren't claimed here — a false negative
/// just falls back to the text card, never a broken render.
pub fn supported() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some() || std::env::var_os("KITTY_PID").is_some()
}

/// Whether `path` is a PNG, by magic bytes (robust to extension/casing). Only
/// PNGs render via native file transmission; everything else shows a note.
pub fn is_png(path: &Path) -> bool {
    let Ok(bytes) = std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf).map(|_| buf)
    }) else {
        return false;
    };
    bytes == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

/// Escape string that positions the cursor at cell `(col, row)` (0-based) and
/// displays the PNG at `path`, scaled into a `cols`×`rows` cell box. Returns
/// `None` when the terminal isn't kitty, the file isn't a readable PNG, or the
/// box is empty — the caller then shows a text placeholder instead.
pub fn place_png_at(path: &Path, col: u16, row: u16, cols: u16, rows: u16) -> Option<String> {
    if !supported() || cols == 0 || rows == 0 || !is_png(path) {
        return None;
    }
    let abs = std::fs::canonicalize(path).ok()?;
    let payload = base64::engine::general_purpose::STANDARD.encode(abs.to_string_lossy().as_bytes());
    // Move cursor (1-based) then transmit+display: file medium (t=f), PNG
    // (f=100), sized to the cell box (c=cols, r=rows), quiet (q=2 → no ack).
    Some(format!(
        "\x1b[{};{}H\x1b_Ga=T,t=f,f=100,c={cols},r={rows},q=2;{payload}\x1b\\",
        row + 1,
        col + 1,
    ))
}

/// Escape that deletes ALL displayed images. Emitted when the viewer closes so
/// the pixels don't linger over the restored UI.
pub const CLEAR_ALL: &str = "\x1b_Ga=d,q=2\x1b\\";

/// Write a graphics escape to the terminal and flush. Bypasses ratatui (its
/// cell buffer doesn't model the image layer), so this must run AFTER the
/// frame paints.
pub fn emit(seq: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Serializes these tests: they share the `KITTY_WINDOW_ID` process env
    /// marker (set/removed per test) and the pid-keyed temp fixture path, so
    /// the default multi-threaded runner intermittently saw a sibling test
    /// delete the file / unset the marker mid-assertion.
    fn kitty_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn tmp_png() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ocean-kitty-test-{}.png", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        // PNG signature + a byte so read_exact(8) succeeds.
        f.write_all(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00])
            .unwrap();
        p
    }

    #[test]
    fn is_png_matches_signature_only() {
        let _guard = kitty_test_lock();
        let png = tmp_png();
        assert!(is_png(&png));
        let dir = std::env::temp_dir();
        let notpng = dir.join(format!("ocean-kitty-test-{}.txt", std::process::id()));
        std::fs::write(&notpng, b"hello, not a png at all").unwrap();
        assert!(!is_png(&notpng));
        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(notpng);
    }

    #[test]
    fn place_png_builds_a_graphics_escape_when_kitty() {
        let _guard = kitty_test_lock();
        // Force the kitty env marker for this test.
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let png = tmp_png();
        let seq = place_png_at(&png, 4, 2, 40, 20).expect("kitty + png → escape");
        assert!(seq.contains("\x1b_Ga=T"), "has the graphics APC");
        assert!(seq.contains("f=100"), "PNG format");
        assert!(seq.contains("c=40,r=20"), "cell box sizing");
        assert!(seq.starts_with("\x1b[3;5H"), "cursor positioned (1-based)");
        assert!(seq.ends_with("\x1b\\"), "ST terminated");
        std::env::remove_var("KITTY_WINDOW_ID");
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn place_png_none_for_non_png_or_empty_box() {
        let _guard = kitty_test_lock();
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let png = tmp_png();
        assert!(place_png_at(&png, 0, 0, 0, 20).is_none(), "empty box");
        let dir = std::env::temp_dir();
        let txt = dir.join(format!("ocean-kitty-test2-{}.txt", std::process::id()));
        std::fs::write(&txt, b"nope").unwrap();
        assert!(place_png_at(&txt, 0, 0, 40, 20).is_none(), "non-png");
        std::env::remove_var("KITTY_WINDOW_ID");
        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(txt);
    }
}
