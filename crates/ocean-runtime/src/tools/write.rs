use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct WriteTool {
    cwd: Option<PathBuf>,
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

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
        let display_path = path.to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), path);
        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await.map_err(|e| {
                    format!(
                        "could not create parent directory {}: {e}",
                        parent.display()
                    )
                })?;
            }
        }
        fs::write(&path, content)
            .await
            .map_err(|e| format!("write {display_path}: {e}"))?;
        Ok(AgentToolResult::text(format!(
            "wrote {} bytes to {}",
            content.len(),
            display_path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When the would-be parent directory is actually a regular file,
    /// `create_dir_all` fails. The write tool must surface that dir-creation
    /// failure (naming the directory + underlying cause) rather than letting
    /// the later `fs::write` mask it as a confusing not-found.
    #[tokio::test]
    async fn write_surfaces_create_dir_all_failure_when_parent_is_a_file() {
        // Create a temp file that we'll then treat as a directory.
        let mut blocker = std::env::temp_dir();
        blocker.push(format!("ocean187-parent-{}", std::process::id()));
        // Ensure a clean slate, then create it as a FILE.
        let _ = fs::remove_file(&blocker).await;
        let _ = fs::remove_dir_all(&blocker).await;
        fs::write(&blocker, b"i am a file, not a dir")
            .await
            .expect("create blocker file");

        // Target lives "under" the file → create_dir_all(blocker) must fail.
        let target = blocker.join("child.txt");

        let tool = WriteTool::new();
        let args = json!({
            "path": target.to_str().unwrap(),
            "content": "hello",
        });

        let err = tool
            .execute("call-1", args)
            .await
            .expect_err("writing under a file path must fail");

        // The error names the offending directory and is the dir-creation
        // error, not a generic not-found from the later file write.
        assert!(
            err.contains("could not create parent directory"),
            "expected a parent-directory creation error, got: {err}"
        );
        assert!(
            err.contains(&blocker.display().to_string()),
            "error should name the directory {}, got: {err}",
            blocker.display()
        );

        // cleanup
        let _ = fs::remove_file(&blocker).await;
    }
}
