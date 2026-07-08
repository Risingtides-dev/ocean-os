//! Atomic read-merge-write of Ocean's auth JSON.
//!
//! The auth file is a single JSON object keyed by provider. Unrelated blocks
//! (deepseek, kimi, …) MUST be preserved. Writes are atomic (temp file + rename
//! in the same directory, temp named `.auth.json.tmp-{pid}` to match
//! `ocean-agent::oauth_refresh`) and restricted to 0600 on Unix.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Merge `block` under `key`, preserving every other key, and write atomically.
pub(crate) fn merge_and_write(auth_file: &Path, key: &str, block: Value) -> Result<()> {
    let mut root = read_root(auth_file)?;
    let map = root
        .as_object_mut()
        .context("auth file root is not a JSON object")?;
    map.insert(key.to_string(), block);
    let serialized = serde_json::to_string_pretty(&root)?;
    atomic_write_private(auth_file, &serialized)?;
    Ok(())
}

/// Read the existing root object, or an empty object when the file is missing
/// or blank.
fn read_root(path: &Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                return Ok(json!({}));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("auth file {} is not valid JSON", path.display()))?;
            if !value.is_object() {
                bail!("auth file {} root is not a JSON object", path.display());
            }
            Ok(value)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(err).with_context(|| format!("failed to read auth file {}", path.display())),
    }
}

fn atomic_write_private(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create auth dir {}", parent.display()))?;

    // Match oauth_refresh's temp-name convention exactly.
    let tmp = parent.join(format!(".auth.json.tmp-{}", std::process::id()));

    {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("failed to create temp auth file {}", tmp.display()))?;
        // Restrict permissions BEFORE writing sensitive content.
        set_private(&tmp)?;
        std::io::Write::write_all(&mut file, content.as_bytes())?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to finalize auth file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::merge_and_write;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII temp dir; cleaned up even on panic.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fresh() -> (TempDir, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ocean-oauth-store-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let auth = dir.join("auth.json");
        (TempDir(dir), auth)
    }

    #[cfg(unix)]
    fn mode_is_0600(path: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777 == 0o600)
            .unwrap_or(false)
    }

    #[test]
    fn preserves_unrelated_keys_and_replaces_same_key_block() {
        let (_guard, auth) = fresh();
        std::fs::write(&auth, r#"{"deepseek":{"k":"v"},"claude-code":{"old":true}}"#).unwrap();
        merge_and_write(
            &auth,
            "claude-code",
            json!({"type":"oauth","access":"a","refresh":"r","expires":1}),
        )
        .unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
        // Unrelated key preserved byte-for-value.
        assert_eq!(v["deepseek"]["k"], "v");
        // Same-key block fully replaced — no leftover "old".
        assert_eq!(v["claude-code"]["type"], "oauth");
        assert_eq!(v["claude-code"]["access"], "a");
        assert!(v["claude-code"].get("old").is_none(), "old block not replaced: {v}");
    }

    #[test]
    fn creates_parent_dirs_and_file_when_absent() {
        let (_guard, auth) = fresh();
        // Remove the dir so the file's parent doesn't exist yet.
        let parent = auth.parent().unwrap();
        std::fs::remove_dir_all(parent).unwrap();
        assert!(!parent.exists());

        merge_and_write(
            &auth,
            "claude-code",
            json!({"type":"oauth","access":"a","refresh":"r","expires":7}),
        )
        .unwrap();

        assert!(auth.exists(), "auth file should have been created");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
        assert_eq!(v["claude-code"]["expires"], 7);
        #[cfg(unix)]
        assert!(mode_is_0600(&auth), "expected 0600 permissions on auth file");
    }

    #[test]
    fn whitespace_only_file_is_treated_as_empty() {
        let (_guard, auth) = fresh();
        std::fs::write(&auth, "   \n  ").unwrap(); // blank -> treated as {}
        merge_and_write(
            &auth,
            "openai-codex",
            json!({"type":"oauth","access":"a","refresh":"r","expires":9}),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
        assert_eq!(v["openai-codex"]["access"], "a");
    }

    #[test]
    fn invalid_json_errors_without_clobbering() {
        let (_guard, auth) = fresh();
        let garbage = "{not valid json";
        std::fs::write(&auth, garbage).unwrap();
        let err = merge_and_write(&auth, "claude-code", json!({"type":"oauth"})).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "err: {err}");
        // Original bytes untouched.
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), garbage);
    }

    #[test]
    fn output_is_pretty_printed() {
        let (_guard, auth) = fresh();
        merge_and_write(
            &auth,
            "claude-code",
            json!({"type":"oauth","access":"a","refresh":"r","expires":1}),
        )
        .unwrap();
        let contents = std::fs::read_to_string(&auth).unwrap();
        assert!(contents.contains("\n  \"claude-code\""), "not pretty: {contents}");
    }
}
