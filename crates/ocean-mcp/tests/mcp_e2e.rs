//! End-to-end test of the MCP client against a real child process (the
//! `fake_server` test binary). Exercises the full path: spawn → initialize →
//! tools/list → tools/call, plus the provider's namespacing and the
//! non-fatal-failure behaviour for a bad command.

use std::sync::Arc;

use ocean_mcp::{McpProvider, McpServerConfig, McpTransportKind};
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
