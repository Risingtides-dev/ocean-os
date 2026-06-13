//! A minimal stdio plugin used by the integration test. It implements just
//! enough of the plugin wire to exercise [`SubprocessPlugin`] end-to-end:
//! `list_tools` (advertises one tool, `echo`) and `invoke_tool` (the `echo` tool
//! returns its `args` back; any other tool name yields a JSON-RPC error).
//!
//! Built as a test binary; the integration test resolves its path via
//! `env!("CARGO_BIN_EXE_echo_plugin")` and spawns it as the plugin child.

use std::io::{BufRead, Write};

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
            Err(_) => continue,
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = match msg.get("id").cloned() {
            // A request with no id is a notification we don't reply to.
            None | Some(serde_json::Value::Null) => continue,
            Some(id) => id,
        };

        let response = match method {
            "list_tools" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo the args back",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "message": { "type": "string" } }
                            }
                        }
                    ]
                }
            }),
            "invoke_tool" => {
                let params = msg.get("params").cloned().unwrap_or_default();
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params
                    .get("args")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if name == "echo" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "echoed": args }
                    })
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown tool: {name}") }
                    })
                }
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }),
        };

        writeln!(stdout, "{response}").ok();
        stdout.flush().ok();
    }
}
