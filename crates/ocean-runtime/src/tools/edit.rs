use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::types::{AgentTool, AgentToolResult};

pub struct EditTool;

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn requires_permission(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Replace one occurrence (or all, with replace_all) of old_string with new_string in the file at path."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "default": false}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path'")?;
        let old_s = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or("missing 'old_string'")?;
        let new_s = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or("missing 'new_string'")?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let text = fs::read_to_string(path)
            .await
            .map_err(|e| format!("read {path}: {e}"))?;
        let count = text.matches(old_s).count();
        if count == 0 {
            return Err(format!("old_string not found in {path}"));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "old_string occurs {count} times in {path}; pass replace_all=true or expand the match"
            ));
        }
        let updated = if replace_all {
            text.replace(old_s, new_s)
        } else {
            text.replacen(old_s, new_s, 1)
        };
        fs::write(path, updated)
            .await
            .map_err(|e| format!("write {path}: {e}"))?;
        let n = if replace_all { count } else { 1 };
        Ok(AgentToolResult::text(format!(
            "edited {path}: {n} replacement(s)"
        )))
    }
}
