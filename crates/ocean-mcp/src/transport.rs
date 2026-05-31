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
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self> {
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

        let stdin = child
            .stdin
            .take()
            .context("MCP child stdin not captured")?;
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
