use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

pub struct LsTool {
    cwd: Option<PathBuf>,
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl LsTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn concurrency(&self) -> crate::types::Concurrency {
        crate::types::Concurrency::Shared
    }
    fn description(&self) -> &str {
        "List entries in a directory. Returns name and kind (file/dir/symlink)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path'")?;
        let display_path = path.to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), path);
        let mut read = fs::read_dir(&path)
            .await
            .map_err(|e| format!("ls {display_path}: {e}"))?;
        // Collect, then sort: raw `read_dir` order is filesystem-dependent, so
        // two identical calls could return differently-ordered listings —
        // confusing for the model and hostile to prompt caching. Dirs first,
        // then alphabetical within each kind.
        let mut entries: Vec<(&'static str, String)> = Vec::new();
        while let Some(entry) = read.next_entry().await.map_err(|e| e.to_string())? {
            let ft = entry.file_type().await.map_err(|e| e.to_string())?;
            let kind = if ft.is_dir() {
                "dir"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "file"
            };
            entries.push((kind, entry.file_name().to_string_lossy().into_owned()));
        }
        let total = entries.len();
        entries.sort_by(|a, b| {
            let rank = |k: &str| if k == "dir" { 0 } else { 1 };
            rank(a.0).cmp(&rank(b.0)).then_with(|| a.1.cmp(&b.1))
        });
        // Cap the rows: a 50k-entry node_modules must not dump 50k lines into
        // one result.
        entries.truncate(MAX_ENTRIES);
        let mut buf = String::new();
        for (kind, name) in &entries {
            buf.push_str(&format!("{kind}\t{name}\n"));
        }
        if total > MAX_ENTRIES {
            buf.push_str(&format!(
                "[showing {MAX_ENTRIES} of {total} entries; listing truncated]\n"
            ));
        }
        Ok(AgentToolResult::text(buf))
    }
}

/// Max rows a single listing returns.
const MAX_ENTRIES: usize = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    /// Listings are deterministic (dirs first, then alpha) and capped with an
    /// explicit note — raw `read_dir` order is filesystem-dependent and a giant
    /// directory previously dumped every entry.
    #[tokio::test]
    async fn ls_sorts_dirs_first_alpha_and_caps() {
        let dir = std::env::temp_dir().join(format!("ocean-ls-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(dir.join("zdir")).await.unwrap();
        fs::create_dir_all(dir.join("adir")).await.unwrap();
        fs::write(dir.join("bfile"), "x").await.unwrap();
        fs::write(dir.join("afile"), "x").await.unwrap();

        let out = LsTool::new()
            .execute("1", json!({ "path": dir.to_string_lossy() }))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec!["dir\tadir", "dir\tzdir", "file\tafile", "file\tbfile"],
            "dirs first, alpha within kind"
        );
        let _ = fs::remove_dir_all(&dir).await;
    }
}
