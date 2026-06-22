use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::types::{AgentTool, AgentToolResult};

pub struct BashTool {
    cwd: Option<PathBuf>,
}

impl BashTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
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
        let fut = command.output();
        let output = match timeout(Duration::from_millis(timeout_ms), fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("spawn: {e}")),
            Err(_) => return Err(format!("command timed out after {timeout_ms}ms")),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let code = output.status.code().unwrap_or(-1);
        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str("[stderr]\n");
            combined.push_str(&stderr);
        }
        combined.push_str(&format!("\n[exit {code}]"));
        Ok(AgentToolResult::text(combined))
    }
}
