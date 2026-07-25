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

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ratatui::{
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};

use super::markdown::{MarkdownImage, INLINE_IMAGE_ROWS};

/// One image currently intended for a terminal-cell rectangle. `App` diffs
/// these after each Ratatui frame so static images are not retransmitted at the
/// render cadence, while scrolling/resizing clears stale graphics immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub path: PathBuf,
    pub rect: Rect,
}

/// An image's reserved logical-row range before viewport scroll is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalImage {
    pub path: PathBuf,
    pub line: usize,
    pub rows: u16,
}

/// Is this a kitty-graphics-capable terminal? Kitty, Ghostty, and WezTerm all
/// implement the protocol. Environment detection stays conservative so an
/// unknown terminal receives the compact text fallback rather than raw APCs.
pub fn supported() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("KITTY_PID").is_some()
        || std::env::var_os("WEZTERM_PANE").is_some()
        || std::env::var("TERM_PROGRAM")
            .is_ok_and(|program| matches!(program.to_ascii_lowercase().as_str(), "ghostty" | "wezterm"))
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

/// Whether a file path names an image format the TUI can safely open as a
/// read-only preview. Actual decoding is delegated to the platform converter.
pub fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "tif"
            | "tiff"
            | "bmp"
            | "heic"
            | "heif"
            | "svg"
            | "ico"
            | "avif"
    )
}

pub fn normalize_to_png(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    if is_png(&canonical) {
        return Some(canonical);
    }
    let metadata = std::fs::metadata(&canonical).ok()?;
    // Keep conversion bounded. The provider-side image attachment limit is 20
    // MiB; editor previews get a little more headroom for camera originals.
    if metadata.len() == 0 || metadata.len() > 64 * 1024 * 1024 {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hash);
    metadata.len().hash(&mut hash);
    metadata.modified().ok()?.hash(&mut hash);
    let cache = std::env::temp_dir().join("ocean-tui-image-cache");
    std::fs::create_dir_all(&cache).ok()?;
    let output = cache.join(format!("{:016x}.png", hash.finish()));
    if is_png(&output) {
        return Some(output);
    }

    #[cfg(target_os = "macos")]
    let converted = std::process::Command::new("sips")
        .args(["-s", "format", "png"])
        .arg(&canonical)
        .args(["--out"])
        .arg(&output)
        .output()
        .ok()
        .is_some_and(|result| result.status.success());

    #[cfg(not(target_os = "macos"))]
    let converted = std::process::Command::new("magick")
        .arg(&canonical)
        .arg(&output)
        .output()
        .ok()
        .is_some_and(|result| result.status.success());

    (converted && is_png(&output)).then_some(output)
}

fn decode_data_image(source: &str) -> Option<PathBuf> {
    let (meta, encoded) = source.split_once(',')?;
    if !meta.starts_with("data:image/") || !meta.ends_with(";base64") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hash);
    let cache = std::env::temp_dir().join("ocean-tui-image-cache");
    std::fs::create_dir_all(&cache).ok()?;
    let source_path = cache.join(format!("data-{:016x}.image", hash.finish()));
    if !source_path.exists() {
        std::fs::write(&source_path, bytes).ok()?;
    }
    normalize_to_png(&source_path)
}

/// Resolve a Markdown/gallery source without performing network I/O. Relative
/// sources follow Markdown semantics against `base`; `file://`, absolute paths,
/// and bounded base64 data images are accepted. Local JPEG/WebP/GIF/TIFF/HEIC/
/// SVG/etc. are normalized into a cached PNG because kitty's file transport is
/// PNG-native. URL/artifact sources keep their text fallback.
pub fn resolve_local_png(base: &Path, source: &str) -> Option<PathBuf> {
    if !supported() {
        return None;
    }
    let source = source
        .trim()
        .trim_matches(|character| matches!(character, '<' | '>'));
    if source.starts_with("data:image/") {
        return decode_data_image(source);
    }
    let local = source.strip_prefix("file://").unwrap_or(source);
    if local.is_empty() || (local.contains("://") && local == source) {
        return None;
    }
    let path = Path::new(local);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_to_png(&candidate)
}

/// Insert pixel beds after displayable Markdown image captions and return their
/// logical rows. Process refs in source order so earlier insertions shift later
/// line metadata deterministically.
pub fn reserve_markdown_images(
    lines: &mut Vec<Line<'static>>,
    links: &mut [super::markdown::MarkdownLink],
    images: &[MarkdownImage],
    base: &Path,
) -> Vec<LogicalImage> {
    let mut reserved = Vec::new();
    let mut inserted = 0usize;
    for image in images {
        let Some(path) = resolve_local_png(base, &image.path) else {
            continue;
        };
        let caption_line = image.line.saturating_add(inserted).min(lines.len());
        let image_line = caption_line.saturating_add(1).min(lines.len());
        lines.splice(
            image_line..image_line,
            (0..INLINE_IMAGE_ROWS).map(|_| Line::from("")),
        );
        for link in links.iter_mut().filter(|link| link.line >= image_line) {
            link.line = link.line.saturating_add(INLINE_IMAGE_ROWS as usize);
        }
        reserved.push(LogicalImage {
            path,
            line: image_line,
            rows: INLINE_IMAGE_ROWS,
        });
        inserted = inserted.saturating_add(INLINE_IMAGE_ROWS as usize);
    }
    reserved
}

/// Project fully-visible logical image beds into absolute terminal cells. A
/// partially clipped image stays as blank reserved rows instead of being
/// rescaled into the visible fragment (which would jump while scrolling).
pub fn project_visible(
    lines: &[Line<'static>],
    images: &[LogicalImage],
    viewport: Rect,
    scroll: u16,
    wrap_width: u16,
    image_x: u16,
    image_width: u16,
) -> Vec<Placement> {
    if viewport.height == 0 || wrap_width == 0 || image_width == 0 {
        return Vec::new();
    }
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut row = 0u16;
    starts.push(row);
    for line in lines {
        let rows = Paragraph::new(vec![line.clone()])
            .wrap(Wrap { trim: false })
            .line_count(wrap_width)
            .min(u16::MAX as usize) as u16;
        row = row.saturating_add(rows);
        starts.push(row);
    }
    let visible_end = scroll.saturating_add(viewport.height);
    images
        .iter()
        .filter_map(|image| {
            let start = *starts.get(image.line)?;
            let end_line = image.line.saturating_add(image.rows as usize);
            let end = *starts.get(end_line)?;
            (start >= scroll && end <= visible_end && end > start).then(|| Placement {
                path: image.path.clone(),
                rect: Rect::new(
                    image_x,
                    viewport.y.saturating_add(start - scroll),
                    image_width,
                    end - start,
                ),
            })
        })
        .collect()
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
    let payload =
        base64::engine::general_purpose::STANDARD.encode(abs.to_string_lossy().as_bytes());
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

    #[test]
    fn resolve_local_png_never_fetches_remote_sources() {
        let _guard = kitty_test_lock();
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let base = std::env::temp_dir();
        // Remote schemes are a text-fallback case: resolving them must not
        // touch the network or invent a local path.
        for remote in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "s3://bucket/a.png",
        ] {
            assert!(
                resolve_local_png(&base, remote).is_none(),
                "remote source must not resolve: {remote}"
            );
        }
        assert!(resolve_local_png(&base, "").is_none(), "empty source");
        assert!(
            resolve_local_png(&base, "does-not-exist.png").is_none(),
            "missing file"
        );
        std::env::remove_var("KITTY_WINDOW_ID");
    }

    #[test]
    fn resolve_local_png_accepts_absolute_relative_and_file_url() {
        let _guard = kitty_test_lock();
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let png = tmp_png();
        let base = png.parent().unwrap().to_path_buf();
        let name = png.file_name().unwrap().to_string_lossy().into_owned();
        let want = std::fs::canonicalize(&png).unwrap();

        assert_eq!(
            resolve_local_png(&base, &png.display().to_string()),
            Some(want.clone()),
            "absolute path"
        );
        assert_eq!(
            resolve_local_png(&base, &name),
            Some(want.clone()),
            "relative to base"
        );
        assert_eq!(
            resolve_local_png(&base, &format!("file://{}", png.display())),
            Some(want.clone()),
            "file:// url"
        );
        assert_eq!(
            resolve_local_png(&base, &format!("  <{}>  ", png.display())),
            Some(want),
            "angle-bracketed / padded source"
        );
        std::env::remove_var("KITTY_WINDOW_ID");
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn resolve_local_png_declines_when_terminal_lacks_graphics() {
        let _guard = kitty_test_lock();
        std::env::remove_var("KITTY_WINDOW_ID");
        std::env::remove_var("KITTY_PID");
        std::env::remove_var("WEZTERM_PANE");
        let saved = std::env::var("TERM_PROGRAM").ok();
        std::env::set_var("TERM_PROGRAM", "Apple_Terminal");
        let png = tmp_png();
        let base = png.parent().unwrap().to_path_buf();
        assert!(
            resolve_local_png(&base, &png.display().to_string()).is_none(),
            "no graphics protocol → text fallback, never a placement"
        );
        match saved {
            Some(value) => std::env::set_var("TERM_PROGRAM", value),
            None => std::env::remove_var("TERM_PROGRAM"),
        }
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn reserve_markdown_images_inserts_bed_and_shifts_link_rows() {
        let _guard = kitty_test_lock();
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let png = tmp_png();
        let base = png.parent().unwrap().to_path_buf();

        let mut lines: Vec<Line<'static>> =
            (0..8).map(|i| Line::from(format!("line {i}"))).collect();
        let mut links = vec![
            crate::shell::markdown::MarkdownLink {
                line: 1,
                span: 0,
                target: "before.md".into(),
            },
            crate::shell::markdown::MarkdownLink {
                line: 5,
                span: 0,
                target: "after.md".into(),
            },
        ];
        // Caption sits on line 2; the pixel bed belongs directly beneath it.
        let images = vec![MarkdownImage {
            line: 2,
            path: png.display().to_string(),
        }];

        let reserved = reserve_markdown_images(&mut lines, &mut links, &images, &base);

        assert_eq!(reserved.len(), 1, "one resolvable image");
        assert_eq!(reserved[0].line, 3, "bed starts under the caption");
        assert_eq!(reserved[0].rows, INLINE_IMAGE_ROWS);
        assert_eq!(
            lines.len(),
            8 + INLINE_IMAGE_ROWS as usize,
            "bed rows inserted"
        );
        assert_eq!(
            plain(&lines[2]),
            "line 2",
            "caption row itself is untouched"
        );
        assert_eq!(links[0].line, 1, "link above the image does not move");
        assert_eq!(
            links[1].line,
            5 + INLINE_IMAGE_ROWS as usize,
            "link below the image shifts by the bed height"
        );
        std::env::remove_var("KITTY_WINDOW_ID");
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn reserve_markdown_images_skips_unresolvable_sources() {
        let _guard = kitty_test_lock();
        std::env::set_var("KITTY_WINDOW_ID", "1");
        let base = std::env::temp_dir();
        let mut lines: Vec<Line<'static>> =
            (0..4).map(|i| Line::from(format!("line {i}"))).collect();
        let mut links: Vec<crate::shell::markdown::MarkdownLink> = Vec::new();
        let images = vec![
            MarkdownImage {
                line: 1,
                path: "https://example.com/remote.png".into(),
            },
            MarkdownImage {
                line: 2,
                path: "nope-missing.png".into(),
            },
        ];

        let reserved = reserve_markdown_images(&mut lines, &mut links, &images, &base);

        assert!(reserved.is_empty(), "nothing resolvable → no beds");
        assert_eq!(lines.len(), 4, "caption-only fallback keeps layout intact");
        std::env::remove_var("KITTY_WINDOW_ID");
    }

    #[test]
    fn project_visible_places_only_fully_visible_images() {
        let lines: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("row {i}"))).collect();
        let images = vec![LogicalImage {
            path: std::path::PathBuf::from("/tmp/a.png"),
            line: 5,
            rows: INLINE_IMAGE_ROWS,
        }];
        let viewport = Rect::new(0, 4, 40, 20);

        // Fully inside the window → placed, offset by the viewport origin.
        let placed = project_visible(&lines, &images, viewport, 0, 40, 1, 38);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].rect.y, viewport.y + 5, "row 5 under viewport top");
        assert_eq!(placed[0].rect.height, INLINE_IMAGE_ROWS);
        assert_eq!(placed[0].rect.x, 1);
        assert_eq!(placed[0].rect.width, 38);

        // Bottom half clipped by a short viewport → withheld rather than
        // squeezed into the fragment (which would jump while scrolling).
        let clipped = project_visible(&lines, &images, Rect::new(0, 4, 40, 10), 0, 40, 1, 38);
        assert!(clipped.is_empty(), "partially visible image is not placed");
    }

    #[test]
    fn project_visible_follows_scroll_offset() {
        let lines: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("row {i}"))).collect();
        let images = vec![LogicalImage {
            path: std::path::PathBuf::from("/tmp/a.png"),
            line: 10,
            rows: INLINE_IMAGE_ROWS,
        }];
        let viewport = Rect::new(0, 0, 40, 12);

        // Scrolled so the bed sits flush at the top of the window.
        let placed = project_visible(&lines, &images, viewport, 10, 40, 0, 40);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].rect.y, 0, "scroll subtracts from the logical row");

        // Scrolled past the image entirely → nothing to place.
        let gone = project_visible(&lines, &images, viewport, 20, 40, 0, 40);
        assert!(gone.is_empty(), "image scrolled above the window is dropped");
    }

    fn plain(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}
