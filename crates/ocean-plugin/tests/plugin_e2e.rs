//! End-to-end tests for `ocean-plugin`:
//!
//! 1. `manifest_fixture_parses` — the `tests/fixtures/plugin.toml` parses into a
//!    [`PluginManifest`] with the expected identity and tools.
//! 2. `subprocess_round_trip` — launch the real `echo_plugin` test binary as a
//!    child and prove `list_tools` + `invoke_tool` round-trip over JSON-RPC stdio
//!    for real (no mocks, no in-process shortcut).
//! 3. `subprocess_rpc_error_surfaces` — an unknown tool maps the plugin's
//!    JSON-RPC error object onto [`PluginError::Rpc`].

use ocean_plugin::{Plugin, PluginError, PluginManifest, SubprocessPlugin};
use serde_json::json;

/// The integration test resolves the echo plugin binary that Cargo built.
fn echo_plugin_bin() -> String {
    env!("CARGO_BIN_EXE_echo_plugin").to_string()
}

#[test]
fn manifest_fixture_parses() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plugin.toml");
    let manifest = PluginManifest::from_path(path).expect("fixture manifest parses");

    assert_eq!(manifest.name, "echo-pack");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.entry, "bin/echo-plugin");

    let tools = manifest.plugin_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description.as_deref(), Some("Echo the args back"));
    assert_eq!(tools[0].input_schema["type"], "object");
}

#[tokio::test]
async fn subprocess_round_trip() {
    // Launch the real echo plugin as a child process and speak JSON-RPC to it.
    let plugin =
        SubprocessPlugin::launch_command("echo-pack", "0.1.0", &echo_plugin_bin(), &[], &[])
            .expect("launch echo plugin");

    assert_eq!(plugin.name(), "echo-pack");
    assert_eq!(plugin.version(), "0.1.0");

    // list_tools round-trip.
    let tools = plugin.list_tools().await.expect("list_tools round-trips");
    assert_eq!(tools.len(), 1, "echo plugin advertises exactly one tool");
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].input_schema["type"], "object");

    // invoke_tool round-trip: the echo plugin returns { "echoed": <args> }.
    let args = json!({ "message": "hello plugins" });
    let result = plugin
        .invoke_tool("echo", args.clone())
        .await
        .expect("invoke_tool round-trips");
    assert_eq!(result["echoed"], args);

    // Two concurrent invocations must both resolve (multiplexed, not serialized).
    let (a, b) = tokio::join!(
        plugin.invoke_tool("echo", json!({ "n": 1 })),
        plugin.invoke_tool("echo", json!({ "n": 2 })),
    );
    assert_eq!(a.unwrap()["echoed"], json!({ "n": 1 }));
    assert_eq!(b.unwrap()["echoed"], json!({ "n": 2 }));
}

#[tokio::test]
async fn subprocess_rpc_error_surfaces() {
    let plugin =
        SubprocessPlugin::launch_command("echo-pack", "0.1.0", &echo_plugin_bin(), &[], &[])
            .expect("launch echo plugin");

    let err = plugin
        .invoke_tool("nope", json!({}))
        .await
        .expect_err("unknown tool yields an error");
    match err {
        PluginError::Rpc { code, message } => {
            assert_eq!(code, -32601);
            assert!(message.contains("unknown tool"), "got: {message}");
        }
        other => panic!("expected PluginError::Rpc, got {other:?}"),
    }
}
