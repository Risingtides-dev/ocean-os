use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::types::{AgentTool, AgentToolResult};

pub struct WriteTool;

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn requires_permission(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Write the given content to the path, replacing any existing file. Creates parent directories as needed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path'")?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("missing 'content'")?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await.ok();
            }
        }
        fs::write(path, content)
            .await
            .map_err(|e| format!("write {path}: {e}"))?;
        Ok(AgentToolResult::text(format!(
            "wrote {} bytes to {}",
            content.len(),
            path
        )))
    }
}
