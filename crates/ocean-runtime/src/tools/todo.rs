//! Lightweight in-memory todo list scoped to a bound session (or one unbound
//! agent run). The model can `add`, `complete`, `list`, or `clear` items.
//! Useful for long-horizon tasks.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult};

#[derive(Default)]
pub struct TodoTool {
    items: Mutex<Vec<TodoItem>>,
}

#[derive(Clone)]
struct TodoItem {
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
        "Track tasks in memory for the bound session (or one unbound agent run). action ∈ {add, complete, list, clear}. add expects 'text'; complete expects 'index' (1-based)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "complete", "list", "clear"]},
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
                items.push(TodoItem { text, done: false });
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
        out.push_str(&format!("{} {} {}\n", i + 1, mark, item.text));
    }
    out
}
