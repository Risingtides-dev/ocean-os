use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult};

/// Fetch a URL and return a coarse text representation of the response body.
/// Very intentionally simple: strips HTML tags by removing `<...>` sequences,
/// collapses whitespace, and truncates to a sensible size. For richer parsing,
/// upstream callers can install a custom tool.
pub struct WebFetchTool;

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL via HTTPS and return a text-only excerpt of the response body. Use for documentation pages, GitHub READMEs, status checks, etc."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute URL to fetch"},
                "max_chars": {"type": "integer", "default": 8000}
            },
            "required": ["url"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("missing 'url'")?
            .to_string();
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(8000) as usize;

        let resp = reqwest::Client::new()
            .get(&url)
            .header("user-agent", "pi-coding-agent/1.0")
            .send()
            .await
            .map_err(|e| format!("fetch {url}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let stripped = strip_html(&text);
        let truncated = if stripped.len() > max_chars {
            format!(
                "{}\n...(truncated, {} chars total)",
                &stripped[..max_chars],
                stripped.len()
            )
        } else {
            stripped
        };
        Ok(AgentToolResult::text(format!(
            "GET {url} [{status}]\n{truncated}"
        )))
    }
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_ws = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            c if in_tag => {
                let _ = c;
            }
            c if c.is_whitespace() => {
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            c => {
                out.push(c);
                last_ws = false;
            }
        }
    }
    out.trim().to_string()
}
