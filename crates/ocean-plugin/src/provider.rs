//! `PluginProvider` — adapts one [`Plugin`] to the runtime's
//! [`CapabilityProvider`] seam, so plugin-declared tools compose into the same
//! [`CapabilityRegistry`](ocean_runtime::CapabilityRegistry) as built-ins and
//! MCP tools.
//!
//! This is the runtime integration the crate's top-level docs deferred: OCEAN-91
//! shipped the [`Plugin`]/[`PluginTool`] seam library-only, on purpose, with a
//! written note that "when the daemon adopts this crate it can wrap each
//! `PluginTool` in a thin `AgentTool` adapter whose `execute` forwards to
//! [`Plugin::invoke_tool`]". This module is exactly that adapter, plus the
//! provider that lists the wrapped tools — mirroring how `ocean-mcp`'s
//! `McpProvider` plugs an MCP server into the same seam.
//!
//! ## Dependency direction
//!
//! Like `ocean-mcp`, this depends **up** into `ocean-runtime`; `ocean-runtime`
//! never depends back. The plugin trait/data types ([`Plugin`], [`PluginTool`])
//! stay free of any runtime dependency — only this opt-in module pulls the two
//! together, behind the `runtime` feature, so a consumer that just wants the
//! plugin transport pays nothing for the runtime seam.
//!
//! ## Namespacing
//!
//! Tools are exposed to the agent as `plugin__<plugin-name>__<tool>`, so a plugin
//! can never collide with a built-in (`bash`) or an MCP tool (`mcp__…`), and two
//! plugins can't collide with each other. The registry's first-wins dedup makes
//! built-ins unshadowable regardless.
//!
//! ## Caching
//!
//! [`CapabilityProvider::tools`] must be cheap — it is called once per turn. The
//! provider discovers the plugin's tools once at [`connect`](PluginProvider::connect)
//! and serves that cached snapshot; tool *calls* go back to the live plugin
//! through the shared [`Plugin`] handle. A plugin that fails to list its tools
//! yields an empty, [`Unavailable`](ProviderHealth::Unavailable) provider that
//! contributes nothing rather than blocking a turn.

use std::sync::Arc;

use async_trait::async_trait;
use ocean_protocol::Content;
use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::Value;

use crate::plugin::{Plugin, PluginTool};

/// Separator between the plugin name and the tool name in the namespaced id
/// exposed to the agent. Matches the `__` convention `ocean-mcp` uses.
const NS: &str = "__";

/// A [`CapabilityProvider`] backed by a single [`Plugin`].
///
/// Holds the plugin behind an `Arc` (shared with every wrapped tool so calls go
/// to the live plugin) and a snapshot of the wrapped tools captured at connect
/// time.
pub struct PluginProvider {
    id: String,
    tools: Vec<SharedTool>,
    health: ProviderHealth,
}

impl PluginProvider {
    /// Discover a plugin's tools once and wrap them as runtime tools.
    ///
    /// Never returns `Err` for a plugin-side failure: a plugin that can't list
    /// its tools produces a healthy-typed but empty provider with
    /// [`ProviderHealth::Unavailable`], logged at warn — same posture as
    /// `McpProvider`. The provider stays registered and simply contributes
    /// nothing, so one broken plugin can't wedge a turn.
    pub async fn connect(plugin: Arc<dyn Plugin>) -> Self {
        let id = format!("plugin:{}", plugin.name());
        match plugin.list_tools().await {
            Ok(defs) => {
                let tools = build_tools(plugin.name(), defs, &plugin);
                let health = if tools.is_empty() {
                    // Reachable but advertises nothing: degraded, not ready.
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Ready
                };
                tracing::info!(
                    plugin = %plugin.name(),
                    tools = tools.len(),
                    "plugin ready"
                );
                Self { id, tools, health }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin.name(),
                    error = %e,
                    "plugin list_tools failed; contributing no tools"
                );
                Self {
                    id,
                    tools: Vec::new(),
                    health: ProviderHealth::Unavailable,
                }
            }
        }
    }
}

/// Wrap a plugin's advertised tools into namespaced [`SharedTool`]s, each bound
/// to the shared plugin handle so `execute` forwards to the live plugin.
fn build_tools(
    plugin_name: &str,
    defs: Vec<PluginTool>,
    plugin: &Arc<dyn Plugin>,
) -> Vec<SharedTool> {
    defs.into_iter()
        .map(|def| {
            let tool: SharedTool = Arc::new(PluginAgentTool {
                namespaced_name: format!("plugin{NS}{plugin_name}{NS}{}", def.name),
                remote_name: def.name,
                description: def.description.unwrap_or_else(|| "plugin tool".to_string()),
                parameters: normalize_schema(def.input_schema),
                plugin: plugin.clone(),
            });
            tool
        })
        .collect()
}

#[async_trait]
impl CapabilityProvider for PluginProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        // Cheap: clone the cached snapshot's `Arc`s.
        self.tools.clone()
    }

    async fn health(&self) -> ProviderHealth {
        self.health
    }
}

/// A single plugin tool presented to the agent as an [`AgentTool`]. `execute`
/// forwards to the live plugin via [`Plugin::invoke_tool`].
struct PluginAgentTool {
    namespaced_name: String,
    remote_name: String,
    description: String,
    parameters: Value,
    plugin: Arc<dyn Plugin>,
}

#[async_trait]
impl AgentTool for PluginAgentTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    /// Plugin tools run arbitrary out-of-process code; require permission by
    /// default, same as `bash`/`write`/`edit` and MCP tools. The daemon's
    /// existing `PermissionPolicy` gates the call — plugins never bypass it.
    fn requires_permission(&self) -> bool {
        true
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        match self.plugin.invoke_tool(&self.remote_name, args).await {
            Ok(value) => Ok(AgentToolResult {
                content: vec![value_to_content(value)],
                ..Default::default()
            }),
            // Surface the plugin error as an error tool result string so the
            // model can react, not a hard loop failure.
            Err(e) => Err(format!("plugin tool `{}` failed: {e}", self.remote_name)),
        }
    }
}

/// Map a plugin tool's opaque JSON result into a single content block. A plain
/// JSON string passes through verbatim; any other value is rendered as compact
/// JSON so structured results reach the model losslessly.
fn value_to_content(value: Value) -> Content {
    match value {
        Value::String(s) => Content::text(s),
        other => Content::text(other.to_string()),
    }
}

/// Ensure the advertised schema is a JSON object (providers require an object
/// schema). A plugin that omits or mis-types `inputSchema` gets a permissive
/// empty object — same normalization `ocean-mcp` applies.
fn normalize_schema(schema: Value) -> Value {
    if schema.is_object() {
        schema
    } else {
        serde_json::json!({ "type": "object" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginError;
    use serde_json::json;

    /// An in-process fake plugin: serves a fixed tool list and echoes invoke
    /// args back under `{ "echoed": <args> }`, so the adapter can be tested
    /// without spawning a subprocess.
    struct FakePlugin {
        tools: Vec<PluginTool>,
        fail_list: bool,
    }

    #[async_trait]
    impl Plugin for FakePlugin {
        fn name(&self) -> &str {
            "echo-pack"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        async fn list_tools(&self) -> Result<Vec<PluginTool>, PluginError> {
            if self.fail_list {
                return Err(PluginError::Protocol("boom".into()));
            }
            Ok(self.tools.clone())
        }
        async fn invoke_tool(&self, name: &str, args: Value) -> Result<Value, PluginError> {
            if name == "echo" {
                Ok(json!({ "echoed": args }))
            } else {
                Err(PluginError::UnknownTool(name.to_string()))
            }
        }
    }

    fn ctx() -> SessionContext {
        SessionContext::default()
    }

    fn echo_plugin() -> Arc<dyn Plugin> {
        Arc::new(FakePlugin {
            tools: vec![PluginTool {
                name: "echo".into(),
                description: Some("Echo the args back".into()),
                input_schema: json!({ "type": "object" }),
            }],
            fail_list: false,
        })
    }

    #[tokio::test]
    async fn provider_namespaces_and_lists_plugin_tools() {
        let provider = PluginProvider::connect(echo_plugin()).await;
        assert_eq!(provider.id(), "plugin:echo-pack");
        assert_eq!(provider.health().await, ProviderHealth::Ready);

        let tools = provider.tools(&ctx()).await;
        assert_eq!(tools.len(), 1);
        // Namespaced so it can't collide with a built-in or an MCP tool.
        assert_eq!(tools[0].name(), "plugin__echo-pack__echo");
        assert_eq!(tools[0].description(), "Echo the args back");
        // Plugin tools require permission by default.
        assert!(tools[0].requires_permission());
    }

    #[tokio::test]
    async fn execute_forwards_to_invoke_tool() {
        let provider = PluginProvider::connect(echo_plugin()).await;
        let tools = provider.tools(&ctx()).await;
        let res = tools[0]
            .execute("c1", json!({ "message": "hi" }))
            .await
            .expect("invoke succeeds");
        // The echo plugin returns { "echoed": <args> }; the adapter renders the
        // structured result as JSON text reaching the model.
        let text = match &res.content[0] {
            Content::Text { text } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("echoed"), "got: {text}");
        assert!(text.contains("hi"), "got: {text}");
    }

    #[tokio::test]
    async fn invoke_error_surfaces_as_error_result() {
        // A tool whose name the plugin rejects.
        let provider = PluginProvider::connect(Arc::new(FakePlugin {
            tools: vec![PluginTool::new("nope", json!({ "type": "object" }))],
            fail_list: false,
        }))
        .await;
        let tools = provider.tools(&ctx()).await;
        let err = tools[0]
            .execute("c1", json!({}))
            .await
            .expect_err("unknown tool errors");
        assert!(err.contains("failed"), "got: {err}");
    }

    #[tokio::test]
    async fn failed_list_yields_empty_unavailable_provider() {
        let provider = PluginProvider::connect(Arc::new(FakePlugin {
            tools: vec![],
            fail_list: true,
        }))
        .await;
        assert_eq!(provider.health().await, ProviderHealth::Unavailable);
        assert!(provider.tools(&ctx()).await.is_empty());
    }

    #[tokio::test]
    async fn composes_into_capability_registry_with_builtins() {
        use ocean_runtime::capability::{BuiltinProvider, CapabilityRegistry};

        let registry = CapabilityRegistry::new(vec![
            Arc::new(BuiltinProvider::new()),
            Arc::new(PluginProvider::connect(echo_plugin()).await),
        ]);
        let names: Vec<String> = registry
            .tools_for_session(&ctx())
            .await
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        // Built-ins survive; the plugin tool is appended, namespaced, last.
        assert!(names.iter().any(|n| n == "bash"), "built-ins present");
        assert!(
            names.iter().any(|n| n == "plugin__echo-pack__echo"),
            "plugin tool present, got: {names:?}"
        );
    }
}
