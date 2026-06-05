//! A minimal stdio MCP server used by the integration test. It implements just
//! enough of the protocol to exercise the client end-to-end: `initialize`,
//! `notifications/initialized` (ignored), `tools/list` (two tools, one page),
//! and `tools/call` (echoes args for `echo`, returns isError for `boom`).
//!
//! It also exercises the cache-invalidation and media paths:
//! - `shot` returns an `image` content block (OCEAN-48).
//! - `grow` emits a `notifications/tools/list_changed` notification and flips an
//!   internal flag so the *next* `tools/list` advertises an extra `grown` tool
//!   (OCEAN-32).
//!
//! Built as a test binary; the integration test resolves its path via
//! `env!("CARGO_BIN_EXE_fake_server")` and spawns it as the MCP child.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    // Set true after the `grow` tool is called; makes tools/list advertise the
    // extra `grown` tool so the test can observe the refreshed snapshot.
    let mut grown = false;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        // Notifications carry no id and expect no reply.
        if id.is_none() {
            // notifications/initialized etc. — just acknowledge by doing nothing.
            continue;
        }
        let id = id.unwrap();

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fake-server", "version": "0.0.1" }
            }),
            "tools/list" => {
                let mut tools = vec![
                    serde_json::json!({
                        "name": "echo",
                        "description": "Echo the message argument back",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "message": { "type": "string" } },
                            "required": ["message"]
                        }
                    }),
                    serde_json::json!({
                        "name": "boom",
                        "description": "Always returns a tool error",
                        "inputSchema": { "type": "object" }
                    }),
                    serde_json::json!({
                        "name": "shot",
                        "description": "Returns an image content block",
                        "inputSchema": { "type": "object" }
                    }),
                    serde_json::json!({
                        "name": "grow",
                        "description": "Announces tools/list_changed and grows the list",
                        "inputSchema": { "type": "object" }
                    }),
                ];
                if grown {
                    tools.push(serde_json::json!({
                        "name": "grown",
                        "description": "Appeared after tools/list_changed",
                        "inputSchema": { "type": "object" }
                    }));
                }
                serde_json::json!({ "tools": tools })
            }
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_default();
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_default();
                match name {
                    "echo" => {
                        let m = args
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_string();
                        serde_json::json!({
                            "content": [ { "type": "text", "text": format!("echo: {m}") } ],
                            "isError": false
                        })
                    }
                    "boom" => serde_json::json!({
                        "content": [ { "type": "text", "text": "kaboom" } ],
                        "isError": true
                    }),
                    "shot" => serde_json::json!({
                        "content": [
                            { "type": "text", "text": "here is a screenshot" },
                            { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" }
                        ],
                        "isError": false
                    }),
                    "grow" => {
                        // Flip the flag, then emit the list_changed notification
                        // BEFORE the call result so the client's request loop
                        // observes it while draining toward the response.
                        grown = true;
                        let note = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed"
                        });
                        writeln!(stdout, "{note}").ok();
                        stdout.flush().ok();
                        serde_json::json!({
                            "content": [ { "type": "text", "text": "grew the tool list" } ],
                            "isError": false
                        })
                    }
                    other => serde_json::json!({
                        "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
                        "isError": true
                    }),
                }
            }
            _ => {
                // Unknown method → JSON-RPC error.
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                });
                writeln!(stdout, "{resp}").ok();
                stdout.flush().ok();
                continue;
            }
        };

        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}
