//! Browser **shell** tools — tab control. These give the agent the Layer-3
//! jump: instead of driving one heuristically-chosen tab, it can enumerate
//! every tab, address a specific one, and open/switch/close. Thin wrappers over
//! `ocean_browser::BrowserHandle`'s shell methods.

use async_trait::async_trait;
use ocean_browser::TabId;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

/// Read-only: list every open tab with its id/url/title/active flag. This is
/// the agent's "what's open across the whole browser right now" — no permission
/// needed, and it doesn't launch Chrome unless one is already running… (it does
/// `.get()` which launches; listing tabs is a real browser action, unlike
/// listing *tools*). Returns the structured BrowserContext as JSON. Because it
/// round-trips to the live browser (CDP) it emits `BrowserActivity` for the
/// side-panel handoff, even though it's read-only and permission-free.
pub struct BrowserListTabsTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserListTabsTool {
    fn name(&self) -> &str {
        "browser_list_tabs"
    }
    fn description(&self) -> &str {
        "List every open browser tab (id, url, title, which is active). Use this \
         to see what's open across the whole browser and to get a tab id before \
         switching or closing a specific tab."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let ctx = self
            .ctx
            .lazy
            .get()
            .await?
            .list_tabs()
            .await
            .map_err(|e| e.to_string())?;
        let json = serde_json::to_string(&ctx).map_err(|e| e.to_string())?;
        // Live browser action: enumerating tabs round-trips to CDP
        // (snapshot_pages + per-page url/title), so it engages the live browser
        // and must flag activity for the side-panel handoff.
        Ok(active_result(json))
    }
}

/// Open a new tab at a URL and focus it. Returns the new tab's id.
pub struct BrowserOpenTabTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserOpenTabTool {
    fn name(&self) -> &str {
        "browser_open_tab"
    }
    fn description(&self) -> &str {
        "Open a NEW browser tab at a URL and make it the active tab. Returns the \
         new tab's id. Use this to work across multiple tabs instead of \
         replacing the current page."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string", "description": "Absolute URL to open" } },
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
        let id = self
            .ctx
            .lazy
            .get()
            .await?
            .open_tab(url)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("opened tab {id} at {url}")))
    }
}

/// Switch the agent's focus to an existing tab by id (and bring it to front).
pub struct BrowserSwitchTabTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserSwitchTabTool {
    fn name(&self) -> &str {
        "browser_switch_tab"
    }
    fn description(&self) -> &str {
        "Switch to an existing tab by id (from browser_list_tabs) and bring it to \
         the front. Subsequent page tools (read/click/type) then act on this tab."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "tab_id": { "type": "string", "description": "Tab id from browser_list_tabs" } },
            "required": ["tab_id"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let tab_id = args
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'tab_id'")?;
        self.ctx
            .lazy
            .get()
            .await?
            .switch_tab(&TabId(tab_id.to_string()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("switched to tab {tab_id}")))
    }
}

/// Close a tab by id.
pub struct BrowserCloseTabTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserCloseTabTool {
    fn name(&self) -> &str {
        "browser_close_tab"
    }
    fn description(&self) -> &str {
        "Close a browser tab by id (from browser_list_tabs)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "tab_id": { "type": "string", "description": "Tab id from browser_list_tabs" } },
            "required": ["tab_id"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let tab_id = args
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'tab_id'")?;
        self.ctx
            .lazy
            .get()
            .await?
            .close_tab(&TabId(tab_id.to_string()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("closed tab {tab_id}")))
    }
}
