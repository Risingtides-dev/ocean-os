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
    fn concurrency(&self) -> crate::types::Concurrency {
        crate::types::Concurrency::Shared
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
        let truncated = truncate_to_budget(stripped, max_chars);
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

/// Truncate `s` to roughly `max_chars` BYTES, appending a "(truncated, N chars
/// total)" note. `max_chars` is a byte budget but `&str[..n]` panics if `n`
/// splits a multibyte UTF-8 char — common on any non-ASCII page — so we walk
/// back to the nearest char boundary at or below the budget.
fn truncate_to_budget(s: String, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s;
    }
    let mut cut = max_chars;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n...(truncated, {} chars total)", &s[..cut], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_never_splits_a_utf8_char() {
        // 20 bytes of 'é' (2 bytes each); a byte budget of 5 lands mid-char.
        // Pre-fix this panicked; now it walks back to byte 4.
        let s = "é".repeat(10);
        let out = truncate_to_budget(s.clone(), 5);
        assert!(out.starts_with("éé"), "kept whole chars: {out}");
        assert!(out.contains("truncated"));
        // under budget → returned untouched
        assert_eq!(truncate_to_budget("hi".to_string(), 100), "hi");
        // ASCII at an exact boundary still works
        assert!(truncate_to_budget("abcdef".to_string(), 3).starts_with("abc"));
    }
}
