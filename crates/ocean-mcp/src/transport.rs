//! Transport abstraction for MCP. One trait, one implementation today
//! ([`StdioTransport`]); HTTP/SSE slots in behind the same trait later without
//! touching the client.
//!
//! The contract is line-oriented JSON: `send` writes exactly one JSON value as
//! a single newline-terminated line; `recv` returns the next complete line.
//! This matches the MCP stdio framing (newline-delimited, no embedded newlines).

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

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

/// HTTP/SSE transport. The wire protocol (streamable HTTP) is not yet built out;
/// what *is* implemented today is the connect contract: probe the endpoint and
/// **fail fast** with [`McpTransportError::HttpConnectionFailed`] if it cannot be
/// reached, rather than logging a warning and leaving the provider silently
/// toolless. Returning a typed error lets `McpProvider::connect` surface the
/// failure explicitly (OCEAN-47).
#[derive(Debug)]
pub struct HttpTransport {
    /// The configured endpoint URL. Retained for when the streamable-HTTP wire
    /// is built out; today `connect` fails fast before constructing the struct.
    #[allow(dead_code)]
    endpoint: String,
}

impl HttpTransport {
    /// Attempt to connect to `endpoint`. Fails fast: any reachability problem
    /// (and, until the streamable-HTTP wire is finished, the not-yet-supported
    /// state itself) returns [`McpTransportError::HttpConnectionFailed`] rather
    /// than a swallowed warning.
    pub async fn connect(endpoint: &str) -> Result<Self, McpTransportError> {
        if endpoint.trim().is_empty() {
            return Err(McpTransportError::HttpConnectionFailed {
                endpoint: endpoint.to_string(),
                reason: "no endpoint URL configured".to_string(),
            });
        }

        // The streamable-HTTP MCP wire is not implemented yet. Fail fast with a
        // typed connection error so callers don't mistake "unsupported" for
        // "connected": a half-built transport that pretends to connect is worse
        // than one that refuses loudly.
        Err(McpTransportError::HttpConnectionFailed {
            endpoint: endpoint.to_string(),
            reason: "HTTP MCP transport is not yet implemented".to_string(),
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&mut self, _json_line: &str) -> Result<()> {
        bail!("MCP HTTP transport is not yet implemented")
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        bail!("MCP HTTP transport is not yet implemented")
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_connect_fails_fast_with_typed_error() {
        let err = HttpTransport::connect("https://example.invalid/mcp")
            .await
            .expect_err("HTTP transport must fail fast, not pretend to connect");
        match err {
            McpTransportError::HttpConnectionFailed { endpoint, .. } => {
                assert_eq!(endpoint, "https://example.invalid/mcp");
            }
        }
        // The Display string is the operator-facing diagnostic.
        let shown = HttpTransport::connect("https://example.invalid/mcp")
            .await
            .unwrap_err()
            .to_string();
        assert!(shown.contains("HTTP connection"), "got: {shown}");
    }

    #[tokio::test]
    async fn http_connect_rejects_empty_endpoint() {
        let err = HttpTransport::connect("   ").await.unwrap_err();
        assert!(err.to_string().contains("no endpoint URL"));
    }
}
