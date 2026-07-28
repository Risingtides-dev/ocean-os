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
    // Kitty APC passthrough depends on multiplexer configuration that Ocean
    // cannot prove. Fail closed instead of emitting graphics into tmux/screen.
    if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
        return false;
    }
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    (term.contains("kitty")
        && (std::env::var_os("KITTY_WINDOW_ID").is_some()
            || std::env::var_os("KITTY_PID").is_some()))
        || (program == "wezterm" && std::env::var_os("WEZTERM_PANE").is_some())
        || program == "ghostty"
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

/// Whether a file path names an image format the TUI should open as a read-only
/// image tab. Only a bounded PNG receives pixels; other formats show fallback
/// metadata and are never decoded or converted by Ocean.
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

const MAX_PNG_BYTES: u64 = 20 * 1024 * 1024;
const MAX_PNG_DIMENSION: u32 = 8192;
const MAX_PNG_PIXELS: u64 = 8 * 1024 * 1024;
const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;
const MAX_IMAGES_PER_RENDER: usize = 8;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
static CACHE_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
static FILE_RESULTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<FileKey, Option<PathBuf>>>,
> = std::sync::OnceLock::new();
static RESOLVE_RESULTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<ResolveKey, Option<PathBuf>>>,
> = std::sync::OnceLock::new();
static FILE_WORK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SNAPSHOT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileKey {
    path: PathBuf,
    len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolveKey {
    base: PathBuf,
    root: PathBuf,
    source: String,
}

/// Validate a complete PNG container before asking the terminal to decode it.
/// This rejects malformed chunk order, missing image data, unknown critical
/// chunks, CRC errors, illegal IHDR combinations, and decompression bombs.
fn valid_png(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return false;
    }
    let mut offset = 8usize;
    let mut color_type = 0u8;
    let mut bit_depth = 0u8;
    let mut seen_ihdr = false;
    let mut seen_plte = false;
    let mut seen_idat = false;
    let mut idat_ended = false;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four-byte chunk length"),
        ) as usize;
        let kind_start = offset + 4;
        let data_start = offset + 8;
        let Some(data_end) = data_start.checked_add(length) else {
            return false;
        };
        let Some(chunk_end) = data_end.checked_add(4) else {
            return false;
        };
        if chunk_end > bytes.len() {
            return false;
        }
        let kind = &bytes[kind_start..data_start];
        let expected_crc = u32::from_be_bytes(
            bytes[data_end..chunk_end]
                .try_into()
                .expect("four-byte chunk CRC"),
        );
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(&bytes[data_start..data_end]);
        if crc.finalize() != expected_crc {
            return false;
        }

        if !seen_ihdr {
            if kind != b"IHDR" || length != 13 {
                return false;
            }
            let data = &bytes[data_start..data_end];
            let width = u32::from_be_bytes(data[0..4].try_into().expect("IHDR width"));
            let height = u32::from_be_bytes(data[4..8].try_into().expect("IHDR height"));
            bit_depth = data[8];
            color_type = data[9];
            let depth_valid = match color_type {
                0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
                2 | 4 | 6 => matches!(bit_depth, 8 | 16),
                3 => matches!(bit_depth, 1 | 2 | 4 | 8),
                _ => false,
            };
            if width == 0
                || height == 0
                || width > MAX_PNG_DIMENSION
                || height > MAX_PNG_DIMENSION
                || u64::from(width) * u64::from(height) > MAX_PNG_PIXELS
                || !depth_valid
                || data[10] != 0
                || data[11] != 0
                || data[12] > 1
            {
                return false;
            }
            seen_ihdr = true;
        } else {
            match kind {
                b"IHDR" => return false,
                b"PLTE" => {
                    if seen_plte
                        || seen_idat
                        || matches!(color_type, 0 | 4)
                        || length == 0
                        || length > 768
                        || !length.is_multiple_of(3)
                        || (color_type == 3 && length / 3 > (1usize << bit_depth))
                    {
                        return false;
                    }
                    seen_plte = true;
                }
                b"IDAT" => {
                    if idat_ended || (color_type == 3 && !seen_plte) {
                        return false;
                    }
                    seen_idat = true;
                }
                b"IEND" => {
                    if length != 0 || !seen_idat || offset + 12 != bytes.len() {
                        return false;
                    }
                    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
                    let Ok(mut reader) = decoder.read_info() else {
                        return false;
                    };
                    let output_size = reader.output_buffer_size();
                    if output_size == 0 || output_size > MAX_DECODE_BYTES {
                        return false;
                    }
                    let mut decoded = vec![0; output_size];
                    return reader.next_frame(&mut decoded).is_ok();
                }
                _ => {
                    // Uppercase first byte marks a critical chunk. Only the
                    // four standard critical chunk types are admitted above.
                    if kind.first().is_some_and(u8::is_ascii_uppercase) {
                        return false;
                    }
                    if seen_idat {
                        idat_ended = true;
                    }
                }
            }
        }
        offset = chunk_end;
    }
    false
}

fn cache_dir() -> Option<&'static Path> {
    CACHE_DIR
        .get_or_init(|| {
            let base = std::env::temp_dir();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos();
            for attempt in 0..16u8 {
                let path = base.join(format!(
                    "ocean-tui-image-cache-{}-{nonce:x}-{attempt}",
                    std::process::id()
                ));
                #[cfg(unix)]
                let created = {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700).create(&path)
                };
                #[cfg(not(unix))]
                let created = std::fs::create_dir(&path);
                match created {
                    Ok(()) => return Some(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => return None,
                }
            }
            None
        })
        .as_deref()
}

fn cache_has_capacity(cache: &Path, incoming: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return false;
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        count = count.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    count < MAX_CACHE_ENTRIES && bytes.saturating_add(incoming) <= MAX_CACHE_BYTES
}

fn reserve_file_work(bytes: u64) -> bool {
    FILE_WORK_BYTES
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |used| (used.saturating_add(bytes) <= MAX_CACHE_BYTES).then_some(used + bytes),
        )
        .is_ok()
}

#[cfg(unix)]
fn open_confined_regular(
    path: &Path,
    allowed_root: &Path,
) -> Option<(std::fs::File, PathBuf, std::fs::Metadata)> {
    let root = std::fs::canonicalize(allowed_root).ok()?;
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let source = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&canonical)
        .ok()?;
    let metadata = source.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }

    // Rebind the opened descriptor to a still-confined name and, on Unix,
    // prove that name identifies the exact inode held by the descriptor.
    let rebound = std::fs::canonicalize(&canonical).ok()?;
    if !rebound.starts_with(&root) {
        return None;
    }
    let rebound_metadata = std::fs::metadata(&rebound).ok()?;
    use std::os::unix::fs::MetadataExt;
    if metadata.dev() != rebound_metadata.dev() || metadata.ino() != rebound_metadata.ino() {
        return None;
    }
    Some((source, rebound, metadata))
}

#[cfg(not(unix))]
fn open_confined_regular(
    _path: &Path,
    _allowed_root: &Path,
) -> Option<(std::fs::File, PathBuf, std::fs::Metadata)> {
    // Exact descriptor identity/reparse confinement is not implemented here;
    // fail closed rather than weakening the workspace boundary.
    None
}

/// Snapshot a descriptor-confined regular PNG into the private cache before
/// giving a pathname to the terminal. Reads are exact-length and snapshots are
/// made read-only before publication.
pub fn normalize_to_png(path: &Path, allowed_root: &Path) -> Option<PathBuf> {
    let (mut source, canonical, metadata) = open_confined_regular(path, allowed_root)?;
    let key = FileKey {
        path: canonical,
        len: metadata.len(),
    };
    let results =
        FILE_RESULTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let results = results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = results.get(&key) {
            return cached.clone();
        }
        if results.len() >= MAX_CACHE_ENTRIES
            || key.len == 0
            || key.len > MAX_PNG_BYTES
            || !reserve_file_work(key.len)
        {
            return None;
        }
    }

    let resolved = (|| {
        let cache = cache_dir()?;
        if !cache_has_capacity(cache, key.len) {
            return None;
        }
        let length = usize::try_from(key.len).ok()?;
        let mut bytes = vec![0; length];
        use std::io::Read;
        source.read_exact(&mut bytes).ok()?;
        let mut extra = [0u8; 1];
        if source.read(&mut extra).ok()? != 0 || !valid_png(&bytes) {
            return None;
        }
        let id = SNAPSHOT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output_path = cache.join(format!("snapshot-{id}.png"));
        #[cfg(unix)]
        let mut output = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&output_path)
                .ok()?
        };
        #[cfg(not(unix))]
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .ok()?;
        use std::io::Write;
        if output.write_all(&bytes).is_err()
            || output.flush().is_err()
            || output.sync_all().is_err()
        {
            let _ = std::fs::remove_file(&output_path);
            return None;
        }
        drop(output);
        let mut permissions = std::fs::metadata(&output_path).ok()?.permissions();
        permissions.set_readonly(true);
        if std::fs::set_permissions(&output_path, permissions).is_err() {
            let _ = std::fs::remove_file(&output_path);
            return None;
        }
        Some(output_path)
    })();

    results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, resolved.clone());
    resolved
}

/// Remove this process's bounded immutable image snapshots on ordinary exit or panic.
pub fn cleanup_cache() {
    if let Some(results) = FILE_RESULTS.get() {
        results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
    if let Some(results) = RESOLVE_RESULTS.get() {
        results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
    FILE_WORK_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Some(Some(cache)) = CACHE_DIR.get() {
        // Read-only snapshots must be made writable before directory removal on
        // platforms that enforce the readonly bit for unlink.
        if let Ok(entries) = std::fs::read_dir(cache) {
            for entry in entries.flatten() {
                if let Ok(_metadata) = entry.metadata() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            entry.path(),
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    #[cfg(not(unix))]
                    {
                        let mut permissions = _metadata.permissions();
                        permissions.set_readonly(false);
                        let _ = std::fs::set_permissions(entry.path(), permissions);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(cache);
    }
}

/// Resolve a Markdown/gallery source without performing network I/O. Relative,
/// `file://`, and absolute paths resolve against `base` and must canonicalize
/// inside `allowed_root`; only structurally validated, bounded local PNGs cross
/// the terminal graphics boundary. Data, remote, escaping, and other-format
/// sources keep their text fallback.
pub fn resolve_local_png(base: &Path, allowed_root: &Path, source: &str) -> Option<PathBuf> {
    if !supported() {
        return None;
    }
    let source = source
        .trim()
        .trim_matches(|character| matches!(character, '<' | '>'));
    if source.starts_with("data:") {
        return None;
    }
    let local = source.strip_prefix("file://").unwrap_or(source);
    if local.is_empty() || (local.contains("://") && local == source) {
        return None;
    }
    let key = ResolveKey {
        base: base.to_path_buf(),
        root: allowed_root.to_path_buf(),
        source: source.to_string(),
    };
    let results =
        RESOLVE_RESULTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let results = results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = results.get(&key) {
            return cached.clone();
        }
        if results.len() >= MAX_CACHE_ENTRIES {
            return None;
        }
    }
    let resolved = (|| {
        let base = std::fs::canonicalize(base).ok()?;
        let allowed_root = std::fs::canonicalize(allowed_root).ok()?;
        let path = Path::new(local);
        let candidate = std::fs::canonicalize(if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        })
        .ok()?;
        if !candidate.starts_with(&allowed_root) {
            return None;
        }
        normalize_to_png(&candidate, &allowed_root)
    })();
    results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, resolved.clone());
    resolved
}

/// Insert pixel beds after displayable Markdown image captions and return their
/// logical rows. Process refs in source order so earlier insertions shift later
/// line metadata deterministically.
pub fn reserve_markdown_images(
    lines: &mut Vec<Line<'static>>,
    links: &mut [super::markdown::MarkdownLink],
    images: &[MarkdownImage],
    base: &Path,
    allowed_root: &Path,
) -> Vec<LogicalImage> {
    let mut reserved = Vec::new();
    let mut inserted = 0usize;
    for image in images.iter().take(MAX_IMAGES_PER_RENDER) {
        let Some(path) = resolve_local_png(base, allowed_root, &image.path) else {
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
    if !supported() || cols == 0 || rows == 0 {
        return None;
    }
    let abs = std::fs::canonicalize(path).ok()?;
    let cache = std::fs::canonicalize(CACHE_DIR.get()?.as_ref()?).ok()?;
    let metadata = std::fs::metadata(&abs).ok()?;
    if !abs.starts_with(&cache)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PNG_BYTES
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o222 != 0 {
            return None;
        }
    }
    let mut snapshot = std::fs::File::open(&abs).ok()?;
    let mut bytes = vec![0; usize::try_from(metadata.len()).ok()?];
    use std::io::Read;
    snapshot.read_exact(&mut bytes).ok()?;
    let mut extra = [0u8; 1];
    if snapshot.read(&mut extra).ok()? != 0 || !valid_png(&bytes) {
        return None;
    }
    let payload =
        base64::engine::general_purpose::STANDARD.encode(abs.to_string_lossy().as_bytes());
    // Preserve ratatui's composer caret around the out-of-band cursor move,
    // then transmit+display: file medium (t=f), PNG (f=100), sized to the cell
    // box (c=cols, r=rows), quiet (q=2 → no ack). DEC save/restore is supported
    // by every terminal admitted by `supported()` and prevents the graphics
    // placement from leaving the real cursor at the image origin.
    Some(format!(
        "\x1b7\x1b[{};{}H\x1b_Ga=T,t=f,f=100,c={cols},r={rows},q=2;{payload}\x1b\\\x1b8",
        row + 1,
        col + 1,
    ))
}

/// Escape that deletes ALL displayed images. Emitted when the viewer closes so
/// the pixels don't linger over the restored UI.
pub const CLEAR_ALL: &str = "\x1b_Ga=d,q=2\x1b\\";

/// Clear displayed graphics only when this terminal is safely admitted.
pub fn clear_all() {
    if supported() {
        emit(CLEAR_ALL);
    }
}

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

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn set_env(changes: &[(&'static str, Option<&str>)]) -> EnvGuard {
        let mut saved = Vec::new();
        for (key, value) in changes {
            saved.push((*key, std::env::var_os(key)));
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        EnvGuard(saved)
    }

    fn kitty_env() -> EnvGuard {
        set_env(&[
            ("KITTY_WINDOW_ID", Some("1")),
            ("TERM", Some("xterm-kitty")),
            ("TERM_PROGRAM", None),
            ("WEZTERM_PANE", None),
            ("TMUX", None),
            ("STY", None),
        ])
    }

    fn tmp_png() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ocean-kitty-test-{}.png", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        // Complete, CRC-valid 1x1 PNG.
        let bytes = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        f.write_all(&bytes).unwrap();
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
    fn valid_png_rejects_truncation_bad_crc_and_oversized_geometry() {
        let _guard = kitty_test_lock();
        let dir = std::env::temp_dir();
        let short = dir.join(format!("ocean-kitty-short-{}.png", std::process::id()));
        std::fs::write(&short, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
        assert!(!valid_png(&std::fs::read(&short).unwrap()));

        let valid_path = tmp_png();
        let huge = dir.join(format!("ocean-kitty-huge-{}.png", std::process::id()));
        let mut huge_bytes = std::fs::read(&valid_path).unwrap();
        huge_bytes[16..20].copy_from_slice(&9000u32.to_be_bytes());
        let mut ihdr_crc = crc32fast::Hasher::new();
        ihdr_crc.update(&huge_bytes[12..29]);
        huge_bytes[29..33].copy_from_slice(&ihdr_crc.finalize().to_be_bytes());
        std::fs::write(&huge, &huge_bytes).unwrap();
        assert!(!valid_png(&huge_bytes), "CRC-valid oversized IHDR rejected");

        let valid_bytes = std::fs::read(&valid_path).unwrap();
        let mut bad_crc = valid_bytes.clone();
        let last = bad_crc.len() - 1;
        bad_crc[last] ^= 1;
        assert!(!valid_png(&bad_crc));

        let idat = valid_bytes
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("fixture IDAT");
        let chunk_start = idat - 4;
        let chunk_len = u32::from_be_bytes(
            valid_bytes[chunk_start..idat]
                .try_into()
                .expect("IDAT length"),
        ) as usize;
        let mut no_idat = valid_bytes.clone();
        no_idat.drain(chunk_start..idat + 4 + chunk_len + 4);
        assert!(!valid_png(&no_idat), "IHDR→IEND without IDAT rejected");

        let mut illegal_ihdr = valid_bytes.clone();
        illegal_ihdr[24] = 1; // bit depth 1 is illegal for RGBA (color type 6)
        illegal_ihdr[25] = 6;
        let mut ihdr_crc = crc32fast::Hasher::new();
        ihdr_crc.update(&illegal_ihdr[12..29]);
        illegal_ihdr[29..33].copy_from_slice(&ihdr_crc.finalize().to_be_bytes());
        assert!(!valid_png(&illegal_ihdr));
        assert!(valid_png(&valid_bytes));

        let _ = std::fs::remove_file(short);
        let _ = std::fs::remove_file(huge);
        let _ = std::fs::remove_file(valid_path);
    }

    #[test]
    fn multiplexer_environment_fails_graphics_detection_closed() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        std::env::set_var("TMUX", "1");
        assert!(!supported());
    }

    #[test]
    fn place_png_builds_a_graphics_escape_when_kitty() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let source = tmp_png();
        let png = normalize_to_png(&source, source.parent().unwrap()).expect("private snapshot");
        let seq = place_png_at(&png, 4, 2, 40, 20).expect("kitty + png → escape");
        assert!(seq.contains("\x1b_Ga=T"), "has the graphics APC");
        assert!(seq.contains("f=100"), "PNG format");
        assert!(seq.contains("c=40,r=20"), "cell box sizing");
        assert!(
            seq.starts_with("\x1b7\x1b[3;5H"),
            "ratatui caret saved before 1-based cursor positioning"
        );
        assert!(
            seq.ends_with("\x1b\\\x1b8"),
            "graphics APC is terminated before the ratatui caret is restored"
        );
        let _ = std::fs::remove_file(source);
    }

    #[test]
    fn place_png_none_for_non_png_or_empty_box() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let source = tmp_png();
        let png = normalize_to_png(&source, source.parent().unwrap()).expect("private snapshot");
        assert!(place_png_at(&png, 0, 0, 0, 20).is_none(), "empty box");
        let dir = std::env::temp_dir();
        let txt = dir.join(format!("ocean-kitty-test2-{}.txt", std::process::id()));
        std::fs::write(&txt, b"nope").unwrap();
        assert!(place_png_at(&txt, 0, 0, 40, 20).is_none(), "non-png");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(txt);
    }

    #[test]
    fn resolve_local_png_never_fetches_remote_sources() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let base = std::env::temp_dir();
        // Remote schemes are a text-fallback case: resolving them must not
        // touch the network or invent a local path.
        for remote in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "s3://bucket/a.png",
        ] {
            assert!(
                resolve_local_png(&base, &base, remote).is_none(),
                "remote source must not resolve: {remote}"
            );
        }
        assert!(
            resolve_local_png(&base, &base, "").is_none(),
            "empty source"
        );
        assert!(
            resolve_local_png(&base, &base, "does-not-exist.png").is_none(),
            "missing file"
        );
    }

    #[test]
    fn resolve_local_png_snapshots_absolute_relative_and_file_url() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let png = tmp_png();
        let base = png.parent().unwrap().to_path_buf();
        let name = png.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::canonicalize(&png).unwrap();
        let snapshot = resolve_local_png(&base, &base, &png.display().to_string())
            .expect("absolute path snapshot");
        assert_ne!(
            snapshot, source,
            "terminal receives a private immutable copy"
        );
        assert!(valid_png(&std::fs::read(&snapshot).unwrap()));
        assert!(
            std::fs::metadata(&snapshot)
                .unwrap()
                .permissions()
                .readonly(),
            "published snapshot is read-only"
        );
        assert_eq!(
            resolve_local_png(&base, &base, &name),
            Some(snapshot.clone()),
            "relative to base reuses snapshot"
        );
        assert_eq!(
            resolve_local_png(&base, &base, &format!("file://{}", png.display())),
            Some(snapshot.clone()),
            "file:// reuses snapshot"
        );
        assert_eq!(
            resolve_local_png(&base, &base, &format!("  <{}>  ", png.display())),
            Some(snapshot),
            "angle-bracketed / padded source reuses snapshot"
        );
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn resolve_local_png_uses_document_base_but_confines_to_workspace_root() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let root = std::env::temp_dir().join(format!("ocean-kitty-root-{}", std::process::id()));
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let inside = root.join("inside.png");
        let outside = tmp_png();
        std::fs::copy(&outside, &inside).unwrap();

        let inside_snapshot = resolve_local_png(&docs, &root, "../inside.png")
            .expect("Markdown parent traversal inside workspace");
        assert!(valid_png(&std::fs::read(inside_snapshot).unwrap()));
        assert!(
            resolve_local_png(&docs, &root, &outside.display().to_string()).is_none(),
            "absolute path outside the workspace is rejected"
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }

    #[cfg(unix)]
    #[test]
    fn normalize_rejects_fifo_without_waiting_for_a_writer() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let root = std::env::temp_dir().join(format!("ocean-kitty-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fifo = root.join("blocked.png");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo available on Unix");
        assert!(status.success());
        assert!(normalize_to_png(&fifo, &root).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn data_images_are_a_text_fallback_without_decode_or_cache_work() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
        let before = FILE_WORK_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(resolve_local_png(
            &std::env::temp_dir(),
            &std::env::temp_dir(),
            "data:image/png;base64,iVBORw0KGgo="
        )
        .is_none());
        assert_eq!(
            FILE_WORK_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn resolve_local_png_declines_when_terminal_lacks_graphics() {
        let _guard = kitty_test_lock();
        let _env = set_env(&[
            ("KITTY_WINDOW_ID", None),
            ("KITTY_PID", None),
            ("WEZTERM_PANE", None),
            ("TERM", Some("xterm-256color")),
            ("TERM_PROGRAM", Some("Apple_Terminal")),
            ("TMUX", None),
            ("STY", None),
        ]);
        let png = tmp_png();
        let base = png.parent().unwrap().to_path_buf();
        assert!(
            resolve_local_png(&base, &base, &png.display().to_string()).is_none(),
            "no graphics protocol → text fallback, never a placement"
        );
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn reserve_markdown_images_inserts_bed_and_shifts_link_rows() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
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

        let reserved = reserve_markdown_images(&mut lines, &mut links, &images, &base, &base);

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
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn reserve_markdown_images_skips_unresolvable_sources() {
        let _guard = kitty_test_lock();
        let _env = kitty_env();
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

        let reserved = reserve_markdown_images(&mut lines, &mut links, &images, &base, &base);

        assert!(reserved.is_empty(), "nothing resolvable → no beds");
        assert_eq!(lines.len(), 4, "caption-only fallback keeps layout intact");
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
        assert!(
            gone.is_empty(),
            "image scrolled above the window is dropped"
        );
    }

    fn plain(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}
