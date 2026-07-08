//! The LSP client: spawns one language-server child, runs the
//! `initialize` → `initialized` handshake, and multiplexes requests over the
//! Content-Length-framed stdio stream.
//!
//! Concurrency mirrors `ocean-mcp`'s client: a dedicated background **I/O
//! task** is the only owner of the child's stdout; callers allocate a request
//! id (atomic), register a `oneshot` waiter under a brief registry lock, queue
//! the framed line to a writer channel, and await the waiter lock-free — so two
//! concurrent requests overlap instead of serializing, and a slow request never
//! head-of-line blocks its neighbours.
//!
//! `textDocument/publishDiagnostics` notifications are folded into a per-file
//! diagnostics store; a `tokio::sync::Notify` wakes any `wait_for_diagnostics`
//! caller so post-edit diagnostics can be awaited with a bounded timeout.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

use crate::framing::{read_frame, write_frame};

/// Per-request deadline. A language server that never answers must not hang a
/// turn — the request errors out and the tool surfaces it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One diagnostic, flattened from the LSP shape to what the tool renders.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// 1-based line.
    pub line: u32,
    /// "error" | "warning" | "information" | "hint"
    pub severity: &'static str,
    pub message: String,
}

impl Diagnostic {
    fn from_lsp(v: &Value) -> Option<Self> {
        let range = v.get("range")?;
        let line = range.get("start")?.get("line")?.as_u64()? as u32 + 1;
        let severity = match v.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) {
            1 => "error",
            2 => "warning",
            3 => "information",
            _ => "hint",
        };
        let message = v.get("message")?.as_str()?.to_string();
        Some(Self {
            line,
            severity,
            message,
        })
    }
}

type ResponseSlot = oneshot::Sender<std::result::Result<Value, String>>;
type Pending = Arc<Mutex<HashMap<u64, ResponseSlot>>>;
/// path → (doc version at publish time if known, diagnostics)
type DiagStore = Arc<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>>;

/// Server busy-ness derived from `$/progress` begin/end notifications. Real
/// servers (rust-analyzer especially) answer `null` to queries that arrive
/// while they are still indexing — a fresh client must be able to wait for
/// quiescence before its first real question.
#[derive(Default)]
struct Progress {
    /// Tokens currently between a `begin` and an `end`.
    active: usize,
    /// Whether any progress cycle has been observed at all.
    seen_any: bool,
    /// Set only by rust-analyzer's `experimental/serverStatus quiescent:true` —
    /// the authoritative "fully loaded" signal when the server provides one.
    quiescent_signal: bool,
    /// Instant of the last active-count change, for the sustained-idle check.
    last_change: Option<std::time::Instant>,
}

pub struct LspClient {
    /// Server name, for logs ("rust-analyzer").
    pub server: String,
    /// Workspace root the server was initialized against.
    pub root: PathBuf,
    outbound: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Pending,
    diagnostics: DiagStore,
    diag_notify: Arc<Notify>,
    progress: Arc<Mutex<Progress>>,
    progress_notify: Arc<Notify>,
    /// Files we have `didOpen`-ed, with their current version counter.
    open_docs: Mutex<HashMap<PathBuf, i64>>,
}

impl LspClient {
    /// Spawn `command args…` in `root`, run the handshake, and return a live
    /// client. The child dies with the client handle (`kill_on_drop`).
    pub async fn start(server: &str, command: &str, args: &[String], root: &Path) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {server} ({command})"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: DiagStore = Arc::new(Mutex::new(HashMap::new()));
        let diag_notify = Arc::new(Notify::new());
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let progress_notify = Arc::new(Notify::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        // Writer task: sole owner of stdin. Serializes writes without callers
        // holding a lock across their response wait.
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = out_rx.recv().await {
                if write_frame(&mut stdin, &line).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: sole owner of stdout. Routes responses to waiters,
        // folds publishDiagnostics into the store, answers server→client
        // requests, and tracks $/progress busy-ness. `_child` moves in so the
        // process handle lives exactly as long as the reader (kill_on_drop
        // fires when the reader ends and drops it).
        {
            let pending = pending.clone();
            let diagnostics = diagnostics.clone();
            let diag_notify = diag_notify.clone();
            let progress = progress.clone();
            let progress_notify = progress_notify.clone();
            let outbound = out_tx.clone();
            let server_name = server.to_string();
            tokio::spawn(async move {
                let _child = child;
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_frame(&mut reader).await {
                        Ok(Some(text)) => {
                            let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                                continue;
                            };
                            route_incoming(
                                &server_name,
                                msg,
                                &pending,
                                &diagnostics,
                                &diag_notify,
                                &progress,
                                &progress_notify,
                                &outbound,
                            );
                        }
                        Ok(None) => break, // clean EOF — server exited
                        Err(e) => {
                            tracing::warn!(server = %server_name, error = %e, "LSP read error; closing");
                            break;
                        }
                    }
                }
                // Fail every waiter so requests error instead of hanging.
                let mut map = pending.lock().unwrap_or_else(|p| p.into_inner());
                for (_, slot) in map.drain() {
                    let _ = slot.send(Err("language server exited".into()));
                }
            });
        }

        let client = Self {
            server: server.to_string(),
            root: root.to_path_buf(),
            outbound: out_tx,
            next_id: AtomicU64::new(1),
            pending,
            diagnostics,
            diag_notify,
            progress,
            progress_notify,
            open_docs: Mutex::new(HashMap::new()),
        };

        // Handshake. Minimal client capabilities: we consume publishDiagnostics
        // and make read-only queries + rename; nothing exotic advertised.
        let root_uri = uri_for(root);
        client
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
                    "capabilities": {
                        "textDocument": {
                            "publishDiagnostics": { "relatedInformation": false },
                            "hover": { "contentFormat": ["plaintext", "markdown"] }
                        },
                        "workspace": { "workspaceFolders": true, "configuration": true },
                        // Without window.workDoneProgress a server sends NO
                        // $/progress at all — quiescence tracking would never
                        // see indexing start. serverStatusNotification is
                        // rust-analyzer's direct "quiescent" signal.
                        "window": { "workDoneProgress": true },
                        "experimental": { "serverStatusNotification": true }
                    }
                }),
            )
            .await
            .map_err(|e| anyhow!("initialize failed: {e}"))?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    /// Send a request and await its response, bounded by [`REQUEST_TIMEOUT`].
    pub async fn request(&self, method: &str, params: Value) -> std::result::Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.outbound.send(frame.to_string()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err("language server connection closed".into());
        }
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("language server dropped the request".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(format!(
                    "{} did not answer {method} within {}s",
                    self.server,
                    REQUEST_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Send a notification (no response expected).
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.outbound
            .send(frame.to_string())
            .map_err(|_| anyhow!("language server connection closed"))
    }

    /// Ensure `path` is open on the server (LSP requires `didOpen` before
    /// document requests). Re-opening after an on-disk change bumps the version
    /// via `didChange` with full content, so the server sees current bytes.
    pub async fn ensure_open(&self, path: &Path) -> Result<()> {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let mut docs = self.open_docs.lock().unwrap_or_else(|p| p.into_inner());
        let uri = uri_for(path);
        match docs.get_mut(&path.to_path_buf()) {
            None => {
                docs.insert(path.to_path_buf(), 1);
                drop(docs);
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri,
                        "languageId": language_id(path),
                        "version": 1,
                        "text": text
                    }}),
                )?;
            }
            Some(version) => {
                *version += 1;
                let v = *version;
                drop(docs);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": v },
                        "contentChanges": [{ "text": text }]
                    }),
                )?;
            }
        }
        Ok(())
    }

    /// Wait until the server reports no active `$/progress` work, or `wait`
    /// elapses. Real servers (rust-analyzer) answer `null` to queries that
    /// arrive mid-indexing, so callers should settle a FRESH server before its
    /// first real question. Returns immediately once the server has been
    /// quiescent at least once. Servers that never emit progress (the fake
    /// test server, simple servers) pass through after a short grace period.
    pub async fn wait_quiescent(&self, wait: Duration) {
        // How long `active == 0` must HOLD before we call the server settled.
        // Progress cycles have gaps (metadata-fetch ends before indexing
        // begins); returning in a gap hands the caller a server that still
        // answers `null`. rust-analyzer's explicit quiescent signal skips this.
        const SETTLE: Duration = Duration::from_millis(750);
        let deadline = tokio::time::Instant::now() + wait;
        // Grace: give a fresh server a beat to BEGIN reporting progress —
        // "no progress yet" right after initialize usually means "not started",
        // not "already done".
        let grace = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            let notified = self.progress_notify.notified();
            {
                let p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
                let now = tokio::time::Instant::now();
                if p.quiescent_signal {
                    return;
                }
                if p.active == 0 {
                    let idle_long_enough = match p.last_change {
                        Some(t) => t.elapsed() >= SETTLE,
                        // No progress ever seen: settled only after the grace
                        // window (simple servers that never report progress).
                        None => now >= grace,
                    };
                    if idle_long_enough && (p.seen_any || now >= grace) {
                        return;
                    }
                }
                if now >= deadline {
                    return;
                }
            }
            // Wake on the next progress event or a short poll tick, whichever
            // comes first — the settle check is time-based, so we must re-check
            // even without new events.
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }

    /// Diagnostics currently known for `path` (may be empty).
    pub fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Wait up to `wait` for a diagnostics publish for `path`, then return
    /// whatever is known. Never errors — no publish within the window just
    /// means "no (new) diagnostics yet", which is itself an answer.
    pub async fn wait_for_diagnostics(&self, path: &Path, wait: Duration) -> Vec<Diagnostic> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.diag_notify.notified();
            {
                let store = self.diagnostics.lock().unwrap_or_else(|p| p.into_inner());
                if store.contains_key(path) {
                    return store.get(path).cloned().unwrap_or_default();
                }
            }
            if timeout(deadline - tokio::time::Instant::now(), notified)
                .await
                .is_err()
            {
                return self.diagnostics_for(path);
            }
        }
    }
}

/// Route one incoming message: a response to a waiter, a server→client
/// request (answered so the server never stalls on us), or a notification.
#[allow(clippy::too_many_arguments)]
fn route_incoming(
    server: &str,
    msg: Value,
    pending: &Pending,
    diagnostics: &DiagStore,
    diag_notify: &Arc<Notify>,
    progress: &Arc<Mutex<Progress>>,
    progress_notify: &Arc<Notify>,
    outbound: &mpsc::UnboundedSender<String>,
) {
    // Response: has an id and result/error, no method.
    if msg.get("method").is_none() {
        let Some(id) = msg.get("id").and_then(|i| i.as_u64()) else {
            return;
        };
        let slot = pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if let Some(slot) = slot {
            let outcome = if let Some(err) = msg.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                Err(format!("{server}: {message}"))
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = slot.send(outcome);
        }
        return;
    }
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    // Server → client request. MUST be answered — rust-analyzer stalls its
    // startup pipeline waiting on `window/workDoneProgress/create` and
    // `workspace/configuration`. `configuration` expects one entry per asked
    // item (all null = "use your defaults"); everything else takes null.
    if let Some(id) = msg.get("id") {
        let result = if method == "workspace/configuration" {
            let n = msg
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(|i| i.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            Value::Array(vec![Value::Null; n])
        } else {
            Value::Null
        };
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = outbound.send(reply.to_string());
        return;
    }
    // Notifications.
    match method {
        "textDocument/publishDiagnostics" => {
            let Some(params) = msg.get("params") else {
                return;
            };
            let Some(path) = params
                .get("uri")
                .and_then(|u| u.as_str())
                .and_then(path_from_uri)
            else {
                return;
            };
            let diags: Vec<Diagnostic> = params
                .get("diagnostics")
                .and_then(|d| d.as_array())
                .map(|arr| arr.iter().filter_map(Diagnostic::from_lsp).collect())
                .unwrap_or_default();
            diagnostics
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(path, diags);
            diag_notify.notify_waiters();
        }
        // rust-analyzer's direct readiness signal (requires the
        // serverStatusNotification client capability). quiescent:true means
        // "everything loaded, answers are real now".
        "experimental/serverStatus" => {
            let quiescent = msg
                .get("params")
                .and_then(|p| p.get("quiescent"))
                .and_then(|q| q.as_bool())
                .unwrap_or(false);
            if quiescent {
                let mut p = progress.lock().unwrap_or_else(|p| p.into_inner());
                p.active = 0;
                p.seen_any = true;
                p.quiescent_signal = true;
                drop(p);
                progress_notify.notify_waiters();
            }
        }
        "$/progress" => {
            let kind = msg
                .get("params")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("kind"))
                .and_then(|k| k.as_str())
                .unwrap_or("");
            let mut p = progress.lock().unwrap_or_else(|p| p.into_inner());
            match kind {
                "begin" => {
                    p.active += 1;
                    p.seen_any = true;
                    p.last_change = Some(std::time::Instant::now());
                }
                "end" => {
                    p.active = p.active.saturating_sub(1);
                    p.last_change = Some(std::time::Instant::now());
                }
                _ => {}
            }
            drop(p);
            progress_notify.notify_waiters();
        }
        _ => {}
    }
}

/// `file://` URI for a path.
pub fn uri_for(path: &Path) -> String {
    // Percent-encode the minimal set that breaks URIs in practice (spaces).
    let s = path.to_string_lossy().replace(' ', "%20");
    format!("file://{s}")
}

/// Path from a `file://` URI.
pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    Some(PathBuf::from(raw.replace("%20", " ")))
}

/// LSP languageId from the file extension.
pub fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "json" => "json",
        "toml" => "toml",
        "md" => "markdown",
        _ => "plaintext",
    }
}

/// UTF-16 code-unit column (LSP's default position encoding) for a byte offset
/// `col_bytes` into `line`.
pub fn utf16_col(line: &str, col_bytes: usize) -> u32 {
    line[..col_bytes.min(line.len())]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// Byte offset in `line` for a UTF-16 code-unit column. Saturates at line end.
pub fn byte_col_from_utf16(line: &str, utf16: u32) -> usize {
    let mut units: u32 = 0;
    for (byte_idx, ch) in line.char_indices() {
        if units >= utf16 {
            return byte_idx;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// The set of file extensions a client's open-tracking considers; exposed for
/// the tool's file routing.
pub fn known_extensions() -> &'static HashSet<&'static str> {
    static EXTS: std::sync::OnceLock<HashSet<&'static str>> = std::sync::OnceLock::new();
    EXTS.get_or_init(|| {
        [
            "rs", "ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs", "py", "go",
        ]
        .into_iter()
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_round_trip() {
        let line = "let café = 1;"; // 'é' is 1 UTF-16 unit, 2 bytes
        let byte = line.find('=').unwrap();
        let utf16 = utf16_col(line, byte);
        assert_eq!(byte_col_from_utf16(line, utf16), byte);

        let emoji = "x 😀 y"; // 😀 is 2 UTF-16 units, 4 bytes
        let byte_y = emoji.find('y').unwrap();
        let utf16_y = utf16_col(emoji, byte_y);
        assert_eq!(utf16_y, 5); // x, space, 2 units, space
        assert_eq!(byte_col_from_utf16(emoji, utf16_y), byte_y);
    }

    #[test]
    fn uri_round_trip() {
        let p = Path::new("/tmp/my project/main.rs");
        assert_eq!(path_from_uri(&uri_for(p)).unwrap(), p);
    }
}
