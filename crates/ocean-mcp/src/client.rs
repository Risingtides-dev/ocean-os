//! The MCP client: drives one server connection through the lifecycle
//! (`initialize` → `notifications/initialized`), discovers tools
//! (`tools/list`, following pagination), and invokes them (`tools/call`).
//!
//! ## Concurrency (OCEAN-44)
//!
//! A single MCP server connection is one duplex line-stream shared across every
//! session that can reach the server's tools. The naive design — wrap the whole
//! client in one `Mutex` and hold it for the entire request round-trip — turns
//! that shared stream into a head-of-line bottleneck: a slow `tools/call` in
//! session A holds the lock and blocks session B's unrelated call for the full
//! duration (up to the 30s per-request timeout).
//!
//! This client removes that blocking by **multiplexing** over the stream. A
//! dedicated background **I/O task** owns the transport and is the only thing
//! that ever touches it — so reads and writes never interleave across callers.
//! Callers don't lock the client across the await:
//!
//! 1. allocate a request id (atomic counter, no lock),
//! 2. register a `oneshot` waiter under a *brief* registry lock,
//! 3. hand the framed line to the I/O task over an mpsc channel (the task does
//!    the actual `send`, so writes are serialized without callers blocking each
//!    other),
//! 4. await the `oneshot` — lock-free — bounded by the per-request timeout.
//!
//! The I/O task reads each inbound line and routes responses to the matching
//! waiter by id; `tools/list_changed` notifications fire the `tools_changed`
//! signal; everything else is logged and dropped. Because awaiting the response
//! holds no lock, two concurrent `call_tool`s on the same server now overlap
//! instead of serializing — a slow A no longer blocks B.
//!
//! All public methods therefore take `&self`: the provider no longer needs an
//! outer `Mutex<McpClient>`, just an `Arc<McpClient>`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use ocean_protocol::Content;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{timeout, Duration};

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

/// The outcome the I/O task routes back to a waiting request: either the
/// JSON-RPC `result` (or `Null`) or an error string lifted from the response's
/// `error` object.
type ResponseSlot = oneshot::Sender<Result<Value, String>>;

/// Pending-response registry: request id → its waiter. Locked only briefly to
/// insert/remove — never across the await of the response itself.
type Pending = Arc<Mutex<HashMap<u64, ResponseSlot>>>;

/// One line queued for the I/O task to write. An optional `ack` lets `notify()`
/// surface a transport write error (a request, by contrast, waits on its
/// response slot, so it leaves `ack` empty).
struct Outgoing {
    line: String,
    ack: Option<oneshot::Sender<Result<()>>>,
}

pub struct McpClient {
    /// Channel into the I/O task: every outbound line goes through here so the
    /// task — the sole owner of the transport — performs the actual write. This
    /// serializes writes without forcing callers to hold a lock across their
    /// response wait.
    outbound: mpsc::UnboundedSender<Outgoing>,
    /// Monotonic request id source. Atomic so `request()` needs no lock to
    /// allocate an id.
    next_id: AtomicU64,
    /// Pending responses keyed by request id. The I/O task removes and fires the
    /// matching `oneshot` when a response arrives.
    pending: Pending,
    /// Per-request timeout. Bounds the whole request, so a slow or silent server
    /// can't make a caller wait forever. The spec requires senders to time out
    /// rather than hang.
    request_timeout: Duration,
    /// Notified whenever a `tools/list_changed` notification is observed by the
    /// I/O task. The provider waits on this to trigger a background re-fetch +
    /// atomic swap of its cached tool list (OCEAN-32).
    tools_changed: Arc<Notify>,
}

impl McpClient {
    pub fn new(transport: Box<dyn Transport + Send>) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let tools_changed = Arc::new(Notify::new());
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Outgoing>();

        // The I/O task owns the transport outright and is the *only* thing that
        // touches it — so reads and writes never interleave across callers.
        spawn_io_task(
            transport,
            outbound_rx,
            pending.clone(),
            tools_changed.clone(),
        );

        Self {
            outbound: outbound_tx,
            next_id: AtomicU64::new(1),
            pending,
            request_timeout: Duration::from_secs(30),
            tools_changed,
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

    fn take_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Run the initialize handshake. Returns the server's reported name, when
    /// present. Sends `initialize`, waits for the result, then fires the
    /// `notifications/initialized` notification as required before any other
    /// requests.
    pub async fn initialize(&self, client_name: &str) -> Result<String> {
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
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
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
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpCallResult> {
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

    /// Best-effort shutdown signal. The real teardown happens when the last
    /// `McpClient` is dropped: that closes the `outbound` channel, the I/O task
    /// observes the close and shuts the transport down. Retained as an awaitable
    /// API for callers that previously relied on `close()`.
    pub async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Send a request and await its response, multiplexed over the shared stream.
    ///
    /// Registers a response slot, hands the framed line to the I/O task, then
    /// awaits the slot bounded by `request_timeout`. Crucially, **no lock is held
    /// across the await** — concurrent requests from other sessions proceed in
    /// parallel rather than queuing behind this one (OCEAN-44).
    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.take_id();
        let req = Request::new(id, method, params);
        let line = serde_json::to_string(&req)?;

        // Register the waiter BEFORE sending so a fast server can't answer before
        // we're listening for it.
        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        self.pending.lock().await.insert(id, tx);

        // Hand the line to the I/O task to write. If the channel is closed the
        // I/O task is gone (server exited / transport dropped).
        if self.outbound.send(Outgoing { line, ack: None }).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("MCP request `{method}` failed: connection closed"));
        }

        // Await the response with no lock held. On timeout, clean up the slot so
        // a late response doesn't leak an entry.
        match timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_err))) => Err(anyhow!(rpc_err)),
            Ok(Err(_recv)) => {
                // I/O task dropped the sender → connection closed mid-flight.
                Err(anyhow!(
                    "MCP server closed the connection during `{method}`"
                ))
            }
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("MCP request `{method}` timed out"))
            }
        }
    }

    /// Fire a notification (no id, no response expected). Still routed through
    /// the I/O task so it can't interleave with a concurrent request's write;
    /// awaits the write ack so a transport error still surfaces.
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let note = Notification::new(method, params);
        let line = serde_json::to_string(&note)?;
        let (ack_tx, ack_rx) = oneshot::channel::<Result<()>>();
        self.outbound
            .send(Outgoing {
                line,
                ack: Some(ack_tx),
            })
            .map_err(|_| anyhow!("MCP notify `{method}` failed: connection closed"))?;
        ack_rx
            .await
            .map_err(|_| anyhow!("MCP notify `{method}` failed: connection closed"))?
    }
}

/// Spawn the single I/O task that owns the transport. It drives both directions:
/// drains the outbound channel to write framed lines, and reads inbound lines to
/// route responses to waiters / fire the `tools_changed` signal. Owning the
/// transport in one task is what makes concurrent callers safe without a lock
/// held across the round-trip (OCEAN-44).
fn spawn_io_task(
    mut transport: Box<dyn Transport + Send>,
    mut outbound: mpsc::UnboundedReceiver<Outgoing>,
    pending: Pending,
    tools_changed: Arc<Notify>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Outbound: a caller queued a line to write.
                maybe_out = outbound.recv() => {
                    let Some(out) = maybe_out else {
                        // All client handles dropped: shut the connection down.
                        let _ = transport.close().await;
                        break;
                    };
                    let res = transport.send(&out.line).await;
                    match out.ack {
                        Some(ack) => {
                            let _ = ack.send(res);
                        }
                        None => {
                            if let Err(e) = res {
                                // A request's write failed. The line is opaque
                                // here, so we can't map it back to a specific id;
                                // log it and let the waiting request surface a
                                // timeout. Rare — stdin writes fail only on a dead
                                // child, which also closes recv and ends the task.
                                tracing::warn!(error = %e, "MCP transport write failed");
                            }
                        }
                    }
                }

                // Inbound: the server sent a line.
                read = transport.recv() => {
                    match read {
                        Ok(Some(raw)) => {
                            route_inbound(&raw, &pending, &tools_changed).await;
                        }
                        Ok(None) => {
                            // EOF: server exited. Fail every pending waiter so
                            // in-flight requests return promptly instead of
                            // waiting out their timeout.
                            fail_all_pending(&pending).await;
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "MCP transport read failed; closing connection");
                            fail_all_pending(&pending).await;
                            let _ = transport.close().await;
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Parse one inbound line and route it: a response goes to its waiter by id; a
/// `tools/list_changed` notification fires the signal; anything else is logged.
async fn route_inbound(raw: &str, pending: &Pending, tools_changed: &Arc<Notify>) {
    let msg: Incoming = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "unparseable MCP message; skipping");
            return;
        }
    };

    if !msg.is_response() {
        // Server-initiated notification. `tools/list_changed` invalidates the
        // provider's cached tool snapshot (OCEAN-32).
        //
        // Scope: notifications are logged and dropped — there is no dispatch
        // table beyond the single `tools/list_changed` case. Any future MCP
        // server emitting other notifications (progress, resource updates,
        // log messages, etc.) will be silently dropped here. Upgrade path: a
        // `broadcast::channel` for async notifications that subscribers can
        // observe, replacing this single-signal `Notify`. The `trace!` below
        // keeps the drop visible when debugging until then.
        if let Some(m) = &msg.method {
            if m == TOOLS_LIST_CHANGED {
                tracing::info!(
                    notification = %m,
                    "MCP tools/list_changed received; invalidating cached tool list"
                );
                tools_changed.notify_one();
            } else {
                tracing::trace!(
                    notification = %m,
                    "dropping unhandled MCP server notification (no dispatch table)"
                );
            }
        }
        return;
    }

    // A response: pull the matching waiter by id and deliver. MCP ids are the
    // numbers we sent (string-encoded ids accepted via `response_id`).
    let Some(id) = response_id(&msg) else {
        tracing::warn!(got = ?msg.id, "MCP response with unusable id; skipping");
        return;
    };
    let slot = pending.lock().await.remove(&id);
    match slot {
        Some(tx) => {
            let payload = if let Some(err) = msg.error {
                Err(err.to_string())
            } else {
                Ok(msg.result.unwrap_or(Value::Null))
            };
            // If the receiver is gone (caller timed out), this just drops.
            let _ = tx.send(payload);
        }
        None => {
            tracing::warn!(id, "out-of-order or stale MCP response; no waiter");
        }
    }
}

/// Extract the numeric id from a response, accepting a string-encoded number
/// (some servers echo ids as strings).
fn response_id(msg: &Incoming) -> Option<u64> {
    match &msg.id {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// Fail every still-pending request so callers don't wait out their full timeout
/// when the connection drops. Dropping each `oneshot` sender (via `clear`) makes
/// the waiting `request()` see a recv error and return "connection closed".
async fn fail_all_pending(pending: &Pending) {
    pending.lock().await.clear();
}

/// Total bytes of TEXT content retained per MCP tool call. Servers are
/// third-party processes — a rogue or verbose one can return a response of any
/// size, and pre-cap that arrived as one unbounded `String` in daemon RAM (the
/// transcript cap only bounds what the MODEL re-reads, not what the daemon
/// holds or ships over SSE). Text past the cap is dropped with a loud marker;
/// 2 MiB matches the bash/web_fetch output caps.
const MAX_TEXT_BYTES_PER_CALL: usize = 2 * 1024 * 1024;

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
    let mut text_budget = MAX_TEXT_BYTES_PER_CALL;
    let mut capped = false;
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if capped {
                        continue; // budget already spent; the marker was pushed
                    }
                    if t.len() > text_budget {
                        // Cut on a char boundary at the remaining budget.
                        let mut cut = text_budget;
                        while cut > 0 && !t.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        out.push(Content::text(&t[..cut]));
                        out.push(Content::text(format!(
                            "[MCP result capped at {MAX_TEXT_BYTES_PER_CALL} bytes of text; \
                             remainder dropped]"
                        )));
                        tracing::warn!(
                            tool = %tool_name,
                            "MCP tool text result exceeded the per-call cap; truncated"
                        );
                        capped = true;
                        continue;
                    }
                    text_budget -= t.len();
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
                    out.push(Content::text(
                        "[image content omitted: malformed image block]",
                    ));
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
    fn oversized_text_result_is_capped_with_marker() {
        // One giant block + one that would follow it: the giant is cut at the
        // budget, a loud marker is appended, and later text is dropped.
        let giant = "x".repeat(MAX_TEXT_BYTES_PER_CALL + 1024);
        let blocks = vec![
            json!({ "type": "text", "text": giant }),
            json!({ "type": "text", "text": "after" }),
        ];
        let content = map_content("t", &blocks);
        let total: usize = content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.len())
            .sum();
        assert!(
            total < MAX_TEXT_BYTES_PER_CALL + 256,
            "retained text must stay near the cap, got {total}"
        );
        assert!(
            content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t.contains("MCP result capped")),
            "cap marker present"
        );
        assert!(
            !content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t == "after"),
            "text after the cap is dropped"
        );
    }

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
        assert!(content[0]
            .as_text()
            .unwrap()
            .contains("audio content omitted"));
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
