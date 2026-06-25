use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct LsTool {
    cwd: Option<PathBuf>,
}

impl LsTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List entries in a directory. Returns name and kind (file/dir/symlink)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path'")?;
        let display_path = path.to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), path);
        let mut read = fs::read_dir(&path)
            .await
            .map_err(|e| format!("ls {display_path}: {e}"))?;
        let mut buf = String::new();
        while let Some(entry) = read.next_entry().await.map_err(|e| e.to_string())? {
            let ft = entry.file_type().await.map_err(|e| e.to_string())?;
            let kind = if ft.is_dir() {
                "dir"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "file"
            };
            buf.push_str(&format!(
                "{}\t{}\n",
                kind,
                entry.file_name().to_string_lossy()
            ));
        }
        Ok(AgentToolResult::text(buf))
    }
}
