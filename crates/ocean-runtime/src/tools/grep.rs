use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

use crate::tools::path::resolve_against_cwd;
use crate::types::{AgentTool, AgentToolResult};

/// Files larger than this are skipped entirely (almost certainly generated or
/// binary-ish; reading them into memory per-search is pure waste).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Max chars of a matched line shown in the output. A minified bundle with one
/// enormous line would otherwise dump hundreds of KB into a single match row.
const MAX_LINE_CHARS: usize = 500;

fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let clipped: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{clipped}… [line clipped]")
}

pub struct GrepTool {
    cwd: Option<PathBuf>,
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepTool {
    pub fn new() -> Self {
        Self { cwd: None }
    }

    pub fn for_cwd(cwd: PathBuf) -> Self {
        Self { cwd: Some(cwd) }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn concurrency(&self) -> crate::types::Concurrency {
        crate::types::Concurrency::Shared
    }
    fn description(&self) -> &str {
        "Search file contents under a directory with a regular expression (falls back to a \
         literal substring if the pattern is not valid regex). Honors .gitignore by default."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex to search for (Rust regex syntax); an invalid regex is searched as a literal substring"},
                "path": {"type": "string", "description": "Directory to search (default: cwd)"},
                "max_matches": {"type": "integer", "default": 200}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern'")?
            .to_string();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let path = resolve_against_cwd(self.cwd.as_deref(), &path);
        let max = args
            .get("max_matches")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;

        // Regex when the pattern compiles, literal substring otherwise. The
        // fallback keeps the tool forgiving: a model that meant `foo(` literally
        // still gets matches instead of a hard error, and the note in the output
        // makes the interpretation explicit.
        let compiled = regex::Regex::new(&pattern).ok();
        let literal_fallback = compiled.is_none();

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let matches_line = |line: &str| -> bool {
                match &compiled {
                    Some(re) => re.is_match(line),
                    None => line.contains(&pattern),
                }
            };
            let mut buf = String::new();
            if literal_fallback {
                buf.push_str("(pattern is not valid regex; searched as a literal substring)\n");
            }
            let mut hits = 0usize;
            let walker = WalkBuilder::new(&path).follow_links(false).build();
            for entry in walker.flatten() {
                if hits >= max {
                    buf.push_str(&format!("... (truncated at {max} matches)\n"));
                    break;
                }
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                // Size guard: skip anything over the cap rather than pulling a
                // multi-hundred-MB artifact into memory per file.
                if entry
                    .metadata()
                    .map(|m| m.len() > MAX_FILE_BYTES)
                    .unwrap_or(false)
                {
                    continue;
                }
                let text = match fs::read_to_string(p) {
                    Ok(t) => t,
                    Err(_) => continue, // skip binary or unreadable
                };
                for (i, line) in text.lines().enumerate() {
                    if matches_line(line) {
                        buf.push_str(&format!("{}:{}:{}\n", p.display(), i + 1, clip_line(line)));
                        hits += 1;
                        if hits >= max {
                            break;
                        }
                    }
                }
            }
            Ok(buf)
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(AgentToolResult::text(if result.is_empty() {
            "(no matches)".to_string()
        } else {
            result
        }))
    }
}
