//! Rich network **capture** tools — the scraping unlock. Where `browser_network`
//! only sees resource timings after the fact, these tap the live CDP network
//! stream: the agent starts capture, lists every request (url/status/mime), and
//! pulls the actual **response body** (the JSON/HTML an API returned) by id.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

/// Start buffering network responses on the active page. Call this BEFORE the
/// navigation/action whose traffic you want to capture.
pub struct BrowserCaptureNetworkTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserCaptureNetworkTool {
    fn name(&self) -> &str {
        "browser_capture_network"
    }
    fn description(&self) -> &str {
        "Start capturing network responses on the current page (url, status, \
         mime, and request id). Call this BEFORE navigating or triggering the \
         action whose API traffic you want, then use browser_captured_requests \
         and browser_response_body to read what came back. The high-fidelity way \
         to scrape data a page loads over XHR/fetch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max responses to buffer (default 200)" }
            }
        })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let cap = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;
        self.ctx
            .lazy
            .get()
            .await?
            .start_netcap(cap)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!(
            "network capture started (buffering up to {cap} responses)"
        )))
    }
}

/// List the captured responses (metadata only). Find the request you want here,
/// then fetch its body with browser_response_body.
pub struct BrowserCapturedRequestsTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserCapturedRequestsTool {
    fn name(&self) -> &str {
        "browser_captured_requests"
    }
    fn description(&self) -> &str {
        "List network responses captured since browser_capture_network (url, \
         status, mime type, request_id). Use the request_id with \
         browser_response_body to read the actual payload."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let reqs = self.ctx.lazy.get().await?.captured_requests().await;
        let json = serde_json::to_string(&reqs).map_err(|e| e.to_string())?;
        Ok(AgentToolResult::text(json))
    }
}

/// Fetch the response body for a captured request id.
pub struct BrowserResponseBodyTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserResponseBodyTool {
    fn name(&self) -> &str {
        "browser_response_body"
    }
    fn description(&self) -> &str {
        "Fetch the response BODY (the actual JSON/HTML/text payload) for a \
         captured request, by request_id from browser_captured_requests. This is \
         how you read the data an API returned to the page."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string", "description": "request_id from browser_captured_requests" }
            },
            "required": ["request_id"]
        })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let request_id = args
            .get("request_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'request_id'")?;
        let body = self
            .ctx
            .lazy
            .get()
            .await?
            .response_body(request_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(AgentToolResult::text(body))
    }
}
