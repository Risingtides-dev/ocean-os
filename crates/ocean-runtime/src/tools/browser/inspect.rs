use async_trait::async_trait;
use serde_json::{json, Value};

#[cfg(feature = "legacy-chromium")]
use super::active_result;
use super::BrowserToolCtx;
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
        #[cfg(feature = "legacy-chromium")]
        {
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
        #[cfg(not(feature = "legacy-chromium"))]
        {
            let _ = args;
            Err(super::browser_host_unavailable())
        }
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
    fn requires_permission(&self) -> bool {
        // Read-only perception; never mutates page state.
        false
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        #[cfg(feature = "legacy-chromium")]
        {
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
        #[cfg(not(feature = "legacy-chromium"))]
        {
            Err(super::browser_host_unavailable())
        }
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
    fn requires_permission(&self) -> bool {
        // Read-only perception; never mutates page state.
        false
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        #[cfg(feature = "legacy-chromium")]
        {
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
        #[cfg(not(feature = "legacy-chromium"))]
        {
            Err(super::browser_host_unavailable())
        }
    }
}
