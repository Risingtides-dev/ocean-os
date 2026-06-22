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
        let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let end = limit
            .map(|l| std::cmp::min(start + l, lines.len()))
            .unwrap_or(lines.len());

        let mut buf = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            buf.push_str(&format!("{:>5}\t{}\n", start + i + 1, line));
        }
        Ok(AgentToolResult::text(buf))
    }
}
