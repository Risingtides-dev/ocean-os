use super::query_flag_truthy;
use axum::{extract::Query, http::StatusCode, Json};
use serde_json::json;

/// Expand a leading `~` to `$HOME`. Returns the literal path unchanged when
/// `HOME` is unset or the path doesn't start with `~`.
pub(super) fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            if path == "~" {
                return home;
            }
            return format!("{}/{}", home, &path[2..]);
        }
    }
    path.to_string()
}

/// True when `child` is exactly `parent` or a direct descendant
/// (`parent/something`), guarding against sibling-prefix attacks like
/// `/home/user2` passing a `/home/user` sandbox check.
pub(super) fn path_is_under(child: &str, parent: &str) -> bool {
    child == parent
        || (child.starts_with(parent) && child.as_bytes().get(parent.len()) == Some(&b'/'))
}

/// Canonicalize `path`, mapping the OS error to a short string suitable for
/// the `error` field of an API response.
pub(super) fn try_canonicalize(path: &str) -> Result<String, String> {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("cannot resolve path: {e}"))
}

/// Structured outcome of resolving an fs-endpoint path against the `$HOME`
/// sandbox. The resolution logic is shared (`resolve_under_home`); each handler
/// maps a variant to its own status code so the security-critical sandbox check
/// lives in exactly one place.
enum FsResolveError {
    /// `$HOME` is unset.
    HomeUnset,
    /// `$HOME` itself cannot be canonicalized (server misconfig).
    HomeUnresolved(String),
    /// The requested path does not exist (canonicalize failed).
    NotFound(String),
    /// The requested path resolves outside `$HOME`.
    OutsideHome { raw: String },
}

impl FsResolveError {
    /// Stable message for the `error` JSON field, matching the wording `fs_dirs`
    /// has always produced.
    fn message(&self) -> String {
        match self {
            Self::HomeUnset => "HOME not set".to_string(),
            Self::HomeUnresolved(e) => format!("cannot resolve HOME: {e}"),
            Self::NotFound(e) => format!("path does not exist: {e}"),
            Self::OutsideHome { raw } => {
                format!("access denied: {raw} is outside home directory")
            }
        }
    }

    /// Status code `fs_dirs` returns for this error (preserved verbatim from
    /// the pre-extraction inline handling).
    fn dirs_status(&self) -> StatusCode {
        match self {
            Self::OutsideHome { .. } => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Status code `fs/file` returns: 403 outside `$HOME`, 404 for a missing
    /// file, 500 for a server-side `$HOME` misconfig.
    fn file_status(&self) -> StatusCode {
        match self {
            Self::OutsideHome { .. } => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Resolve `raw` against the shared `$HOME` sandbox used by the fs endpoints:
/// expand a leading `~`, canonicalize, and require the result to live under
/// `$HOME`. Returns `(home_canonical, target_canonical)` on success, or a
/// structured error each handler maps to its own status code. This is the ONE
/// place the sandbox check is performed — `fs_dirs` and `fs/file` both go
/// through it.
fn resolve_under_home(raw: &str) -> Result<(String, std::path::PathBuf), FsResolveError> {
    let home_raw = std::env::var("HOME").map_err(|_| FsResolveError::HomeUnset)?;
    let home_canon = std::fs::canonicalize(&home_raw)
        .map_err(|e| FsResolveError::HomeUnresolved(e.to_string()))?;
    let home_canon_str = home_canon.to_string_lossy().to_string();

    let expanded = expand_tilde(raw);
    let target =
        std::fs::canonicalize(&expanded).map_err(|e| FsResolveError::NotFound(e.to_string()))?;

    let target_str = target.to_string_lossy().to_string();
    if !path_is_under(&target_str, &home_canon_str) {
        return Err(FsResolveError::OutsideHome {
            raw: raw.to_string(),
        });
    }

    Ok((home_canon_str, target))
}

/// Query for `GET /v1/fs/dirs`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct FsDirsQuery {
    /// Path to list subdirectories of. Defaults to `$HOME` when omitted.
    #[serde(default)]
    pub(super) path: Option<String>,
    /// When truthy (`1`/`true`/`yes`/`on`, parsed like the SSE `?all=` flag),
    /// the response also includes `files[]` — the regular files in the
    /// directory (dotfiles INCLUDED; the workspace tree filters client-side).
    /// Defaults off, in which case `files[]` is omitted entirely.
    #[serde(default)]
    pub(super) files: Option<String>,
}

/// `GET /v1/fs/dirs?path=&files=1` — list subdirectories under a path,
/// sandboxed to `$HOME`. Dot-directories are skipped; only directories are
/// returned under `dirs[]` (alphabetical) with `"is_repo"` and `"git_branch"`
/// per entry via a pure filesystem HEAD read. `parent` is the canonical parent
/// directory, `null` at `$HOME` or the filesystem root. With `files=1` the
/// response also gains `files[]` — the regular files in the directory (dotfiles
/// INCLUDED), each `{name, path, size}`, sorted by name; `files[]` is omitted
/// entirely when the flag is unset, so callers that never ask for it see the
/// same body they always have.
pub(super) async fn fs_dirs(Query(q): Query<FsDirsQuery>) -> (StatusCode, Json<serde_json::Value>) {
    // Default the path to `$HOME`; `resolve_under_home` reports HomeUnset
    // (→ 500 "HOME not set") when `$HOME` is unset, matching the old behavior.
    let raw = match q.path {
        Some(p) => p,
        None => std::env::var("HOME").unwrap_or_default(),
    };

    let (home_canon_str, target) = match resolve_under_home(&raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                e.dirs_status(),
                Json(json!({"ok": false, "error": e.message()})),
            );
        }
    };
    let target_str = target.to_string_lossy().to_string();
    let include_files = query_flag_truthy(q.files.as_deref());

    // Parent is null at $HOME or at the filesystem root.
    let parent: Option<String> = if target_str == home_canon_str {
        None
    } else {
        target.parent().and_then(|p| {
            let ps = p.to_string_lossy().to_string();
            if ps.is_empty() {
                None
            } else {
                Some(ps)
            }
        })
    };

    let mut dirs: Vec<serde_json::Value> = Vec::new();
    let mut files: Vec<serde_json::Value> = Vec::new();
    match std::fs::read_dir(&target) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy().to_string();
                let path = entry.path();
                if path.is_dir() {
                    // Dot-directories are skipped (existing behavior).
                    if name_str.starts_with('.') {
                        continue;
                    }
                    let (is_repo, git_branch) = ocean_agent::git_head_info(&path);
                    dirs.push(json!({
                        "name": name_str,
                        "path": path.to_string_lossy().to_string(),
                        "is_repo": is_repo,
                        "git_branch": git_branch,
                    }));
                } else if include_files && path.is_file() {
                    // Regular files — dotfiles INCLUDED; the workspace tree
                    // filters client-side. `size` falls back to 0 if stat fails.
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push(json!({
                        "name": name_str,
                        "path": path.to_string_lossy().to_string(),
                        "size": size,
                    }));
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("cannot read directory: {e}")})),
            );
        }
    }

    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    files.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    // Build the response; only attach `files[]` when requested so the no-flag
    // body is byte-compatible with the pre-existing shape.
    let mut resp = json!({
        "ok": true,
        "path": target_str,
        "parent": parent,
        "home": home_canon_str,
        "dirs": dirs,
    });
    if include_files {
        resp["files"] = json!(files);
    }
    (StatusCode::OK, Json(resp))
}

/// Query for `GET /v1/fs/file`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct FsFileQuery {
    /// Absolute (or `~`-relative) path of the file to read. Required.
    pub(super) path: String,
}

/// Maximum number of bytes returned in `content`. Reads fetch `cap + 1` bytes
/// so truncation is detectable without a second syscall; `content` is capped at
/// exactly `FS_FILE_CAP` lossy-UTF-8 bytes.
pub(super) const FS_FILE_CAP: usize = 512 * 1024;

/// Number of leading bytes inspected for a NUL when deciding the file is binary.
const FS_FILE_BINARY_SNIFF: usize = 8 * 1024;

/// `GET /v1/fs/file?path=<abs>` — read a (small) file sandboxed to `$HOME`,
/// the same guard `fs_dirs` uses. Returns up to `FS_FILE_CAP` bytes as lossy
/// UTF-8 text; a NUL byte in the first 8 KiB marks the file binary (empty
/// content). The response is a uniform envelope `{path, content, truncated,
/// binary, size, error}` — `error` is `null` on success and the consumer's
/// success predicate is `error.is_none()` (the daemon does NOT send an `ok`
/// field on this route). Errors map to 403 (outside `$HOME`) or 404
/// (missing/unreadable).
pub(super) async fn fs_file(Query(q): Query<FsFileQuery>) -> (StatusCode, Json<serde_json::Value>) {
    let raw = q.path;
    let (_home, target) = match resolve_under_home(&raw) {
        Ok(v) => v,
        Err(e) => {
            return (e.file_status(), Json(fs_file_error_body(&e.message())));
        }
    };
    let target_str = target.to_string_lossy().to_string();

    // Stat first for an honest `size` and a clean 404 when the path is gone.
    let size = match std::fs::metadata(&target) {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(fs_file_error_body(&format!("cannot read file: {e}"))),
            );
        }
    };

    // Read up to cap + 1 bytes: the +1 lets us detect truncation precisely.
    let mut bytes = match read_capped(&target, FS_FILE_CAP) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(fs_file_error_body(&format!("cannot read file: {e}"))),
            );
        }
    };
    let truncated = bytes.len() > FS_FILE_CAP;

    // Binary sniff: a NUL anywhere in the first 8 KiB.
    let sniff_end = bytes.len().min(FS_FILE_BINARY_SNIFF);
    let binary = bytes[..sniff_end].contains(&0u8);

    let content = if binary {
        String::new()
    } else {
        if bytes.len() > FS_FILE_CAP {
            bytes.truncate(FS_FILE_CAP);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };

    (
        StatusCode::OK,
        Json(json!({
            "path": target_str,
            "content": content,
            "truncated": truncated,
            "binary": binary,
            "size": size,
            "error": null,
        })),
    )
}

/// Read at most `cap + 1` bytes of `path`. Returns the bytes actually read
/// (length `0..=cap+1`) so the caller detects truncation via `len > cap`.
fn read_capped(path: &std::path::Path, cap: usize) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap + 1];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Uniform error envelope for `fs_file` — every field the success body carries
/// is present (defaults for the non-error fields) so a single consumer struct
/// deserializes both success and error and keys off `error.is_none()`.
fn fs_file_error_body(message: &str) -> serde_json::Value {
    json!({
        "path": "",
        "content": "",
        "truncated": false,
        "binary": false,
        "size": 0,
        "error": message,
    })
}
