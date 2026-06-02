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

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{timeout_at, Duration, Instant};

use crate::jsonrpc::{Incoming, Notification, Request};
use crate::transport::Transport;

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

/// Result of a `tools/call`: flattened content text plus the error flag.
#[derive(Debug, Clone)]
pub struct McpCallResult {
    /// Concatenated text of all `text` content blocks. Non-text blocks (image,
    /// audio, resource) are summarized with a placeholder line.
    pub text: String,
    /// The server's `isError` flag (tool-execution error, distinct from a
    /// protocol error which surfaces as `Err`).
    pub is_error: bool,
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
}

impl McpClient {
    pub fn new(transport: Box<dyn Transport + Send>) -> Self {
        Self {
            transport,
            next_id: 1,
            request_timeout: Duration::from_secs(30),
        }
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

        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| flatten_content(blocks))
            .unwrap_or_default();

        Ok(McpCallResult { text, is_error })
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
                // Server-initiated notification (e.g. tools/list_changed). Not
                // handled yet; log and keep waiting for our response.
                if let Some(m) = &msg.method {
                    tracing::debug!(notification = %m, "ignoring MCP server notification");
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

/// Flatten MCP content blocks into a single string for the agent transcript.
/// Text blocks are concatenated; other kinds get a short placeholder so the
/// model knows non-text content came back.
fn flatten_content(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
            Some(other) => parts.push(format!("[{other} content omitted]")),
            None => {}
        }
    }
    parts.join("\n")
}
