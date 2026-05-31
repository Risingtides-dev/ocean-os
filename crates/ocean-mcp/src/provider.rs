//! `McpProvider` — adapts one MCP server connection to the runtime's
//! [`CapabilityProvider`] seam. Its tools are exposed to the agent namespaced
//! `mcp__<server>__<tool>`, so a server can never collide with a built-in and
//! two servers can't collide with each other.
//!
//! Discovery happens once, at construction (`connect`), with a timeout and
//! non-fatally: a server that fails to start or list its tools yields an empty
//! provider that simply contributes nothing — it never blocks daemon startup or
//! a turn. After connect, `tools()` serves the cached snapshot; calls go back
//! to the live server through a `Mutex`-guarded client.

use std::sync::Arc;

use async_trait::async_trait;
use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Duration;

use crate::client::{McpClient, McpToolDef};
use crate::config::{McpServerConfig, McpTransportKind};
use crate::transport::StdioTransport;

/// Separator between the server name and the remote tool name in the namespaced
/// id exposed to the agent. Double underscore matches the convention used by
/// other MCP hosts and is unlikely to appear in a built-in tool name.
const NS: &str = "__";

/// A provider backed by a single MCP server.
pub struct McpProvider {
    id: String,
    /// Cached, namespaced tools discovered at connect time. Empty if the server
    /// failed to start (provider stays registered but contributes nothing).
    tools: Vec<SharedTool>,
    health: ProviderHealth,
}

impl McpProvider {
    /// Connect to a configured server, run the handshake, and discover its
    /// tools — all bounded by `connect_timeout`. Never returns `Err` for a
    /// server-side failure: a broken server produces a healthy-typed but empty
    /// provider with `ProviderHealth::Unavailable`, logged at warn. Returns
    /// `Err` only for a genuinely unusable *config* (e.g. missing command).
    pub async fn connect<F>(
        cfg: &McpServerConfig,
        env_lookup: F,
        connect_timeout: Duration,
    ) -> anyhow::Result<Self>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let id = format!("mcp:{}", cfg.name);

        if !cfg.enabled {
            tracing::info!(server = %cfg.name, "MCP server disabled; skipping");
            return Ok(Self::empty(id, ProviderHealth::Unavailable));
        }

        if cfg.transport != McpTransportKind::Stdio {
            tracing::warn!(
                server = %cfg.name,
                "only stdio MCP transport is implemented; skipping this server"
            );
            return Ok(Self::empty(id, ProviderHealth::Unavailable));
        }

        let command = match &cfg.command {
            Some(c) => c.clone(),
            None => anyhow::bail!("MCP server `{}` has stdio transport but no command", cfg.name),
        };

        // Resolve declared secrets by name. Missing ones are a warning; we log
        // only the NAMES, never values.
        let (env, missing) = cfg.resolve_env(env_lookup);
        if !missing.is_empty() {
            tracing::warn!(
                server = %cfg.name,
                missing = ?missing,
                "MCP server is missing required env vars; starting without them"
            );
        }

        // Everything below is best-effort; fold failures into an empty provider.
        match Self::connect_inner(&cfg.name, &command, &cfg.args, &env, connect_timeout).await {
            Ok(tools) => {
                tracing::info!(server = %cfg.name, tools = tools.len(), "MCP server ready");
                // A server that started cleanly but advertises no tools is
                // reachable-but-useless: Degraded, not Ready.
                let health = if tools.is_empty() {
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Ready
                };
                Ok(Self { id, tools, health })
            }
            Err(e) => {
                tracing::warn!(server = %cfg.name, error = %e, "MCP server unavailable; contributing no tools");
                Ok(Self::empty(id, ProviderHealth::Unavailable))
            }
        }
    }

    fn empty(id: String, health: ProviderHealth) -> Self {
        Self {
            id,
            tools: Vec::new(),
            health,
        }
    }

    /// The fallible core of connect: spawn, handshake, list, wrap. Bounded by
    /// `connect_timeout` end-to-end so a hung server can't wedge startup.
    async fn connect_inner(
        server_name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        connect_timeout: Duration,
    ) -> anyhow::Result<Vec<SharedTool>> {
        let transport = StdioTransport::spawn(command, args, env)?;
        // Keep the client's default per-call timeout (30s) for live tool calls.
        // The handshake below is bounded separately by the outer `timeout`, so we
        // deliberately do NOT shrink the client's request timeout to
        // `connect_timeout` here — doing so would also throttle every later
        // `tools/call` to the (short) connect budget.
        let client = McpClient::new(Box::new(transport));
        let client = Arc::new(Mutex::new(client));

        // Handshake + discovery under one overall deadline.
        let defs: Vec<McpToolDef> = tokio::time::timeout(connect_timeout, async {
            let mut guard = client.lock().await;
            guard.initialize("ocean").await?;
            guard.list_tools().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP server `{server_name}` connect timed out"))??;

        let tools: Vec<SharedTool> = defs
            .into_iter()
            .map(|def| {
                let tool: SharedTool = Arc::new(McpTool {
                    namespaced_name: format!("mcp{NS}{server_name}{NS}{}", def.name),
                    remote_name: def.name,
                    description: def
                        .description
                        .unwrap_or_else(|| "MCP tool".to_string()),
                    parameters: normalize_schema(def.input_schema),
                    client: client.clone(),
                });
                tool
            })
            .collect();

        Ok(tools)
    }
}

#[async_trait]
impl CapabilityProvider for McpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        self.tools.clone()
    }

    async fn health(&self) -> ProviderHealth {
        self.health
    }
}

/// A single MCP tool, presented to the agent as an [`AgentTool`]. `execute`
/// forwards to the live server via the shared client.
struct McpTool {
    namespaced_name: String,
    remote_name: String,
    description: String,
    parameters: Value,
    client: Arc<Mutex<McpClient>>,
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    /// MCP tools mutate external systems; require permission by default, same as
    /// `bash`/`write`/`edit`.
    fn requires_permission(&self) -> bool {
        true
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        let mut client = self.client.lock().await;
        match client.call_tool(&self.remote_name, args).await {
            Ok(res) if res.is_error => {
                // Tool-execution error: surface as an error tool result so the
                // model can react, not as a hard failure.
                Err(if res.text.is_empty() {
                    format!("MCP tool `{}` reported an error", self.remote_name)
                } else {
                    res.text
                })
            }
            Ok(res) => Ok(AgentToolResult::text(res.text)),
            Err(e) => Err(format!("MCP call failed: {e}")),
        }
    }
}

/// Ensure the advertised schema is a JSON object (providers require an object
/// schema). A server that omits `inputSchema` gets a permissive empty object.
fn normalize_schema(schema: Value) -> Value {
    if schema.is_object() {
        schema
    } else {
        serde_json::json!({ "type": "object" })
    }
}
