//! [`SubprocessPlugin`] — a [`Plugin`] backed by a child process spoken to over
//! JSON-RPC 2.0 on stdio.
//!
//! This mirrors `ocean-mcp`'s client: a single background **I/O task** owns the
//! transport (the only thing that touches it, so reads and writes never
//! interleave), callers register a `oneshot` waiter keyed by request id, hand the
//! framed line to the I/O task, and await their slot under a per-request timeout
//! with **no lock held across the await**. That keeps concurrent `invoke_tool`
//! calls from serializing behind one another.
//!
//! ## Wire protocol
//!
//! Two methods, both request/response:
//!
//! - `list_tools` → `{ "tools": [ { name, description?, inputSchema? }, ... ] }`
//! - `invoke_tool` with params `{ "name": <string>, "args": <value> }` →
//!   the tool's JSON result (opaque to this crate).
//!
//! The plugin replies with a standard JSON-RPC `result` or `error` object keyed
//! by the request `id` we sent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

use crate::jsonrpc::{Incoming, Request};
use crate::manifest::PluginManifest;
use crate::plugin::{Plugin, PluginError, PluginTool};
use crate::transport::{StdioTransport, Transport};

/// JSON-RPC method names on the plugin wire.
const METHOD_LIST_TOOLS: &str = "list_tools";
const METHOD_INVOKE_TOOL: &str = "invoke_tool";

/// Default per-request timeout. A slow or silent plugin can't make a caller wait
/// forever.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What the I/O task routes back to a waiting request: the JSON-RPC `result`, or
/// an error lifted from the response's `error` object.
type ResponseSlot = oneshot::Sender<Result<Value, PluginError>>;
type Pending = Arc<Mutex<HashMap<u64, ResponseSlot>>>;

/// A plugin running as a child process.
pub struct SubprocessPlugin {
    name: String,
    version: String,
    /// Channel into the I/O task: every outbound line goes through here so the
    /// task — sole owner of the transport — performs the write, serializing
    /// writes without forcing callers to hold a lock across their response wait.
    outbound: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Pending,
    request_timeout: Duration,
}

impl SubprocessPlugin {
    /// Launch a plugin from its parsed manifest, resolving the `entry` executable
    /// relative to `base_dir` (typically the manifest's own directory). An
    /// absolute `entry` is used as-is.
    pub fn launch(
        manifest: &PluginManifest,
        base_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, PluginError> {
        let entry_path = {
            let p = std::path::Path::new(&manifest.entry);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_dir.as_ref().join(p)
            }
        };
        let entry = entry_path.to_string_lossy().into_owned();
        Self::launch_command(&manifest.name, &manifest.version, &entry, &[], &[])
    }

    /// Launch a plugin by spawning `command` directly. The lower-level entry
    /// point; [`launch`](Self::launch) resolves a manifest down to this.
    pub fn launch_command(
        name: &str,
        version: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, PluginError> {
        let transport = StdioTransport::spawn(command, args, env).map_err(|e| PluginError::Spawn {
            entry: command.to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self::with_transport(name, version, Box::new(transport)))
    }

    /// Build a plugin over an arbitrary transport. Used by [`launch_command`] and
    /// directly by tests with an in-process transport.
    pub fn with_transport(
        name: &str,
        version: &str,
        transport: Box<dyn Transport + Send>,
    ) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<String>();
        spawn_io_task(transport, outbound_rx, pending.clone());

        Self {
            name: name.to_string(),
            version: version.to_string(),
            outbound: outbound_tx,
            next_id: AtomicU64::new(1),
            pending,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Override the per-request timeout.
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }

    fn take_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a request and await its response, multiplexed over the shared stream.
    /// No lock is held across the await, so concurrent requests overlap.
    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, PluginError> {
        let id = self.take_id();
        let req = Request::new(id, method, params);
        let line = serde_json::to_string(&req)
            .map_err(|e| PluginError::Protocol(format!("serialize {method}: {e}")))?;

        // Register the waiter BEFORE sending so a fast plugin can't answer before
        // we're listening.
        let (tx, rx) = oneshot::channel::<Result<Value, PluginError>>();
        self.pending.lock().await.insert(id, tx);

        if self.outbound.send(line).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(PluginError::Transport(format!(
                "request `{method}` failed: plugin connection closed"
            )));
        }

        match timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv)) => Err(PluginError::Transport(format!(
                "plugin closed the connection during `{method}`"
            ))),
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                Err(PluginError::Transport(format!(
                    "request `{method}` timed out"
                )))
            }
        }
    }
}

#[async_trait]
impl Plugin for SubprocessPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }

    async fn list_tools(&self) -> Result<Vec<PluginTool>, PluginError> {
        #[derive(Deserialize)]
        struct ListResult {
            #[serde(default)]
            tools: Vec<PluginTool>,
        }
        let result = self.request(METHOD_LIST_TOOLS, None).await?;
        let parsed: ListResult = serde_json::from_value(result)
            .map_err(|e| PluginError::Protocol(format!("parse list_tools result: {e}")))?;
        Ok(parsed.tools)
    }

    async fn invoke_tool(&self, name: &str, args: Value) -> Result<Value, PluginError> {
        let params = json!({ "name": name, "args": args });
        self.request(METHOD_INVOKE_TOOL, Some(params)).await
    }
}

/// Spawn the single I/O task that owns the transport: drains the outbound channel
/// to write framed lines, reads inbound lines, and routes each response to its
/// waiter by id. Owning the transport in one task is what makes concurrent
/// callers safe without holding a lock across the round-trip.
fn spawn_io_task(
    transport: Box<dyn Transport + Send>,
    outbound: mpsc::UnboundedReceiver<String>,
    pending: Pending,
) {
    tokio::spawn(async move {
        // Run the loop in an inner task so a panic in the body (e.g. a bug in
        // `route_inbound` on unexpected input) is observed here rather than
        // silently swallowed by the outer `tokio::spawn`. A swallowed panic
        // would leave the `SubprocessPlugin` holding a live `outbound` sender
        // whose I/O task is dead, so every later `request()` would hang until
        // its per-request timeout. The graceful break paths (EOF / read-error /
        // outbound-closed) already fail pending waiters; this net catches the
        // *ungraceful* exit so callers never wedge on a panicked task either.
        let cleanup_pending = pending.clone();
        let inner = tokio::spawn(io_loop(transport, outbound, pending));
        if let Err(join_err) = inner.await {
            if join_err.is_panic() {
                tracing::error!("plugin io task panicked; failing pending waiters");
                fail_all_pending(&cleanup_pending, "plugin io task panicked").await;
            }
            // A cancelled inner task (not expected — we always await it to
            // completion) leaves nothing to clean up.
        }
    });
}

/// The transport-owning read/write loop. Returns on any graceful exit (outbound
/// channel closed, EOF, or read error), having already failed pending waiters on
/// the EOF / read-error paths. Lifted out of [`spawn_io_task`] so it can run
/// inside an inner task whose panic is observable via its `JoinHandle`, which is
/// how panic safety is added without swallowing the existing graceful paths.
async fn io_loop(
    mut transport: Box<dyn Transport + Send>,
    mut outbound: mpsc::UnboundedReceiver<String>,
    pending: Pending,
) {
    loop {
        tokio::select! {
            maybe_out = outbound.recv() => {
                let Some(line) = maybe_out else {
                    // All handles dropped: shut the connection down.
                    let _ = transport.close().await;
                    break;
                };
                if let Err(e) = transport.send(&line).await {
                    tracing::warn!(error = %e, "plugin transport write failed");
                }
            }
            read = transport.recv() => {
                match read {
                    Ok(Some(raw)) => route_inbound(&raw, &pending).await,
                    Ok(None) => {
                        // EOF: plugin exited. Fail every pending waiter so
                        // in-flight requests return promptly.
                        fail_all_pending(&pending, "plugin exited").await;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "plugin transport read failed; closing");
                        fail_all_pending(&pending, "plugin transport read failed").await;
                        let _ = transport.close().await;
                        break;
                    }
                }
            }
        }
    }
}

/// Parse one inbound line and route it to its waiter by id.
async fn route_inbound(raw: &str, pending: &Pending) {
    let msg: Incoming = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "unparseable plugin message; skipping");
            return;
        }
    };

    if !msg.is_response() {
        // A line with no id is not a response we asked for. Plugins don't call
        // back into the host, so this is unexpected — log and drop.
        tracing::debug!(line = %raw, "ignoring non-response plugin line");
        return;
    }

    // Find the numeric id this response answers.
    let Some(id) = numeric_id(&msg) else {
        tracing::warn!(line = %raw, "plugin response had a non-numeric id; dropping");
        return;
    };

    let Some(slot) = pending.lock().await.remove(&id) else {
        // No waiter (already timed out and cleaned up, or a duplicate).
        tracing::debug!(id, "plugin response had no waiter; dropping");
        return;
    };

    let outcome = if let Some(err) = msg.error {
        Err(PluginError::Rpc {
            code: err.code,
            message: err.message,
        })
    } else {
        Ok(msg.result.unwrap_or(Value::Null))
    };
    let _ = slot.send(outcome);
}

/// Extract the numeric request id a response answers, accepting a string-encoded
/// id too (some peers echo ids as strings).
fn numeric_id(msg: &Incoming) -> Option<u64> {
    match &msg.id {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// Fail every pending waiter — used when the connection ends so in-flight
/// requests return promptly instead of waiting out their timeout.
async fn fail_all_pending(pending: &Pending, reason: &str) {
    let mut guard = pending.lock().await;
    for (_id, slot) in guard.drain() {
        let _ = slot.send(Err(PluginError::Transport(reason.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    /// A transport whose `recv()` panics once a signal fires. `send` and `close`
    /// are no-ops that succeed, so a `request()` registers its waiter and the
    /// outbound write goes through — then, when the test signals, the I/O loop's
    /// `recv()` branch panics, standing in for any panic in the task body (e.g. a
    /// bug in `route_inbound` on malformed input). The point of the test is the
    /// *effect* of a panicked io-task, not the specific line that panics.
    struct PanicAfterSignal {
        panic_now: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Transport for PanicAfterSignal {
        async fn send(&mut self, _json_line: &str) -> Result<()> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            // Park until the test tells us to blow up, then panic from inside
            // the io-loop body exactly as a buggy handler would.
            self.panic_now.notified().await;
            panic!("simulated panic inside plugin io task");
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// A panic in the I/O task body must fail pending waiters instead of leaving
    /// the caller to hang until its per-request timeout. Before the fix the panic
    /// was swallowed by `tokio::spawn`, the `outbound` sender stayed live, and the
    /// request blocked for the full 30s. With the `JoinHandle::is_panic()` net the
    /// request resolves promptly with a transport error.
    #[tokio::test]
    async fn panic_in_io_task_fails_pending_waiters() {
        let panic_now = Arc::new(tokio::sync::Notify::new());
        let transport = PanicAfterSignal {
            panic_now: panic_now.clone(),
        };

        // Long per-request timeout: if cleanup is broken, this test hangs well
        // past the harness timeout instead of passing — proving we resolve via
        // the panic net, not the request timeout.
        let plugin = SubprocessPlugin::with_transport("panicky", "0.0.0", Box::new(transport))
            .with_timeout(Duration::from_secs(60));

        // Fire a request concurrently; it registers a waiter and sends.
        let call = tokio::spawn(async move { plugin.list_tools().await });

        // Give the request a beat to register its waiter, then trigger the panic.
        tokio::task::yield_now().await;
        panic_now.notify_one();

        // The call must come back as an Err quickly (well under the 60s request
        // timeout) because the panic net fails the pending waiter.
        let outcome = timeout(Duration::from_secs(5), call)
            .await
            .expect("request should resolve promptly after io-task panic, not hang")
            .expect("request task should not itself panic");

        match outcome {
            Err(PluginError::Transport(msg)) => {
                assert!(
                    msg.contains("panicked"),
                    "expected panic-cleanup transport error, got: {msg}"
                );
            }
            other => panic!("expected Err(Transport(..)) from panicked io task, got: {other:?}"),
        }
    }

    /// Sanity check that the ordinary EOF path still fails pending waiters — the
    /// panic net must not have disturbed the graceful exits.
    #[tokio::test]
    async fn eof_still_fails_pending_waiters() {
        struct EofAfterSignal {
            eof_now: Arc<tokio::sync::Notify>,
        }

        #[async_trait]
        impl Transport for EofAfterSignal {
            async fn send(&mut self, _json_line: &str) -> Result<()> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<Option<String>> {
                self.eof_now.notified().await;
                Ok(None) // EOF
            }
            async fn close(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let eof_now = Arc::new(tokio::sync::Notify::new());
        let transport = EofAfterSignal {
            eof_now: eof_now.clone(),
        };
        let plugin = SubprocessPlugin::with_transport("eofy", "0.0.0", Box::new(transport))
            .with_timeout(Duration::from_secs(60));

        let call = tokio::spawn(async move { plugin.list_tools().await });
        tokio::task::yield_now().await;
        eof_now.notify_one();

        let outcome = timeout(Duration::from_secs(5), call)
            .await
            .expect("request should resolve promptly on EOF")
            .expect("request task should not panic");

        match outcome {
            Err(PluginError::Transport(msg)) => {
                assert!(
                    msg.contains("exited"),
                    "expected EOF transport error, got: {msg}"
                );
            }
            other => panic!("expected Err(Transport(..)) on EOF, got: {other:?}"),
        }
    }
}
