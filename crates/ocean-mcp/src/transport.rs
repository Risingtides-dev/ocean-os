//! Transport abstraction for MCP. One trait, two implementations:
//! [`StdioTransport`] (spawn a child, speak newline-delimited JSON over its
//! stdio) and [`HttpTransport`] (the MCP streamable-HTTP transport). Both sit
//! behind the same trait so [`McpClient`](crate::client::McpClient) drives
//! either unchanged.
//!
//! The contract is line-oriented JSON: `send` writes exactly one JSON value;
//! `recv` returns the next complete JSON message. For stdio this is literal
//! newline framing; for HTTP each POST round-trip queues the JSON-RPC messages
//! from its response for `recv` to drain (see [`HttpTransport`]).

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex};

/// Transport-level failures, modeled explicitly so callers can fail fast on a
/// connection problem instead of swallowing it as a generic warning. Today the
/// only non-stdio transport is HTTP, whose connect path is the motivating case:
/// a server we cannot reach must produce a hard error, not a logged shrug that
/// leaves the provider quietly toolless.
///
/// Implemented by hand (rather than via `thiserror`) so the crate keeps its
/// lean dependency set — there is exactly one variant today.
#[derive(Debug)]
pub enum McpTransportError {
    /// The HTTP transport could not establish a connection to the configured
    /// endpoint (DNS, refused, TLS, timeout, or — until the HTTP transport is
    /// fully built out — that it is not yet reachable). Carries the endpoint and
    /// the underlying reason for diagnostics.
    HttpConnectionFailed { endpoint: String, reason: String },
}

impl std::fmt::Display for McpTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpTransportError::HttpConnectionFailed { endpoint, reason } => {
                write!(f, "MCP HTTP connection to `{endpoint}` failed: {reason}")
            }
        }
    }
}

impl std::error::Error for McpTransportError {}

/// Hard ceiling on a single MCP message (one JSON line). MCP messages are
/// small — a tool result is the largest, and the agent loop caps those at 32 KB
/// downstream anyway. Without this bound, a buggy or hostile server (these are
/// third-party `npx` processes) could emit one newline-less multi-gigabyte line
/// and OOM the whole daemon, which is shared across every session. A message
/// past this cap fails the read, which folds into the non-fatal provider path.
const MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// A bidirectional line-delimited JSON channel to an MCP server.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Write one JSON line (the impl appends the newline).
    async fn send(&mut self, json_line: &str) -> Result<()>;
    /// Read the next JSON line. `Ok(None)` means the peer closed the stream
    /// (EOF) — i.e. the server exited.
    async fn recv(&mut self) -> Result<Option<String>>;
    /// Best-effort shutdown. For stdio: close stdin, then terminate the child.
    async fn close(&mut self) -> Result<()>;
}

/// stdio transport: spawns the MCP server as a child process and speaks
/// newline-delimited JSON over its stdin/stdout. The child's stderr is
/// inherited so its logs surface in the daemon's terminal (the MCP spec
/// reserves stderr for server logging).
pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioTransport {
    /// Spawn `command` with `args` and `env` (each `(name, value)` set on the
    /// child only). The parent environment is inherited so the server can see
    /// PATH etc.; the explicit `env` entries are the secrets resolved by name
    /// from the daemon's process env — they are NOT logged here.
    pub fn spawn(command: &str, args: &[String], env: &[(String, String)]) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn MCP server `{command}`"))?;

        let stdin = child.stdin.take().context("MCP child stdin not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP child stdout not captured")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&mut self, json_line: &str) -> Result<()> {
        self.stdin.write_all(json_line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        // Bounded read: read up to one newline, but never more than
        // MAX_MESSAGE_BYTES, so a server that never sends a newline can't make
        // us allocate without limit. `take` caps the bytes the reader will yield.
        let mut buf = Vec::new();
        let n = (&mut self.stdout)
            .take(MAX_MESSAGE_BYTES + 1)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            return Ok(None); // EOF: server exited.
        }
        // If we hit the cap without seeing a newline, the message is oversized
        // (or the server is wedged producing one). Fail rather than keep going —
        // the provider folds this into its unavailable path.
        if n as u64 > MAX_MESSAGE_BYTES {
            bail!("MCP server message exceeded {MAX_MESSAGE_BYTES} bytes; dropping connection");
        }
        // Trim the trailing newline (and any CR) before handing back.
        while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
            buf.pop();
        }
        let line = String::from_utf8(buf).context("MCP server sent invalid UTF-8")?;
        Ok(Some(line))
    }

    async fn close(&mut self) -> Result<()> {
        // Closing stdin signals the server to shut down (per the MCP stdio
        // lifecycle). kill_on_drop handles the hard stop if it lingers.
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        Ok(())
    }
}

/// Streamable-HTTP transport (MCP spec `2025-06-18`, the "Streamable HTTP"
/// transport). It speaks the same line-oriented JSON contract as
/// [`StdioTransport`] so [`McpClient`](crate::client::McpClient) drives it
/// unchanged: `send` writes one JSON-RPC message, `recv` yields the next.
///
/// ## How the line contract maps onto HTTP
///
/// The wire is request/response, not a long-lived duplex pipe, so we bridge it
/// to the trait's streaming shape with an internal inbound queue:
///
/// - `send(line)` HTTP-POSTs the JSON-RPC message to the endpoint with
///   `Accept: application/json, text/event-stream`. Per spec the server answers
///   a request with **either** a single `application/json` body **or** a
///   `text/event-stream` (SSE) carrying one or more JSON-RPC messages. Either
///   way, every JSON-RPC message in the response is pushed onto the inbound
///   queue. A notification/response we send gets `202 Accepted` with no body —
///   nothing is queued, matching stdio (where a notification draws no reply).
/// - `recv()` pops the next queued message. `Ok(None)` means the transport was
///   closed (the client's I/O task is shutting down).
///
/// ## Session id
///
/// The server **MAY** assign an `Mcp-Session-Id` on the `initialize` response.
/// If it does, we capture it and echo it on every subsequent request, as the
/// spec requires. A `404` to a request carrying a session id means the session
/// was terminated server-side; we surface that as a transport error so the
/// connection is torn down (the provider then contributes no tools rather than
/// hanging).
#[derive(Debug)]
pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
    /// Negotiated session id, set from the `Mcp-Session-Id` response header (if
    /// the server assigns one) and echoed on every later request. Shared so the
    /// `send` task can update it after `initialize` without `&mut`.
    session_id: Arc<Mutex<Option<String>>>,
    /// Inbound JSON-RPC messages parsed out of POST responses (single JSON or
    /// SSE), waiting for `recv` to drain them.
    inbound_rx: mpsc::UnboundedReceiver<String>,
    inbound_tx: mpsc::UnboundedSender<String>,
}

impl HttpTransport {
    /// Construct a streamable-HTTP transport for `endpoint`. Validates the URL is
    /// non-empty and well-formed; a malformed/empty endpoint is a typed
    /// [`McpTransportError::HttpConnectionFailed`] so the provider fails fast
    /// rather than silently contributing no tools. Reachability of the server
    /// itself is proven by the `initialize` handshake the client runs next — a
    /// dead endpoint surfaces there as a POST error, which folds into the
    /// provider's unavailable path.
    pub async fn connect(endpoint: &str) -> Result<Self, McpTransportError> {
        let endpoint = endpoint.trim().to_string();
        if endpoint.is_empty() {
            return Err(McpTransportError::HttpConnectionFailed {
                endpoint,
                reason: "no endpoint URL configured".to_string(),
            });
        }
        // Reject anything that isn't an http(s) URL up front, with the same typed
        // error — a `command`-style value (e.g. `npx`) in an http server entry is
        // a config mistake we want to name loudly, not POST to.
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(McpTransportError::HttpConnectionFailed {
                endpoint: endpoint.clone(),
                reason: "endpoint must be an http:// or https:// URL".to_string(),
            });
        }

        let client = reqwest::Client::builder()
            // No global request timeout here: per-request bounds live in the
            // client (handshake) and `McpClient` (live calls). A streaming SSE
            // response can legitimately stay open longer than a single call.
            .build()
            .map_err(|e| McpTransportError::HttpConnectionFailed {
                endpoint: endpoint.clone(),
                reason: format!("could not build HTTP client: {e}"),
            })?;

        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        Ok(Self {
            client,
            endpoint,
            session_id: Arc::new(Mutex::new(None)),
            inbound_rx,
            inbound_tx,
        })
    }

    /// POST one JSON-RPC line and route every JSON-RPC message in the response
    /// onto the inbound queue. Handles both response shapes the spec permits:
    /// a single `application/json` body, or a `text/event-stream` (SSE) of one
    /// or more messages.
    async fn post_line(&self, line: &str) -> Result<()> {
        let mut req = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .body(line.to_string());

        // Echo the negotiated session id on every request after initialize.
        if let Some(sid) = self.session_id.lock().await.clone() {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("POST to MCP endpoint `{}`", self.endpoint))?;

        let status = resp.status();

        // A terminated session: the client must start a fresh one. We don't
        // auto-reinitialize mid-stream; tearing down is the safe, visible
        // behavior (the provider then contributes no tools rather than wedging).
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!("MCP HTTP session was terminated by the server (404); connection closed");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP HTTP request failed with status {status}: {body}");
        }

        // Capture (or refresh) the session id the server assigned. Per spec this
        // arrives on the initialize response, but accepting it on any response is
        // harmless and robust.
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
        {
            *self.session_id.lock().await = Some(sid);
        }

        // `202 Accepted` (no content) is the spec's answer to a notification or a
        // response we sent — nothing to queue.
        if status == reqwest::StatusCode::ACCEPTED {
            return Ok(());
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            // SSE: each `data:` event is one JSON-RPC message. Stream them onto
            // the inbound queue as they arrive. The stream ends when the server
            // closes it (it does so once it has sent the response to our
            // request, per spec).
            let mut events = resp.bytes_stream().eventsource();
            while let Some(event) = events.next().await {
                let event = event.context("reading MCP SSE event")?;
                let data = event.data;
                if data.trim().is_empty() {
                    continue;
                }
                // Drop only if the client (and its inbound queue) is gone.
                if self.inbound_tx.send(data).is_err() {
                    break;
                }
            }
        } else {
            // Single JSON body (the common case for a stateless server).
            let body = resp
                .text()
                .await
                .context("reading MCP HTTP response body")?;
            if !body.trim().is_empty() {
                let _ = self.inbound_tx.send(body);
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&mut self, json_line: &str) -> Result<()> {
        self.post_line(json_line).await
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        // Yield the next message a prior POST queued. `None` once the channel is
        // empty AND closed — i.e. the transport is being torn down.
        Ok(self.inbound_rx.recv().await)
    }

    async fn close(&mut self) -> Result<()> {
        // Best-effort: tell the server to drop the session (spec: HTTP DELETE
        // with the session id). Failure is ignored — the connection is going
        // away regardless.
        if let Some(sid) = self.session_id.lock().await.clone() {
            let _ = self
                .client
                .delete(&self.endpoint)
                .header("Mcp-Session-Id", sid)
                .send()
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn http_connect_rejects_empty_endpoint() {
        let err = HttpTransport::connect("   ").await.unwrap_err();
        assert!(err.to_string().contains("no endpoint URL"));
    }

    #[tokio::test]
    async fn http_connect_rejects_non_http_endpoint() {
        // A `command`-style value (e.g. `npx`) wrongly placed in an http server
        // entry is a config mistake we name loudly rather than POST to.
        let err = HttpTransport::connect("npx").await.unwrap_err();
        assert!(
            err.to_string().contains("http:// or https://"),
            "got: {err}"
        );
        match err {
            McpTransportError::HttpConnectionFailed { endpoint, .. } => {
                assert_eq!(endpoint, "npx");
            }
        }
    }

    /// A minimal one-request-per-connection HTTP MCP server for tests. It reads
    /// one POSTed JSON-RPC line, dispatches on the method, and replies with a
    /// single `application/json` JSON-RPC body. Enough to exercise the transport
    /// end-to-end without pulling a web framework into this crate.
    async fn spawn_mock_http_mcp() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // Read headers + body. Bodies here are tiny and sent in one
                    // shot, so a single read of the request is sufficient.
                    let mut buf = vec![0u8; 8192];
                    let n = match tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    let parsed: serde_json::Value =
                        serde_json::from_str(body.trim()).unwrap_or(serde_json::Value::Null);
                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = parsed.get("id").cloned();

                    // Notifications (no id) → 202 with no body, per spec.
                    // `Connection: close`: this mock serves one request per accepted
                    // socket, so it must opt out of keep-alive — otherwise reqwest
                    // pools the server-closed connection and reuses it for the next
                    // request, racing the FIN under parallel test load.
                    if id.is_none() {
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                            )
                            .await;
                        return;
                    }

                    let result = match method {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": { "listChanged": true } },
                            "serverInfo": { "name": "mock-http", "version": "0.0.0" }
                        }),
                        "tools/list" => serde_json::json!({
                            "tools": [{
                                "name": "echo",
                                "description": "echoes input",
                                "inputSchema": { "type": "object" }
                            }]
                        }),
                        _ => serde_json::json!({}),
                    };
                    let payload = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    })
                    .to_string();

                    // Assign a session id on the initialize response, as a real
                    // streamable-HTTP server may.
                    let session_hdr = if method == "initialize" {
                        "Mcp-Session-Id: test-session-123\r\n"
                    } else {
                        ""
                    };
                    // `Connection: close` — one request per socket, so opt out of
                    // keep-alive to avoid the client reusing a server-closed
                    // connection (see the 202 branch above).
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n{session_hdr}Content-Length: {}\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn http_transport_sends_and_receives_json_response() {
        let endpoint = spawn_mock_http_mcp().await;
        let mut t = HttpTransport::connect(&endpoint).await.unwrap();

        // initialize round-trip.
        t.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await
            .unwrap();
        let line = t.recv().await.unwrap().expect("a response line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "mock-http");

        // The session id from the initialize response is captured for reuse.
        assert_eq!(
            t.session_id.lock().await.clone(),
            Some("test-session-123".to_string())
        );

        // A notification draws no queued reply (202, no body).
        t.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .unwrap();

        // tools/list round-trip yields the advertised tool.
        t.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
            .await
            .unwrap();
        let line = t.recv().await.unwrap().expect("a tools/list response");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["result"]["tools"][0]["name"], "echo");
    }

    #[tokio::test]
    async fn http_transport_surfaces_404_as_terminated_session() {
        // A server that always 404s a request → the transport reports the session
        // terminated rather than hanging.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let mut t = HttpTransport::connect(&format!("http://{addr}/mcp"))
            .await
            .unwrap();
        let err = t
            .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("terminated"), "got: {err}");
    }
}
