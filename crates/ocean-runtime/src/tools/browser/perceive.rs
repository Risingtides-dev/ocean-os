use async_trait::async_trait;
#[cfg(feature = "legacy-chromium")]
use ocean_protocol::Content;
use serde_json::{json, Value};

#[cfg(feature = "legacy-chromium")]
use super::active_result;
use super::BrowserToolCtx;
#[cfg(feature = "legacy-chromium")]
use crate::types::ToolSideEffect;
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserReadPageTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserReadPageTool {
    fn name(&self) -> &str {
        "browser_read_page"
    }
    fn description(&self) -> &str {
        "Read the current page: title, URL, visible interactive elements (each with a `ref` selector usable by browser_click), and visible text. Cheap and precise — prefer this before screenshotting. If `visual_hint` is true, follow up with browser_screenshot."
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
            let read = self
                .ctx
                .lazy
                .get()
                .await?
                .read_page()
                .await
                .map_err(|e| e.to_string())?;
            let body = serde_json::to_string_pretty(&read).map_err(|e| e.to_string())?;
            Ok(active_result(body))
        }
        #[cfg(not(feature = "legacy-chromium"))]
        {
            Err(super::browser_host_unavailable())
        }
    }
}

pub struct BrowserScreenshotTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }
    fn description(&self) -> &str {
        "Capture a PNG screenshot of the current page. Use for visual pages (canvas/video/maps) or when browser_read_page is insufficient. Pair with browser_click x/y to act on what you see."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "full_page": { "type": "boolean", "default": false } }
        })
    }
    fn requires_permission(&self) -> bool {
        // Read-only perception; never mutates page state.
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        #[cfg(feature = "legacy-chromium")]
        {
            let full = args
                .get("full_page")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let b64 = self
                .ctx
                .lazy
                .get()
                .await?
                .screenshot(full)
                .await
                .map_err(|e| e.to_string())?;
            Ok(AgentToolResult {
                content: vec![
                    Content::text("screenshot:"),
                    Content::Image {
                        data: b64,
                        mime_type: "image/png".to_string(),
                    },
                ],
                details: Value::Null,
                terminate: false,
                side_effects: vec![ToolSideEffect::BrowserActivity { active: true }],
            })
        }
        #[cfg(not(feature = "legacy-chromium"))]
        {
            let _ = args;
            Err(super::browser_host_unavailable())
        }
    }
}
