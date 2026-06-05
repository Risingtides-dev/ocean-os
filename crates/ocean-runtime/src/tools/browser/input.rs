use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserClickTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click an element. Provide `ref` (a selector from browser_read_page) for precise clicks, OR `x`/`y` viewport coordinates for visual pages."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "CSS selector from browser_read_page" },
                "x": { "type": "number" },
                "y": { "type": "number" }
            }
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
            self.ctx
                .lazy
                .get()
                .await?
                .click_selector(r)
                .await
                .map_err(|e| e.to_string())?;
            Ok(active_result(format!("clicked {r}")))
        } else if let (Some(x), Some(y)) = (
            args.get("x").and_then(|v| v.as_f64()),
            args.get("y").and_then(|v| v.as_f64()),
        ) {
            self.ctx
                .lazy
                .get()
                .await?
                .click_xy(x, y)
                .await
                .map_err(|e| e.to_string())?;
            Ok(active_result(format!("clicked ({x},{y})")))
        } else {
            Err("provide either 'ref' or both 'x' and 'y'".to_string())
        }
    }
}

pub struct BrowserTypeTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserTypeTool {
    fn name(&self) -> &str {
        "browser_type"
    }
    fn description(&self) -> &str {
        "Type text into the currently focused element (real keystrokes). Click an input first."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or("missing 'text'")?;
        self.ctx
            .lazy
            .get()
            .await?
            .type_text(text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("typed {} chars", text.len())))
    }
}

pub struct BrowserKeyTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserKeyTool {
    fn name(&self) -> &str {
        "browser_key"
    }
    fn description(&self) -> &str {
        "Press a key, e.g. 'Enter', 'Tab', 'Backspace', 'ArrowDown'."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("missing 'key'")?;
        self.ctx
            .lazy
            .get()
            .await?
            .press_key(key)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("pressed {key}")))
    }
}

pub struct BrowserScrollTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserScrollTool {
    fn name(&self) -> &str {
        "browser_scroll"
    }
    fn description(&self) -> &str {
        "Scroll the page by a pixel delta (dx, dy). Positive dy scrolls down."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "dx": { "type": "number", "default": 0 }, "dy": { "type": "number", "default": 600 } }
        })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let dx = args.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dy = args.get("dy").and_then(|v| v.as_f64()).unwrap_or(600.0);
        self.ctx
            .lazy
            .get()
            .await?
            .scroll_by(dx, dy)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("scrolled ({dx},{dy})")))
    }
}
