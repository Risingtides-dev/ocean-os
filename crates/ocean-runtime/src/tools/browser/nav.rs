use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserNavigateTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Navigate the Ocean-controlled Chrome to a URL. Returns the page title."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string", "description": "Absolute URL" } },
            "required": ["url"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("missing 'url'")?;
        let title = self
            .ctx
            .handle
            .navigate(url)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("navigated to {url} — title: {title}")))
    }
}
