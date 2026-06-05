//! The MCP client: drives one server connection through the lifecycle
//! (`initialize` → `notifications/initialized`), discovers tools
//! (`tools/list`, following pagination), and invokes them (`tools/call`).
//!
//! This is a deliberately small, synchronous-request-over-async-transport
//! client: each call writes a request and reads lines until it sees the
//! matching response id. Server-initiated notifications encountered in between
//! are logged and skipped. That is sufficient for a single-caller-at-a-time
//! provider (the registry calls one server at a time during startup discovery
//! and serialized per turn). It is NOT safe to share one `McpClient` across
//! concurrent callers without external locking — the provider wraps it in a
//! `Mutex` for exactly this reason.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use ocean_protocol::Content;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::time::{timeout_at, Duration, Instant};

use crate::jsonrpc::{Incoming, Notification, Request};
use crate::transport::Transport;

/// The MCP method a server sends to announce its tool list changed. Receiving
/// this invalidates the provider's cached tool snapshot (OCEAN-32).
const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";

/// Protocol version this client speaks. The server echoes the same version if
/// it supports it, or negotiates down; we accept whatever it returns.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A tool as advertised by an MCP server's `tools/list`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments. MCP calls this `inputSchema`.
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// Result of a `tools/call`: the server's content blocks mapped onto Ocean's
/// shared [`Content`] model, plus the error flag.
#[derive(Debug, Clone)]
pub struct McpCallResult {
    /// The result's content blocks. `text` blocks become [`Content::Text`];
    /// `image` blocks are preserved as [`Content::Image`] (base64 + mime) so the
    /// model actually receives the image rather than a dropped placeholder.
    /// Genuinely unsupported kinds (audio, embedded resources) are logged and
    /// rendered as a text placeholder so the model knows content was elided.
    pub content: Vec<Content>,
    /// The server's `isError` flag (tool-execution error, distinct from a
    /// protocol error which surfaces as `Err`).
    pub is_error: bool,
}

impl McpCallResult {
    /// Concatenated text of all text blocks. Used for the error path (the model
    /// surfaces a tool error as a string) and for callers that only want text.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| c.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct McpClient {
    // `+ Send` is required (not implied by the `Send` supertrait): an erased
    // `dyn Transport` does not carry the `Send` auto-trait, which would make the
    // whole client `!Send` and violate the runtime's `CapabilityProvider` /
    // `AgentTool` Send bounds.
    transport: Box<dyn Transport + Send>,
    next_id: u64,
    /// Per-request timeout. Bounds the whole request (not each line), so a
    /// chatty server can't stall a call indefinitely. The spec requires senders
    /// to time out rather than hang forever.
    request_timeout: Duration,
    /// Notified whenever a `tools/list_changed` notification is observed while
    /// draining the request loop. The provider waits on this to trigger a
    /// background re-fetch + atomic swap of its cached tool list (OCEAN-32).
    tools_changed: Arc<Notify>,
}

impl McpClient {
    pub fn new(transport: Box<dyn Transport + Send>) -> Self {
        Self {
            transport,
            next_id: 1,
            request_timeout: Duration::from_secs(30),
            tools_changed: Arc::new(Notify::new()),
        }
    }

    /// A handle that fires each time the server announces `tools/list_changed`.
    /// The provider clones this and spawns a watcher that re-discovers tools.
    pub fn tools_changed_signal(&self) -> Arc<Notify> {
        self.tools_changed.clone()
    }

    /// Override the per-request timeout. (Connect bounds the handshake with its
    /// own outer deadline, so it deliberately does NOT call this — live calls
    /// keep the default. Retained as public API for embedders that want a custom
    /// timeout.)
    #[allow(dead_code)]
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Run the initialize handshake. Returns the server's reported name, when
    /// present. Sends `initialize`, waits for the result, then fires the
    /// `notifications/initialized` notification as required before any other
    /// requests.
    pub async fn initialize(&mut self, client_name: &str) -> Result<String> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self
            .request("initialize", Some(params))
            .await
            .context("MCP initialize")?;

        // Best-effort: pull the server name for logs. Non-fatal if absent.
        let server_name = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Required: tell the server we're ready before issuing more requests.
        self.notify("notifications/initialized", None)
            .await
            .context("MCP initialized notification")?;

        Ok(server_name)
    }

    /// List all tools, following `nextCursor` pagination to completion.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDef>> {
        #[derive(Deserialize)]
        struct ListResult {
            #[serde(default)]
            tools: Vec<McpToolDef>,
            #[serde(default, rename = "nextCursor")]
            next_cursor: Option<String>,
        }

        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let result = self.request("tools/list", params).await?;
            let page: ListResult =
                serde_json::from_value(result).context("parse tools/list result")?;
            all.extend(page.tools);
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(all)
    }

    /// Invoke a tool by its server-side name with JSON arguments.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<McpCallResult> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", Some(params)).await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| map_content(name, blocks))
            .unwrap_or_default();

        Ok(McpCallResult { content, is_error })
    }

    /// Cleanly shut the connection down.
    pub async fn close(&mut self) -> Result<()> {
        self.transport.close().await
    }

    /// Send a request and read lines until the matching response arrives.
    /// Bounded by `request_timeout` for the whole call. Skips and logs any
    /// server notification or mismatched-id message seen in the meantime.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.take_id();
        let req = Request::new(id, method, params);
        let line = serde_json::to_string(&req)?;
        self.transport.send(&line).await?;

        // Absolute deadline for the WHOLE request, computed once. The loop below
        // may read several lines (skipping server notifications / stale ids), so
        // a per-line timeout would let a chatty server stall us indefinitely as
        // long as each individual line arrived in time. `timeout_at` against a
        // fixed instant bounds the total wait regardless of how many interleaved
        // messages arrive.
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let next = timeout_at(deadline, self.transport.recv())
                .await
                .map_err(|_| anyhow!("MCP request `{method}` timed out"))??;
            let Some(raw) = next else {
                return Err(anyhow!(
                    "MCP server closed the connection during `{method}`"
                ));
            };
            let msg: Incoming = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "unparseable MCP message; skipping");
                    continue;
                }
            };
            if !msg.is_response() {
                // Server-initiated notification. `tools/list_changed` invalidates
                // the provider's cached tool snapshot: fire the signal so the
                // provider's watcher re-fetches in the background (OCEAN-32).
                // Other notifications are logged and skipped while we keep
                // waiting for our response.
                if let Some(m) = &msg.method {
                    if m == TOOLS_LIST_CHANGED {
                        tracing::info!(
                            notification = %m,
                            "MCP tools/list_changed received; invalidating cached tool list"
                        );
                        self.tools_changed.notify_one();
                    } else {
                        tracing::debug!(notification = %m, "ignoring MCP server notification");
                    }
                }
                continue;
            }
            if !msg.matches_id(id) {
                tracing::warn!(got = ?msg.id, want = id, "out-of-order MCP response; skipping");
                continue;
            }
            if let Some(err) = msg.error {
                return Err(anyhow!(err));
            }
            return Ok(msg.result.unwrap_or(Value::Null));
        }
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let note = Notification::new(method, params);
        let line = serde_json::to_string(&note)?;
        self.transport.send(&line).await
    }
}

/// Map MCP content blocks onto Ocean's [`Content`] model.
///
/// - `text` → [`Content::Text`].
/// - `image` → [`Content::Image`] (base64 data + mime). Previously these were
///   silently dropped behind an `[image content omitted]` placeholder; now the
///   image actually reaches the model. MCP image blocks are
///   `{ "type": "image", "data": "<base64>", "mimeType": "image/png" }`.
/// - anything else (audio, embedded resources, unknown kinds) is genuinely
///   unsupported by the content model today: it is logged clearly and replaced
///   with a text placeholder so the model knows content was elided rather than
///   the call having returned nothing.
fn map_content(tool_name: &str, blocks: &[Value]) -> Vec<Content> {
    let mut out = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    out.push(Content::text(t));
                }
            }
            Some("image") => match image_block(b) {
                Some(content) => out.push(content),
                None => {
                    tracing::warn!(
                        tool = %tool_name,
                        "MCP image result block missing `data`/`mimeType`; dropping it"
                    );
                    out.push(Content::text("[image content omitted: malformed image block]"));
                }
            },
            Some(other) => {
                // Audio, embedded resources, and unknown kinds have no Content
                // variant today. Log loudly (so an operator can see a real tool
                // is returning content we discard) and leave a breadcrumb for
                // the model instead of silently swallowing it.
                tracing::warn!(
                    tool = %tool_name,
                    kind = %other,
                    "MCP tool returned an unsupported content block; representing it as a placeholder"
                );
                out.push(Content::text(format!(
                    "[{other} content omitted: unsupported by Ocean's content model]"
                )));
            }
            None => {}
        }
    }
    out
}

/// Build a [`Content::Image`] from an MCP `image` block, if it has the required
/// `data` (base64) and `mimeType` fields.
fn image_block(b: &Value) -> Option<Content> {
    let data = b.get("data").and_then(|d| d.as_str())?;
    let mime = b
        .get("mimeType")
        .and_then(|m| m.as_str())
        .unwrap_or("image/png");
    Some(Content::Image {
        data: data.to_string(),
        mime_type: mime.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_blocks_map_to_text_content() {
        let blocks = vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "text", "text": "world" }),
        ];
        let content = map_content("t", &blocks);
        assert_eq!(content.len(), 2);
        assert_eq!(content[0].as_text(), Some("hello"));
        assert_eq!(content[1].as_text(), Some("world"));
    }

    #[test]
    fn image_block_is_preserved_as_image_content() {
        // OCEAN-48: image results must reach the model as a real image, not a
        // dropped placeholder.
        let blocks = vec![json!({
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png"
        })];
        let content = map_content("screenshot", &blocks);
        assert_eq!(content.len(), 1);
        match &content[0] {
            Content::Image { data, mime_type } => {
                assert_eq!(data, "aGVsbG8=");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected image content, got {other:?}"),
        }
    }

    #[test]
    fn image_without_mime_defaults_to_png() {
        let blocks = vec![json!({ "type": "image", "data": "Zm9v" })];
        let content = map_content("t", &blocks);
        match &content[0] {
            Content::Image { mime_type, .. } => assert_eq!(mime_type, "image/png"),
            other => panic!("expected image content, got {other:?}"),
        }
    }

    #[test]
    fn malformed_image_block_becomes_a_placeholder() {
        // Missing `data` → can't build an image; leave a breadcrumb, don't drop
        // silently.
        let blocks = vec![json!({ "type": "image", "mimeType": "image/png" })];
        let content = map_content("t", &blocks);
        assert_eq!(content.len(), 1);
        assert!(content[0]
            .as_text()
            .unwrap()
            .contains("image content omitted"));
    }

    #[test]
    fn unsupported_block_is_logged_placeholder_not_dropped() {
        // OCEAN-48: audio/resource have no Content variant; represent them with a
        // clear placeholder rather than silently dropping the call's output.
        let blocks = vec![
            json!({ "type": "audio", "data": "...", "mimeType": "audio/wav" }),
            json!({ "type": "text", "text": "caption" }),
        ];
        let content = map_content("t", &blocks);
        assert_eq!(content.len(), 2);
        assert!(content[0].as_text().unwrap().contains("audio content omitted"));
        assert_eq!(content[1].as_text(), Some("caption"));
    }

    #[test]
    fn call_result_text_accessor_joins_only_text() {
        let res = McpCallResult {
            content: vec![
                Content::text("a"),
                Content::Image {
                    data: "x".into(),
                    mime_type: "image/png".into(),
                },
                Content::text("b"),
            ],
            is_error: false,
        };
        assert_eq!(res.text(), "a\nb");
    }
}
