//! `ocean-example-plugin` — a runnable example Ocean plugin.
//!
//! This is a complete, real plugin: a standalone executable that speaks the
//! Ocean plugin wire (JSON-RPC 2.0 over stdio) and exposes two genuinely useful
//! demo tools. Pair it with the sibling `plugin.toml` (same directory) and the
//! daemon's plugin discovery (OCEAN-110) loads it like any other plugin.
//!
//! ## The wire it speaks
//!
//! One JSON object per line on stdin (a request), one JSON object per line on
//! stdout (the response). Two methods, both request/response:
//!
//! - `list_tools` (no params) → `{ "tools": [ { name, description?, inputSchema? }, … ] }`
//! - `invoke_tool` with params `{ "name": <string>, "args": <object> }` → the
//!   tool's JSON result (opaque to the runtime — whatever shape you return is
//!   handed to the model as the tool result).
//!
//! A request with no `id` is a notification and gets no reply. An unknown method
//! or tool name returns a JSON-RPC `error` object (code `-32601`).
//!
//! ## The tools it exposes
//!
//! - `reverse_text` — reverse a string (`{ "text": "abc" }` → `{ "reversed": "cba" }`).
//! - `current_time` — the current Unix time in seconds since the epoch, no args
//!   (`{}` → `{ "unix_seconds": 1717545600 }`).
//!
//! ## Run it by hand
//!
//! ```text
//! $ cargo build --example ocean-example-plugin -p ocean-plugin
//! $ echo '{"jsonrpc":"2.0","id":1,"method":"list_tools"}' \
//!     | ./target/debug/examples/ocean-example-plugin
//! {"jsonrpc":"2.0","id":1,"result":{"tools":[ … ]}}
//! ```
//!
//! Kept dependency-free beyond `serde_json` (already an `ocean-plugin` dep) and
//! the standard library, so it builds with the crate and copies cleanly into a
//! standalone plugin pack.

use std::io::{BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            // A line we can't parse as JSON: skip it rather than crash. A real
            // plugin could log to stderr here (stdout is reserved for the wire).
            Err(_) => continue,
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = match msg.get("id").cloned() {
            // A request with no id is a notification we don't reply to.
            None | Some(serde_json::Value::Null) => continue,
            Some(id) => id,
        };

        let response = match method {
            "list_tools" => list_tools_response(id),
            "invoke_tool" => invoke_tool_response(&msg, id),
            _ => rpc_error(id, -32601, format!("method not found: {method}")),
        };

        writeln!(stdout, "{response}").ok();
        stdout.flush().ok();
    }
}

/// Advertise this plugin's tools. The runtime reads `inputSchema` (JSON Schema)
/// to validate/describe the tool's arguments to the model.
fn list_tools_response(id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "reverse_text",
                    "description": "Reverse a string. Returns { reversed: <string> }.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {
                                "type": "string",
                                "description": "The text to reverse."
                            }
                        },
                        "required": ["text"]
                    }
                },
                {
                    "name": "current_time",
                    "description": "Current Unix time in seconds. Takes no arguments. \
                                    Returns { unix_seconds: <number> }.",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }
    })
}

/// Dispatch an `invoke_tool` call to the named tool.
fn invoke_tool_response(msg: &serde_json::Value, id: serde_json::Value) -> serde_json::Value {
    let params = msg.get("params").cloned().unwrap_or_default();
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match name {
        "reverse_text" => match args.get("text").and_then(|t| t.as_str()) {
            Some(text) => {
                let reversed: String = text.chars().rev().collect();
                ok_result(id, serde_json::json!({ "reversed": reversed }))
            }
            None => rpc_error(
                id,
                -32602,
                "reverse_text requires a string `text` argument".to_string(),
            ),
        },
        "current_time" => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            ok_result(id, serde_json::json!({ "unix_seconds": secs }))
        }
        other => rpc_error(id, -32601, format!("unknown tool: {other}")),
    }
}

/// A JSON-RPC success response carrying `result`.
fn ok_result(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error response.
fn rpc_error(id: serde_json::Value, code: i64, message: String) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
