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
use tokio::sync::RwLock;
use tokio::time::Duration;

use crate::client::{McpClient, McpToolDef};
use crate::config::{McpServerConfig, McpTransportKind};
use crate::transport::{HttpTransport, StdioTransport};

/// Separator between the server name and the remote tool name in the namespaced
/// id exposed to the agent. Double underscore matches the convention used by
/// other MCP hosts and is unlikely to appear in a built-in tool name.
const NS: &str = "__";

/// The cached tool list, behind an async lock and an `Arc` so it can be swapped
/// atomically when the server announces `tools/list_changed`. Readers clone the
/// inner `Arc` under a short read lock; the watcher swaps the whole `Arc` under
/// a write lock — so an in-flight `tools()` call never observes a partial list.
type ToolCache = Arc<RwLock<Arc<Vec<SharedTool>>>>;

/// A provider backed by a single MCP server.
pub struct McpProvider {
    id: String,
    /// Cached, namespaced tools. Seeded at connect time and atomically swapped
    /// by the background watcher on `tools/list_changed` (OCEAN-32). Empty if
    /// the server failed to start (provider stays registered but contributes
    /// nothing).
    tools: ToolCache,
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

        if cfg.transport == McpTransportKind::Http {
            // Streamable-HTTP transport (OCEAN-166). The endpoint URL is carried
            // in `command` for HTTP servers. Construct the transport (fails fast
            // with a typed error on a bad/empty URL, OCEAN-47), then run the SAME
            // handshake + discovery the stdio path does — so an HTTP MCP server
            // actually contributes its tools instead of silently yielding none.
            let endpoint = cfg.command.clone().unwrap_or_default();
            let transport = match HttpTransport::connect(&endpoint).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(
                        server = %cfg.name,
                        endpoint = %endpoint,
                        error = %e,
                        "MCP HTTP endpoint is invalid; contributing no tools"
                    );
                    return Ok(Self::empty(id, ProviderHealth::Unavailable));
                }
            };

            // Best-effort, exactly like stdio: a server that fails to handshake
            // or list tools folds into an empty, Unavailable provider rather than
            // blocking startup.
            match Self::handshake(&cfg.name, Box::new(transport), connect_timeout).await {
                Ok((client, defs)) => {
                    let tools = build_tools(&cfg.name, defs, &client);
                    tracing::info!(
                        server = %cfg.name,
                        endpoint = %endpoint,
                        tools = tools.len(),
                        "MCP HTTP server ready"
                    );
                    let health = if tools.is_empty() {
                        ProviderHealth::Degraded
                    } else {
                        ProviderHealth::Ready
                    };
                    let cache: ToolCache = Arc::new(RwLock::new(Arc::new(tools)));
                    let signal = client.tools_changed_signal();
                    spawn_tools_changed_watcher(cfg.name.clone(), client, cache.clone(), signal);
                    return Ok(Self {
                        id,
                        tools: cache,
                        health,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        server = %cfg.name,
                        endpoint = %endpoint,
                        error = %e,
                        "MCP HTTP server unavailable; contributing no tools"
                    );
                    return Ok(Self::empty(id, ProviderHealth::Unavailable));
                }
            }
        }

        let command = match &cfg.command {
            Some(c) => c.clone(),
            None => anyhow::bail!(
                "MCP server `{}` has stdio transport but no command",
                cfg.name
            ),
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
            Ok((client, defs)) => {
                let tools = build_tools(&cfg.name, defs, &client);
                tracing::info!(server = %cfg.name, tools = tools.len(), "MCP server ready");
                // A server that started cleanly but advertises no tools is
                // reachable-but-useless: Degraded, not Ready.
                let health = if tools.is_empty() {
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Ready
                };
                let cache: ToolCache = Arc::new(RwLock::new(Arc::new(tools)));
                // Watch for tools/list_changed and refresh the cache in the
                // background (OCEAN-32).
                let signal = client.tools_changed_signal();
                spawn_tools_changed_watcher(cfg.name.clone(), client, cache.clone(), signal);
                Ok(Self {
                    id,
                    tools: cache,
                    health,
                })
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
            tools: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            health,
        }
    }

    /// The fallible core of the stdio connect: spawn the child, then run the
    /// shared [`handshake`](Self::handshake). Tool *wrapping* happens in
    /// [`build_tools`] so the same path serves connect and a later
    /// `tools/list_changed` refresh.
    async fn connect_inner(
        server_name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        connect_timeout: Duration,
    ) -> anyhow::Result<(Arc<McpClient>, Vec<McpToolDef>)> {
        let transport = StdioTransport::spawn(command, args, env)?;
        Self::handshake(server_name, Box::new(transport), connect_timeout).await
    }

    /// Transport-agnostic handshake + discovery: wrap any [`Transport`] in a
    /// client, run `initialize` → `tools/list` under one overall deadline, and
    /// return the shared client and discovered tool defs. Shared by the stdio and
    /// streamable-HTTP paths (OCEAN-166) so both negotiate identically.
    ///
    /// Bounded by `connect_timeout` end-to-end so a hung server can't wedge
    /// startup. The client keeps its default per-call timeout (30s) for live tool
    /// calls — we deliberately do NOT shrink it to `connect_timeout`, which would
    /// also throttle every later `tools/call` to the (short) connect budget.
    ///
    /// The client multiplexes internally (OCEAN-44): its methods take `&self` and
    /// a single I/O task owns the transport, so concurrent callers don't
    /// serialize. We therefore share it as a plain `Arc`, with no outer mutex.
    async fn handshake(
        server_name: &str,
        transport: Box<dyn crate::transport::Transport + Send>,
        connect_timeout: Duration,
    ) -> anyhow::Result<(Arc<McpClient>, Vec<McpToolDef>)> {
        let client = Arc::new(McpClient::new(transport));

        let defs: Vec<McpToolDef> = tokio::time::timeout(connect_timeout, async {
            client.initialize("ocean").await?;
            client.list_tools().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP server `{server_name}` connect timed out"))??;

        Ok((client, defs))
    }
}

/// Wrap discovered tool definitions into namespaced [`SharedTool`]s bound to the
/// shared client. Shared by initial connect and `tools/list_changed` refresh so
/// both produce identically-shaped tools.
fn build_tools(
    server_name: &str,
    defs: Vec<McpToolDef>,
    client: &Arc<McpClient>,
) -> Vec<SharedTool> {
    defs.into_iter()
        .map(|def| {
            let tool: SharedTool = Arc::new(McpTool {
                namespaced_name: format!("mcp{NS}{server_name}{NS}{}", def.name),
                remote_name: def.name,
                description: def.description.unwrap_or_else(|| "MCP tool".to_string()),
                parameters: normalize_schema(def.input_schema),
                client: client.clone(),
            });
            tool
        })
        .collect()
}

/// How many times to attempt a `tools/list` re-fetch after a
/// `tools/list_changed` notification before giving up and keeping the previous
/// snapshot. A server that just changed its tools but briefly errors on re-list
/// would otherwise leave the agent calling a stale tool table until the next
/// notification — which may never come (OCEAN-175).
const REFETCH_MAX_ATTEMPTS: u32 = 4;

/// Base delay for the bounded exponential backoff between re-fetch attempts.
/// Delays grow `BASE`, `BASE*2`, `BASE*4`, … capped at [`REFETCH_MAX_BACKOFF`].
const REFETCH_BASE_BACKOFF: Duration = Duration::from_millis(200);

/// Ceiling for the per-attempt backoff delay so a flapping server can't push the
/// retry spacing arbitrarily high.
const REFETCH_MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Re-fetch the tool list with bounded retry + exponential backoff (OCEAN-175).
///
/// Calls `fetch` up to [`REFETCH_MAX_ATTEMPTS`] times, sleeping a capped,
/// exponentially-growing delay between attempts. Returns the first successful
/// list, or the last error after exhausting attempts — it never spins forever.
/// Generic over the fetcher and the sleeper so the retry/backoff policy is
/// unit-testable without a live MCP server (tests inject a no-op sleep and a
/// counting fetcher); the real watcher passes `client.list_tools()` and
/// `tokio::time::sleep`.
async fn refetch_tools_with_retry<Fetch, Fut, Sleep, SleepFut>(
    server_name: &str,
    mut fetch: Fetch,
    mut sleep: Sleep,
) -> anyhow::Result<Vec<McpToolDef>>
where
    Fetch: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Vec<McpToolDef>>>,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: std::future::Future<Output = ()>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=REFETCH_MAX_ATTEMPTS {
        match fetch().await {
            Ok(defs) => return Ok(defs),
            Err(e) => {
                last_err = Some(e);
                // Don't sleep after the final attempt — give up immediately.
                if attempt < REFETCH_MAX_ATTEMPTS {
                    // Exponential backoff, capped: BASE * 2^(attempt-1).
                    let backoff = REFETCH_BASE_BACKOFF
                        .saturating_mul(1u32 << (attempt - 1))
                        .min(REFETCH_MAX_BACKOFF);
                    tracing::warn!(
                        server = %server_name,
                        attempt,
                        max_attempts = REFETCH_MAX_ATTEMPTS,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %last_err.as_ref().unwrap(),
                        "MCP tools/list re-fetch failed; retrying after backoff"
                    );
                    sleep(backoff).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("MCP tools/list re-fetch failed")))
}

/// Spawn a background task that, each time the server announces
/// `tools/list_changed`, re-fetches `tools/list` and atomically swaps the cached
/// snapshot (OCEAN-32). The re-fetch is retried with bounded exponential backoff
/// (OCEAN-175) so a transient error right after a change doesn't strand the agent
/// on a stale tool table; only after every attempt fails do we give up and keep
/// the previous snapshot. The task ends when the client is dropped (the `Notify`
/// handle's last sender goes away) — i.e. when the provider is torn down.
fn spawn_tools_changed_watcher(
    server_name: String,
    client: Arc<McpClient>,
    cache: ToolCache,
    signal: Arc<tokio::sync::Notify>,
) {
    tokio::spawn(async move {
        loop {
            signal.notified().await;
            tracing::info!(server = %server_name, "refreshing MCP tool list after list_changed");
            let fetched =
                refetch_tools_with_retry(&server_name, || client.list_tools(), tokio::time::sleep)
                    .await;
            match fetched {
                Ok(defs) => {
                    let count = defs.len();
                    let rebuilt = build_tools(&server_name, defs, &client);
                    // Atomic swap: replace the whole Arc under a write lock so a
                    // concurrent reader sees either the old or new list, never a
                    // partial one.
                    *cache.write().await = Arc::new(rebuilt);
                    tracing::info!(
                        server = %server_name,
                        tools = count,
                        "MCP tool list refreshed after list_changed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        attempts = REFETCH_MAX_ATTEMPTS,
                        error = %e,
                        "MCP tools/list re-fetch failed after all retries; keeping previous tool list"
                    );
                }
            }
        }
    });
}

#[async_trait]
impl CapabilityProvider for McpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        // Clone the current snapshot's contents under a short read lock. Cloning
        // the `SharedTool` Arcs is cheap; the lock is released immediately.
        let snapshot = self.tools.read().await.clone();
        (*snapshot).clone()
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
    client: Arc<McpClient>,
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
        // No lock across the call: the client multiplexes internally, so a slow
        // call here does not block other sessions' calls to the same server
        // (OCEAN-44).
        match self.client.call_tool(&self.remote_name, args).await {
            Ok(res) if res.is_error => {
                // Tool-execution error: surface as an error tool result so the
                // model can react, not as a hard failure.
                let text = res.text();
                Err(if text.is_empty() {
                    format!("MCP tool `{}` reported an error", self.remote_name)
                } else {
                    text
                })
            }
            Ok(res) => Ok(AgentToolResult {
                // Preserve the full content blocks (text + image), so image
                // results reach the model instead of being flattened to a
                // placeholder string.
                content: res.content,
                ..Default::default()
            }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn def(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
        }
    }

    /// A `sleep` that records each requested delay but never actually waits, so
    /// the retry/backoff policy can be tested without spending real time.
    fn noop_sleep(slept: &Cell<u32>) -> impl FnMut(Duration) -> std::future::Ready<()> + '_ {
        move |_d| {
            slept.set(slept.get() + 1);
            std::future::ready(())
        }
    }

    // OCEAN-175: a re-fetch that fails N-1 times then succeeds must NOT leave the
    // cache stale — the helper retries and returns the eventual success.
    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let calls = Cell::new(0u32);
        let slept = Cell::new(0u32);
        // Fail on the first (MAX-1) attempts, then succeed on the last one.
        let fetch = || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < REFETCH_MAX_ATTEMPTS {
                    Err(anyhow::anyhow!("transient list error #{n}"))
                } else {
                    Ok(vec![def("alpha"), def("beta")])
                }
            }
        };

        let result = refetch_tools_with_retry("srv", fetch, noop_sleep(&slept)).await;

        let defs = result.expect("should eventually succeed");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "alpha");
        // Exactly MAX attempts were made, with a backoff sleep before each retry.
        assert_eq!(calls.get(), REFETCH_MAX_ATTEMPTS);
        assert_eq!(slept.get(), REFETCH_MAX_ATTEMPTS - 1);
    }

    // OCEAN-175: if every attempt fails, the helper gives up (no infinite spin),
    // returns the last error, and does not sleep after the final attempt. The
    // caller (watcher) then keeps the previous snapshot.
    #[tokio::test]
    async fn retry_gives_up_after_all_attempts_fail() {
        let calls = Cell::new(0u32);
        let slept = Cell::new(0u32);
        let fetch = || {
            let n = calls.get() + 1;
            calls.set(n);
            async move { Err::<Vec<McpToolDef>, _>(anyhow::anyhow!("persistent error #{n}")) }
        };

        let result = refetch_tools_with_retry("srv", fetch, noop_sleep(&slept)).await;

        let err = result.expect_err("should give up and return the last error");
        // Last error is surfaced, not a generic one.
        assert!(err.to_string().contains(&format!("#{REFETCH_MAX_ATTEMPTS}")));
        // Bounded: exactly MAX attempts, capped — never spins forever.
        assert_eq!(calls.get(), REFETCH_MAX_ATTEMPTS);
        // No sleep after the final attempt.
        assert_eq!(slept.get(), REFETCH_MAX_ATTEMPTS - 1);
    }

    // A first-attempt success short-circuits: no retries, no sleeps.
    #[tokio::test]
    async fn retry_first_attempt_success_no_backoff() {
        let calls = Cell::new(0u32);
        let slept = Cell::new(0u32);
        let fetch = || {
            calls.set(calls.get() + 1);
            async move { Ok(vec![def("only")]) }
        };

        let defs = refetch_tools_with_retry("srv", fetch, noop_sleep(&slept))
            .await
            .expect("first attempt should succeed");

        assert_eq!(defs.len(), 1);
        assert_eq!(calls.get(), 1);
        assert_eq!(slept.get(), 0);
    }

    // Backoff grows exponentially and is capped at REFETCH_MAX_BACKOFF, so a
    // flapping server can't push the retry spacing arbitrarily high.
    #[tokio::test]
    async fn backoff_is_exponential_and_capped() {
        let recorded: std::cell::RefCell<Vec<Duration>> = std::cell::RefCell::new(Vec::new());
        let fetch = || async { Err::<Vec<McpToolDef>, _>(anyhow::anyhow!("always fails")) };
        let sleep = |d: Duration| {
            recorded.borrow_mut().push(d);
            std::future::ready(())
        };

        let _ = refetch_tools_with_retry("srv", fetch, sleep).await;

        let delays = recorded.borrow();
        assert_eq!(delays.len() as u32, REFETCH_MAX_ATTEMPTS - 1);
        // Each successive delay is >= the previous (monotonic), and none exceeds
        // the cap.
        for w in delays.windows(2) {
            assert!(w[1] >= w[0], "backoff should not decrease: {w:?}");
        }
        for d in delays.iter() {
            assert!(*d <= REFETCH_MAX_BACKOFF, "backoff exceeded cap: {d:?}");
        }
        // First retry uses the base delay.
        assert_eq!(delays[0], REFETCH_BASE_BACKOFF);
    }
}
