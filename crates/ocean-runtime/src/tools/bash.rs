use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::types::{AgentTool, AgentToolResult};

/// Max bytes captured per stream (stdout, stderr). Output beyond the cap is
/// discarded while the command runs to completion — side effects and the exit
/// code are preserved, only the *capture* is bounded, so a chatty build can't
/// balloon daemon memory. The transcript is capped far lower by the loop
/// (`cap_tool_content`); this bound is about process memory, not tokens.
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

pub struct BashTool {
    cwd: Option<PathBuf>,
}

/// Unix shell commands run in their own process group. Dropping an in-flight
/// BashTool future (the runtime's Halt boundary) must terminate descendants as
/// well as the direct `bash` child; Tokio's `kill_on_drop` only targets that one
/// PID. Non-Unix platforms retain direct-child `kill_on_drop` behavior.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = pid;
        Self {
            #[cfg(unix)]
            pgid: pid.and_then(|pid| i32::try_from(pid).ok()),
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: `pgid` is the positive PID returned for the child we
            // spawned after requesting process_group(0). Negating it asks kill
            // to signal that child-owned group. ESRCH is an already-dead group.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

/// Read a child stream to completion, keeping at most `MAX_CAPTURE_BYTES`.
/// Draining continues past the cap (storing nothing) so the child never blocks
/// on a full pipe. Returns the captured bytes and whether the cap was hit.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut stream: R) -> (Vec<u8>, bool) {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if captured.len() < MAX_CAPTURE_BYTES {
                    let take = n.min(MAX_CAPTURE_BYTES - captured.len());
                    captured.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    (captured, truncated)
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn requires_permission(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Run a shell command via `bash -lc <cmd>`. Returns combined stdout/stderr and exit code."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "default": 120000}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let cmd = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or("missing 'command'")?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);

        let mut command = Command::new("bash");
        command.arg("-lc").arg(cmd);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        // stdin is closed, not inherited: a command that prompts interactively
        // (sudo, a pager, `read`) fails fast instead of hanging until timeout.
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        // The child dies with its handle. Without this, a timed-out command —
        // or one whose turn is CANCELLED (the loop drops in-flight tool futures
        // on cancel) — kept running as an orphan forever: `sleep 600` outliving
        // the turn, a hung server surviving the session.
        command.kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| format!("spawn: {e}"))?;
        let mut process_group = ProcessGroupGuard::new(child.id());
        let stdout_pipe = child.stdout.take().expect("stdout piped above");
        let stderr_pipe = child.stderr.take().expect("stderr piped above");

        let work = async {
            // Drain both pipes before reaping the group leader. A descendant
            // can inherit a pipe and then escape the process group; retaining
            // the unreaped leader prevents its PID/PGID from being reused while
            // the guard remains armed and the inherited pipe stays open.
            let (stdout_res, stderr_res) =
                tokio::join!(read_capped(stdout_pipe), read_capped(stderr_pipe));
            let status = child.wait().await;
            (stdout_res, stderr_res, status)
        };
        let ((stdout_bytes, stdout_trunc), (stderr_bytes, stderr_trunc), status) =
            match timeout(Duration::from_millis(timeout_ms), work).await {
                Ok(r) => r,
                // On elapse the process-group guard kills descendants and
                // kill_on_drop also targets the direct child.
                Err(_) => return Err(format!("command timed out after {timeout_ms}ms")),
            };
        let status = match status {
            Ok(status) => {
                // child.wait() succeeded, so this PID/PGID can eventually be
                // reused; never let the guard signal it after this point.
                process_group.disarm();
                status
            }
            Err(error) => return Err(format!("wait: {error}")),
        };

        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
        let code = status.code().unwrap_or(-1);
        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
        }
        if stdout_trunc {
            combined.push_str("\n[stdout capped at 2MiB; the command ran to completion]");
        }
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str("[stderr]\n");
            combined.push_str(&stderr);
        }
        if stderr_trunc {
            combined.push_str("\n[stderr capped at 2MiB; the command ran to completion]");
        }
        combined.push_str(&format!("\n[exit {code}]"));
        Ok(AgentToolResult::text(combined))
    }
}
