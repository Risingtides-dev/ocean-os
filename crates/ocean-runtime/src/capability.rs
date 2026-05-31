//! Capability registry: the seam between the agent loop and its tool sources.
//!
//! The agent loop never builds tools directly. It asks a [`CapabilityRegistry`]
//! for the tools available to a session. The registry holds an ordered list of
//! [`CapabilityProvider`]s — sources of tools. The built-in toolset is just one
//! provider ([`BuiltinProvider`]); later, an MCP client and skill packs register
//! as additional providers behind the same trait. Nothing downstream of the
//! registry knows or cares where a tool came from.
//!
//! This is the abstraction the project's Goose-comparison audit calls for: tools
//! load *dynamically* through one seam instead of being hardcoded into the agent.
//! Adding a new tool source must never mean editing the agent loop or this crate
//! — the `ocean-mcp` crate, for example, depends on `ocean-runtime` and registers
//! an `McpProvider`; this crate never depends back on it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::tools::default_tools;
use crate::types::AgentTool;

/// Shared, reference-counted tool handle. The loop and registry pass these
/// around; cloning is a cheap `Arc` bump.
pub type SharedTool = Arc<dyn AgentTool>;

/// Per-turn context a provider may use to decide which tools to offer.
///
/// `AgentTool::execute` itself takes no context (tools resolve cwd from the
/// process and permissions are gated separately on `AgentConfig`), so this is
/// *only* a selection hint: a provider can vary its toolset by workspace or
/// session. Built-ins ignore it. Kept deliberately small; grow it (room id,
/// selected skills) rather than inventing a parallel context type.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    /// Working directory for this turn.
    pub cwd: PathBuf,
    /// Session id, when the turn belongs to a known session.
    pub session_id: Option<String>,
}

/// Coarse health of a capability provider, surfaced for diagnostics.
///
/// Built-ins are always [`Ready`](ProviderHealth::Ready). A remote provider
/// (e.g. an MCP server) reports [`Degraded`](ProviderHealth::Degraded) when it
/// is reachable but partial, or [`Unavailable`](ProviderHealth::Unavailable)
/// when its tools can't be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    Ready,
    Degraded,
    Unavailable,
}

/// A source of tools for the agent.
///
/// **Caching is the provider's responsibility, not the registry's.** Built-ins
/// hold their `Vec` and clone it (cheap `Arc` bumps). A remote provider must
/// cache its discovered toolset internally and refresh on its own signals, so
/// that `tools()` stays cheap enough to call once per turn and never blocks the
/// turn on a live network/process handshake.
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Stable identifier for this provider, used in diagnostics and dedup
    /// warnings (e.g. `"builtin"`, `"mcp:linear"`).
    fn id(&self) -> &str;

    /// Tools this provider offers for the given session context.
    ///
    /// Must be cheap — it is called once at the start of every turn. Do any
    /// network/process work behind an internal cache.
    async fn tools(&self, ctx: &SessionContext) -> Vec<SharedTool>;

    /// Coarse health for diagnostics. Defaults to [`ProviderHealth::Ready`].
    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}

/// The built-in toolset, wrapped as a [`CapabilityProvider`].
///
/// Constructs the built-in tools once via [`default_tools`] and returns a clone
/// on each `tools()` call. Behaviour-preserving: the exact set, in the exact
/// order, that the agent loop used before the registry existed.
pub struct BuiltinProvider {
    tools: Vec<SharedTool>,
}

impl BuiltinProvider {
    pub fn new() -> Self {
        Self {
            tools: default_tools(),
        }
    }
}

impl Default for BuiltinProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CapabilityProvider for BuiltinProvider {
    fn id(&self) -> &str {
        "builtin"
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        self.tools.clone()
    }
}

/// Ordered set of capability providers, flattened per session.
///
/// Providers are consulted in order. The first provider to claim a tool name
/// wins; later providers offering the same name are dropped with a warning.
/// Because built-ins are registered first, they can never be shadowed by a later
/// provider (an MCP server can't override `bash`).
pub struct CapabilityRegistry {
    providers: Vec<Arc<dyn CapabilityProvider>>,
}

impl std::fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn CapabilityProvider` isn't Debug; list provider ids instead.
        f.debug_struct("CapabilityRegistry")
            .field(
                "providers",
                &self.providers.iter().map(|p| p.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl CapabilityRegistry {
    /// Build a registry from an ordered list of providers. Order is significant:
    /// earlier providers win name collisions. Put built-ins first.
    pub fn new(providers: Vec<Arc<dyn CapabilityProvider>>) -> Self {
        Self { providers }
    }

    /// Convenience constructor: built-in tools only. Equivalent to the old
    /// `default_tools()` path, for the daemon's default and for tests.
    pub fn builtin_only() -> Self {
        Self::new(vec![Arc::new(BuiltinProvider::new())])
    }

    /// The registered providers, in order.
    pub fn providers(&self) -> &[Arc<dyn CapabilityProvider>] {
        &self.providers
    }

    /// Flatten all providers' tools for this session, deduped by name
    /// (first-wins). The result is guaranteed to have unique tool names — the
    /// invariant the agent loop's dispatch map relies on.
    pub async fn tools_for_session(&self, ctx: &SessionContext) -> Vec<SharedTool> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<SharedTool> = Vec::new();
        for provider in &self.providers {
            for tool in provider.tools(ctx).await {
                let name = tool.name().to_string();
                if seen.insert(name.clone()) {
                    out.push(tool);
                } else {
                    tracing::warn!(
                        tool = %name,
                        provider = %provider.id(),
                        "duplicate tool name; keeping earlier provider's tool"
                    );
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentToolResult;
    use serde_json::{json, Value};

    fn ctx() -> SessionContext {
        SessionContext {
            cwd: PathBuf::from("/tmp"),
            session_id: Some("test-session".into()),
        }
    }

    /// A minimal test-only tool with a configurable name and marker description.
    struct FakeTool {
        name: String,
        desc: String,
    }
    #[async_trait]
    impl AgentTool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.desc
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            Ok(AgentToolResult::text("fake"))
        }
    }

    /// A test-only provider returning a fixed list of tools.
    struct FakeProvider {
        id: String,
        tools: Vec<SharedTool>,
    }
    #[async_trait]
    impl CapabilityProvider for FakeProvider {
        fn id(&self) -> &str {
            &self.id
        }
        async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
            self.tools.clone()
        }
    }

    fn names(tools: &[SharedTool]) -> Vec<String> {
        tools.iter().map(|t| t.name().to_string()).collect()
    }

    #[tokio::test]
    async fn builtin_provider_returns_all_default_tools() {
        let provider = BuiltinProvider::new();
        let got = provider.tools(&ctx()).await;
        let expected = default_tools();
        assert_eq!(names(&got), names(&expected));
    }

    #[tokio::test]
    async fn registry_builtin_only_matches_default_tools() {
        let registry = CapabilityRegistry::builtin_only();
        let got = registry.tools_for_session(&ctx()).await;
        let expected = default_tools();
        // Behaviour preservation: the exact vec (names + order) that used to
        // reach run_agent still does.
        assert_eq!(names(&got), names(&expected));
    }

    #[tokio::test]
    async fn registry_merges_second_provider() {
        let fake = Arc::new(FakeProvider {
            id: "fake".into(),
            tools: vec![Arc::new(FakeTool {
                name: "fake_tool".into(),
                desc: "a fake".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), fake]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        assert!(got.contains(&"fake_tool".to_string()));
        assert_eq!(got.last().unwrap(), "fake_tool", "extra tool appended last");
        for n in names(&default_tools()) {
            assert!(got.contains(&n), "missing built-in {n}");
        }
    }

    #[tokio::test]
    async fn registry_dedup_first_wins_builtins_cannot_be_shadowed() {
        // A provider that tries to shadow `bash`.
        let shadow = Arc::new(FakeProvider {
            id: "shadow".into(),
            tools: vec![Arc::new(FakeTool {
                name: "bash".into(),
                desc: "SHADOW".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), shadow]);
        let got = registry.tools_for_session(&ctx()).await;
        let bashes: Vec<_> = got.iter().filter(|t| t.name() == "bash").collect();
        assert_eq!(bashes.len(), 1, "exactly one bash survives dedup");
        assert_ne!(
            bashes[0].description(),
            "SHADOW",
            "built-in bash wins over the shadow"
        );
    }

    #[tokio::test]
    async fn registry_preserves_order_across_providers() {
        let a = Arc::new(FakeProvider {
            id: "a".into(),
            tools: vec![Arc::new(FakeTool {
                name: "a_tool".into(),
                desc: "".into(),
            })],
        });
        let b = Arc::new(FakeProvider {
            id: "b".into(),
            tools: vec![Arc::new(FakeTool {
                name: "b_tool".into(),
                desc: "".into(),
            })],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), a, b]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        let i_a = got.iter().position(|n| n == "a_tool").unwrap();
        let i_b = got.iter().position(|n| n == "b_tool").unwrap();
        let n_builtin = names(&default_tools()).len();
        assert!(i_a < i_b, "provider order preserved");
        assert!(i_a >= n_builtin, "built-ins come first");
    }

    #[tokio::test]
    async fn tools_for_session_has_unique_names() {
        // The invariant run_agent's dispatch map relies on.
        let dup = Arc::new(FakeProvider {
            id: "dup".into(),
            tools: vec![
                Arc::new(FakeTool {
                    name: "read".into(),
                    desc: "".into(),
                }),
                Arc::new(FakeTool {
                    name: "x".into(),
                    desc: "".into(),
                }),
                Arc::new(FakeTool {
                    name: "x".into(),
                    desc: "".into(),
                }),
            ],
        });
        let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new()), dup]);
        let got = names(&registry.tools_for_session(&ctx()).await);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            got.len(),
            "no duplicate tool names reach the loop"
        );
    }
}
