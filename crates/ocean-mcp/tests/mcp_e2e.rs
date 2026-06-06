//! End-to-end test of the MCP client against a real child process (the
//! `fake_server` test binary). Exercises the full path: spawn → initialize →
//! tools/list → tools/call, plus the provider's namespacing and the
//! non-fatal-failure behaviour for a bad command.
//!
//! Also covers the streamable-HTTP transport (OCEAN-166) end-to-end against a
//! mock HTTP MCP server, proving an `http`-typed config actually yields tools.

use std::sync::Arc;

use ocean_mcp::{McpProvider, McpServerConfig, McpTransportKind};
use ocean_protocol::Content;
use ocean_runtime::capability::{
    CapabilityProvider, CapabilityRegistry, ProviderHealth, SessionContext,
};
use ocean_runtime::BuiltinProvider;
use serde_json::json;
use std::time::Duration;

fn fake_server_path() -> String {
    env!("CARGO_BIN_EXE_fake_server").to_string()
}

fn server_cfg() -> McpServerConfig {
    McpServerConfig {
        name: "fake".into(),
        transport: McpTransportKind::Stdio,
        command: Some(fake_server_path()),
        args: vec![],
        env: vec![],
        enabled: true,
    }
}

#[tokio::test]
async fn connects_lists_and_namespaces_tools() {
    let provider = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .expect("connect should succeed (config is valid)");

    assert_eq!(provider.health().await, ProviderHealth::Ready);

    let tools = provider.tools(&SessionContext::default()).await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&"mcp__fake__echo"),
        "echo tool should be namespaced, got {names:?}"
    );
    assert!(names.contains(&"mcp__fake__boom"));

    // The namespaced tool keeps the server's description and schema.
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__echo")
        .unwrap();
    assert_eq!(echo.description(), "Echo the message argument back");
    assert_eq!(echo.parameters()["type"], "object");
    // MCP tools require permission by default.
    assert!(echo.requires_permission());
}

#[tokio::test]
async fn calls_a_tool_and_gets_result() {
    let provider = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .unwrap();
    let tools = provider.tools(&SessionContext::default()).await;
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__echo")
        .unwrap();

    let out = echo
        .execute("call-1", json!({ "message": "hi there" }))
        .await
        .expect("echo should succeed");
    let text = out.content.iter().find_map(|c| c.as_text()).unwrap_or("");
    assert_eq!(text, "echo: hi there");
}

#[tokio::test]
async fn tool_execution_error_surfaces_as_err() {
    let provider = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .unwrap();
    let tools = provider.tools(&SessionContext::default()).await;
    let boom = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__boom")
        .unwrap();

    let err = boom.execute("call-2", json!({})).await.unwrap_err();
    assert!(err.contains("kaboom"), "isError result becomes Err: {err}");
}

#[tokio::test]
async fn bad_command_is_non_fatal_and_contributes_no_tools() {
    let mut cfg = server_cfg();
    cfg.command = Some("this-binary-does-not-exist-ocean".into());

    // connect must NOT error for a server-side/spawn failure.
    let provider = McpProvider::connect(&cfg, |_| None, Duration::from_secs(5))
        .await
        .expect("a broken server must not fail the whole connect");
    assert_eq!(provider.health().await, ProviderHealth::Unavailable);
    assert!(provider.tools(&SessionContext::default()).await.is_empty());
}

#[tokio::test]
async fn image_result_reaches_caller_as_image_content() {
    // OCEAN-48: an MCP `image` result block must arrive as Content::Image, not a
    // dropped placeholder.
    let provider = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .unwrap();
    let tools = provider.tools(&SessionContext::default()).await;
    let shot = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__shot")
        .expect("shot tool present");

    let out = shot.execute("call-shot", json!({})).await.unwrap();
    // Text caption preserved...
    assert!(out
        .content
        .iter()
        .any(|c| c.as_text() == Some("here is a screenshot")));
    // ...and the image is real, not a placeholder string.
    let img = out
        .content
        .iter()
        .find(|c| matches!(c, Content::Image { .. }))
        .expect("image content present");
    if let Content::Image { data, mime_type } = img {
        assert_eq!(data, "aGVsbG8=");
        assert_eq!(mime_type, "image/png");
    }
}

#[tokio::test]
async fn list_changed_refreshes_cached_tools() {
    // OCEAN-32: after a tools/list_changed notification, the provider's cached
    // tool snapshot must pick up the server's new tool via background re-fetch.
    let provider = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .unwrap();

    let before = provider.tools(&SessionContext::default()).await;
    assert!(
        !before.iter().any(|t| t.name() == "mcp__fake__grown"),
        "grown tool should not exist before list_changed"
    );

    // Calling `grow` makes the server emit notifications/tools/list_changed; the
    // client observes it in its request loop and signals the watcher, which
    // re-fetches and swaps the cache.
    let grow = before
        .iter()
        .find(|t| t.name() == "mcp__fake__grow")
        .expect("grow tool present");
    grow.execute("call-grow", json!({})).await.unwrap();

    // The refresh happens on a background task; poll briefly for the swap.
    let mut found = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = provider.tools(&SessionContext::default()).await;
        if now.iter().any(|t| t.name() == "mcp__fake__grown") {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "cached tool list should include `grown` after list_changed refresh"
    );
}

#[tokio::test]
async fn registry_merges_builtins_with_live_mcp_server() {
    // The real payoff: built-ins + a live MCP server, flattened through the
    // same registry, with MCP tools namespaced and built-ins intact.
    let mcp = McpProvider::connect(&server_cfg(), |_| None, Duration::from_secs(10))
        .await
        .unwrap();
    let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), Arc::new(mcp)]);

    let tools = registry.tools_for_session(&SessionContext::default()).await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    // Built-ins present.
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read"));
    // MCP tools present and namespaced.
    assert!(names.contains(&"mcp__fake__echo"));
    // Unique names (the dispatch-map invariant).
    let mut sorted: Vec<&str> = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len());
}

/// OCEAN-166: an `http`-transport server must actually contribute its tools, not
/// silently yield none. This drives `McpProvider::connect` against a mock
/// streamable-HTTP MCP server (a raw TCP listener replying with `application/json`
/// JSON-RPC) and asserts the provider comes up `Ready` with namespaced tools.
#[tokio::test]
async fn http_transport_provider_yields_tools() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                let parsed: serde_json::Value =
                    serde_json::from_str(body.trim()).unwrap_or(serde_json::Value::Null);
                let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = parsed.get("id").cloned();

                // Notification → 202, no body.
                if id.is_none() {
                    let _ = sock
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }

                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-http", "version": "0.0.0" }
                    }),
                    "tools/list" => json!({
                        "tools": [{
                            "name": "ping",
                            "description": "ping over http",
                            "inputSchema": { "type": "object" }
                        }]
                    }),
                    _ => json!({}),
                };
                let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cfg = McpServerConfig {
        name: "remote".into(),
        transport: McpTransportKind::Http,
        command: Some(format!("http://{addr}/mcp")),
        args: vec![],
        env: vec![],
        enabled: true,
    };

    let provider = McpProvider::connect(&cfg, |_| None, Duration::from_secs(10))
        .await
        .expect("http connect should succeed against a reachable endpoint");

    assert_eq!(
        provider.health().await,
        ProviderHealth::Ready,
        "an http server that lists tools must be Ready, not silently toolless"
    );

    let tools = provider.tools(&SessionContext::default()).await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&"mcp__remote__ping"),
        "http server's tool must be namespaced and present, got {names:?}"
    );
}
