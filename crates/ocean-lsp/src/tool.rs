//! The `lsp` agent tool: one tool, action-dispatched (oh-my-pi's shape).
//!
//! Actions: `status`, `diagnostics`, `definition`, `references`, `hover`,
//! `symbols`, `rename`, `reload`. Positions are addressed model-friendly — by
//! `file` + `line` (1-based) + `symbol` substring on that line — never by
//! character column, which models reliably get wrong.
//!
//! Clients are shared process-wide per `(server, root)` so ten sessions on one
//! repo share one rust-analyzer instead of spawning ten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::{json, Value};

use crate::client::{byte_col_from_utf16, path_from_uri, uri_for, utf16_col, LspClient};
use crate::ledger::DiagnosticsLedger;
use crate::servers::{detect, server_for_file, ServerDef};

/// How long a `diagnostics` action waits for the server to publish before
/// answering with what is known.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(4);
/// How long a query waits for a freshly-started server to finish indexing
/// (rust-analyzer answers `null` to questions asked mid-index). Once a server
/// has settled once this returns instantly.
const QUIESCENT_WAIT: Duration = Duration::from_secs(45);
/// Cap on locations/symbols/diagnostics rendered into one result.
const MAX_ROWS: usize = 50;

/// Process-global client registry: `(server, root)` → live client. Ten
/// sessions on one repo share one rust-analyzer.
type ClientMap = HashMap<(String, PathBuf), Arc<LspClient>>;
fn clients() -> &'static tokio::sync::Mutex<ClientMap> {
    static CLIENTS: OnceLock<tokio::sync::Mutex<ClientMap>> = OnceLock::new();
    CLIENTS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Session-scoped diagnostics ledgers. Keyed by session id so dedupe never
/// bleeds across sessions (a new session sees the full picture again).
fn ledgers() -> &'static Mutex<HashMap<String, DiagnosticsLedger>> {
    static LEDGERS: OnceLock<Mutex<HashMap<String, DiagnosticsLedger>>> = OnceLock::new();
    LEDGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct LspTool {
    cwd: PathBuf,
    /// Ledger key; "adhoc" when the turn has no session.
    session_key: String,
}

impl LspTool {
    pub fn new(cwd: PathBuf, session_id: Option<String>) -> Self {
        Self {
            cwd,
            session_key: session_id.unwrap_or_else(|| "adhoc".into()),
        }
    }

    async fn client_for_file(&self, path: &Path) -> Result<Arc<LspClient>, String> {
        let detected = detect(&self.cwd);
        let (def, root) = server_for_file(&detected, path)
            .ok_or_else(|| format!("no language server available for {}", path.display()))?;
        get_or_start(def, root).await
    }

    /// Resolve `{file, line, symbol, occurrence}` args to (path, 0-based line,
    /// utf16 col at the symbol).
    async fn resolve_position(&self, args: &Value) -> Result<(PathBuf, u32, u32), String> {
        let path = self.resolve_file(args)?;
        let line_no = args
            .get("line")
            .and_then(|v| v.as_u64())
            .ok_or("missing 'line' (1-based)")? as usize;
        let symbol = args
            .get("symbol")
            .and_then(|v| v.as_str())
            .ok_or("missing 'symbol' (a substring of the target line)")?;
        let occurrence = args
            .get("occurrence")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let line = text
            .lines()
            .nth(line_no.saturating_sub(1))
            .ok_or_else(|| format!("{} has no line {line_no}", path.display()))?;
        // Nth occurrence of the symbol on the line; error (never guess) when
        // absent — the model re-reads rather than acting on a wrong target.
        let mut at = None;
        let mut from = 0usize;
        for _ in 0..occurrence {
            match line[from..].find(symbol) {
                Some(i) => {
                    at = Some(from + i);
                    from = from + i + symbol.len().max(1);
                }
                None => {
                    return Err(format!(
                        "symbol {symbol:?} not found {occurrence} time(s) on line {line_no} of {} \
                         (line is: {line:?}); re-read the file",
                        path.display()
                    ))
                }
            }
        }
        let col = utf16_col(line, at.unwrap_or(0));
        Ok((path, line_no as u32 - 1, col))
    }

    fn resolve_file(&self, args: &Value) -> Result<PathBuf, String> {
        let file = args
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or("missing 'file'")?;
        let p = Path::new(file);
        Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        })
    }
}

async fn get_or_start(def: &'static ServerDef, root: &Path) -> Result<Arc<LspClient>, String> {
    let key = (def.name.to_string(), root.to_path_buf());
    let mut map = clients().lock().await;
    if let Some(c) = map.get(&key) {
        return Ok(c.clone());
    }
    let args: Vec<String> = def.args.iter().map(|s| s.to_string()).collect();
    let client = LspClient::start(def.name, def.command, &args, root)
        .await
        .map_err(|e| format!("starting {}: {e}", def.name))?;
    let client = Arc::new(client);
    map.insert(key, client.clone());
    Ok(client)
}

#[async_trait]
impl AgentTool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }
    fn description(&self) -> &str {
        "Code intelligence via the project's language servers (rust-analyzer, typescript, pyright, gopls — auto-detected). \
         action ∈ {status, diagnostics, definition, references, hover, symbols, rename, reload}. \
         Positions are file + line (1-based) + symbol (a substring on that line). \
         diagnostics: pass file; only NEW problems since the last check are shown (all:true for everything). \
         rename: pass new_name; preview by default, apply:true to write. \
         symbols: pass query for a workspace symbol search."
    }
    fn requires_permission(&self) -> bool {
        // `rename` with apply:true rewrites files across the workspace; the
        // whole tool is gated rather than special-casing one action.
        true
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "diagnostics", "definition", "references", "hover", "symbols", "rename", "reload"]},
                "file": {"type": "string", "description": "Path (absolute or cwd-relative)"},
                "line": {"type": "integer", "description": "1-based line of the symbol"},
                "symbol": {"type": "string", "description": "Substring of the target line naming the symbol"},
                "occurrence": {"type": "integer", "description": "Nth occurrence of symbol on the line (default 1)"},
                "query": {"type": "string", "description": "symbols: workspace symbol query"},
                "new_name": {"type": "string", "description": "rename: the new identifier"},
                "apply": {"type": "boolean", "description": "rename: write the edits (default false = preview)"},
                "all": {"type": "boolean", "description": "diagnostics: include already-reported problems (default false)"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or("missing 'action'")?;
        match action {
            "status" => {
                let detected = detect(&self.cwd);
                if detected.is_empty() {
                    return Ok(AgentToolResult::text(
                        "no language servers detected (no root marker + binary pair found)",
                    ));
                }
                let running = clients().lock().await;
                let mut out = String::new();
                for (def, root) in &detected {
                    let live = running.contains_key(&(def.name.to_string(), root.clone()));
                    out.push_str(&format!(
                        "{}\troot={}\t{}\n",
                        def.name,
                        root.display(),
                        if live { "running" } else { "available" }
                    ));
                }
                Ok(AgentToolResult::text(out))
            }
            "reload" => {
                let mut map = clients().lock().await;
                let before = map.len();
                map.retain(|(_, root), _| {
                    !root.starts_with(&self.cwd) && !self.cwd.starts_with(root)
                });
                Ok(AgentToolResult::text(format!(
                    "dropped {} client(s); they restart on next use",
                    before - map.len()
                )))
            }
            "diagnostics" => {
                let path = self.resolve_file(&args)?;
                let client = self.client_for_file(&path).await?;
                client.wait_quiescent(QUIESCENT_WAIT).await;
                client.ensure_open(&path).await.map_err(|e| e.to_string())?;
                let diags = client.wait_for_diagnostics(&path, DIAGNOSTICS_WAIT).await;
                let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
                let mut ledgers = ledgers().lock().unwrap_or_else(|p| p.into_inner());
                let ledger = ledgers.entry(self.session_key.clone()).or_default();
                let shown = if all {
                    ledger.reset(&path);
                    let _ = ledger.reduce(&path, &diags); // record for next time
                    diags.clone()
                } else {
                    ledger.reduce(&path, &diags)
                };
                if shown.is_empty() {
                    return Ok(AgentToolResult::text(if diags.is_empty() {
                        "no diagnostics".to_string()
                    } else {
                        format!(
                            "no NEW diagnostics ({} known problem(s) already reported; pass all:true to see them)",
                            diags.len()
                        )
                    }));
                }
                let mut out = String::new();
                for d in shown.iter().take(MAX_ROWS) {
                    out.push_str(&format!("line {} {}: {}\n", d.line, d.severity, d.message));
                }
                if shown.len() > MAX_ROWS {
                    out.push_str(&format!("[+{} more]\n", shown.len() - MAX_ROWS));
                }
                Ok(AgentToolResult::text(out))
            }
            "definition" | "references" | "hover" => {
                let (path, line, col) = self.resolve_position(&args).await?;
                let client = self.client_for_file(&path).await?;
                client.wait_quiescent(QUIESCENT_WAIT).await;
                client.ensure_open(&path).await.map_err(|e| e.to_string())?;
                let method = match action {
                    "definition" => "textDocument/definition",
                    "references" => "textDocument/references",
                    _ => "textDocument/hover",
                };
                let mut params = json!({
                    "textDocument": { "uri": uri_for(&path) },
                    "position": { "line": line, "character": col }
                });
                if action == "references" {
                    params["context"] = json!({ "includeDeclaration": true });
                }
                let result = client.request(method, params).await?;
                Ok(AgentToolResult::text(match action {
                    "hover" => render_hover(&result),
                    _ => render_locations(&result),
                }))
            }
            "symbols" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or("missing 'query'")?;
                // Any detected server can answer workspace/symbol; prefer the
                // first. (Per-language routing needs a file context we don't
                // have for a workspace-wide query.)
                let detected = detect(&self.cwd);
                let (def, root) = detected
                    .first()
                    .ok_or("no language server available in this workspace")?;
                let client = get_or_start(def, root).await?;
                let result = client
                    .request("workspace/symbol", json!({ "query": query }))
                    .await?;
                Ok(AgentToolResult::text(render_symbols(&result)))
            }
            "rename" => {
                let new_name = args
                    .get("new_name")
                    .and_then(|v| v.as_str())
                    .ok_or("missing 'new_name'")?;
                let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);
                let (path, line, col) = self.resolve_position(&args).await?;
                let client = self.client_for_file(&path).await?;
                client.wait_quiescent(QUIESCENT_WAIT).await;
                client.ensure_open(&path).await.map_err(|e| e.to_string())?;
                let edit = client
                    .request(
                        "textDocument/rename",
                        json!({
                            "textDocument": { "uri": uri_for(&path) },
                            "position": { "line": line, "character": col },
                            "newName": new_name
                        }),
                    )
                    .await?;
                if edit.is_null() {
                    return Err(
                        "server returned no rename edit (symbol not renameable here)".into(),
                    );
                }
                let per_file = collect_workspace_edit(&edit)?;
                if !apply {
                    let mut out = format!("rename preview → {new_name}\n");
                    for (file, edits) in &per_file {
                        out.push_str(&format!("{}: {} edit(s)\n", file.display(), edits.len()));
                    }
                    out.push_str("(pass apply:true to write)");
                    return Ok(AgentToolResult::text(out));
                }
                let mut out = format!("renamed → {new_name}\n");
                for (file, edits) in &per_file {
                    apply_edits_to_file(file, edits).await?;
                    // The server's view of the file is now stale; re-sync.
                    let _ = client.ensure_open(file).await;
                    out.push_str(&format!(
                        "{}: {} edit(s) applied\n",
                        file.display(),
                        edits.len()
                    ));
                }
                Ok(AgentToolResult::text(out))
            }
            other => Err(format!("unknown action: {other}")),
        }
    }
}

/// One text edit: (start_line, start_col_utf16, end_line, end_col_utf16, new_text).
type Edit = (u32, u32, u32, u32, String);

/// Flatten a WorkspaceEdit (`changes` or `documentChanges`) to per-file edits.
fn collect_workspace_edit(edit: &Value) -> Result<Vec<(PathBuf, Vec<Edit>)>, String> {
    let mut out: Vec<(PathBuf, Vec<Edit>)> = Vec::new();
    let mut push = |uri: &str, arr: &[Value]| -> Result<(), String> {
        let path = path_from_uri(uri).ok_or_else(|| format!("bad uri {uri}"))?;
        let mut edits = Vec::new();
        for e in arr {
            let range = e.get("range").ok_or("edit missing range")?;
            let get = |p: &Value, k: &str, k2: &str| -> Result<u32, String> {
                p.get(k)
                    .and_then(|x| x.get(k2))
                    .and_then(|x| x.as_u64())
                    .map(|x| x as u32)
                    .ok_or_else(|| "bad range".to_string())
            };
            edits.push((
                get(range, "start", "line")?,
                get(range, "start", "character")?,
                get(range, "end", "line")?,
                get(range, "end", "character")?,
                e.get("newText")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            ));
        }
        out.push((path, edits));
        Ok(())
    };
    if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, arr) in changes {
            push(uri, arr.as_array().map(|a| a.as_slice()).unwrap_or(&[]))?;
        }
    }
    if let Some(doc_changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for dc in doc_changes {
            // Only TextDocumentEdit is supported; file create/rename/delete ops
            // are refused loudly rather than half-applied.
            if dc.get("kind").is_some() {
                return Err("rename requires file create/rename/delete operations, which are not supported yet".into());
            }
            let uri = dc
                .get("textDocument")
                .and_then(|t| t.get("uri"))
                .and_then(|u| u.as_str())
                .ok_or("documentChange missing uri")?;
            push(
                uri,
                dc.get("edits")
                    .and_then(|e| e.as_array())
                    .map(|a| a.as_slice())
                    .unwrap_or(&[]),
            )?;
        }
    }
    if out.is_empty() {
        return Err("workspace edit contained no text edits".into());
    }
    Ok(out)
}

/// Apply LSP text edits to one file on disk. Edits are applied last-first so
/// earlier offsets stay valid.
async fn apply_edits_to_file(path: &Path, edits: &[Edit]) -> Result<(), String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    // Byte offset of each line start.
    let mut line_starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let offset = |line: u32, chr: u32| -> Result<usize, String> {
        let ls = *line_starts
            .get(line as usize)
            .ok_or_else(|| format!("edit line {line} out of range in {}", path.display()))?;
        let line_str = text[ls..].split('\n').next().unwrap_or("");
        Ok(ls + byte_col_from_utf16(line_str, chr))
    };
    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for (sl, sc, el, ec, new_text) in edits {
        spans.push((offset(*sl, *sc)?, offset(*el, *ec)?, new_text));
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    // Overlapping edits would corrupt the file — refuse.
    for w in spans.windows(2) {
        if w[1].1 > w[0].0 {
            return Err(format!("overlapping rename edits in {}", path.display()));
        }
    }
    let mut out = text.clone();
    for (start, end, new_text) in spans {
        out.replace_range(start..end, new_text);
    }
    tokio::fs::write(path, out)
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Render Location | Location[] | LocationLink[] as `path:line` rows.
fn render_locations(result: &Value) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut push_loc = |v: &Value| {
        let uri = v
            .get("uri")
            .or_else(|| v.get("targetUri"))
            .and_then(|u| u.as_str());
        let range = v.get("range").or_else(|| v.get("targetSelectionRange"));
        if let (Some(uri), Some(range)) = (uri, range) {
            if let (Some(path), Some(line)) = (
                path_from_uri(uri),
                range
                    .get("start")
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64()),
            ) {
                rows.push(format!("{}:{}", path.display(), line + 1));
            }
        }
    };
    match result {
        Value::Array(arr) => arr.iter().for_each(&mut push_loc),
        Value::Null => {}
        one => push_loc(one),
    }
    rows.dedup();
    if rows.is_empty() {
        return "(no results)".into();
    }
    let total = rows.len();
    let mut out = rows
        .into_iter()
        .take(MAX_ROWS)
        .collect::<Vec<_>>()
        .join("\n");
    if total > MAX_ROWS {
        out.push_str(&format!("\n[+{} more]", total - MAX_ROWS));
    }
    out
}

/// Render a Hover result's contents to plain text.
fn render_hover(result: &Value) -> String {
    fn content_text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("value")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            Value::Array(arr) => arr.iter().map(content_text).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }
    let text = result.get("contents").map(content_text).unwrap_or_default();
    if text.trim().is_empty() {
        return "(no hover info)".into();
    }
    const MAX: usize = 4000;
    if text.len() > MAX {
        let mut cut = MAX;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &text[..cut])
    } else {
        text
    }
}

/// Render workspace/symbol results.
fn render_symbols(result: &Value) -> String {
    let Some(arr) = result.as_array() else {
        return "(no results)".into();
    };
    if arr.is_empty() {
        return "(no results)".into();
    }
    const KINDS: &[&str] = &[
        "?",
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum-member",
        "struct",
        "event",
        "operator",
        "type-param",
    ];
    let mut out = String::new();
    for s in arr.iter().take(MAX_ROWS) {
        let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let kind = s
            .get("kind")
            .and_then(|k| k.as_u64())
            .and_then(|k| KINDS.get(k as usize))
            .unwrap_or(&"?");
        let loc = s.get("location").map(render_locations).unwrap_or_default();
        out.push_str(&format!("{kind}\t{name}\t{loc}\n"));
    }
    if arr.len() > MAX_ROWS {
        out.push_str(&format!("[+{} more]\n", arr.len() - MAX_ROWS));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_locations_handles_all_shapes() {
        // Single Location
        let one = json!({ "uri": "file:///w/a.rs", "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 4, "character": 5 } } });
        assert_eq!(render_locations(&one), "/w/a.rs:5");
        // Array of LocationLink
        let links = json!([{ "targetUri": "file:///w/b.rs", "targetSelectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]);
        assert_eq!(render_locations(&links), "/w/b.rs:1");
        assert_eq!(render_locations(&Value::Null), "(no results)");
    }

    #[test]
    fn workspace_edit_shapes_are_collected() {
        let changes = json!({ "changes": { "file:///w/a.rs": [
            { "range": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6} }, "newText": "bar" }
        ]}});
        let per_file = collect_workspace_edit(&changes).unwrap();
        assert_eq!(per_file.len(), 1);
        assert_eq!(per_file[0].1.len(), 1);

        let doc_changes = json!({ "documentChanges": [{
            "textDocument": { "uri": "file:///w/a.rs", "version": 2 },
            "edits": [{ "range": { "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3} }, "newText": "baz" }]
        }]});
        assert_eq!(collect_workspace_edit(&doc_changes).unwrap()[0].1.len(), 1);

        // Resource ops are refused, not half-applied.
        let with_rename_file = json!({ "documentChanges": [{ "kind": "rename", "oldUri": "file:///a", "newUri": "file:///b" }]});
        assert!(collect_workspace_edit(&with_rename_file).is_err());
    }

    #[tokio::test]
    async fn apply_edits_rewrites_disk_last_first() {
        let dir = std::env::temp_dir().join(format!("ocean-lsp-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn foo() {}\nfoo();\n").unwrap();
        // Rename both `foo`s → `bar`: two edits on different lines.
        let edits: Vec<Edit> = vec![(0, 3, 0, 6, "bar".into()), (1, 0, 1, 3, "bar".into())];
        apply_edits_to_file(&file, &edits).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn bar() {}\nbar();\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn overlapping_edits_are_refused() {
        let dir = std::env::temp_dir().join(format!("ocean-lsp-overlap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        std::fs::write(&file, "abcdef\n").unwrap();
        let edits: Vec<Edit> = vec![(0, 0, 0, 4, "X".into()), (0, 2, 0, 6, "Y".into())];
        let err = apply_edits_to_file(&file, &edits).await.unwrap_err();
        assert!(err.contains("overlapping"), "{err}");
        // File untouched on refusal.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "abcdef\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
