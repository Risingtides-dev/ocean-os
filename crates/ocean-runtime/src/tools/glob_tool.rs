use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct GlobTool {
    cwd: Option<PathBuf>,
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Expand a glob pattern (e.g. 'src/**/*.rs') and return matching paths."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"pattern": {"type": "string"}, "max": {"type": "integer", "default": 500}},
            "required": ["pattern"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let supplied_pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern'")?;
        let display_pattern = supplied_pattern.to_string();
        let pattern = resolve_against_cwd(self.cwd.as_deref(), supplied_pattern)
            .to_string_lossy()
            .into_owned();
        let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(500) as usize;

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut buf = String::new();
            let entries =
                glob::glob(&pattern).map_err(|e| format!("glob {display_pattern}: {e}"))?;
            for (count, path) in entries.flatten().enumerate() {
                if count >= max {
                    buf.push_str(&format!("... (truncated at {max})\n"));
                    break;
                }
                buf.push_str(&format!("{}\n", path.display()));
            }
            Ok(buf)
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(AgentToolResult::text(if result.is_empty() {
            "(no matches)".to_string()
        } else {
            result
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentTool;
    use serde_json::json;
    use uuid::Uuid;

    #[tokio::test]
    async fn glob_resolves_relative_pattern_against_bound_cwd() {
        let root = std::env::temp_dir().join(format!("ocean-glob-cwd-{}", Uuid::new_v4()));
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).expect("create temp tree");
        std::fs::write(nested.join("main.rs"), "fn main() {}\n").expect("write fixture");

        let tool = GlobTool::for_cwd(root.clone());
        let out = AgentTool::execute(&tool, "test", json!({"pattern":"src/*.rs"}))
            .await
            .expect("glob succeeds")
            .content
            .into_iter()
            .filter_map(|content| content.as_text().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            out.contains(&root.join("src/main.rs").display().to_string()),
            "glob output should be rooted at bound cwd, got: {out}"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
