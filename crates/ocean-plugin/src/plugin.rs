//! The plugin capability seam: the [`Plugin`] trait and its data types.
//!
//! This is the contract every plugin implementation honors, independent of how
//! it runs. [`SubprocessPlugin`](crate::SubprocessPlugin) implements it over a
//! stdio child today; a future `WasmPlugin` implements the *same* trait over a
//! `wasmtime` instance with no change to anything here.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool a plugin advertises. Deliberately shaped to map 1:1 onto
/// `ocean-runtime`'s `AgentTool` (`name` / `description` / `parameters`), so a
/// thin adapter in the daemon can expose a plugin tool as a runtime tool without
/// reshaping anything. `input_schema` is JSON Schema for the tool's arguments —
/// the same role MCP calls `inputSchema` and the runtime calls `parameters`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginTool {
    /// Tool name, unique within the plugin. This is the string passed to
    /// [`Plugin::invoke_tool`].
    pub name: String,
    /// Human/model-facing description. Optional so a terse plugin can omit it.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing the tool's arguments object. Defaults to an empty
    /// object schema (`{}`) when a plugin declares a tool that takes no args.
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

impl PluginTool {
    /// Construct a tool with a name and schema, no description.
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema,
        }
    }
}

/// Errors a [`Plugin`] operation can surface. Kept as a small hand-rolled enum
/// (no `thiserror`) to match the lean dependency posture of sibling crates like
/// `ocean-mcp`.
#[derive(Debug)]
pub enum PluginError {
    /// The plugin's entry executable could not be spawned (missing file, not
    /// executable, etc.). Carries the entry path and the underlying reason.
    Spawn { entry: String, reason: String },
    /// A JSON-RPC round-trip failed at the transport level (write/read error,
    /// EOF mid-request, or timeout). Carries a human-readable reason.
    Transport(String),
    /// The plugin returned a JSON-RPC error object for the request. Carries the
    /// error code and message.
    Rpc { code: i64, message: String },
    /// The plugin's response did not match the expected shape (e.g. a malformed
    /// `tools/list` payload).
    Protocol(String),
    /// A tool was invoked that the plugin does not advertise.
    UnknownTool(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Spawn { entry, reason } => {
                write!(f, "failed to launch plugin entry `{entry}`: {reason}")
            }
            PluginError::Transport(r) => write!(f, "plugin transport error: {r}"),
            PluginError::Rpc { code, message } => {
                write!(f, "plugin returned JSON-RPC error {code}: {message}")
            }
            PluginError::Protocol(r) => write!(f, "plugin protocol error: {r}"),
            PluginError::UnknownTool(name) => write!(f, "plugin has no tool named `{name}`"),
        }
    }
}

impl std::error::Error for PluginError {}

/// The plugin capability seam.
///
/// Every plugin — subprocess today, WASM tomorrow — implements this trait. The
/// runtime depends only on these four methods and never on *how* the plugin
/// runs. Async because the canonical implementation
/// ([`SubprocessPlugin`](crate::SubprocessPlugin)) does I/O to a child process;
/// an in-process WASM implementation can still satisfy an async signature
/// trivially.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin's name (from its manifest). Stable identifier for logs and for
    /// namespacing its tools.
    fn name(&self) -> &str;

    /// The plugin's version string (from its manifest).
    fn version(&self) -> &str;

    /// The tools this plugin offers. For a subprocess plugin this is a live
    /// `tools/list` round-trip; an implementation may also serve a cached
    /// snapshot. Returns the declared tools in plugin order.
    async fn list_tools(&self) -> Result<Vec<PluginTool>, PluginError>;

    /// Invoke a tool by name with JSON arguments, returning the tool's JSON
    /// result. The result shape is the plugin's own contract; the runtime adapter
    /// is responsible for mapping it into an `AgentToolResult`.
    async fn invoke_tool(&self, name: &str, args: Value) -> Result<Value, PluginError>;
}
