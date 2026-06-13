//! End-to-end proof that the bundled **example plugin** loads and runs through
//! `ocean-plugin`'s real runtime — the same path the daemon's plugin discovery
//! (OCEAN-110) drives.
//!
//! These tests launch the actual built `ocean-example-plugin` example binary as
//! a child process (no mocks, no in-process shortcut) and:
//!
//! 1. `example_manifest_parses` — its real `plugin.toml` parses into the
//!    expected identity and two tools.
//! 2. `example_subprocess_round_trip` — `SubprocessPlugin::launch` from that
//!    manifest, then `list_tools` + `invoke_tool` for both `reverse_text` and
//!    `current_time` over JSON-RPC stdio.
//! 3. `example_loads_through_runtime` (feature `runtime`) — wrap the launched
//!    plugin in `PluginProvider` and confirm the daemon-facing surface: tools
//!    namespaced `plugin__example-plugin__<tool>`, permission-gated, and
//!    `execute` forwards to the live subprocess.
//!
//! Cargo does not export `CARGO_BIN_EXE_<name>` for `[[example]]` targets, so we
//! resolve the built example binary relative to this test executable: integration
//! tests run from `target/<profile>/deps/`, and examples land in the sibling
//! `target/<profile>/examples/` directory.

use std::path::PathBuf;

use ocean_plugin::{Plugin, PluginManifest, SubprocessPlugin};
use serde_json::json;

/// Directory holding the example plugin pack (manifest + source).
fn example_pack_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example_plugin")
}

/// Resolve the built `ocean-example-plugin` example binary. The test exe lives at
/// `target/<profile>/deps/<name>-<hash>`; the example lives at
/// `target/<profile>/examples/ocean-example-plugin`.
fn example_plugin_bin() -> PathBuf {
    let test_exe = std::env::current_exe().expect("test exe path");
    // .../deps/<test> -> .../deps -> .../<profile>
    let profile_dir = test_exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("profile dir");
    let bin = profile_dir.join("examples").join(if cfg!(windows) {
        "ocean-example-plugin.exe"
    } else {
        "ocean-example-plugin"
    });
    assert!(
        bin.exists(),
        "example binary not built at {}; run `cargo build --example ocean-example-plugin -p ocean-plugin` first",
        bin.display()
    );
    bin
}

#[test]
fn example_manifest_parses() {
    let manifest_path = example_pack_dir().join("plugin.toml");
    let manifest = PluginManifest::from_path(&manifest_path).expect("example manifest parses");

    assert_eq!(manifest.name, "example-plugin");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.entry, "ocean-example-plugin");

    let tools = manifest.plugin_tools();
    assert_eq!(
        tools.len(),
        2,
        "example advertises reverse_text + current_time"
    );
    assert_eq!(tools[0].name, "reverse_text");
    assert_eq!(tools[0].input_schema["required"][0], "text");
    assert_eq!(tools[1].name, "current_time");
}

#[tokio::test]
async fn example_subprocess_round_trip() {
    // Launch the example exactly as the daemon would: from its manifest, with the
    // built binary as the entry. (We point `entry` at the real built path; the
    // checked-in manifest's relative entry is what you'd use after copying the
    // binary next to plugin.toml in your plugins dir.)
    let mut manifest =
        PluginManifest::from_path(example_pack_dir().join("plugin.toml")).expect("manifest");
    manifest.entry = example_plugin_bin().to_string_lossy().into_owned();

    let plugin = SubprocessPlugin::launch(&manifest, example_pack_dir()).expect("launch example");
    assert_eq!(plugin.name(), "example-plugin");
    assert_eq!(plugin.version(), "0.1.0");

    // list_tools round-trip surfaces both tools.
    let tools = plugin.list_tools().await.expect("list_tools round-trips");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"reverse_text"), "got: {names:?}");
    assert!(names.contains(&"current_time"), "got: {names:?}");

    // reverse_text: real work, real result.
    let res = plugin
        .invoke_tool("reverse_text", json!({ "text": "ocean" }))
        .await
        .expect("reverse_text round-trips");
    assert_eq!(res["reversed"], "naeco");

    // current_time: no-arg tool returns a plausible Unix timestamp.
    let res = plugin
        .invoke_tool("current_time", json!({}))
        .await
        .expect("current_time round-trips");
    let secs = res["unix_seconds"]
        .as_u64()
        .expect("unix_seconds is a number");
    assert!(
        secs > 1_700_000_000,
        "expected a recent timestamp, got {secs}"
    );
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn example_loads_through_runtime() {
    use ocean_plugin::PluginProvider;
    use ocean_protocol::Content;
    use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext};
    use std::sync::Arc;

    let mut manifest =
        PluginManifest::from_path(example_pack_dir().join("plugin.toml")).expect("manifest");
    manifest.entry = example_plugin_bin().to_string_lossy().into_owned();
    let plugin = SubprocessPlugin::launch(&manifest, example_pack_dir()).expect("launch example");

    // Wrap the live subprocess in the runtime adapter — the exact step the daemon
    // performs in `discover_plugin_providers` (OCEAN-110).
    let provider = PluginProvider::connect(Arc::new(plugin) as Arc<dyn Plugin>).await;
    assert_eq!(provider.id(), "plugin:example-plugin");
    assert_eq!(provider.health().await, ProviderHealth::Ready);

    let ctx = SessionContext::default();
    let tools = provider.tools(&ctx).await;
    let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    // Namespaced so it can never collide with a built-in or an MCP tool.
    assert!(
        names.contains(&"plugin__example-plugin__reverse_text".to_string()),
        "got: {names:?}"
    );
    assert!(
        names.contains(&"plugin__example-plugin__current_time".to_string()),
        "got: {names:?}"
    );
    // Plugin tools run out-of-process code: permission-gated like bash, never bypassed.
    assert!(
        tools.iter().all(|t| t.requires_permission()),
        "all plugin tools require permission"
    );

    // Execute through the runtime tool surface and confirm the structured result
    // reaches the model as content.
    let reverse = tools
        .iter()
        .find(|t| t.name() == "plugin__example-plugin__reverse_text")
        .expect("reverse_text tool present");
    let result = reverse
        .execute("call-1", json!({ "text": "runtime" }))
        .await
        .expect("execute forwards to the live plugin");
    let text = match &result.content[0] {
        Content::Text { text } => text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("emitnur"),
        "reversed result reaches model: {text}"
    );
}
