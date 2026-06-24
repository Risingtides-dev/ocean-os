//! Filesystem-first agent definitions — an agent is a **folder**, the way an
//! eve.dev / Next.js app is a folder, but Rust-native and read by the daemon.
//!
//! The daemon classifies *which* agent to run by resolving a name against an
//! agents root directory. Identity comes from the path, never a `name` field
//! inside a file — `agents/researcher/` IS the agent `researcher`.
//!
//! ```text
//! agents/
//!   <name>/
//!     agent.toml        runtime config: model, description, tools, permissions
//!     instructions.md   base system prompt (the only required slot)
//!     skills/*.md       on-demand procedures, discovered by filename
//!     tools/*           tool allowlist entries, discovered by filename stem
//!     subagents/<id>/   nested agents, same shape, recursive
//! ```
//!
//! This is deliberately the smallest useful core: discover, resolve, compose a
//! system prompt. No build step, no codegen — the daemon reads the tree live,
//! so an operator edits a prompt and the next turn picks it up (same hot-read
//! contract as the existing surface profiles).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-agent runtime config, parsed from `agent.toml`. Every field is optional
/// so a bare `instructions.md` folder is already a valid agent (the lazy path);
/// `agent.toml` only exists to override defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    /// Gateway/provider model id, e.g. `claude-opus-4-7`. `None` =
    /// inherit the daemon default.
    #[serde(default)]
    pub model: Option<String>,
    /// One-line description. Required-by-convention on a subagent: the parent
    /// reads it to decide when to delegate (mirrors eve's subagent rule).
    #[serde(default)]
    pub description: Option<String>,
    /// Tool allowlist by name. Empty = inherit/allow all (no narrowing). These
    /// names match already-compiled built-in tools (`ocean_runtime` tool
    /// `name()`s like `web_fetch`, `bash`, `edit`) — tier-0 binding, no rebuild.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Forward-declared capability provider refs the daemon binds this agent to
    /// when it classifies the folder. Each entry names a [`CapabilityProvider`]
    /// source by scheme:
    ///
    /// - `builtin:<tool>`   — a tool already compiled into the daemon (tier 0)
    /// - `subprocess:<path>` — a crate-as-binary spoken over stdio JSON-RPC
    ///   (`ocean-plugin` SubprocessPlugin) — sideloaded, no daemon rebuild
    /// - `wasm:<path>`      — a sandboxed wasmtime skill pack (`ocean-plugin`,
    ///   behind the future `wasm` feature) — sideloaded, no daemon rebuild
    /// - `mcp:<name>`       — a configured MCP server (`ocean-mcp`)
    ///
    /// ponytail: parsed and surfaced now; the resolver does NOT spawn anything.
    /// The daemon binds `subprocess:`/`wasm:` entries to real providers as those
    /// `ocean-plugin` lanes land — this field is the declared contract, not a
    /// loader. `builtin:`/`mcp:` are bindable against today's registry.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Coarse permission posture for this agent. `None` = inherit daemon policy.
    #[serde(default)]
    pub yolo: Option<bool>,
}

/// A discovered skill: `skills/<name>.md`. Body is read lazily by the caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
}

/// A fully resolved agent definition. The product of walking one `<name>/`
/// folder under the agents root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentDef {
    /// Identity, taken from the directory name. Never read from a file.
    pub name: String,
    pub root: PathBuf,
    pub config: AgentConfig,
    /// Contents of `instructions.md`, empty string if absent.
    pub instructions: String,
    /// `skills/*.md`, sorted by name for stable ordering.
    pub skills: Vec<Skill>,
    /// Tool allowlist entry names discovered under `tools/` (filename stems),
    /// merged with any `config.tools`. Empty = no narrowing.
    pub tools: Vec<String>,
    /// Names of child agents under `subagents/`, resolvable with [`resolve`]
    /// against `<root>/subagents`.
    pub subagents: Vec<String>,
}

impl AgentDef {
    /// The composed system prompt for this agent: its `instructions.md`. Returns
    /// `None` when the agent authored no instructions (caller falls back to the
    /// compiled base prompt). Kept separate from `instructions` so the field can
    /// stay a plain `String` while callers get explicit emptiness semantics.
    pub fn system_prompt(&self) -> Option<&str> {
        let trimmed = self.instructions.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    /// Effective tool allowlist: `agent.toml` `tools` plus `tools/` filenames,
    /// de-duplicated, order-stable. Empty means "do not narrow".
    pub fn effective_tools(&self) -> Vec<String> {
        let mut out = self.config.tools.clone();
        for t in &self.tools {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
        out
    }
}

/// List the agent names directly under `root` — every subdirectory that looks
/// like an agent (has `instructions.md` or `agent.toml`). Sorted, stable.
/// Returns an empty vec when `root` does not exist, so callers don't special-case
/// a missing agents dir.
pub fn discover(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("instructions.md").is_file() || path.join("agent.toml").is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Resolve a single agent by name under `root`. This is the daemon's
/// classification entry point: given the invoked agent name, walk
/// `root/<name>/` and produce its [`AgentDef`].
///
/// Errors only on genuinely malformed input — a name that escapes the root, a
/// missing folder, or an `agent.toml` that won't parse. A folder with neither
/// `instructions.md` nor `agent.toml` is treated as "not an agent" (missing).
pub fn resolve(root: &Path, name: &str) -> Result<AgentDef, ResolveError> {
    // Path-derived identity must not let a name traverse out of the root.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ResolveError::InvalidName(name.to_string()));
    }
    let dir = root.join(name);
    let has_instructions = dir.join("instructions.md").is_file();
    let has_config = dir.join("agent.toml").is_file();
    if !dir.is_dir() || (!has_instructions && !has_config) {
        return Err(ResolveError::NotFound(name.to_string()));
    }

    let config = if has_config {
        let raw = fs::read_to_string(dir.join("agent.toml"))
            .map_err(|e| ResolveError::Io(dir.join("agent.toml"), e.to_string()))?;
        toml::from_str(&raw).map_err(|e| ResolveError::Config(name.to_string(), e.to_string()))?
    } else {
        AgentConfig::default()
    };

    let instructions = if has_instructions {
        fs::read_to_string(dir.join("instructions.md"))
            .map_err(|e| ResolveError::Io(dir.join("instructions.md"), e.to_string()))?
    } else {
        String::new()
    };

    let skills = list_stems(&dir.join("skills"), Some("md"))
        .into_iter()
        .map(|(name, path)| Skill { name, path })
        .collect();

    let tools = list_stems(&dir.join("tools"), None)
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    let subagents = discover(&dir.join("subagents"));

    Ok(AgentDef {
        name: name.to_string(),
        root: dir,
        config,
        instructions,
        skills,
        tools,
        subagents,
    })
}

/// List `(stem, path)` for files directly under `dir`, optionally filtered by
/// extension. Sorted by stem. Missing dir = empty.
fn list_stems(dir: &Path, ext: Option<&str>) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(want) = ext {
            if path.extension().and_then(|e| e.to_str()) != Some(want) {
                continue;
            }
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push((stem.to_string(), path.clone()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Failure modes for [`resolve`]. Distinct variants so the daemon can map a
/// bad name to 400 and a missing agent to 404.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Name is empty or contains path separators / `..` (traversal guard).
    InvalidName(String),
    /// No such agent folder, or the folder has no authored slots.
    NotFound(String),
    /// `agent.toml` failed to parse.
    Config(String, String),
    /// Filesystem read error on an authored slot.
    Io(PathBuf, String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::InvalidName(n) => write!(f, "invalid agent name: {n:?}"),
            ResolveError::NotFound(n) => write!(f, "no agent {n:?} under agents root"),
            ResolveError::Config(n, e) => write!(f, "agent {n:?} agent.toml: {e}"),
            ResolveError::Io(p, e) => write!(f, "reading {}: {e}", p.display()),
        }
    }
}

impl std::error::Error for ResolveError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a throwaway agents tree under a unique temp dir. Returns the root.
    /// `tag` makes the path unique per test so parallel tests don't wipe each
    /// other's tree (pid alone collides — all tests share one process).
    fn scaffold(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ocean-agentdir-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        // agents/researcher — full shape
        let r = root.join("researcher");
        fs::create_dir_all(r.join("skills")).unwrap();
        fs::create_dir_all(r.join("tools")).unwrap();
        fs::create_dir_all(r.join("subagents")).unwrap();
        fs::write(
            r.join("agent.toml"),
            "model = \"claude-opus-4-7\"\n\
             description = \"deep researcher\"\n\
             tools = [\"web_search\"]\n\
             yolo = true\n",
        )
        .unwrap();
        fs::write(r.join("instructions.md"), "You are a careful researcher.\n").unwrap();
        fs::write(r.join("skills").join("summarize.md"), "# summarize\n").unwrap();
        fs::write(r.join("skills").join("cite.md"), "# cite\n").unwrap();
        fs::write(r.join("tools").join("fetch.rs"), "// fetch tool\n").unwrap();
        // a child agent
        let child = r.join("subagents").join("fact_checker");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("instructions.md"), "Check facts.\n").unwrap();

        // agents/minimal — just instructions, no agent.toml (lazy path)
        let m = root.join("minimal");
        fs::create_dir_all(&m).unwrap();
        fs::write(m.join("instructions.md"), "Minimal agent.\n").unwrap();

        // a non-agent dir (no slots) — must be ignored by discover
        fs::create_dir_all(root.join("not_an_agent")).unwrap();

        root
    }

    #[test]
    fn discover_lists_only_real_agents() {
        let root = scaffold("discover");
        assert_eq!(discover(&root), vec!["minimal", "researcher"]);
        // missing root is empty, not an error
        assert!(discover(&root.join("nope")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_full_agent() {
        let root = scaffold("full");
        let def = resolve(&root, "researcher").unwrap();
        assert_eq!(def.name, "researcher");
        assert_eq!(def.config.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(def.config.description.as_deref(), Some("deep researcher"));
        assert_eq!(def.config.yolo, Some(true));
        assert_eq!(def.system_prompt(), Some("You are a careful researcher."));
        // skills sorted: cite, summarize
        let skill_names: Vec<_> = def.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(skill_names, vec!["cite", "summarize"]);
        // tools merge: agent.toml web_search + tools/ fetch
        assert_eq!(def.effective_tools(), vec!["web_search", "fetch"]);
        // nested subagent discovered
        assert_eq!(def.subagents, vec!["fact_checker"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_minimal_agent_needs_no_config() {
        let root = scaffold("minimal");
        let def = resolve(&root, "minimal").unwrap();
        assert_eq!(def.config, AgentConfig::default());
        assert_eq!(def.system_prompt(), Some("Minimal agent."));
        assert!(def.skills.is_empty());
        assert!(def.effective_tools().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_rejects_traversal_and_missing() {
        let root = scaffold("reject");
        assert_eq!(
            resolve(&root, "../etc"),
            Err(ResolveError::InvalidName("../etc".into()))
        );
        assert_eq!(
            resolve(&root, "a/b"),
            Err(ResolveError::InvalidName("a/b".into()))
        );
        assert_eq!(
            resolve(&root, "not_an_agent"),
            Err(ResolveError::NotFound("not_an_agent".into()))
        );
        assert_eq!(
            resolve(&root, "ghost"),
            Err(ResolveError::NotFound("ghost".into()))
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The shipped reference agent (docs/examples/agents/researcher) must stay a
    /// valid folder-as-agent: this resolves it for real so the example can't rot
    /// out of sync with the resolver. CARGO_MANIFEST_DIR is the crate dir; the
    /// examples live two levels up at the repo root.
    #[test]
    fn shipped_example_agent_resolves() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/agents");
        assert!(discover(&root).contains(&"researcher".to_string()));

        let def = resolve(&root, "researcher").expect("example agent must resolve");
        // The example's model must be a REAL Ocean alias so model-honoring takes
        // effect rather than silently fail-soft to global on a typo.
        let model = def
            .config
            .model
            .as_deref()
            .expect("example declares a model");
        assert!(
            crate::known_models().iter().any(|m| m.id == model),
            "example model {model:?} must be a known Ocean alias",
        );
        assert!(def.config.description.is_some());
        assert!(def.system_prompt().is_some(), "has instructions.md");
        assert!(def.effective_tools().contains(&"web_fetch".to_string()));
        assert!(def.skills.iter().any(|s| s.name == "summarize"));
        assert_eq!(def.subagents, vec!["fact-checker"]);

        // The declared subagent resolves too, with the required description.
        let child = resolve(&def.root.join("subagents"), "fact-checker")
            .expect("example subagent must resolve");
        assert!(
            child.config.description.is_some(),
            "subagent declares a description"
        );
    }
}
