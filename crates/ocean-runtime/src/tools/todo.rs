//! Lightweight in-memory todo list scoped to a bound session (or one unbound
//! agent run). The model can `add`, `complete`, `list`, or `clear` items.
//! Useful for long-horizon tasks.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

use crate::types::{AgentTool, AgentToolResult};

#[derive(Default)]
pub struct TodoTool {
    items: Mutex<Vec<TodoItem>>,
}

#[derive(Clone)]
struct TodoItem {
    /// Optional concise display label supplied by the creating agent.
    title: Option<String>,
    /// Authoritative task description used for execution and list output.
    text: String,
    done: bool,
}

impl TodoTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Thread-safe emptiness check. Locks the internal mutex and returns
    /// whether the list is empty. If the mutex is poisoned, conservatively
    /// returns `false` (not empty) so the eviction scan never silently
    /// drops a session that may hold real state behind the poisoned lock.
    pub fn is_empty(&self) -> bool {
        self.items
            .lock()
            .map(|items| items.is_empty())
            .unwrap_or(false)
    }
}

#[async_trait]
impl AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Track tasks in memory for the bound session (or one unbound agent run). action ∈ {add, complete, list, clear}. add expects authoritative 'text' and may include a concise imperative 'title' (3–7 words, at most 36 display cells); complete expects 'index' (1-based)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "complete", "list", "clear"]},
                "title": {
                    "type": "string",
                    "maxLength": 36,
                    "description": "Optional concise imperative display label: 3–7 words, at most 36 terminal cells."
                },
                "text": {"type": "string"},
                "index": {"type": "integer"}
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or("missing 'action'")?;
        let mut items = self.items.lock().map_err(|e| e.to_string())?;
        match action {
            "add" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or("missing 'text'")?
                    .to_string();
                let title = args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .map(str::to_string);
                if title
                    .as_ref()
                    .is_some_and(|title| UnicodeWidthStr::width(title.as_str()) > 36)
                {
                    return Err("'title' must be at most 36 display cells".to_string());
                }
                items.push(TodoItem {
                    title,
                    text,
                    done: false,
                });
                Ok(AgentToolResult::text(format_list(&items)))
            }
            "complete" => {
                let idx = args
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .ok_or("missing 'index'")? as usize;
                if idx == 0 || idx > items.len() {
                    return Err(format!("index {idx} out of range (1..={})", items.len()));
                }
                items[idx - 1].done = true;
                Ok(AgentToolResult::text(format_list(&items)))
            }
            "list" => Ok(AgentToolResult::text(format_list(&items))),
            "clear" => {
                items.clear();
                Ok(AgentToolResult::text("(cleared)".to_string()))
            }
            other => Err(format!("unknown action: {other}")),
        }
    }
}

fn format_list(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let mark = if item.done { "[x]" } else { "[ ]" };
        let title = item
            .title
            .as_deref()
            .map(|title| format!(" — {title}"))
            .unwrap_or_default();
        out.push_str(&format!("{} {} {}{}\n", i + 1, mark, item.text, title));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_protocol::Content;

    fn result_text(result: AgentToolResult) -> String {
        match &result.content[0] {
            Content::Text { text } => text.clone(),
            other => panic!("expected text result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn optional_title_does_not_replace_authoritative_text() {
        let tool = TodoTool::new();
        let result = tool
            .execute(
                "add",
                json!({
                    "action": "add",
                    "title": "Group tool drawers",
                    "text": "Group consecutive chat tool drawers under one collapsed parent"
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            result_text(result),
            "1 [ ] Group consecutive chat tool drawers under one collapsed parent — Group tool drawers\n"
        );
        let items = tool.items.lock().unwrap();
        assert_eq!(items[0].title.as_deref(), Some("Group tool drawers"));
        assert_eq!(
            items[0].text,
            "Group consecutive chat tool drawers under one collapsed parent"
        );
    }

    #[tokio::test]
    async fn title_is_optional_and_bounded() {
        let tool = TodoTool::new();
        tool.execute("add", json!({"action": "add", "text": "legacy caller"}))
            .await
            .unwrap();
        assert!(tool.items.lock().unwrap()[0].title.is_none());

        let err = tool
            .execute(
                "add",
                json!({"action": "add", "title": "界".repeat(19), "text": "too wide"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err, "'title' must be at most 36 display cells");
    }
}
