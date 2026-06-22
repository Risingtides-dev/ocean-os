use std::path::{Path, PathBuf};

/// Resolve a model-supplied filesystem path against the turn's bound cwd.
///
/// Absolute paths are left untouched. Relative paths are interpreted relative to
/// the session/turn cwd, not the daemon process cwd. If a tool is used outside a
/// session context (tests/ad-hoc default_tools), it preserves the historical
/// process-cwd behaviour by returning the relative path unchanged.
pub(crate) fn resolve_against_cwd(cwd: Option<&Path>, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else if let Some(cwd) = cwd {
        cwd.join(p)
    } else {
        p
    }
}
