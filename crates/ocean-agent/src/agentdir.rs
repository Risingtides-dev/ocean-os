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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-agent runtime config, parsed from `agent.toml`. Every field is optional
/// so a bare `instructions.md` folder is already a valid agent (the lazy path);
/// `agent.toml` only exists to override defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    /// Gateway/provider model id, e.g. `claude-opus-5`. `None` =
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
    /// Tier-1 **subprocess** capabilities the agent binds when it runs a turn.
    /// Unlike the `capabilities` scheme-strings above (a forward-declared,
    /// surface-only contract), each entry here is a concrete, launchable spec:
    /// the turn constructs an `ocean-plugin` `SubprocessPlugin` -> `PluginProvider`
    /// from it and merges the resulting tools into the turn's `CapabilityRegistry`
    /// alongside the built-ins (tools namespaced `plugin__<name>__<tool>`).
    ///
    /// Declared in `agent.toml` as a TOML array of tables:
    ///
    /// ```toml
    /// [[subprocess_capability]]
    /// name = "scrape"                 # namespaces its tools; defaults to command stem
    /// command = "./tools/scrape"      # relative entries resolve against the agent folder
    /// args = ["--stdio"]
    /// cwd = "."                       # optional; defaults to the agent folder
    /// env = { API_BASE = "https://…" } # optional extra child env
    /// ```
    ///
    /// Empty (the default) = a data-only agent that binds no subprocess tools, so
    /// every existing agent folder is unaffected. Fail-soft at bind time: a spec
    /// whose command can't spawn is logged and skipped, never breaking the turn.
    #[serde(default, rename = "subprocess_capability")]
    pub subprocess_capabilities: Vec<SubprocessCapability>,
    /// Coarse permission posture for this agent. `None` = inherit daemon policy.
    #[serde(default)]
    pub yolo: Option<bool>,
}

/// A concrete tier-1 subprocess capability declared in `agent.toml`. The turn
/// launches `command` (with `args`/`cwd`/`env`) as an `ocean-plugin`
/// `SubprocessPlugin` speaking JSON-RPC over stdio and folds its tools into the
/// turn's `CapabilityRegistry`. This is the *launchable* shape; the string
/// `capabilities` field stays the forward-declared contract for `builtin:`/`mcp:`/
/// `wasm:` schemes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SubprocessCapability {
    /// Stable name used to namespace this capability's tools
    /// (`plugin__<name>__<tool>`). Optional in the TOML: when omitted it defaults
    /// to the `command`'s file stem (see [`SubprocessCapability::effective_name`]),
    /// so a minimal spec is just a `command`.
    #[serde(default)]
    pub name: Option<String>,
    /// The executable to launch. A relative path (including a bare filename)
    /// resolves against the agent folder — the same base-dir rule
    /// `SubprocessPlugin::launch` applies to a plugin pack's `entry`; an absolute
    /// path is used as-is.
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Real working directory for the child, resolved against the canonical
    /// agent folder when relative. `None` defaults to the agent folder. A
    /// declared path must canonicalize to an existing directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Explicit environment variables injected into the child. The child does
    /// not inherit the launcher's ambient environment.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl SubprocessCapability {
    /// The name that namespaces this capability's tools: the explicit `name` if
    /// set, else the `command`'s file stem, else the raw `command`. Never empty
    /// for a non-empty `command`.
    pub fn effective_name(&self) -> String {
        if let Some(name) = self
            .name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
        {
            return name.to_string();
        }
        Path::new(&self.command)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.command.clone())
    }
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

/// Resolve an agent exclusively from an immutable, caller-captured file map.
///
/// Keys are normalized `/`-separated paths relative to the agent directory.
/// The caller owns filesystem confinement and captures every file before this
/// function runs; parsing never reopens a pathname. This lets authorization
/// layers bind the exact bytes they hashed to the runtime profile they admit.
pub fn resolve_snapshot(
    root: &Path,
    name: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<AgentDef, ResolveError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ResolveError::InvalidName(name.to_string()));
    }
    let has_instructions = files.contains_key("instructions.md");
    let has_config = files.contains_key("agent.toml");
    if !has_instructions && !has_config {
        return Err(ResolveError::NotFound(name.to_string()));
    }

    let config = match files.get("agent.toml") {
        Some(bytes) => {
            let raw = std::str::from_utf8(bytes)
                .map_err(|error| ResolveError::Io(root.join("agent.toml"), error.to_string()))?;
            toml::from_str(raw)
                .map_err(|error| ResolveError::Config(name.to_string(), error.to_string()))?
        }
        None => AgentConfig::default(),
    };
    let instructions = match files.get("instructions.md") {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|error| ResolveError::Io(root.join("instructions.md"), error.to_string()))?
            .to_owned(),
        None => String::new(),
    };

    let skills = snapshot_stems(files, "skills", Some("md"))
        .into_iter()
        .map(|name| Skill {
            path: root.join("skills").join(format!("{name}.md")),
            name,
        })
        .collect();
    let tools = snapshot_stems(files, "tools", None);
    let mut subagents = BTreeSet::new();
    for relative in files.keys() {
        let mut components = relative.split('/');
        if components.next() != Some("subagents") {
            continue;
        }
        let Some(child) = components.next() else {
            continue;
        };
        let Some(slot) = components.next() else {
            continue;
        };
        if components.next().is_none() && matches!(slot, "instructions.md" | "agent.toml") {
            subagents.insert(child.to_string());
        }
    }

    Ok(AgentDef {
        name: name.to_string(),
        root: root.to_path_buf(),
        config,
        instructions,
        skills,
        tools,
        subagents: subagents.into_iter().collect(),
    })
}

fn snapshot_stems(
    files: &BTreeMap<String, Vec<u8>>,
    directory: &str,
    extension: Option<&str>,
) -> Vec<String> {
    let mut stems = BTreeSet::new();
    for relative in files.keys() {
        let Some((parent, file_name)) = relative.rsplit_once('/') else {
            continue;
        };
        if parent != directory {
            continue;
        }
        let path = Path::new(file_name);
        if extension.is_some_and(|expected| {
            path.extension().and_then(|value| value.to_str()) != Some(expected)
        }) {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            stems.insert(stem.to_string());
        }
    }
    stems.into_iter().collect()
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

// ---------------------------------------------------------------------------
// Lint / validation
// ---------------------------------------------------------------------------

/// Severity of a lint diagnostic produced by [`validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warn => write!(f, "warn"),
        }
    }
}

/// A single lint finding from [`validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// The file or directory this finding points at, if applicable.
    pub path: Option<PathBuf>,
    /// Human-readable message explaining the problem and, where possible,
    /// how to fix it.
    pub message: String,
}

impl Diagnostic {
    fn error_at(path: PathBuf, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            path: Some(path),
            message: message.into(),
        }
    }
    fn warn_at(path: PathBuf, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            path: Some(path),
            message: message.into(),
        }
    }
}

/// The set of tool names the daemon ships as builtins today
/// (`ocean_runtime::tools::default_tools()` names). Kept as a
/// plain `HashSet` so [`validate`] can O(1)-check the agent's
/// declared allowlist without pulling in the runtime's async
/// machinery.
///
/// Update this list when `ocean_runtime::tools::mod.rs`
/// `default_tools()` gains or loses a tool.
pub fn builtin_tool_names() -> std::collections::HashSet<&'static str> {
    [
        "read",
        "write",
        "edit",
        "bash",
        "ls",
        "grep",
        "glob",
        "web_fetch",
        "todo",
        "component_render",
        "component_unmount",
        "surface_patch",
        "slack_canvas",
        "component_wait",
    ]
    .into_iter()
    .collect()
}

/// Lint a resolved [`AgentDef`] and return all findings.
///
/// `validate` is side-effect-free: it reads skill file bodies
/// lazily to check for empty content, but never writes anything and
/// never invokes the agent.  Returns an empty `Vec` when the agent
/// folder is fully valid.
///
/// **Error** severity means the agent WILL fail or behave incorrectly at
/// invoke time (e.g. no system prompt, unrecognised model alias).
/// **Warn** severity means the agent may behave unexpectedly (e.g. unknown
/// tool name that could be a typo, blank skill, dangling subagent reference).
///
/// The caller (e.g. `ocean agents lint`) should exit non-zero when any
/// `Error`-severity diagnostic is present.
pub fn validate(def: &AgentDef) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // --- instructions.md ---------------------------------------------------
    // A missing or blank instructions.md means the agent has no system prompt
    // at all — the daemon will fall back to the compiled base prompt, which is
    // almost certainly not what the operator intended.
    if def.system_prompt().is_none() {
        diags.push(Diagnostic::error_at(
            def.root.join("instructions.md"),
            "instructions.md is missing or empty; the agent has no system prompt \
             (create instructions.md with at least one non-whitespace line)",
        ));
    }

    // --- model alias -------------------------------------------------------
    // A model id that's not in the known-models catalogue will silently fall
    // back to the daemon's global model at invoke time (fail-soft path in
    // `AgentRuntime::prompt`). Surface it as an error so the operator can
    // see the typo/outdated alias before wasting a turn.
    if let Some(model) = &def.config.model {
        let known = crate::known_models();
        if !known.iter().any(|m| m.id == *model) {
            let mut known_ids: Vec<&str> = known.iter().map(|m| m.id.as_str()).collect();
            known_ids.sort_unstable();
            diags.push(Diagnostic::error_at(
                def.root.join("agent.toml"),
                format!(
                    "model {:?} is not a known Ocean alias; known models: {}",
                    model,
                    known_ids.join(", ")
                ),
            ));
        }
    }

    // --- tool allowlist ----------------------------------------------------
    // Tools listed in the allowlist but absent from the builtin registry may
    // narrow the agent unexpectedly. Warn rather than error — future sideloaded
    // tools via subprocess:/wasm: may legitimately extend the set.
    let builtins = builtin_tool_names();
    for tool in def.effective_tools() {
        if !builtins.contains(tool.as_str()) {
            let mut names: Vec<_> = builtins.iter().copied().collect();
            names.sort_unstable();
            diags.push(Diagnostic::warn_at(
                def.root.join("agent.toml"),
                format!(
                    "tool {:?} is not a known builtin; known builtins: {}. \
                     If this is a sideloaded tool (subprocess:/wasm:) it may be intentional.",
                    tool,
                    names.join(", ")
                ),
            ));
        }
    }

    // --- subagents ---------------------------------------------------------
    // A name listed in subagents/ that can't be resolved is a dangling
    // reference — the agent will fail when trying to delegate to it.
    let subagents_root = def.root.join("subagents");
    for sub_name in &def.subagents {
        if let Err(e) = resolve(&subagents_root, sub_name) {
            diags.push(Diagnostic::warn_at(
                subagents_root.join(sub_name),
                format!("subagent {sub_name:?} does not resolve: {e}"),
            ));
        }
    }

    // --- skills ------------------------------------------------------------
    // A skill file that exists but is entirely whitespace is a blank procedure
    // — the agent will see it in its skill list but get no useful content.
    for skill in &def.skills {
        match fs::read_to_string(&skill.path) {
            Ok(body) if body.trim().is_empty() => {
                diags.push(Diagnostic::warn_at(
                    skill.path.clone(),
                    format!(
                        "skill {:?} exists but is empty; add content or remove the file",
                        skill.name
                    ),
                ));
            }
            // Non-empty or unreadable (a transient IO error on a file that
            // existed at resolve time is not reported as a lint finding).
            _ => {}
        }
    }

    diags
}

/// Everything an operator can author for an agent from a surface.
///
/// This is the *write* counterpart to [`AgentDef`]. `instructions` is the only
/// field the folder truly requires — a bare `instructions.md` is already a
/// valid agent — so everything else is optional and simply omitted from
/// `agent.toml` when unset, keeping hand-written folders and UI-written ones
/// the same shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentSpec {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub yolo: Option<bool>,
    /// The system prompt. Required and non-empty: an agent with no instructions
    /// has nothing to be.
    #[serde(default)]
    pub instructions: String,
}

/// Why an agent write was refused.
#[derive(Debug)]
pub enum WriteError {
    /// Name is empty, too long, contains a separator/`..`, or is not the
    /// restricted charset. Identity comes from the path, so an unsafe name is
    /// a traversal, not a typo.
    InvalidName(String),
    /// `instructions` was empty. The one required slot.
    MissingInstructions,
    /// Create was asked for and the agent already exists.
    AlreadyExists(String),
    /// Update or delete was asked for and it does not.
    NotFound(String),
    /// Filesystem error.
    Io(PathBuf, String),
    /// Serializing `agent.toml` failed.
    Encode(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::InvalidName(n) => write!(
                f,
                "invalid agent name {n:?}: use 1-64 chars of a-z, 0-9, '-' or '_'"
            ),
            WriteError::MissingInstructions => {
                write!(
                    f,
                    "instructions are required; an agent needs a system prompt"
                )
            }
            WriteError::AlreadyExists(n) => write!(f, "agent {n:?} already exists"),
            WriteError::NotFound(n) => write!(f, "no agent named {n:?}"),
            WriteError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            WriteError::Encode(e) => write!(f, "could not encode agent.toml: {e}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Names that are safe as a directory component AND readable in a roster.
///
/// Stricter than [`resolve`]'s traversal guard on purpose: `resolve` must keep
/// reading whatever an operator already hand-created, but anything we CREATE
/// should be conservative. Rejecting leading dots also keeps an agent from
/// being written as a hidden folder that `discover` would skip.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Create or update an agent folder.
///
/// `must_be_new` distinguishes create from update so a surface's "new agent"
/// cannot silently overwrite an existing one that the operator forgot about.
///
/// Writes are additive to the folder: `skills/`, `tools/`, `subagents/` and any
/// other authored slot are left exactly as they are, so editing an agent from a
/// surface never destroys work done by hand on disk.
pub fn write(
    root: &Path,
    name: &str,
    spec: &AgentSpec,
    must_be_new: bool,
) -> Result<AgentDef, WriteError> {
    if !valid_agent_name(name) {
        return Err(WriteError::InvalidName(name.to_string()));
    }
    if spec.instructions.trim().is_empty() {
        return Err(WriteError::MissingInstructions);
    }
    let dir = root.join(name);
    let exists = dir.join("instructions.md").is_file();
    if must_be_new && exists {
        return Err(WriteError::AlreadyExists(name.to_string()));
    }
    if !must_be_new && !exists {
        return Err(WriteError::NotFound(name.to_string()));
    }

    fs::create_dir_all(&dir).map_err(|e| WriteError::Io(dir.clone(), e.to_string()))?;

    let instructions_path = dir.join("instructions.md");
    let mut body = spec.instructions.trim_end().to_string();
    body.push('\n');
    fs::write(&instructions_path, body)
        .map_err(|e| WriteError::Io(instructions_path.clone(), e.to_string()))?;

    // Only emit keys the operator actually set. An agent.toml full of empty
    // defaults reads as configuration that was chosen, when it was not.
    let mut cfg = toml::map::Map::new();
    if let Some(d) = spec.description.as_ref().filter(|d| !d.trim().is_empty()) {
        cfg.insert("description".into(), toml::Value::String(d.trim().into()));
    }
    if let Some(m) = spec.model.as_ref().filter(|m| !m.trim().is_empty()) {
        cfg.insert("model".into(), toml::Value::String(m.trim().into()));
    }
    if !spec.tools.is_empty() {
        cfg.insert(
            "tools".into(),
            toml::Value::Array(
                spec.tools
                    .iter()
                    .map(|t| toml::Value::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.capabilities.is_empty() {
        cfg.insert(
            "capabilities".into(),
            toml::Value::Array(
                spec.capabilities
                    .iter()
                    .map(|c| toml::Value::String(c.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(y) = spec.yolo {
        cfg.insert("yolo".into(), toml::Value::Boolean(y));
    }

    let toml_path = dir.join("agent.toml");
    if cfg.is_empty() {
        // A bare instructions.md is a valid agent; do not leave an empty file
        // implying configuration exists.
        let _ = fs::remove_file(&toml_path);
    } else {
        let rendered = toml::to_string_pretty(&toml::Value::Table(cfg))
            .map_err(|e| WriteError::Encode(e.to_string()))?;
        fs::write(&toml_path, rendered)
            .map_err(|e| WriteError::Io(toml_path.clone(), e.to_string()))?;
    }

    // Re-resolve so the caller gets exactly what the daemon will read next
    // turn, rather than an echo of what we think we wrote.
    resolve(root, name).map_err(|e| WriteError::Io(dir, e.to_string()))
}

/// Delete an agent folder and everything authored under it.
pub fn remove(root: &Path, name: &str) -> Result<(), WriteError> {
    if !valid_agent_name(name) {
        return Err(WriteError::InvalidName(name.to_string()));
    }
    let dir = root.join(name);
    if !dir.is_dir() {
        return Err(WriteError::NotFound(name.to_string()));
    }
    fs::remove_dir_all(&dir).map_err(|e| WriteError::Io(dir, e.to_string()))
}

#[cfg(test)]
mod write_tests {
    use super::*;

    fn spec(instructions: &str) -> AgentSpec {
        AgentSpec {
            description: Some("a test agent".into()),
            model: Some("glm-4.5-flash".into()),
            tools: vec!["read".into(), "bash".into()],
            capabilities: vec!["builtin:read".into()],
            yolo: Some(false),
            instructions: instructions.into(),
        }
    }

    #[test]
    fn create_then_resolve_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let def = write(dir.path(), "builder", &spec("You build things."), true).unwrap();
        assert_eq!(def.name, "builder");
        assert_eq!(def.config.model.as_deref(), Some("glm-4.5-flash"));
        assert_eq!(def.config.tools, vec!["read", "bash"]);
        // and it is discoverable, which is what a surface's picker lists
        assert!(discover(dir.path()).contains(&"builder".to_string()));
    }

    #[test]
    fn instructions_are_required() {
        let dir = tempfile::tempdir().unwrap();
        let err = write(dir.path(), "empty", &spec("   "), true).unwrap_err();
        assert!(matches!(err, WriteError::MissingInstructions));
        assert!(
            !dir.path().join("empty").exists(),
            "nothing partial left behind"
        );
    }

    #[test]
    fn create_refuses_to_overwrite_an_existing_agent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "dup", &spec("first"), true).unwrap();
        let err = write(dir.path(), "dup", &spec("second"), true).unwrap_err();
        assert!(matches!(err, WriteError::AlreadyExists(_)));
        // the original survives
        let def = resolve(dir.path(), "dup").unwrap();
        assert!(def.instructions.contains("first"));
    }

    #[test]
    fn update_requires_the_agent_to_exist() {
        let dir = tempfile::tempdir().unwrap();
        let err = write(dir.path(), "ghost", &spec("x"), false).unwrap_err();
        assert!(matches!(err, WriteError::NotFound(_)));
    }

    #[test]
    fn update_preserves_hand_authored_slots() {
        // Editing from a surface must never delete work done on disk.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "keeper", &spec("v1"), true).unwrap();
        let skills = dir.path().join("keeper/skills");
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("deploy.md"), "hand written").unwrap();

        write(dir.path(), "keeper", &spec("v2"), false).unwrap();
        assert_eq!(
            fs::read_to_string(skills.join("deploy.md")).unwrap(),
            "hand written"
        );
    }

    #[test]
    fn a_name_that_could_traverse_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "../escape",
            "a/b",
            "..",
            "",
            ".hidden",
            "-lead",
            "UPPER",
            "sp ace",
        ] {
            assert!(
                write(dir.path(), bad, &spec("x"), true).is_err(),
                "expected reject: {bad:?}"
            );
        }
        // and nothing was created outside the root
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn an_unset_config_writes_no_agent_toml() {
        let dir = tempfile::tempdir().unwrap();
        let bare = AgentSpec {
            instructions: "Just a prompt.".into(),
            ..Default::default()
        };
        write(dir.path(), "bare", &bare, true).unwrap();
        assert!(dir.path().join("bare/instructions.md").is_file());
        assert!(
            !dir.path().join("bare/agent.toml").exists(),
            "an empty agent.toml would imply configuration that was never chosen"
        );
        // still a valid agent
        assert!(resolve(dir.path(), "bare").is_ok());
    }

    #[test]
    fn remove_deletes_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "temp", &spec("x"), true).unwrap();
        remove(dir.path(), "temp").unwrap();
        assert!(!dir.path().join("temp").exists());
        assert!(matches!(
            remove(dir.path(), "temp").unwrap_err(),
            WriteError::NotFound(_)
        ));
    }

    #[test]
    fn remove_refuses_an_unsafe_name() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            remove(dir.path(), "../etc").unwrap_err(),
            WriteError::InvalidName(_)
        ));
    }
}

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
            "model = \"claude-opus-5\"\n\
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
        assert_eq!(def.config.model.as_deref(), Some("claude-opus-5"));
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

    /// A `[[subprocess_capability]]` table in agent.toml parses into a concrete,
    /// launchable spec. This is the tier-1 binding shape the turn constructs a
    /// SubprocessPlugin from — distinct from the forward-declared `capabilities`
    /// scheme-strings.
    #[test]
    fn resolve_parses_subprocess_capabilities() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-subproc-cap",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("bot");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("instructions.md"), "Do stuff.\n").unwrap();
        fs::write(
            dir.join("agent.toml"),
            "description = \"has a subprocess tool\"\n\
             [[subprocess_capability]]\n\
             name = \"scrape\"\n\
             command = \"./tools/scrape\"\n\
             args = [\"--stdio\"]\n\
             cwd = \".\"\n\
             env = { API_BASE = \"https://example.test\" }\n\
             \n\
             [[subprocess_capability]]\n\
             command = \"/usr/local/bin/enrich\"\n",
        )
        .unwrap();

        let def = resolve(&root, "bot").unwrap();
        let caps = &def.config.subprocess_capabilities;
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].effective_name(), "scrape");
        assert_eq!(caps[0].command, "./tools/scrape");
        assert_eq!(caps[0].args, vec!["--stdio"]);
        assert_eq!(caps[0].cwd.as_deref(), Some("."));
        assert_eq!(
            caps[0].env.get("API_BASE").map(String::as_str),
            Some("https://example.test")
        );
        // Second entry omits `name` → defaults to the command's file stem.
        assert!(caps[1].name.is_none());
        assert_eq!(caps[1].effective_name(), "enrich");

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
    fn snapshot_resolution_uses_only_captured_bytes() {
        let root = PathBuf::from("/captured/agents/builder");
        let mut files = BTreeMap::new();
        files.insert(
            "agent.toml".to_string(),
            b"model = 'claude-sonnet-4-5'\ntools = ['read']\n".to_vec(),
        );
        files.insert(
            "instructions.md".to_string(),
            b"captured instructions\n".to_vec(),
        );
        files.insert("tools/check.sh".to_string(), b"#!/bin/sh\n".to_vec());
        files.insert("skills/review.md".to_string(), b"captured skill\n".to_vec());
        files.insert(
            "subagents/worker/instructions.md".to_string(),
            b"captured worker\n".to_vec(),
        );

        let definition = resolve_snapshot(&root, "builder", &files).unwrap();

        assert_eq!(definition.root, root);
        assert_eq!(definition.system_prompt(), Some("captured instructions"));
        assert_eq!(
            definition.config.model.as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(definition.effective_tools(), vec!["read", "check"]);
        assert_eq!(definition.skills[0].name, "review");
        assert_eq!(definition.subagents, vec!["worker"]);
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

    // --- validate() tests --------------------------------------------------

    /// A valid agent (researcher from scaffold) with a known model must produce
    /// zero diagnostics. The scaffold uses "web_search" and "fetch" which ARE
    /// unknown builtins — swap to a clean minimal agent for this test.
    #[test]
    fn validate_clean_agent_returns_no_diagnostics() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-clean",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"claude-opus-5\"\ndescription = \"test bot\"\ntools = [\"web_fetch\"]\n",
        )
        .unwrap();
        fs::write(agent_dir.join("instructions.md"), "You are a test bot.\n").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "clean agent must have no error diagnostics, got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An agent with no instructions.md (config-only) must produce an Error
    /// diagnostic pointing at instructions.md.
    #[test]
    fn validate_missing_instructions_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-no-instr",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        // Only agent.toml — no instructions.md.
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"claude-opus-5\"\ndescription = \"test bot\"\n",
        )
        .unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "missing instructions.md must produce an error diagnostic"
        );
        assert!(
            errors.iter().any(|d| d.message.contains("instructions.md")),
            "error must mention instructions.md; got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An agent with a whitespace-only instructions.md must also produce an
    /// Error (system_prompt() returns None for whitespace).
    #[test]
    fn validate_blank_instructions_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-blank-instr",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("instructions.md"), "   \n\t\n   ").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "blank instructions.md must produce an error diagnostic"
        );
        assert!(
            errors.iter().any(|d| d.message.contains("instructions.md")),
            "error must mention instructions.md; got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A bad model alias in agent.toml must produce an Error diagnostic
    /// that names the bad alias.
    #[test]
    fn validate_bad_model_alias_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-bad-model",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"not-a-real-model-xyz\"\ndescription = \"test\"\n",
        )
        .unwrap();
        fs::write(agent_dir.join("instructions.md"), "Do stuff.\n").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "bad model alias must produce an error diagnostic"
        );
        assert!(
            errors
                .iter()
                .any(|d| d.message.contains("not-a-real-model-xyz")),
            "error must name the bad model; got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A tool name not in the builtin set must produce a Warn diagnostic
    /// that names the unknown tool.
    #[test]
    fn validate_unknown_tool_is_warn() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-unknown-tool",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"claude-opus-5\"\ndescription = \"test\"\ntools = [\"not_a_real_tool\"]\n",
        )
        .unwrap();
        fs::write(agent_dir.join("instructions.md"), "Do stuff.\n").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let warns: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warn)
            .collect();
        assert!(
            !warns.is_empty(),
            "unknown tool must produce a warn diagnostic"
        );
        assert!(
            warns.iter().any(|d| d.message.contains("not_a_real_tool")),
            "warn must name the unknown tool; got: {warns:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
