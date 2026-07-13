//! Opt-in, local-only capture of provider request bodies for prompt inspection.
//!
//! The capture contains the exact JSON body handed to a provider after Ocean has
//! serialized its system instruction, transcript, and tool schemas. It is
//! deliberately disabled unless `OCEAN_PROMPT_CAPTURE_DIR` names a directory.
//! Request headers and URLs are never captured: those may carry credentials (and
//! Google places its API key in the URL). Captures contain user/model content, so
//! the directory and files are owner-only on Unix.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write an exact serialized provider request to the configured local capture
/// directory. This is strictly diagnostic and fail-open: a bad/unwritable
/// capture directory must never block a model turn.
pub fn capture_request_body(api: &str, provider: &str, model: &str, body: &Value) {
    let Some(dir) = std::env::var_os("OCEAN_PROMPT_CAPTURE_DIR") else {
        return;
    };
    if dir.is_empty() {
        return;
    }

    if let Err(error) = capture_request_body_to_dir(Path::new(&dir), api, provider, model, body) {
        tracing::warn!(%error, "could not capture provider request body");
    }
}

/// Write one redaction-safe request envelope to `dir`.
///
/// This never writes HTTP headers or endpoint URLs. The body is intentionally
/// complete because this function exists to inspect the exact model context.
pub fn capture_request_body_to_dir(
    dir: &Path,
    api: &str,
    provider: &str,
    model: &str,
    body: &Value,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    restrict_dir_to_owner(dir)?;

    let envelope = json!({
        "capture_format": "ocean-provider-request-v1",
        "captured_at_ms": crate::types::now_ms(),
        "api": api,
        "provider": provider,
        "model": model,
        "request_body": body,
    });
    let rendered = serde_json::to_vec_pretty(&envelope).map_err(std::io::Error::other)?;
    let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{}-{}-{}-{}.json",
        crate::types::now_ms(),
        std::process::id(),
        sequence,
        safe_filename_component(model),
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    restrict_file_to_owner(&file)?;
    file.write_all(&rendered)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    tracing::info!(path = %path.display(), "captured provider request body locally");
    Ok(path)
}

fn safe_filename_component(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(unix)]
fn restrict_dir_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_to_owner(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file_to_owner(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_complete_and_owner_only() {
        let root = std::env::temp_dir().join(format!(
            "ocean-prompt-capture-test-{}-{}",
            std::process::id(),
            CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let body = json!({"instructions": "full system", "input": [{"role": "user"}]});
        let path = capture_request_body_to_dir(
            &root,
            "codex-responses",
            "openai-codex",
            "gpt-test",
            &body,
        )
        .expect("capture writes");
        let captured: Value =
            serde_json::from_slice(&fs::read(&path).expect("read capture")).expect("valid json");
        assert_eq!(captured["request_body"], body);
        assert_eq!(captured["provider"], "openai-codex");
        assert!(captured.get("headers").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).expect("remove test capture");
    }
}
