//! Stdio transport for plugins, mirroring `ocean-mcp`'s `StdioTransport`.
//!
//! The contract is line-oriented JSON: `send` writes exactly one JSON value as a
//! single newline-terminated line; `recv` returns the next complete line. This
//! is the same framing MCP uses over stdio, reused here so a plugin subprocess is
//! driven identically to an MCP server child — without taking a dependency on
//! `ocean-mcp`.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Hard ceiling on a single plugin message (one JSON line). Plugins are
/// third-party executables; without this bound a buggy or hostile plugin could
/// emit one newline-less multi-gigabyte line and exhaust memory. A message past
/// this cap fails the read and tears the connection down. Matches the 16 MiB cap
/// `ocean-mcp` uses for the same reason.
const MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// A bidirectional line-delimited JSON channel to a plugin process.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Write one JSON line (the impl appends the newline).
    async fn send(&mut self, json_line: &str) -> Result<()>;
    /// Read the next JSON line. `Ok(None)` means the peer closed the stream
    /// (EOF) — i.e. the plugin exited.
    async fn recv(&mut self) -> Result<Option<String>>;
    /// Best-effort shutdown: close stdin, then terminate the child.
    async fn close(&mut self) -> Result<()>;
}

/// stdio transport: spawns the plugin as a child process and speaks
/// newline-delimited JSON over its stdin/stdout. The child's stderr is inherited
/// so its logs surface in the host's terminal (stderr is reserved for plugin
/// logging, never for protocol messages).
pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioTransport {
    /// Spawn `command` with `args` and `env` (each `(name, value)` set on the
    /// child only). The parent environment is inherited so the plugin can see
    /// PATH etc.; the explicit `env` entries are extra variables the host injects.
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
            .with_context(|| format!("spawn plugin `{command}`"))?;

        let stdin = child
            .stdin
            .take()
            .context("plugin child stdin not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("plugin child stdout not captured")?;

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
        // MAX_MESSAGE_BYTES, so a plugin that never sends a newline can't make us
        // allocate without limit.
        let mut buf = Vec::new();
        let n = (&mut self.stdout)
            .take(MAX_MESSAGE_BYTES + 1)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            return Ok(None); // EOF: plugin exited.
        }
        if n as u64 > MAX_MESSAGE_BYTES {
            bail!("plugin message exceeded {MAX_MESSAGE_BYTES} bytes; dropping connection");
        }
        while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
            buf.pop();
        }
        let line = String::from_utf8(buf).context("plugin sent invalid UTF-8")?;
        Ok(Some(line))
    }

    async fn close(&mut self) -> Result<()> {
        // Closing stdin signals the plugin to shut down; kill_on_drop handles the
        // hard stop if it lingers.
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        Ok(())
    }
}
