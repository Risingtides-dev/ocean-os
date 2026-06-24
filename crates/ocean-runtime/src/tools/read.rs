use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct ReadTool {
    cwd: Option<PathBuf>,
}

impl ReadTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
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
            .execute("1", json!({ "path": file.to_string_lossy(), "offset": 100 }))
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
