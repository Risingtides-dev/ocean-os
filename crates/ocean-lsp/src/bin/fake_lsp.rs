//! A tiny stdio LSP server for tests (mirrors ocean-mcp's fake_server pattern).
//!
//! Speaks just enough of the protocol to exercise the client + tool end to end:
//! - `initialize` / `initialized`
//! - `textDocument/didOpen` → immediately publishes one diagnostic if the text
//!   contains the string `BUG`, else an empty diagnostics set
//! - `textDocument/definition` → a fixed location (line 1 of the same file)
//! - `textDocument/hover` → markdown contents naming the requested position
//! - `textDocument/references` → two fixed locations
//! - `textDocument/rename` → a WorkspaceEdit replacing chars 3..6 of line 0
//! - `shutdown` / `exit`
//!
//! Deliberately synchronous, std-only: no tokio, no deps beyond serde_json.

use std::io::{BufRead, BufReader, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(msg) = read_frame(&mut reader) {
        let Ok(v) = serde_json::from_str::<Value>(&msg) else {
            continue;
        };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = v.get("id").cloned();
        match method {
            "initialize" => {
                respond(
                    &mut out,
                    id,
                    json!({ "capabilities": { "textDocumentSync": 1, "hoverProvider": true,
                        "definitionProvider": true, "referencesProvider": true,
                        "renameProvider": true } }),
                );
            }
            "initialized" => {}
            "textDocument/didOpen" | "textDocument/didChange" => {
                let (uri, text) = if method == "textDocument/didOpen" {
                    (
                        v["params"]["textDocument"]["uri"].as_str().unwrap_or(""),
                        v["params"]["textDocument"]["text"].as_str().unwrap_or(""),
                    )
                } else {
                    (
                        v["params"]["textDocument"]["uri"].as_str().unwrap_or(""),
                        v["params"]["contentChanges"][0]["text"]
                            .as_str()
                            .unwrap_or(""),
                    )
                };
                let diags = if text.contains("BUG") {
                    json!([{ "range": { "start": { "line": 0, "character": 0 },
                                         "end": { "line": 0, "character": 3 } },
                             "severity": 1,
                             "message": "found the string BUG" }])
                } else {
                    json!([])
                };
                notify(
                    &mut out,
                    "textDocument/publishDiagnostics",
                    json!({ "uri": uri, "diagnostics": diags }),
                );
            }
            "textDocument/definition" => {
                let uri = v["params"]["textDocument"]["uri"].as_str().unwrap_or("");
                respond(
                    &mut out,
                    id,
                    json!({ "uri": uri, "range": { "start": { "line": 0, "character": 0 },
                                                    "end": { "line": 0, "character": 1 } } }),
                );
            }
            "textDocument/references" => {
                let uri = v["params"]["textDocument"]["uri"].as_str().unwrap_or("");
                respond(
                    &mut out,
                    id,
                    json!([
                        { "uri": uri, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } },
                        { "uri": uri, "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 1 } } }
                    ]),
                );
            }
            "textDocument/hover" => {
                let line = v["params"]["position"]["line"].as_u64().unwrap_or(0);
                let chr = v["params"]["position"]["character"].as_u64().unwrap_or(0);
                respond(
                    &mut out,
                    id,
                    json!({ "contents": { "kind": "markdown",
                        "value": format!("hover at {line}:{chr}") } }),
                );
            }
            "textDocument/rename" => {
                let uri = v["params"]["textDocument"]["uri"].as_str().unwrap_or("");
                let new_name = v["params"]["newName"].as_str().unwrap_or("renamed");
                respond(
                    &mut out,
                    id,
                    json!({ "changes": { uri: [
                        { "range": { "start": { "line": 0, "character": 3 },
                                      "end": { "line": 0, "character": 6 } },
                          "newText": new_name }
                    ]}}),
                );
            }
            "shutdown" => respond(&mut out, id, Value::Null),
            "exit" => return,
            _ => {
                if let Some(id) = id {
                    respond_err(&mut out, id, -32601, "method not found");
                }
            }
        }
    }
}

fn read_frame<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn write_frame<W: Write>(out: &mut W, body: &str) {
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn respond<W: Write>(out: &mut W, id: Option<Value>, result: Value) {
    let body = json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result });
    write_frame(out, &body.to_string());
}

fn respond_err<W: Write>(out: &mut W, id: Value, code: i64, message: &str) {
    let body = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    write_frame(out, &body.to_string());
}

fn notify<W: Write>(out: &mut W, method: &str, params: Value) {
    let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    write_frame(out, &body.to_string());
}
