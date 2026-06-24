use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct GrepTool {
    cwd: Option<PathBuf>,
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents under a directory for a fixed substring. Honors .gitignore by default."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Substring to search for"},
                "path": {"type": "string", "description": "Directory to search (default: cwd)"},
                "max_matches": {"type": "integer", "default": 200}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern'")?
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), &path);
        let max = args
            .get("max_matches")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut buf = String::new();
            let mut hits = 0usize;
            let walker = WalkBuilder::new(&path).follow_links(false).build();
            for entry in walker.flatten() {
                if hits >= max {
                    buf.push_str(&format!("... (truncated at {max} matches)\n"));
                    break;
                }
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let text = match fs::read_to_string(p) {
                    Ok(t) => t,
                    Err(_) => continue, // skip binary or unreadable
                };
                for (i, line) in text.lines().enumerate() {
                    if line.contains(&pattern) {
                        buf.push_str(&format!("{}:{}:{}\n", p.display(), i + 1, line));
                        hits += 1;
                        if hits >= max {
                            break;
                        }
                    }
                }
            }
            Ok(buf)
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(AgentToolResult::text(if result.is_empty() {
            "(no matches)".to_string()
        } else {
            result
        }))
    }
}
