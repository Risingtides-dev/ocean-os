use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserEvalJsTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserEvalJsTool {
    fn name(&self) -> &str {
        "browser_eval_js"
    }
    fn description(&self) -> &str {
        "Evaluate JavaScript in the page and return its result as JSON."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "source": { "type": "string" } }, "required": ["source"] })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let src = args
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or("missing 'source'")?;
        let out = self
            .ctx
            .lazy
            .get()
            .await?
            .eval_js(src)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}

pub struct BrowserConsoleTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserConsoleTool {
    fn name(&self) -> &str {
        "browser_console"
    }
    fn description(&self) -> &str {
        "Read recent console output (log/warn/error). Note: captures logs emitted after the first read this session."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let out = self
            .ctx
            .lazy
            .get()
            .await?
            .read_console()
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}

pub struct BrowserNetworkTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserNetworkTool {
    fn name(&self) -> &str {
        "browser_network"
    }
    fn description(&self) -> &str {
        "Read recent network requests (resource timings: name, type, duration)."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let out = self
            .ctx
            .lazy
            .get()
            .await?
            .read_network()
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}
