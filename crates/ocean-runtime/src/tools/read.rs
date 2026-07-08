use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ocean_hashline::SnapshotStore;
use serde_json::{json, Value};
use tokio::fs;

use crate::artifacts::SharedArtifacts;
use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

/// Shared, session-scoped hashline snapshot store (W1). `read` records the
/// files it shows the model here; `hashline_edit` reads them back for
/// content-hash validation and recovery.
pub type SharedSnapshots = Arc<Mutex<SnapshotStore>>;

/// URI scheme for reading a spilled artifact back (W3 output-meta + spill).
const ARTIFACT_SCHEME: &str = "artifact://";

/// Max bytes of a file loaded into memory per read. Pre-fix, `read_to_string`
/// loaded the ENTIRE file — a multi-GB log or build artifact ballooned daemon
/// RAM, and the full content then rode the `ToolExecutionEnd` event onto the
/// SSE bus (only the *transcript* copy is capped downstream). Reading stops at
/// the cap; the result says so and points at `offset`/`limit`.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Lines returned when the model passes no `limit`. Pre-fix, no-limit meant the
/// whole file — a 200k-line file dumped in one result. Models paginate with
/// `offset`/`limit`; the result notes when more lines remain.
const DEFAULT_LINE_LIMIT: usize = 2000;

/// Max chars of a single line surfaced to the model. One minified-bundle line
/// could previously put 100 KB+ into a single result row.
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadTool {
    cwd: Option<PathBuf>,
    /// When set (hashline harness profile), `read` emits a `[path#HASH]` tag and
    /// records a snapshot. `None` → the classic plain read, unchanged.
    snapshots: Option<SharedSnapshots>,
    /// When set (artifact-spill harness profile), a `path` of the form
    /// `artifact://<id>` resolves from this session store instead of disk. `None`
    /// → `artifact://` paths are treated as ordinary (and failing) file reads,
    /// so the classic behavior is unchanged.
    artifacts: Option<SharedArtifacts>,
    /// Byte cap per read; [`MAX_READ_BYTES`] in production, shrinkable in tests.
    max_bytes: u64,
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTool {
    pub fn new() -> Self {
        Self {
            cwd: None,
            snapshots: None,
            artifacts: None,
            max_bytes: MAX_READ_BYTES,
        }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd: Some(cwd),
            snapshots: None,
            artifacts: None,
            max_bytes: MAX_READ_BYTES,
        }
    }

    /// Hashline-enabled read: tags output with the file content hash and records
    /// a snapshot into the shared session store.
    pub fn for_cwd_with_snapshots(cwd: PathBuf, snapshots: SharedSnapshots) -> Self {
        Self {
            cwd: Some(cwd),
            snapshots: Some(snapshots),
            artifacts: None,
            max_bytes: MAX_READ_BYTES,
        }
    }

    /// Bind the session artifact store so `read artifact://<id>` resolves spilled
    /// tool outputs (W3). Composable with the hashline constructors. `None`-op
    /// when never called — the classic read is unchanged.
    pub fn with_artifacts(mut self, artifacts: SharedArtifacts) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    /// Resolve an `artifact://<id>` path against the session store, applying
    /// `offset`/`limit` as a 1-based line window. A read with neither returns the
    /// artifact's exact bytes (the "nothing is lost" round-trip); a windowed read
    /// returns the selected lines joined by `\n`, no line-number gutter — the
    /// spilled bytes are tool output, not a file, so they're handed back verbatim.
    fn read_artifact(
        &self,
        id: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<AgentToolResult, String> {
        let store = self
            .artifacts
            .as_ref()
            .ok_or("artifact:// reads are not enabled for this session")?;
        let guard = store.lock().map_err(|_| "artifact store poisoned")?;
        let artifact = guard
            .get(id)
            .ok_or_else(|| format!("artifact not found: {ARTIFACT_SCHEME}{id}"))?;

        // Full read (no window): return the exact spilled bytes.
        if offset.is_none() && limit.is_none() {
            return Ok(AgentToolResult::text(artifact.text.clone()));
        }

        let lines: Vec<&str> = artifact.text.lines().collect();
        let start = offset
            .map(|o| o.saturating_sub(1))
            .unwrap_or(0)
            .min(lines.len());
        let end = limit
            .map(|l| std::cmp::min(start.saturating_add(l), lines.len()))
            .unwrap_or(lines.len());
        Ok(AgentToolResult::text(lines[start..end].join("\n")))
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn concurrency(&self) -> crate::types::Concurrency {
        crate::types::Concurrency::Shared
    }
    fn description(&self) -> &str {
        "Read the contents of a file from disk. Returns text content with optional line numbers."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or relative path to the file"},
                "offset": {"type": "integer", "description": "Line offset (1-based), optional"},
                "limit": {"type": "integer", "description": "Max number of lines, optional"}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path'")?;
        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        // Internal URI: `artifact://<id>` reads a spilled tool output back from
        // the session store instead of touching disk (W3). Only intercepted when
        // the session has an artifact store bound; otherwise it falls through to
        // the disk path (and fails as an ordinary missing file), preserving the
        // classic contract.
        if self.artifacts.is_some() {
            if let Some(id) = path.strip_prefix(ARTIFACT_SCHEME) {
                return self.read_artifact(id, offset, limit);
            }
        }

        let display_path = path.to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), path);

        // Bounded read: load at most `max_bytes` — never the whole of an
        // arbitrarily large file. `read_to_string` before this loaded multi-GB
        // logs wholesale into daemon RAM (and the full content then rode the
        // ToolExecutionEnd event onto the SSE bus).
        let (text, file_len, byte_capped) = read_capped(&path, self.max_bytes)
            .await
            .map_err(|e| format!("read {display_path}: {e}"))?;
        let lines: Vec<&str> = text.lines().collect();
        // Clamp start to the line count: a model-supplied `offset` past EOF must
        // yield an empty read, not a `start > end` slice panic that tears down
        // the turn. `saturating_add` guards a huge `limit` from overflowing usize.
        let start = offset
            .map(|o| o.saturating_sub(1))
            .unwrap_or(0)
            .min(lines.len());
        // No explicit `limit` defaults to a bounded window, not the whole file.
        let end = std::cmp::min(
            start.saturating_add(limit.unwrap_or(DEFAULT_LINE_LIMIT)),
            lines.len(),
        );

        let mut buf = String::new();
        // Hashline harness: prefix a `[path#HASH]` content tag and record the
        // snapshot so `hashline_edit` can validate/recover against exactly what
        // the model saw. The hash is over the WHOLE file (not the shown slice);
        // `seen_lines` records the slice actually surfaced. A byte-capped read
        // did NOT see the whole file, so recording a whole-file snapshot would
        // be a lie the recovery path later trips over — skip it.
        if !byte_capped {
            if let Some(store) = &self.snapshots {
                if let Ok(mut store) = store.lock() {
                    // record() computes the hash; use it for the header too.
                    let hash =
                        store.record(&display_path, &text, [(start + 1, end.max(start + 1))]);
                    buf.push_str(&format!("[{display_path}#{hash}]\n"));
                }
            }
        }
        for (i, line) in lines[start..end].iter().enumerate() {
            let shown: std::borrow::Cow<'_, str> = clip_line(line);
            buf.push_str(&format!("{:>5}\t{}\n", start + i + 1, shown));
        }
        // Say when there is more than what was shown, so the model paginates
        // instead of concluding the file ends here.
        if end < lines.len() {
            buf.push_str(&format!(
                "[showing lines {}-{} of {}; continue with offset={}]\n",
                start + 1,
                end,
                lines.len(),
                end + 1
            ));
        }
        if byte_capped {
            buf.push_str(&format!(
                "[file read capped at {} of {} bytes; content beyond this point not shown]\n",
                self.max_bytes, file_len
            ));
        }
        Ok(AgentToolResult::text(buf))
    }
}

/// Read at most `max_bytes` of the file at `path`. Returns the decoded text,
/// the file's TOTAL length on disk, and whether the read stopped at the cap.
///
/// A capped read can cut mid-way through a multibyte UTF-8 char; the partial
/// trailing char is dropped. An *uncapped* read of invalid UTF-8 stays an error
/// (same contract as the old `read_to_string`).
async fn read_capped(
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<(String, u64, bool), String> {
    use tokio::io::AsyncReadExt;
    let meta = fs::metadata(path).await.map_err(|e| e.to_string())?;
    let file_len = meta.len();
    let file = fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(std::cmp::min(file_len, max_bytes) as usize);
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    let capped = file_len > max_bytes;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) if capped => {
            // The cap split a multibyte char at the very end — drop the partial.
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".to_string())?
        }
        Err(_) => return Err("stream did not contain valid UTF-8".to_string()),
    };
    Ok((text, file_len, capped))
}

/// Clip a single line to [`MAX_LINE_CHARS`] chars with an explicit marker, so
/// one minified-bundle line can't put 100 KB into a single result row.
fn clip_line(line: &str) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => std::borrow::Cow::Owned(format!("{}… [line clipped]", &line[..cut])),
        None => std::borrow::Cow::Borrowed(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the first text block out of a tool result.
    fn body(r: &AgentToolResult) -> &str {
        r.content.first().and_then(|c| c.as_text()).unwrap_or("")
    }

    /// Regression: a model-supplied `offset` past EOF must return an empty read,
    /// not a `start > end` slice panic that tears the turn down.
    #[tokio::test]
    async fn offset_past_eof_is_empty_not_panic() {
        let dir = std::env::temp_dir().join(format!("ocean-read-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir).await;
        let file = dir.join("three.txt");
        fs::write(&file, "a\nb\nc\n").await.unwrap();
        let tool = ReadTool::new();

        // offset way past the 3 lines, no limit
        let r = tool
            .execute(
                "1",
                json!({ "path": file.to_string_lossy(), "offset": 100 }),
            )
            .await
            .expect("must not error");
        assert_eq!(body(&r).trim(), "", "past-EOF offset → empty");

        // offset past EOF WITH a limit (end clamps below start without the fix)
        let r = tool
            .execute(
                "2",
                json!({ "path": file.to_string_lossy(), "offset": 100, "limit": 5 }),
            )
            .await
            .expect("must not error");
        assert_eq!(body(&r).trim(), "", "past-EOF offset+limit → empty");

        // sanity: a normal in-range read still works
        let r = tool
            .execute("3", json!({ "path": file.to_string_lossy(), "offset": 2 }))
            .await
            .unwrap();
        assert!(body(&r).contains("b") && body(&r).contains("c"));
        let _ = fs::remove_dir_all(&dir).await;
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ocean-read-{tag}-{}", std::process::id()))
    }

    /// With no `limit`, only DEFAULT_LINE_LIMIT lines return, plus a note that
    /// more remain. Pre-fix, no-limit dumped the whole file.
    #[tokio::test]
    async fn no_limit_defaults_to_bounded_window_with_note() {
        let dir = tmp_dir("deflimit");
        let _ = fs::create_dir_all(&dir).await;
        let file = dir.join("big.txt");
        let content: String = (1..=DEFAULT_LINE_LIMIT + 500)
            .map(|i| format!("line{i}\n"))
            .collect();
        fs::write(&file, content).await.unwrap();

        let r = ReadTool::new()
            .execute("1", json!({ "path": file.to_string_lossy() }))
            .await
            .unwrap();
        let text = body(&r);
        assert!(
            text.contains(&format!("line{DEFAULT_LINE_LIMIT}")),
            "last in-window line shown"
        );
        assert!(
            !text.contains(&format!("line{}\n", DEFAULT_LINE_LIMIT + 1)),
            "beyond-window line must not be shown"
        );
        assert!(
            text.contains(&format!("continue with offset={}", DEFAULT_LINE_LIMIT + 1)),
            "pagination note present: …{}",
            &text[text.len().saturating_sub(200)..]
        );
        let _ = fs::remove_dir_all(&dir).await;
    }

    /// A file larger than the byte cap is read only up to the cap, with an
    /// explicit note. Pre-fix, the whole file was loaded into memory.
    #[tokio::test]
    async fn oversized_file_is_byte_capped_with_note() {
        let dir = tmp_dir("bytecap");
        let _ = fs::create_dir_all(&dir).await;
        let file = dir.join("huge.txt");
        // 64 KiB of lines against a 16 KiB cap (test-shrunk).
        let content: String = (1..=4096)
            .map(|i| format!("row-{i:05}-padding\n"))
            .collect();
        fs::write(&file, &content).await.unwrap();

        let mut tool = ReadTool::new();
        tool.max_bytes = 16 * 1024;
        let r = tool
            .execute("1", json!({ "path": file.to_string_lossy() }))
            .await
            .expect("capped read succeeds");
        let text = body(&r);
        assert!(
            text.contains("file read capped at 16384"),
            "cap note present: …{}",
            &text[text.len().saturating_sub(200)..]
        );
        assert!(text.contains("row-00001"), "head of file shown");
        assert!(!text.contains("row-04096"), "tail beyond cap not shown");
        let _ = fs::remove_dir_all(&dir).await;
    }

    /// One giant line is clipped at MAX_LINE_CHARS with a marker — a minified
    /// bundle line can't put 100 KB into a single result row.
    #[tokio::test]
    async fn giant_single_line_is_clipped() {
        let dir = tmp_dir("clip");
        let _ = fs::create_dir_all(&dir).await;
        let file = dir.join("minified.js");
        let giant = "x".repeat(10_000);
        fs::write(&file, format!("short\n{giant}\n")).await.unwrap();

        let r = ReadTool::new()
            .execute("1", json!({ "path": file.to_string_lossy() }))
            .await
            .unwrap();
        let text = body(&r);
        assert!(text.contains("[line clipped]"), "clip marker present");
        assert!(
            text.len() < 5_000,
            "clipped output stays small, got {} bytes",
            text.len()
        );
        let _ = fs::remove_dir_all(&dir).await;
    }
}
