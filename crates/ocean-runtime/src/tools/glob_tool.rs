use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult};

pub struct GlobTool;

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
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern'")?
            .to_string();
        let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(500) as usize;

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut buf = String::new();
            let entries = glob::glob(&pattern).map_err(|e| e.to_string())?;
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
