//! `prepare()` — the read-only **pre-turn preparation step** for the Longhouse
//! consult-before-acting loop (`docs/LONGHOUSE.md` §"First safe integration
//! slice", lines 101-115).
//!
//! Before a turn runs, the daemon can send Longhouse a compact [`TurnBrief`]
//! (the prompt + a little session context). Longhouse answers with a
//! [`TurnPrep`]: the handful of **skills / SOPs / workflows** most relevant to
//! that prompt, each as a *compact* brief (name + one-line when-to-use, never
//! the full body). The daemon injects those briefs into the main agent prompt;
//! the agent still routes every real action back through daemon permission
//! gates. Nothing here mutates the hive — this module is **read-only** and
//! **fail-open**: if no skills load or nothing matches, it returns an empty
//! [`TurnPrep`] rather than erroring, so a missing/garbled skill library can
//! never block a turn.
//!
//! ## What this is (phase 1 of OCEAN-215)
//!
//! * **Loaders** that index the documented skill sources
//!   (`docs/LONGHOUSE.md` lines 119-122):
//!   - `~/.spawner/skills/**/skill.yaml` (spawner format),
//!   - `~/.codex/skills/**/SKILL.md` (codex format, YAML frontmatter),
//!   - repo-local `./skills/**` (either format).
//!   Missing dirs are skipped, malformed files are skipped + logged — a bad
//!   file never fails the whole load.
//! * A [`SkillIndex`] that caches the loaded [`SkillBrief`]s once (not per-call).
//! * [`SkillIndex::prepare`] — a **cheap, deterministic** keyword-overlap match
//!   between the brief's prompt and each skill's name + description, returning
//!   the top-N most relevant skills. No embeddings, no LLM (the
//!   `docs/LONGHOUSE.md` "cheap fast model reranker" is a later phase).
//!
//! SOPs and workflows have **no real on-disk source yet**, so their briefs are
//! intentionally always empty here (the types exist so the daemon contract is
//! stable and phase 2 can fill them in without a signature change). We do not
//! fabricate SOP/workflow sources.
//!
//! The daemon hook + prompt injection are **phase 2** (separate, `main.rs`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// How many skill briefs `prepare` returns at most. `docs/LONGHOUSE.md` line 129
/// calls for "3–7 compact skill briefs"; 5 sits in the middle.
pub const DEFAULT_TOP_N: usize = 5;

/// The compact task/session brief the daemon sends Longhouse before a turn.
///
/// This mirrors the daemon's own turn request shape (`POST /v1/agent/turns`)
/// minus the heavy bits — Longhouse only needs the prompt to rank skills, plus
/// a little context for future SOP/workflow selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnBrief {
    /// The daemon session this turn belongs to (opaque to Longhouse).
    #[serde(default)]
    pub session_id: String,
    /// The user/agent prompt for the upcoming turn — the text we rank against.
    pub prompt: String,
    /// Working directory of the turn, if known (reserved for repo-local skill
    /// scoping + future SOP selection).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Which client is steering (e.g. `"tui"`, `"surface"`, `"voice"`).
    /// Reserved for future client-aware SOP reminders; unused in v1 ranking.
    #[serde(default)]
    pub client_type: Option<String>,
}

impl TurnBrief {
    /// Convenience constructor for the common "just a prompt" case.
    pub fn from_prompt(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Default::default()
        }
    }
}

/// Where a loaded skill came from — useful for the daemon to fetch the full body
/// later and for debugging which library a brief was sourced from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// `~/.spawner/skills/**/skill.yaml`
    Spawner,
    /// `~/.codex/skills/**/SKILL.md`
    Codex,
    /// Repo-local `./skills/**`
    Repo,
}

/// A *compact* skill brief: name + one-line when-to-use, plus where the full
/// body lives. Deliberately NOT the full skill body — the prep loop surfaces
/// candidates; the daemon fetches the body on demand if a skill is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillBrief {
    /// Human-facing skill name (spawner `name`, codex frontmatter `name`).
    pub name: String,
    /// One-line "when to use" / description (spawner `description`, codex
    /// frontmatter `description`). May be empty if the file omitted it.
    pub description: String,
    /// Absolute path to the source file the brief was parsed from.
    pub source_path: PathBuf,
    /// Which library this came from.
    pub source: SkillSource,
}

/// A compact SOP reminder. **No real on-disk source exists yet** — present so
/// the daemon contract is stable; always empty in phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopBrief {
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
}

/// A compact workflow/routine suggestion. **No real on-disk source exists yet**
/// — present so the daemon contract is stable; always empty in phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowBrief {
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
}

/// The read-only result of a pre-turn preparation: the compact briefs the daemon
/// injects into the main agent prompt.
///
/// `Default` is the **fail-open** result: all empty. `prepare` returns this when
/// nothing loaded or nothing was relevant, so the prep loop never blocks a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPrep {
    pub skills: Vec<SkillBrief>,
    pub sops: Vec<SopBrief>,
    pub workflows: Vec<WorkflowBrief>,
}

impl TurnPrep {
    /// True when there is nothing to inject (the fail-open / no-op case).
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.sops.is_empty() && self.workflows.is_empty()
    }
}

/// Configurable roots for the skill index. Defaults to the documented sources
/// (`docs/LONGHOUSE.md` lines 119-122). Any root may be absent on disk — the
/// loader skips missing dirs silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoots {
    /// `~/.spawner/skills` — spawner `skill.yaml` files.
    pub spawner: Option<PathBuf>,
    /// `~/.codex/skills` — codex `SKILL.md` files.
    pub codex: Option<PathBuf>,
    /// Repo-local `./skills` (relative to a cwd, if provided).
    pub repo: Option<PathBuf>,
}

impl Default for SkillRoots {
    /// The documented default roots, with `~` expanded from `$HOME`.
    fn default() -> Self {
        Self {
            spawner: home_join(".spawner/skills"),
            codex: home_join(".codex/skills"),
            // Repo-local skills are cwd-relative and the daemon supplies the cwd
            // per-turn; the global default index does not assume one.
            repo: None,
        }
    }
}

impl SkillRoots {
    /// Roots scoped to a specific repo cwd: the documented home roots plus the
    /// repo's `./skills` dir. Used when the daemon knows the turn's cwd.
    pub fn for_cwd(cwd: impl AsRef<Path>) -> Self {
        let mut roots = Self::default();
        roots.repo = Some(cwd.as_ref().join("skills"));
        roots
    }
}

/// Expand `~/<rest>` against `$HOME`. Returns `None` if `$HOME` is unset (the
/// loader then simply has no home root — fail-open).
fn home_join(rest: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
}

/// The loaded, cached skill index. Built once via [`SkillIndex::load`] /
/// [`SkillIndex::load_from`], then queried per-turn via [`SkillIndex::prepare`]
/// — the scan does NOT re-run on every prepare call.
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: Vec<SkillBrief>,
}

impl SkillIndex {
    /// Load the index from the documented default roots (`~/.spawner/skills`,
    /// `~/.codex/skills`). Never errors: missing dirs and malformed files are
    /// skipped (the latter logged at `warn`/`debug`).
    pub fn load() -> Self {
        Self::load_from(&SkillRoots::default())
    }

    /// Load the index from explicit roots (used by tests + cwd-scoped loads).
    pub fn load_from(roots: &SkillRoots) -> Self {
        let mut skills = Vec::new();

        if let Some(dir) = &roots.spawner {
            scan_dir(dir, SkillSource::Spawner, &mut skills);
        }
        if let Some(dir) = &roots.codex {
            scan_dir(dir, SkillSource::Codex, &mut skills);
        }
        if let Some(dir) = &roots.repo {
            scan_dir(dir, SkillSource::Repo, &mut skills);
        }

        tracing::debug!(count = skills.len(), "longhouse skill index loaded");
        Self { skills }
    }

    /// Build an index directly from already-parsed briefs (test/embed helper).
    pub fn from_briefs(skills: Vec<SkillBrief>) -> Self {
        Self { skills }
    }

    /// The number of skills currently indexed.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// True when no skills are indexed.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Read-only view of the loaded briefs.
    pub fn skills(&self) -> &[SkillBrief] {
        &self.skills
    }

    /// The pre-turn preparation step. Returns the top [`DEFAULT_TOP_N`] skills
    /// most relevant to the brief's prompt by cheap keyword overlap.
    ///
    /// Deterministic + fail-open: an empty/irrelevant prompt or an empty index
    /// yields an empty [`TurnPrep`]. SOPs/workflows are always empty in phase 1.
    pub fn prepare(&self, brief: &TurnBrief) -> TurnPrep {
        TurnPrep {
            skills: self.rank_skills(&brief.prompt, DEFAULT_TOP_N),
            sops: Vec::new(),
            workflows: Vec::new(),
        }
    }

    /// Same as [`prepare`](Self::prepare) but with an explicit result cap.
    pub fn prepare_top_n(&self, brief: &TurnBrief, top_n: usize) -> TurnPrep {
        TurnPrep {
            skills: self.rank_skills(&brief.prompt, top_n),
            sops: Vec::new(),
            workflows: Vec::new(),
        }
    }

    /// Score every skill by keyword overlap with the prompt and return the best
    /// `top_n` (ties broken by name for determinism). Skills with zero overlap
    /// are dropped — we never pad results with irrelevant skills.
    fn rank_skills(&self, prompt: &str, top_n: usize) -> Vec<SkillBrief> {
        if top_n == 0 || self.skills.is_empty() {
            return Vec::new();
        }

        let prompt_terms = tokenize(prompt);
        if prompt_terms.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(u32, &SkillBrief)> = self
            .skills
            .iter()
            .filter_map(|skill| {
                let score = relevance_score(&prompt_terms, skill);
                (score > 0).then_some((score, skill))
            })
            .collect();

        // Highest score first; deterministic tie-break on name then path.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.cmp(&b.1.name))
                .then_with(|| a.1.source_path.cmp(&b.1.source_path))
        });

        scored
            .into_iter()
            .take(top_n)
            .map(|(_, skill)| skill.clone())
            .collect()
    }
}

/// Walk one root dir for skill files, parsing each into a [`SkillBrief`].
///
/// Missing dir → no-op. Unreadable/garbled file → skipped + logged, never fatal.
/// We look for `skill.yaml` (spawner) and `SKILL.md` (codex); a repo `./skills`
/// dir may hold either, so we accept both regardless of the declared source.
fn scan_dir(dir: &Path, source: SkillSource, out: &mut Vec<SkillBrief>) {
    if !dir.is_dir() {
        // Absent root is the normal case (a machine may have only one library).
        tracing::debug!(dir = %dir.display(), "longhouse skill root absent, skipping");
        return;
    }

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| match e {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::debug!(error = %err, "skipping unreadable skill dir entry");
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        let parsed = match file_name {
            "skill.yaml" | "skill.yml" => parse_skill_yaml(path),
            "SKILL.md" => parse_skill_md(path),
            _ => continue,
        };

        match parsed {
            Some(brief) => out.push(SkillBrief { source, ..brief }),
            None => {
                tracing::warn!(path = %path.display(), "skipping malformed skill file");
            }
        }
    }
}

/// Parse a spawner `skill.yaml`'s top-level `name` + `description` scalars.
///
/// We only need two top-level scalar fields, so we extract them directly rather
/// than pulling in a full YAML dependency. Returns `None` if the file is
/// unreadable or has no usable `name` (a skill with no name is unusable).
fn parse_skill_yaml(path: &Path) -> Option<SkillBrief> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = yaml_scalar(&text, "name")?;
    if name.is_empty() {
        return None;
    }
    let description = yaml_scalar(&text, "description").unwrap_or_default();
    Some(SkillBrief {
        name,
        description,
        source_path: path.to_path_buf(),
        // overwritten by scan_dir with the real source
        source: SkillSource::Spawner,
    })
}

/// Parse a codex `SKILL.md`'s YAML frontmatter `name` + `description`.
///
/// Frontmatter is the `---`-delimited block at the top of the file. Falls back
/// to the first markdown `# Heading` for the name if frontmatter lacks one.
fn parse_skill_md(path: &Path) -> Option<SkillBrief> {
    let text = std::fs::read_to_string(path).ok()?;
    let frontmatter = extract_frontmatter(&text);

    let name = frontmatter
        .as_deref()
        .and_then(|fm| yaml_scalar(fm, "name"))
        .filter(|s| !s.is_empty())
        .or_else(|| first_md_heading(&text))?;

    let description = frontmatter
        .as_deref()
        .and_then(|fm| yaml_scalar(fm, "description"))
        .unwrap_or_default();

    Some(SkillBrief {
        name,
        description,
        source_path: path.to_path_buf(),
        // overwritten by scan_dir with the real source
        source: SkillSource::Codex,
    })
}

/// Pull the `---`…`---` YAML frontmatter block out of a markdown doc, if present.
fn extract_frontmatter(text: &str) -> Option<String> {
    let trimmed = text.trim_start_matches('\u{feff}');
    let mut lines = trimmed.lines();
    // First non-empty content must be the opening fence.
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let mut body = String::new();
    for line in lines {
        if line.trim() == "---" {
            return Some(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    // No closing fence → not valid frontmatter.
    None
}

/// Extract a top-level YAML scalar `key: value` from a small YAML/frontmatter
/// block. Handles quoted ("…" / '…') and bare values, ignores indented
/// (nested) keys, and decodes the few escapes codex descriptions actually use
/// (`\"`, `\\`, `\n`, `\t`, and `\uXXXX`). Good enough for two flat fields; this
/// is deliberately not a general YAML parser.
fn yaml_scalar(block: &str, key: &str) -> Option<String> {
    for raw in block.lines() {
        // Only top-level keys (no leading indentation) — skip nested mappings.
        if raw.starts_with(char::is_whitespace) {
            continue;
        }
        let line = raw.trim_end();
        let rest = match line.strip_prefix(key) {
            Some(r) => r,
            None => continue,
        };
        // Must be exactly `key:` (not `keyfoo:`).
        let rest = match rest.strip_prefix(':') {
            Some(r) => r,
            None => continue,
        };
        let value = rest.trim();
        if value.is_empty() {
            return Some(String::new());
        }
        return Some(unquote_yaml(value));
    }
    None
}

/// Strip matching surrounding quotes and decode the common escapes. Bare values
/// are returned as-is (trimmed). Inline `#` comments on bare values are dropped.
fn unquote_yaml(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return decode_escapes(&value[1..value.len() - 1]);
    }
    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        // YAML single-quotes only escape '' → '. No backslash escapes.
        return value[1..value.len() - 1].replace("''", "'");
    }
    // Bare scalar: drop a trailing inline comment if any, then trim.
    match value.split_once(" #") {
        Some((head, _)) => head.trim().to_string(),
        None => value.trim().to_string(),
    }
}

/// Decode the backslash escapes that appear in double-quoted YAML scalars.
fn decode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => {
                        out.push('\\');
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// First `# Heading` text in a markdown doc, used as a name fallback.
fn first_md_heading(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(h) = t.strip_prefix("# ") {
            let h = h.trim();
            if !h.is_empty() {
                return Some(h.to_string());
            }
        }
    }
    None
}

/// Lowercase alphanumeric tokens of length ≥ 3, deduped. Short tokens (`a`,
/// `to`, `ai`) are dropped to avoid spurious overlaps; this keeps the match
/// cheap and the results stable.
fn tokenize(text: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
    {
        let lower = word.to_ascii_lowercase();
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

/// Relevance = count of prompt terms that appear in the skill's name or
/// description, with a name match weighted higher (the name is the strongest
/// signal of what a skill is for).
fn relevance_score(prompt_terms: &[String], skill: &SkillBrief) -> u32 {
    let name = skill.name.to_ascii_lowercase();
    let desc = skill.description.to_ascii_lowercase();
    let mut score = 0u32;
    for term in prompt_terms {
        if name.contains(term.as_str()) {
            score += 2;
        } else if desc.contains(term.as_str()) {
            score += 1;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    // ---- loader tests ----

    #[test]
    fn loads_spawner_yaml_and_codex_md() {
        let tmp = TempDir::new().unwrap();
        let spawner = tmp.path().join("spawner");
        let codex = tmp.path().join("codex");

        write(
            &spawner,
            "video/remotion/skill.yaml",
            "id: remotion\nname: Remotion Video\nversion: 1.0.0\ndescription: Build programmatic videos in React with Remotion\nowns:\n  - video-rendering\n",
        );
        write(
            &codex,
            "pdf/SKILL.md",
            "---\nname: \"pdf\"\ndescription: \"Use when reading, creating, or reviewing PDF files where layout matters.\"\n---\n\n# PDF Skill\n\nbody here\n",
        );

        let roots = SkillRoots {
            spawner: Some(spawner),
            codex: Some(codex),
            repo: None,
        };
        let index = SkillIndex::load_from(&roots);
        assert_eq!(index.len(), 2, "both skills should parse");

        let remotion = index
            .skills()
            .iter()
            .find(|s| s.name == "Remotion Video")
            .expect("spawner skill parsed");
        assert_eq!(remotion.source, SkillSource::Spawner);
        assert!(remotion.description.contains("programmatic videos"));

        let pdf = index
            .skills()
            .iter()
            .find(|s| s.name == "pdf")
            .expect("codex skill parsed");
        assert_eq!(pdf.source, SkillSource::Codex);
        assert!(pdf.description.contains("PDF files"));
    }

    #[test]
    fn missing_dir_yields_empty_no_error() {
        let roots = SkillRoots {
            spawner: Some(PathBuf::from("/nonexistent/spawner/skills/xyz")),
            codex: Some(PathBuf::from("/nonexistent/codex/skills/xyz")),
            repo: Some(PathBuf::from("/nonexistent/repo/skills")),
        };
        let index = SkillIndex::load_from(&roots);
        assert!(index.is_empty());
    }

    #[test]
    fn malformed_file_is_skipped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("spawner");

        // Good skill.
        write(
            &root,
            "good/skill.yaml",
            "name: Good Skill\ndescription: a usable skill\n",
        );
        // Malformed: no `name` field at all → skipped.
        write(&root, "bad/skill.yaml", "id: bad\nversion: 1.0.0\n");
        // Garbage that is not even close to YAML → skipped, not a panic.
        write(&root, "junk/skill.yaml", "\x00\x01 not yaml at all :::");

        let roots = SkillRoots {
            spawner: Some(root),
            codex: None,
            repo: None,
        };
        let index = SkillIndex::load_from(&roots);
        assert_eq!(index.len(), 1, "only the well-formed skill loads");
        assert_eq!(index.skills()[0].name, "Good Skill");
    }

    #[test]
    fn codex_falls_back_to_heading_when_frontmatter_lacks_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("codex");
        write(
            &root,
            "thing/SKILL.md",
            "---\ndescription: \"does a thing\"\n---\n\n# Thing Skill\n",
        );
        let roots = SkillRoots {
            spawner: None,
            codex: Some(root),
            repo: None,
        };
        let index = SkillIndex::load_from(&roots);
        assert_eq!(index.len(), 1);
        assert_eq!(index.skills()[0].name, "Thing Skill");
    }

    #[test]
    fn repo_skills_dir_accepts_either_format() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        write(
            cwd,
            "skills/a/skill.yaml",
            "name: Repo Yaml\ndescription: repo yaml skill\n",
        );
        write(
            cwd,
            "skills/b/SKILL.md",
            "---\nname: \"Repo Md\"\ndescription: \"repo md skill\"\n---\n",
        );
        let roots = SkillRoots::for_cwd(cwd);
        // Don't depend on the host's real ~/.spawner — only assert the repo ones.
        let index = SkillIndex::load_from(&SkillRoots {
            spawner: None,
            codex: None,
            repo: roots.repo,
        });
        assert_eq!(index.len(), 2);
        assert!(index.skills().iter().all(|s| s.source == SkillSource::Repo));
    }

    // ---- prepare / relevance tests ----

    fn brief(name: &str, desc: &str) -> SkillBrief {
        SkillBrief {
            name: name.to_string(),
            description: desc.to_string(),
            source_path: PathBuf::from(format!("/skills/{name}")),
            source: SkillSource::Repo,
        }
    }

    fn sample_index() -> SkillIndex {
        SkillIndex::from_briefs(vec![
            brief(
                "Remotion Video",
                "Build programmatic videos in React with Remotion compositions",
            ),
            brief(
                "Supabase Postgres",
                "Write and optimize Postgres queries, schema, and RLS policies",
            ),
            brief(
                "Slack Messaging",
                "Send and search Slack messages across channels",
            ),
        ])
    }

    #[test]
    fn prepare_surfaces_the_matching_skill_first() {
        let index = sample_index();
        let prep = index.prepare(&TurnBrief::from_prompt(
            "help me render a programmatic video with remotion",
        ));
        assert!(!prep.skills.is_empty());
        assert_eq!(prep.skills[0].name, "Remotion Video");
        // SOPs/workflows always empty in phase 1.
        assert!(prep.sops.is_empty() && prep.workflows.is_empty());
    }

    #[test]
    fn prepare_matches_on_description_keywords() {
        let index = sample_index();
        let prep = index.prepare(&TurnBrief::from_prompt(
            "optimize a slow postgres query and fix the schema",
        ));
        assert_eq!(prep.skills[0].name, "Supabase Postgres");
    }

    #[test]
    fn unrelated_prompt_returns_empty() {
        let index = sample_index();
        let prep = index.prepare(&TurnBrief::from_prompt(
            "what time is the dentist appointment tomorrow",
        ));
        assert!(
            prep.skills.is_empty(),
            "no skill should match an unrelated prompt, got {:?}",
            prep.skills
        );
    }

    #[test]
    fn empty_index_or_prompt_is_fail_open() {
        let empty = SkillIndex::default();
        assert!(empty.prepare(&TurnBrief::from_prompt("anything")).is_empty());

        let index = sample_index();
        assert!(index.prepare(&TurnBrief::from_prompt("")).is_empty());
    }

    #[test]
    fn prepare_is_deterministic_and_caps_results() {
        let index = sample_index();
        // A prompt that hits all three on a shared-ish term plus specifics.
        let b = TurnBrief::from_prompt("postgres remotion slack video schema messaging");
        let a1 = index.prepare_top_n(&b, 2);
        let a2 = index.prepare_top_n(&b, 2);
        assert_eq!(a1.skills.len(), 2, "cap honored");
        assert_eq!(a1, a2, "deterministic across runs");
    }

    #[test]
    fn top_n_zero_returns_empty() {
        let index = sample_index();
        let prep = index.prepare_top_n(&TurnBrief::from_prompt("remotion video"), 0);
        assert!(prep.skills.is_empty());
    }

    #[test]
    fn turnprep_default_is_empty_fail_open() {
        assert!(TurnPrep::default().is_empty());
    }

    /// Smoke test against the operator's real `~/.spawner` / `~/.codex` skill
    /// libraries. `#[ignore]`d so CI (which has no home skills) stays green; run
    /// locally with `cargo test -p ocean-longhouse -- --ignored real_skill`.
    #[test]
    #[ignore = "depends on the host machine's real skill libraries"]
    fn real_skill_libraries_parse() {
        let index = SkillIndex::load();
        // If the host has any libraries at all, they must parse to named briefs.
        for s in index.skills() {
            assert!(!s.name.is_empty(), "every brief has a name: {s:?}");
        }
        eprintln!("loaded {} real skills", index.len());
        let prep = index.prepare(&TurnBrief::from_prompt(
            "build a remotion video and post it to slack",
        ));
        eprintln!(
            "top matches: {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}
