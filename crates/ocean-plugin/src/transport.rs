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
    /// Bytes consumed from stdout that don't yet form a complete line. Persisted
    /// across `recv` calls: `read_until` is NOT cancellation-safe, and the plugin
    /// io-loop drives `recv` inside a `select!` that drops the read future
    /// whenever an outbound send wins. With a local buffer those already-consumed
    /// partial bytes would be lost, tearing the next JSON frame and hanging the
    /// request that owned the response. (Same fix as ocean-mcp #240.)
    pending: Vec<u8>,
}

/// Pop the first complete line (up to and including the next `\n`) out of
/// `pending`, trimming the trailing `\r?\n`. `None` when no newline is buffered
/// yet. Pure over the buffer so the resume-after-cancellation behavior is
/// unit-testable without spawning a process.
fn try_take_line(pending: &mut Vec<u8>) -> Option<Result<String>> {
    let pos = pending.iter().position(|&b| b == b'\n')?;
    let mut line: Vec<u8> = pending.drain(..=pos).collect();
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(String::from_utf8(line).context("plugin sent invalid UTF-8"))
}

/// Spawn the child, retrying a small bounded number of times on `ETXTBSY`
/// (os error 26 — "text file busy"). Under parallel `cargo test`, a sibling
/// process can briefly inherit the write fd of a just-written plugin script, so
/// `exec` races and fails with `ETXTBSY`. The fd is closed promptly, so a short
/// backoff clears it. Any other spawn error (or exhausting retries) is returned
/// as-is, so a genuinely broken plugin still fails fast and gets skipped.
fn spawn_with_etxtbsy_retry(cmd: &mut tokio::process::Command) -> std::io::Result<Child> {
    const MAX_ATTEMPTS: u32 = 3;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);
    let mut attempt = 1;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(26) && attempt < MAX_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
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

        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
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
            pending: Vec::new(),
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
        loop {
            // A complete line already buffered (e.g. the tail of a previous read
            // that also pulled in the start of this one)? Hand it back.
            if let Some(line) = try_take_line(&mut self.pending) {
                return Ok(Some(line?));
            }
            // Bound total buffered bytes BEFORE reading more, so a plugin that
            // never sends a newline can't make us allocate without limit. Checked
            // on `pending` so the cap holds across the partial reads a cancelled
            // recv may have accumulated.
            if self.pending.len() as u64 > MAX_MESSAGE_BYTES {
                bail!("plugin message exceeded {MAX_MESSAGE_BYTES} bytes; dropping connection");
            }
            // Append onto the PERSISTENT buffer. `read_until` is not
            // cancellation-safe, but because it appends into `self.pending`, a
            // drop of this future (the io-loop select! choosing an outbound send)
            // leaves the consumed bytes in place for the next `recv` to resume.
            let cap = (MAX_MESSAGE_BYTES + 1).saturating_sub(self.pending.len() as u64);
            let n = (&mut self.stdout)
                .take(cap)
                .read_until(b'\n', &mut self.pending)
                .await?;
            if n == 0 {
                // EOF. Surface a trailing newline-less line once, then None.
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let mut line = std::mem::take(&mut self.pending);
                while line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(
                    String::from_utf8(line).context("plugin sent invalid UTF-8")?,
                ));
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Closing stdin signals the plugin to shut down; kill_on_drop handles the
        // hard stop if it lingers.
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_take_line_extracts_and_keeps_remainder() {
        let mut buf = b"line1\nline2\n".to_vec();
        assert_eq!(try_take_line(&mut buf).unwrap().unwrap(), "line1");
        assert_eq!(buf, b"line2\n", "remainder kept for the next line");
        assert_eq!(try_take_line(&mut buf).unwrap().unwrap(), "line2");
        assert!(buf.is_empty());
        let mut partial = b"half".to_vec();
        assert!(try_take_line(&mut partial).is_none());
        assert_eq!(partial, b"half", "no newline -> buffer untouched");
        let mut crlf = b"x\r\n".to_vec();
        assert_eq!(try_take_line(&mut crlf).unwrap().unwrap(), "x");
    }

    #[test]
    fn try_take_line_resumes_after_a_partial_read() {
        // The cancellation-recovery contract: a recv() dropped mid-line leaves
        // its consumed bytes in `pending`; the next read appends the rest and the
        // full frame comes back intact (not torn). Same guarantee as ocean-mcp #240.
        let mut pending = Vec::new();
        pending.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"resu");
        assert!(try_take_line(&mut pending).is_none(), "no newline yet");
        pending.extend_from_slice(b"lt\":{}}\n");
        assert_eq!(
            try_take_line(&mut pending).unwrap().unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
            "frame reassembled intact across the split"
        );
    }
}
