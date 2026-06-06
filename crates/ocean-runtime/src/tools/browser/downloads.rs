//! Download tools — Layer-3 shell control. The agent enables downloading to a
//! known dir, triggers one (by clicking a link / navigating to a file), then
//! reads back the completed file path so the downloaded file flows into its
//! hands (e.g. read it, process it, route it onward).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

/// Turn on downloading to a known directory and start tracking downloads.
pub struct BrowserEnableDownloadsTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserEnableDownloadsTool {
    fn name(&self) -> &str {
        "browser_enable_downloads"
    }
    fn description(&self) -> &str {
        "Enable file downloads to a known directory and start tracking them. Call \
         this BEFORE clicking a download link or navigating to a file. Then use \
         browser_downloads to see completed files and their paths."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "dir": { "type": "string", "description": "Absolute directory to save downloads (optional; defaults to an Ocean temp dir)" }
            }
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let dir = args
            .get("dir")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let path = self
            .ctx
            .lazy
            .get()
            .await?
            .enable_downloads(dir)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!(
            "downloads enabled → {}",
            path.display()
        )))
    }
}

/// List tracked downloads with state + resolved file path.
pub struct BrowserDownloadsTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserDownloadsTool {
    fn name(&self) -> &str {
        "browser_downloads"
    }
    fn description(&self) -> &str {
        "List downloads since browser_enable_downloads — each with its state \
         (InProgress/Completed/Canceled), the source url, and the resolved local \
         file path. Use the path of a Completed download to read or process the \
         file."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let items = self.ctx.lazy.get().await?.download_list().await;
        let json = serde_json::to_string(&items).map_err(|e| e.to_string())?;
        // Exempt from BrowserActivity: this reads the in-memory download-tracking
        // buffer (no CDP round-trip). The download was triggered by an earlier
        // action that already flagged activity; polling the list is bookkeeping.
        Ok(AgentToolResult::text(json))
    }
}
