//! Small helpers for crash-durable file replacement.
//!
//! The idiom throughout this crate is: write a temp sibling, fsync it,
//! then `rename` it over the target so a crash mid-write can never
//! corrupt an existing good file. That idiom is *atomic* but not, on its
//! own, *durable*: POSIX guarantees the temp file's contents survive a
//! power loss once it is fsynced, but the directory entry created by the
//! `rename` is only durable once the containing directory is itself
//! fsynced. Without that final step a power loss shortly after a save can
//! silently revert the target to its previous contents even though the
//! write "succeeded". These helpers close that gap.

use std::path::Path;

/// Best-effort fsync of the directory containing `path`.
///
/// On unix, opening a directory read-only yields a descriptor whose
/// `fsync` flushes the directory's own metadata — the pending `rename`
/// entry — to stable storage. Directory fsync is not portable (Windows
/// cannot open a directory as a file), so this is a no-op off unix, where
/// the temp+rename idiom is not used for durability anyway.
#[cfg(unix)]
pub(crate) fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        // A bare filename has an empty parent, meaning the cwd.
        let dir = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn fsync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Rename `tmp` onto `target`, then fsync the containing directory so the
/// rename is durable across power loss.
///
/// The caller MUST have already fsynced `tmp` (e.g. via
/// [`std::fs::File::sync_all`]) so the renamed file's contents are stable
/// before the directory entry is made durable.
pub(crate) fn durable_rename(tmp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, target)?;
    fsync_parent_dir(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn durable_rename_replaces_target_and_consumes_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        std::fs::write(&target, "old").unwrap();

        let tmp = dir.path().join(".state.json.tmp");
        {
            let mut file = std::fs::File::create(&tmp).unwrap();
            file.write_all(b"new").unwrap();
            file.sync_all().unwrap();
        }

        durable_rename(&tmp, &target).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!tmp.exists(), "temp must be renamed away");
    }

    #[test]
    fn fsync_parent_dir_handles_bare_filename() {
        // A path with an empty parent (a bare filename) must resolve to the
        // cwd rather than erroring; the cwd always exists during tests.
        fsync_parent_dir(Path::new("some-file-name")).unwrap();
    }
}
