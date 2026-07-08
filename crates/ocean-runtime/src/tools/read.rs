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
        }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd: Some(cwd),
            snapshots: None,
            artifacts: None,
        }
    }

    /// Hashline-enabled read: tags output with the file content hash and records
    /// a snapshot into the shared session store.
    pub fn for_cwd_with_snapshots(cwd: PathBuf, snapshots: SharedSnapshots) -> Self {
        Self {
            cwd: Some(cwd),
            snapshots: Some(snapshots),
            artifacts: None,
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

        let text = fs::read_to_string(&path)
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
        let end = limit
            .map(|l| std::cmp::min(start.saturating_add(l), lines.len()))
            .unwrap_or(lines.len());

        let mut buf = String::new();
        // Hashline harness: prefix a `[path#HASH]` content tag and record the
        // snapshot so `hashline_edit` can validate/recover against exactly what
        // the model saw. The hash is over the WHOLE file (not the shown slice);
        // `seen_lines` records the slice actually surfaced.
        if let Some(store) = &self.snapshots {
            if let Ok(mut store) = store.lock() {
                // record() computes the hash; use it for the header too.
                let hash = store.record(&display_path, &text, [(start + 1, end.max(start + 1))]);
                buf.push_str(&format!("[{display_path}#{hash}]\n"));
            }
        }
        for (i, line) in lines[start..end].iter().enumerate() {
            buf.push_str(&format!("{:>5}\t{}\n", start + i + 1, line));
        }
        Ok(AgentToolResult::text(buf))
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
}
