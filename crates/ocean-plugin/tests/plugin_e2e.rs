//! End-to-end tests for `ocean-plugin`:
//!
//! 1. `manifest_fixture_parses` — the `tests/fixtures/plugin.toml` parses into a
//!    [`PluginManifest`] with the expected identity and tools.
//! 2. `subprocess_round_trip` — launch the real `echo_plugin` test binary as a
//!    child and prove `list_tools` + `invoke_tool` round-trip over JSON-RPC stdio
//!    for real (no mocks, no in-process shortcut).
//! 3. `subprocess_rpc_error_surfaces` — an unknown tool maps the plugin's
//!    JSON-RPC error object onto [`PluginError::Rpc`].
//! 4. Phase 0 process-boundary characterization — current insecure inheritance of
//!    environment and real cwd, plus the first initialization request on the wire.

use std::{
    io::{BufRead, Write},
    sync::OnceLock,
};

use ocean_plugin::{Plugin, PluginError, PluginManifest, PluginProvider, SubprocessPlugin};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// The integration test resolves the echo plugin binary that Cargo built.
fn echo_plugin_bin() -> String {
    env!("CARGO_BIN_EXE_echo_plugin").to_string()
}

const PROCESS_PROBE_ENV: &str = "OCEAN_PLUGIN_PHASE0_PROCESS_PROBE";
const PROCESS_PROBE_TEST: &str = "phase0_process_probe_child";
const FIRST_REQUEST_PATH_ENV: &str = "OCEAN_PLUGIN_PHASE0_FIRST_REQUEST_PATH";

fn launch_process_probe(extra_env: &[(String, String)]) -> SubprocessPlugin {
    let command = std::env::current_exe()
        .expect("resolve integration test executable")
        .to_string_lossy()
        .into_owned();
    let args = vec![
        "--exact".to_string(),
        PROCESS_PROBE_TEST.to_string(),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ];
    let mut env = vec![(PROCESS_PROBE_ENV.to_string(), "1".to_string())];
    env.extend_from_slice(extra_env);

    SubprocessPlugin::launch_command("probe", "0.0.0", &command, &args, &env)
        .expect("launch test-only process probe")
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

async fn process_context(plugin: &SubprocessPlugin, env_key: &str) -> Value {
    plugin
        .invoke_tool("__test_process_context", json!({ "env_key": env_key }))
        .await
        .expect("probe process context")
}

struct TempFile(std::path::PathBuf);

impl TempFile {
    fn unique(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "ocean-plugin-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos()
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn phase0_process_probe_child() {
    if std::env::var_os(PROCESS_PROBE_ENV).is_none() {
        return;
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut first_request = true;
    let mut first_response = true;

    for line in stdin.lock().lines() {
        let line = line.expect("read probe request");
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).expect("probe request is JSON");
        if first_request {
            if let Some(path) = std::env::var_os(FIRST_REQUEST_PATH_ENV) {
                std::fs::write(path, &line).expect("record first probe request");
            }
            first_request = false;
        }

        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap_or_default();
        let (response, exit_after_response) = match method {
            "list_tools" => (
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } }),
                false,
            ),
            "invoke_tool" if request["params"]["name"] == "__test_process_context" => {
                let env_key = request["params"]["args"]["env_key"]
                    .as_str()
                    .unwrap_or_default();
                (
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "env": std::env::var(env_key).ok(),
                            "cwd": std::env::current_dir()
                                .expect("read probe cwd")
                                .to_string_lossy()
                        }
                    }),
                    true,
                )
            }
            _ => (
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "unsupported test probe request" }
                }),
                true,
            ),
        };

        // `libtest --nocapture` prints `test ... ` without a trailing newline.
        // Start the first response on a fresh line; the transport skips that
        // non-JSON libtest framing and reads the JSON line that follows.
        if first_response {
            writeln!(stdout, "\n{response}").expect("write first probe response");
            first_response = false;
        } else {
            writeln!(stdout, "{response}").expect("write probe response");
        }
        stdout.flush().expect("flush probe response");
        if exit_after_response {
            return;
        }
    }
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
async fn child_currently_inherits_ambient_env_while_explicit_env_overlays_it() {
    const ENV_KEY: &str = "OCEAN_PLUGIN_PHASE0_UNIQUE_PARENT_ENV";
    let _lock = env_lock().lock().await;
    let _env = EnvVarGuard::set(ENV_KEY, "parent-value");

    let inherited = launch_process_probe(&[]);
    assert_eq!(
        process_context(&inherited, ENV_KEY).await["env"],
        "parent-value",
        "current insecure behavior: the plugin child inherits ambient parent environment"
    );

    let overlay = vec![(ENV_KEY.to_string(), "explicit-value".to_string())];
    let overlaid = launch_process_probe(&overlay);
    assert_eq!(
        process_context(&overlaid, ENV_KEY).await["env"],
        "explicit-value",
        "current behavior: explicit plugin env overlays the inherited ambient value"
    );
}

#[tokio::test]
async fn launch_command_currently_inherits_ambient_real_cwd() {
    let parent_cwd = std::env::current_dir().expect("read parent cwd");
    let plugin = launch_process_probe(&[]);

    let context = process_context(&plugin, "OCEAN_PLUGIN_UNUSED_ENV").await;
    assert_eq!(
        context["cwd"],
        parent_cwd.to_string_lossy().as_ref(),
        "current insecure behavior: launch_command inherits the parent's ambient real cwd"
    );
}

#[tokio::test]
async fn provider_connect_first_request_is_list_tools_id_one_without_params() {
    let first_request = TempFile::unique("first-request");
    let env = vec![(
        FIRST_REQUEST_PATH_ENV.to_string(),
        first_request.0.to_string_lossy().into_owned(),
    )];
    let plugin = std::sync::Arc::new(launch_process_probe(&env));

    let _provider = PluginProvider::connect(plugin.clone()).await;
    let wire: Value = serde_json::from_slice(
        &std::fs::read(&first_request.0).expect("fixture recorded first request"),
    )
    .expect("first request is JSON");
    assert_eq!(
        wire,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "list_tools"
        })
    );

    process_context(&plugin, "OCEAN_PLUGIN_UNUSED_ENV").await;
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
