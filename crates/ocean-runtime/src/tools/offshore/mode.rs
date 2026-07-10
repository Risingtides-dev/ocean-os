//! The local offshore reroute toggle: a flag file at
//! `~/.config/offshore/mode`, read by spawn hooks on every agent spawn to
//! decide whether new work is rerouted to the offshore box. Purely local —
//! this tool never touches the remote daemon.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::json_text;
use crate::types::{AgentTool, AgentToolResult};

pub struct OffshoreModeTool {
    /// `~/.config/offshore/mode`, resolved from `$HOME` at construction.
    /// `None` when `$HOME` is unset (surfaced as an error at execute time).
    mode_file: Option<PathBuf>,
}

impl OffshoreModeTool {
    pub fn new() -> Self {
        Self {
            mode_file: std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".config/offshore/mode")),
        }
    }

    /// Test seam: point the tool at a scratch file instead of the user's
    /// real `~/.config`.
    #[cfg(test)]
    fn with_mode_file(path: PathBuf) -> Self {
        Self {
            mode_file: Some(path),
        }
    }
}

impl Default for OffshoreModeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for OffshoreModeTool {
    fn name(&self) -> &str {
        "offshore_mode"
    }
    fn description(&self) -> &str {
        "Read or set the LOCAL offshore reroute mode — a flag file (~/.config/offshore/mode) that spawn hooks read to decide whether new agent work is rerouted to the offshore box. No args returns the current mode; state \"on\"/\"off\" writes it first. Never touches the remote daemon."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": { "type": "string", "enum": ["on", "off"], "description": "New mode; omit to just read the current one" }
            }
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let Some(mode_file) = &self.mode_file else {
            return Err("cannot resolve the mode file: $HOME is not set".into());
        };
        if let Some(state) = args.get("state").and_then(|v| v.as_str()) {
            if state != "on" && state != "off" {
                return Err(format!(
                    "invalid 'state' '{state}': expected \"on\" or \"off\""
                ));
            }
            if let Some(parent) = mode_file.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            tokio::fs::write(mode_file, format!("{state}\n"))
                .await
                .map_err(|e| format!("writing {}: {e}", mode_file.display()))?;
        }
        let current = match tokio::fs::read_to_string(mode_file).await {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    "off".to_string()
                } else {
                    trimmed.to_string()
                }
            }
            // Never written → the toggle is off, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "off".to_string(),
            Err(e) => return Err(format!("reading {}: {e}", mode_file.display())),
        };
        Ok(json_text(&json!({
            "mode": current,
            "file": mode_file.display().to_string(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_mode_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "ocean-offshore-mode-{name}-{}",
                uuid::Uuid::new_v4()
            ))
            .join("mode")
    }

    #[tokio::test]
    async fn unset_mode_reads_off() {
        let file = scratch_mode_file("unset");
        let tool = OffshoreModeTool::with_mode_file(file.clone());
        let res = tool.execute("t", json!({})).await.unwrap();
        let text = res.content[0].as_text().unwrap();
        assert!(text.contains("\"mode\": \"off\""), "{text}");
        assert!(!file.exists(), "a bare read must not create the file");
    }

    #[tokio::test]
    async fn setting_state_persists_and_reads_back() {
        let file = scratch_mode_file("roundtrip");
        let tool = OffshoreModeTool::with_mode_file(file.clone());

        let res = tool.execute("t", json!({ "state": "on" })).await.unwrap();
        assert!(res.content[0]
            .as_text()
            .unwrap()
            .contains("\"mode\": \"on\""));
        // The file carries a trailing newline, like the Python harness writes.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "on\n");

        let res = tool.execute("t", json!({ "state": "off" })).await.unwrap();
        assert!(res.content[0]
            .as_text()
            .unwrap()
            .contains("\"mode\": \"off\""));

        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn invalid_state_is_rejected_without_writing() {
        let file = scratch_mode_file("invalid");
        let tool = OffshoreModeTool::with_mode_file(file.clone());
        let err = tool
            .execute("t", json!({ "state": "maybe" }))
            .await
            .expect_err("only on/off are valid");
        assert!(err.contains("expected \"on\" or \"off\""), "{err}");
        assert!(!file.exists(), "invalid state must not touch the file");
    }
}
