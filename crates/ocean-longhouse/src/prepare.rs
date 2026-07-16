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
//! ## What this is
//!
//! * **Loaders** that index the documented skill sources
//!   (`docs/LONGHOUSE.md` §"Skill Librarian"):
//!   - `~/.config/ocean-rs/skills/**` (Ocean's native user library, either
//!     format; `OCEAN_SKILLS_DIR` overrides),
//!   - `~/.spawner/skills/**/skill.yaml` (spawner format),
//!   - `~/.codex/skills/**/SKILL.md` (codex format, YAML frontmatter),
//!   - repo-local `./skills/**` (either format).
//!
//!   Missing dirs are skipped, malformed files are skipped + logged — a bad
//!   file never fails the whole load.
//! * A [`SkillIndex`] that caches the loaded [`SkillBrief`]s once (not per-call).
//! * A **process-wide TTL cache** ([`cached_index_for`]) so the disk walk runs
//!   at most once per [`CACHE_TTL`] window per root-set — not on every turn.
//!   Now that the daemon's prepare-hook is **default-on** (OCEAN-283), a turn
//!   must never re-walk `~/.spawner` / `~/.codex` from scratch; the cache makes
//!   the steady-state cost of a consult a couple of string scans over an
//!   already-loaded `Vec<SkillBrief>`.
//! * [`SkillIndex::prepare`] — a **cheap, deterministic** relevance match
//!   between the brief's prompt and each skill's name + description (OCEAN-283
//!   ranking: term-set overlap, name-weighted, with a distinct-coverage bonus,
//!   de-duplicated, and a minimum-score floor so a single weak common-word hit
//!   never injects noise), returning the top-N most relevant skills. No
//!   embeddings, no LLM — the `docs/LONGHOUSE.md` "cheap fast model reranker"
//!   remains the named follow-up; this is the bounded deterministic prefilter
//!   (step 1 of the documented selection path) that it would sit on top of.
//!
//! SOPs have no real on-disk source yet; their briefs are intentionally always
//! empty (the type exists so the daemon contract is stable; we do not fabricate
//! SOP sources).
//!
//! Workflows **are** now populated: [`WorkflowIndex`] scans
//! `{cwd}/docs/orchestrator/workflows/` (or any explicit dir set in
//! [`WorkflowRoots`]), parses each `.md` file's YAML frontmatter (`name:` +
//! `description:`), and surfaces ranked [`WorkflowBrief`] entries. The same
//! fail-open and TTL-cache discipline applies: a missing dir returns empty,
//! malformed files are skipped, and the scan runs at most once per TTL window
//! per root via [`cached_workflows_for`] (a sibling cache keyed by
//! [`WorkflowRoots`], separate from the skill cache to prevent cross-stale
//! invalidation).
//!
//! The daemon hook + prompt injection live in `main.rs` (default-on, fail-open,
//! time-bounded); this module is the read-only library half it calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// How many skill briefs `prepare` returns at most. `docs/LONGHOUSE.md`
/// §"Skill Librarian" calls for "3–7 compact skill briefs"; 5 sits in the middle.
pub const DEFAULT_TOP_N: usize = 5;

/// How long a cached [`SkillIndex`] stays fresh before the next consult re-walks
/// disk. A skill library changes rarely (an operator edits a `skill.yaml` now
/// and then), so a coarse TTL keeps default-on consults off the disk almost
/// always while still picking up edits within a minute — no file-watcher needed.
/// Override with `OCEAN_LONGHOUSE_SKILL_TTL_SECS` (0 disables caching entirely,
/// e.g. for a test that wants a guaranteed-fresh scan).
pub const CACHE_TTL: Duration = Duration::from_secs(60);

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
    /// `~/.config/ocean-rs/skills/**` — Ocean's own user library (either format).
    Ocean,
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
/// the daemon contract is stable; always empty until a SOP loader is added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopBrief {
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
}

/// A compact workflow/routine suggestion sourced from a `.md` file with YAML
/// frontmatter (`name:` + `description:`). Populated by [`WorkflowIndex`]
/// scanning `docs/orchestrator/workflows/` (or any configured dir).
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

/// One selected skill together with the exact deterministic ranking evidence.
/// Only compact metadata is exposed; the skill body is never loaded or returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedSkillMatch {
    pub brief: SkillBrief,
    pub score: u32,
    pub matched_prompt_terms: Vec<String>,
}

/// One selected workflow together with the exact deterministic ranking evidence.
/// Only frontmatter-derived compact metadata is exposed; the body is never returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedWorkflowMatch {
    pub brief: WorkflowBrief,
    pub score: u32,
    pub matched_prompt_terms: Vec<String>,
}

/// Read-only inspection of the ordinary preparation ranking.
///
/// `prep` is the exact ordinary [`TurnPrep`] for the same brief and cap. Candidate
/// counts are the de-duplicated entries that cleared the ordinary relevance floor,
/// before `top_n` truncation; indexed counts are all compact briefs in each index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPrepInspection {
    pub skills_indexed: usize,
    pub skill_candidates: usize,
    pub workflows_indexed: usize,
    pub workflow_candidates: usize,
    pub selected_skills: Vec<ExplainedSkillMatch>,
    pub selected_workflows: Vec<ExplainedWorkflowMatch>,
    pub prep: TurnPrep,
}

/// Configurable roots for the skill index. Defaults to the documented sources
/// (`docs/LONGHOUSE.md` §"Skill Librarian"). Any root may be absent on disk — the
/// loader skips missing dirs silently.
///
/// `Hash`/`Eq` so it can key the process-wide [`cached_index_for`] cache: two
/// turns with the same roots share one loaded index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillRoots {
    /// `~/.config/ocean-rs/skills` — Ocean's native user skill library
    /// (`OCEAN_SKILLS_DIR` overrides). Accepts both `skill.yaml` and `SKILL.md`.
    pub ocean: Option<PathBuf>,
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
            ocean: ocean_root_from(std::env::var_os("OCEAN_SKILLS_DIR")),
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
        Self {
            repo: Some(cwd.as_ref().join("skills")),
            ..Self::default()
        }
    }
}

/// Expand `~/<rest>` against `$HOME`. Returns `None` if `$HOME` is unset (the
/// loader then simply has no home root — fail-open).
fn home_join(rest: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
}

/// Resolve the Ocean-native skill root: an explicit non-empty
/// `OCEAN_SKILLS_DIR` wins; otherwise `~/.config/ocean-rs/skills`. Pure in its
/// argument so tests can exercise both branches without touching the process
/// environment (env mutation races parallel tests).
fn ocean_root_from(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    match env_override {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home_join(".config/ocean-rs/skills"),
    }
}

/// The loaded, cached skill index. Built once via [`SkillIndex::load`] /
/// [`SkillIndex::load_from`], then queried per-turn via [`SkillIndex::prepare`]
/// — the scan does NOT re-run on every prepare call.
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: Vec<SkillBrief>,
}

impl SkillIndex {
    /// Load the index from the documented default roots
    /// (`~/.config/ocean-rs/skills`, `~/.spawner/skills`, `~/.codex/skills`).
    /// Never errors: missing dirs and malformed files are skipped (the latter
    /// logged at `warn`/`debug`).
    pub fn load() -> Self {
        Self::load_from(&SkillRoots::default())
    }

    /// Load the index from explicit roots (used by tests + cwd-scoped loads).
    pub fn load_from(roots: &SkillRoots) -> Self {
        let mut skills = Vec::new();

        // Ocean's native library scans first — product-owned packs lead the
        // index; foreign libraries (spawner/codex) follow.
        if let Some(dir) = &roots.ocean {
            scan_dir(dir, SkillSource::Ocean, &mut skills);
        }
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
    /// most relevant to the brief's prompt by cheap keyword overlap, plus ranked
    /// [`WorkflowBrief`]s sourced from `{cwd}/docs/orchestrator/workflows/`
    /// when the brief carries a `cwd`. SOPs remain empty (no on-disk source yet).
    ///
    /// Deterministic + fail-open: an empty/irrelevant prompt, an empty skill
    /// index, or a missing workflow dir all yield empty results without error.
    pub fn prepare(&self, brief: &TurnBrief) -> TurnPrep {
        self.prepare_top_n(brief, DEFAULT_TOP_N)
    }

    /// Same as [`prepare`](Self::prepare) but with an explicit result cap
    /// applied to both skills and workflows.
    pub fn prepare_top_n(&self, brief: &TurnBrief, top_n: usize) -> TurnPrep {
        let workflows = workflows_for_brief(brief, top_n);
        TurnPrep {
            skills: self.rank_skills(&brief.prompt, top_n),
            sops: Vec::new(),
            workflows,
        }
    }

    /// Inspect the exact ordinary ranking with the default result cap.
    pub fn inspect(&self, brief: &TurnBrief) -> TurnPrepInspection {
        self.inspect_top_n(brief, DEFAULT_TOP_N)
    }

    /// Inspect the exact ordinary ranking with an explicit result cap.
    ///
    /// This is a projection of the same scored candidate lists used by
    /// [`prepare_top_n`](Self::prepare_top_n), not a second debug scorer. It
    /// reports only compact briefs, scores, and matched prompt terms.
    pub fn inspect_top_n(&self, brief: &TurnBrief, top_n: usize) -> TurnPrepInspection {
        let (selected_skills, skill_candidates) = self.rank_skills_explained(&brief.prompt, top_n);
        let workflow_inspection = workflows_inspection_for_brief(brief, top_n);

        let prep = TurnPrep {
            skills: selected_skills
                .iter()
                .map(|selected| selected.brief.clone())
                .collect(),
            sops: Vec::new(),
            workflows: workflow_inspection
                .selected
                .iter()
                .map(|selected| selected.brief.clone())
                .collect(),
        };

        TurnPrepInspection {
            skills_indexed: self.len(),
            skill_candidates,
            workflows_indexed: workflow_inspection.indexed,
            workflow_candidates: workflow_inspection.candidates,
            selected_skills,
            selected_workflows: workflow_inspection.selected,
            prep,
        }
    }

    /// Rank every skill against the prompt and return the best `top_n`.
    ///
    /// OCEAN-283 ranking — a better deterministic prefilter than raw keyword
    /// overlap, still no model in the loop:
    ///
    /// * **Term-set match, not raw count.** Prompt and skill text are reduced to
    ///   *sets* of distinct terms, so a skill is scored by *which* prompt terms
    ///   it covers, never by how many times one word repeats — a long
    ///   description can't win by sheer length.
    /// * **Name weighted over description.** A prompt term hitting the skill's
    ///   *name* (the strongest signal of what it's for) scores more than one
    ///   hitting only the description.
    /// * **Distinct-coverage bonus.** Matching several *different* prompt terms
    ///   beats matching one term, so a skill that's relevant on multiple axes
    ///   outranks an incidental single-word collision.
    /// * **Minimum-score floor.** A lone weak description-only hit falls below
    ///   [`MIN_RELEVANCE_SCORE`] and is dropped — default-on means every turn
    ///   consults, so we inject a brief only when it's *genuinely* relevant,
    ///   never noise.
    /// * **De-duplicated.** The same skill reachable from two roots (e.g. a repo
    ///   `./skills` copy shadowing a home one) collapses to a single brief, so
    ///   we never inject the same name twice or waste a slot.
    ///
    /// Skills below the floor are dropped — we never pad results to `top_n` with
    /// irrelevant skills. Ties break deterministically (score, then name, then
    /// path) so the same prompt always yields the same briefs.
    fn rank_skills(&self, prompt: &str, top_n: usize) -> Vec<SkillBrief> {
        if top_n == 0 || self.skills.is_empty() {
            return Vec::new();
        }

        let prompt_terms = tokenize(prompt);
        if prompt_terms.is_empty() {
            return Vec::new();
        }

        // Preserve the automatic hook's original allocation profile: score and
        // sort once, then stop de-duplicating as soon as `top_n` is filled. Only
        // explicit inspection materializes explanations and counts every candidate.
        let mut seen: std::collections::HashSet<(&str, &Path)> = std::collections::HashSet::new();
        self.scored_skills(&prompt_terms)
            .into_iter()
            .filter(|(_, skill)| seen.insert((skill.name.as_str(), skill.source_path.as_path())))
            .take(top_n)
            .map(|(_, skill)| skill.clone())
            .collect()
    }

    /// Inspect the authoritative scored skill order. Candidate count is measured
    /// after the ordinary de-duplication rule and before the result cap; matched
    /// term strings are allocated only for selected inspection entries.
    fn rank_skills_explained(
        &self,
        prompt: &str,
        top_n: usize,
    ) -> (Vec<ExplainedSkillMatch>, usize) {
        if self.skills.is_empty() {
            return (Vec::new(), 0);
        }

        let prompt_terms = tokenize(prompt);
        if prompt_terms.is_empty() {
            return (Vec::new(), 0);
        }

        let mut seen: std::collections::HashSet<(&str, &Path)> = std::collections::HashSet::new();
        let mut selected = Vec::new();
        let mut candidate_count = 0usize;
        for (score, skill) in self.scored_skills(&prompt_terms) {
            if !seen.insert((skill.name.as_str(), skill.source_path.as_path())) {
                continue;
            }
            candidate_count += 1;
            if selected.len() < top_n {
                let evidence = relevance_evidence(&prompt_terms, &skill.name, &skill.description);
                debug_assert_eq!(score, evidence.score);
                selected.push(ExplainedSkillMatch {
                    brief: skill.clone(),
                    score,
                    matched_prompt_terms: evidence.matched_prompt_terms,
                });
            }
        }
        (selected, candidate_count)
    }

    /// Score and sort relevant skills once. Both ordinary preparation and
    /// inspection project this same order, floor, and tie-break path.
    fn scored_skills<'a>(&'a self, prompt_terms: &[String]) -> Vec<(u32, &'a SkillBrief)> {
        let mut scored: Vec<_> = self
            .skills
            .iter()
            .filter_map(|skill| {
                let score = relevance_score(prompt_terms, &skill.name, &skill.description);
                (score >= MIN_RELEVANCE_SCORE).then_some((score, skill))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.cmp(&b.1.name))
                .then_with(|| a.1.source_path.cmp(&b.1.source_path))
        });
        scored
    }
}

/// Process-wide TTL cache of loaded [`SkillIndex`]es, keyed by [`SkillRoots`].
///
/// Now that the daemon's prepare-hook is **default-on** (OCEAN-283), every turn
/// consults — so we must NOT re-walk `~/.spawner` / `~/.codex` (+ repo `./skills`)
/// from disk each time. [`cached_index_for`] loads an index at most once per
/// [`CACHE_TTL`] window per root-set; subsequent consults in that window reuse
/// the in-memory `Arc<SkillIndex>`, making the steady-state cost of a consult a
/// couple of string scans over an already-loaded `Vec<SkillBrief>`.
///
/// Std-only (`OnceLock<Mutex<…>>`) and **poison-tolerant** (a panic while another
/// thread held the lock must not wedge every future turn — the same discipline
/// as the rest of this repo's long-lived shared state, OCEAN-287): we recover the
/// guard from a poisoned lock rather than propagate the panic.
struct CacheEntry {
    index: std::sync::Arc<SkillIndex>,
    loaded_at: Instant,
}

fn index_cache() -> &'static Mutex<HashMap<SkillRoots, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<SkillRoots, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache TTL, overridable via `OCEAN_LONGHOUSE_SKILL_TTL_SECS`. A value of
/// `0` disables caching (every call re-walks disk) — useful for a test that
/// needs a guaranteed-fresh scan, or an operator who edits skills constantly.
fn cache_ttl() -> Duration {
    match std::env::var("OCEAN_LONGHOUSE_SKILL_TTL_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => CACHE_TTL,
        },
        Err(_) => CACHE_TTL,
    }
}

/// Return a cached [`SkillIndex`] for `roots`, loading it from disk only if the
/// cache is cold or the cached copy is older than [`cache_ttl`]. This is the
/// entry point the daemon's default-on prepare-hook uses so a consult does not
/// re-walk the skill libraries every turn.
///
/// Fail-open like everything in this module: loading never errors (missing dirs
/// and malformed files are skipped), and a poisoned cache lock is recovered, so
/// a consult can always get *some* index back (an empty one at worst).
pub fn cached_index_for(roots: &SkillRoots) -> std::sync::Arc<SkillIndex> {
    let ttl = cache_ttl();

    // TTL == 0 → caching disabled: always load fresh, don't touch the map.
    if ttl.is_zero() {
        return std::sync::Arc::new(SkillIndex::load_from(roots));
    }

    let mut cache = index_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(entry) = cache.get(roots) {
        if entry.loaded_at.elapsed() < ttl {
            return std::sync::Arc::clone(&entry.index);
        }
    }

    // Cold or stale → load once, store, hand back.
    let index = std::sync::Arc::new(SkillIndex::load_from(roots));
    cache.insert(
        roots.clone(),
        CacheEntry {
            index: std::sync::Arc::clone(&index),
            loaded_at: Instant::now(),
        },
    );
    index
}

/// Convenience: cached index for the documented default roots (no repo `./skills`).
pub fn cached_index() -> std::sync::Arc<SkillIndex> {
    cached_index_for(&SkillRoots::default())
}

/// Drop all cached indexes. Test-only seam so a test that plants a skill on disk
/// can force the next [`cached_index_for`] to re-scan rather than serve a stale
/// entry left by an earlier test in the same process.
#[doc(hidden)]
pub fn clear_index_cache() {
    index_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

// ── Workflow index ────────────────────────────────────────────────────────────

/// Configurable root for the workflow index. The documented default is
/// `{cwd}/docs/orchestrator/workflows/`; the dir may be absent on disk — the
/// loader skips it silently (fail-open).
///
/// `Hash`/`Eq` so it can key the process-wide [`cached_workflows_for`] cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowRoots {
    /// The directory to scan for `*.md` workflow docs with YAML frontmatter.
    pub dir: Option<PathBuf>,
}

impl WorkflowRoots {
    /// Roots derived from a known cwd: `{cwd}/docs/orchestrator/workflows/`.
    pub fn for_cwd(cwd: impl AsRef<Path>) -> Self {
        Self {
            dir: Some(cwd.as_ref().join("docs/orchestrator/workflows")),
        }
    }
}

/// The loaded, cached workflow index. Built once via [`WorkflowIndex::load_from`],
/// then queried per-turn — the scan does NOT re-run on every prepare call.
#[derive(Debug, Clone, Default)]
pub struct WorkflowIndex {
    workflows: Vec<WorkflowBrief>,
}

impl WorkflowIndex {
    /// Load the index from explicit roots. Never errors: missing dirs and
    /// malformed files are skipped (the latter logged at `warn`/`debug`).
    pub fn load_from(roots: &WorkflowRoots) -> Self {
        let mut workflows = Vec::new();
        if let Some(dir) = &roots.dir {
            scan_workflow_dir(dir, &mut workflows);
        }
        tracing::debug!(count = workflows.len(), "longhouse workflow index loaded");
        Self { workflows }
    }

    /// The number of workflow briefs currently indexed.
    pub fn len(&self) -> usize {
        self.workflows.len()
    }

    /// True when no workflows are indexed.
    pub fn is_empty(&self) -> bool {
        self.workflows.is_empty()
    }

    /// Read-only view of the loaded briefs.
    pub fn workflows(&self) -> &[WorkflowBrief] {
        &self.workflows
    }

    /// Rank workflows against a prompt and return the top `top_n`. Uses the
    /// same term-set scoring as skills: name hits weighted higher than
    /// description hits, minimum-score floor, deterministic tie-break.
    pub fn rank(&self, prompt: &str, top_n: usize) -> Vec<WorkflowBrief> {
        if top_n == 0 || self.workflows.is_empty() {
            return Vec::new();
        }
        let prompt_terms = tokenize(prompt);
        if prompt_terms.is_empty() {
            return Vec::new();
        }

        let mut seen: std::collections::HashSet<(&str, &Path)> = std::collections::HashSet::new();
        self.scored_workflows(&prompt_terms)
            .into_iter()
            .filter(|(_, workflow)| {
                seen.insert((workflow.name.as_str(), workflow.source_path.as_path()))
            })
            .take(top_n)
            .map(|(_, workflow)| workflow.clone())
            .collect()
    }

    fn rank_explained(&self, prompt: &str, top_n: usize) -> (Vec<ExplainedWorkflowMatch>, usize) {
        if self.workflows.is_empty() {
            return (Vec::new(), 0);
        }
        let prompt_terms = tokenize(prompt);
        if prompt_terms.is_empty() {
            return (Vec::new(), 0);
        }

        let mut seen: std::collections::HashSet<(&str, &Path)> = std::collections::HashSet::new();
        let mut selected = Vec::new();
        let mut candidate_count = 0usize;
        for (score, workflow) in self.scored_workflows(&prompt_terms) {
            if !seen.insert((workflow.name.as_str(), workflow.source_path.as_path())) {
                continue;
            }
            candidate_count += 1;
            if selected.len() < top_n {
                let evidence =
                    relevance_evidence(&prompt_terms, &workflow.name, &workflow.description);
                debug_assert_eq!(score, evidence.score);
                selected.push(ExplainedWorkflowMatch {
                    brief: workflow.clone(),
                    score,
                    matched_prompt_terms: evidence.matched_prompt_terms,
                });
            }
        }
        (selected, candidate_count)
    }

    fn scored_workflows<'a>(&'a self, prompt_terms: &[String]) -> Vec<(u32, &'a WorkflowBrief)> {
        let mut scored: Vec<_> = self
            .workflows
            .iter()
            .filter_map(|workflow| {
                let score = relevance_score(prompt_terms, &workflow.name, &workflow.description);
                (score >= MIN_RELEVANCE_SCORE).then_some((score, workflow))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.cmp(&b.1.name))
                .then_with(|| a.1.source_path.cmp(&b.1.source_path))
        });
        scored
    }
}

/// Walk one dir for `*.md` workflow docs, parsing each file's YAML frontmatter.
///
/// Missing dir → no-op (fail-open). Unreadable/garbled file → skipped + logged.
fn scan_workflow_dir(dir: &Path, out: &mut Vec<WorkflowBrief>) {
    if !dir.is_dir() {
        tracing::debug!(
            dir = %dir.display(),
            "longhouse workflow dir absent, skipping"
        );
        return;
    }

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| match e {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::debug!(error = %err, "skipping unreadable workflow dir entry");
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_md = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }

        match parse_workflow_md(path) {
            Some(brief) => out.push(brief),
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "skipping workflow doc: no usable name in frontmatter"
                );
            }
        }
    }
}

/// Parse a workflow `.md` file's YAML frontmatter for `name:` + `description:`.
///
/// Reuses the existing [`extract_frontmatter`] + [`yaml_scalar`] helpers —
/// the same path as `parse_skill_md`, but without the `SKILL.md` filename gate
/// and with no heading fallback (a workflow doc without a frontmatter `name` is
/// simply skipped).
fn parse_workflow_md(path: &Path) -> Option<WorkflowBrief> {
    let text = std::fs::read_to_string(path).ok()?;
    let fm = extract_frontmatter(&text)?;
    let name = yaml_scalar(&fm, "name").filter(|s| !s.is_empty())?;
    let description = yaml_scalar(&fm, "description").unwrap_or_default();
    Some(WorkflowBrief {
        name,
        description,
        source_path: path.to_path_buf(),
    })
}

struct WorkflowCacheEntry {
    index: std::sync::Arc<WorkflowIndex>,
    loaded_at: Instant,
}

fn workflow_cache() -> &'static Mutex<HashMap<WorkflowRoots, WorkflowCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<WorkflowRoots, WorkflowCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return a cached [`WorkflowIndex`] for `roots`, loading from disk only when
/// the cache is cold or older than [`cache_ttl`]. Separate from the skill
/// cache (`index_cache`) so editing a workflow doc doesn't stale-invalidate
/// the skill index and vice versa. Same fail-open, poison-tolerant discipline.
pub fn cached_workflows_for(roots: &WorkflowRoots) -> std::sync::Arc<WorkflowIndex> {
    let ttl = cache_ttl();

    if ttl.is_zero() {
        return std::sync::Arc::new(WorkflowIndex::load_from(roots));
    }

    let mut cache = workflow_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(entry) = cache.get(roots) {
        if entry.loaded_at.elapsed() < ttl {
            return std::sync::Arc::clone(&entry.index);
        }
    }

    let index = std::sync::Arc::new(WorkflowIndex::load_from(roots));
    cache.insert(
        roots.clone(),
        WorkflowCacheEntry {
            index: std::sync::Arc::clone(&index),
            loaded_at: Instant::now(),
        },
    );
    index
}

/// Drop all cached workflow indexes. Test-only seam (mirrors [`clear_index_cache`]).
#[doc(hidden)]
pub fn clear_workflow_cache() {
    workflow_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Derive and rank workflow briefs from the cwd encoded in a [`TurnBrief`].
/// Returns empty when the brief has no cwd or the workflow dir is absent.
/// This is the single call-site that [`SkillIndex::prepare`] and
/// [`SkillIndex::prepare_top_n`] use to populate `TurnPrep::workflows`.
fn workflows_for_brief(brief: &TurnBrief, top_n: usize) -> Vec<WorkflowBrief> {
    let cwd = match brief.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(cwd) => cwd,
        None => return Vec::new(),
    };
    let roots = WorkflowRoots::for_cwd(cwd);
    cached_workflows_for(&roots).rank(&brief.prompt, top_n)
}

#[derive(Default)]
struct WorkflowInspection {
    indexed: usize,
    candidates: usize,
    selected: Vec<ExplainedWorkflowMatch>,
}

/// Load the same cwd-confined cached workflow index as ordinary preparation,
/// then derive the explicit inspection projection from its authoritative score.
fn workflows_inspection_for_brief(brief: &TurnBrief, top_n: usize) -> WorkflowInspection {
    let cwd = match brief.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(c) => c,
        None => return WorkflowInspection::default(),
    };
    let roots = WorkflowRoots::for_cwd(cwd);
    let index = cached_workflows_for(&roots);
    let indexed = index.len();
    let (selected, candidates) = index.rank_explained(&brief.prompt, top_n);
    WorkflowInspection {
        indexed,
        candidates,
        selected,
    }
}

// ── Skill scoring ─────────────────────────────────────────────────────────────

/// Minimum relevance score for a skill to be surfaced. A single description-only
/// term hit scores `DESC_HIT` (1) + the 1-distinct-term coverage bonus (1) = 2;
/// we require strictly more than that so a lone incidental word match is dropped
/// — only a name hit, or coverage of ≥2 distinct prompt terms, clears the floor.
/// This is the "don't inject noise into every turn" guard for default-on.
const MIN_RELEVANCE_SCORE: u32 = 3;

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

/// Lowercase alphanumeric tokens of length ≥ 3, deduped, with high-frequency
/// English/agent stop-words dropped. Short tokens (`a`, `to`, `ai`) and filler
/// (`the`, `with`, `please`, `help`) are removed so they can't drive spurious
/// overlaps now that every turn is consulted — the match stays on *content*
/// words. Cheap and order-stable (insertion order preserved).
fn tokenize(text: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
    {
        let lower = word.to_ascii_lowercase();
        if is_stop_word(&lower) {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

/// Common filler words that carry no skill-selection signal. Dropping them keeps
/// a generic prompt ("please help me with the thing") from colliding with a
/// skill description that happens to contain "help" or "with". Deliberately
/// small + conservative — only words that are almost never a skill's *topic*.
fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "for"
            | "you"
            | "your"
            | "with"
            | "this"
            | "that"
            | "from"
            | "into"
            | "can"
            | "will"
            | "would"
            | "should"
            | "could"
            | "have"
            | "has"
            | "are"
            | "was"
            | "were"
            | "but"
            | "not"
            | "use"
            | "using"
            | "used"
            | "via"
            | "per"
            | "out"
            | "get"
            | "got"
            | "let"
            | "lets"
            | "make"
            | "made"
            | "want"
            | "need"
            | "help"
            | "please"
            | "now"
            | "then"
            | "than"
            | "when"
            | "what"
            | "which"
            | "how"
            | "who"
            | "why"
            | "all"
            | "any"
            | "some"
            | "here"
            | "there"
            | "about"
            | "over"
            | "under"
            | "more"
    )
}

/// Relevance of a skill to the prompt — a better deterministic score than raw
/// keyword overlap (OCEAN-283). Term *sets*, not counts; name-weighted; with a
/// distinct-coverage bonus.
///
/// For each *distinct* prompt term: +[`NAME_HIT`] if it appears in the skill's
/// name (the strongest signal of what the skill is for), else +[`DESC_HIT`] if
/// it appears in the description. Then +1 per distinct prompt term matched
/// anywhere (the coverage bonus), so a skill relevant on several axes outranks
/// one with a single incidental collision. A skill that repeats a word in a long
/// description gains nothing extra — only *distinct* term coverage counts.
#[derive(Debug)]
struct RelevanceEvidence {
    score: u32,
    matched_prompt_terms: Vec<String>,
}

/// The single relevance algorithm used by ordinary preparation and inspection
/// for both skills and workflows. The callback lets explicit inspection retain
/// matched terms while the default-on automatic hook computes scores without
/// allocating per-candidate evidence.
fn analyze_relevance(
    prompt_terms: &[String],
    name: &str,
    description: &str,
    mut matched_term: impl FnMut(&str),
) -> u32 {
    let name = name.to_ascii_lowercase();
    let desc = description.to_ascii_lowercase();
    let mut weighted = 0u32;
    let mut distinct_hits = 0u32;
    for term in prompt_terms {
        if name.contains(term.as_str()) {
            weighted += NAME_HIT;
            distinct_hits += 1;
            matched_term(term);
        } else if desc.contains(term.as_str()) {
            weighted += DESC_HIT;
            distinct_hits += 1;
            matched_term(term);
        }
    }
    weighted + distinct_hits
}

fn relevance_score(prompt_terms: &[String], name: &str, description: &str) -> u32 {
    analyze_relevance(prompt_terms, name, description, |_| {})
}

fn relevance_evidence(prompt_terms: &[String], name: &str, description: &str) -> RelevanceEvidence {
    let mut matched_prompt_terms = Vec::new();
    let score = analyze_relevance(prompt_terms, name, description, |term| {
        matched_prompt_terms.push(term.to_string());
    });
    RelevanceEvidence {
        score,
        matched_prompt_terms,
    }
}

/// Score for a prompt term found in a skill's **name** — the strongest signal.
const NAME_HIT: u32 = 2;
/// Score for a prompt term found only in a skill's **description**.
const DESC_HIT: u32 = 1;

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
            ocean: None,
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
            ocean: Some(PathBuf::from("/nonexistent/ocean/skills/xyz")),
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
            ocean: None,
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
            ocean: None,
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
            ocean: None,
            spawner: None,
            codex: None,
            repo: roots.repo,
        });
        assert_eq!(index.len(), 2);
        assert!(index.skills().iter().all(|s| s.source == SkillSource::Repo));
    }

    #[test]
    fn ocean_root_accepts_either_format_and_scans_first() {
        let tmp = TempDir::new().unwrap();
        let ocean = tmp.path().join("ocean-skills");
        let spawner = tmp.path().join("spawner");
        write(
            &ocean,
            "deploys/skill.yaml",
            "name: Ocean Yaml\ndescription: ocean yaml pack\n",
        );
        write(
            &ocean,
            "captions/SKILL.md",
            "---\nname: \"Ocean Md\"\ndescription: \"ocean md pack\"\n---\n\n# Ocean Md\n",
        );
        write(
            &spawner,
            "video/skill.yaml",
            "name: Foreign\ndescription: foreign library skill\n",
        );
        let index = SkillIndex::load_from(&SkillRoots {
            ocean: Some(ocean),
            spawner: Some(spawner),
            codex: None,
            repo: None,
        });
        assert_eq!(index.len(), 3);
        // Ocean's native library leads the index; the foreign library follows.
        assert!(
            index.skills()[..2]
                .iter()
                .all(|s| s.source == SkillSource::Ocean),
            "ocean packs must scan first, got {:?}",
            index.skills().iter().map(|s| s.source).collect::<Vec<_>>()
        );
        assert_eq!(index.skills()[2].source, SkillSource::Spawner);
    }

    #[test]
    fn ocean_root_from_env_override_wins_and_falls_back_to_home() {
        // Explicit non-empty override wins verbatim.
        assert_eq!(
            ocean_root_from(Some("/custom/packs".into())),
            Some(PathBuf::from("/custom/packs"))
        );
        // Empty override falls through to the home default (suffix-checked so
        // the assertion holds under any $HOME; None only if HOME is unset).
        for path in [ocean_root_from(Some("".into())), ocean_root_from(None)]
            .into_iter()
            .flatten()
        {
            assert!(
                path.ends_with(".config/ocean-rs/skills"),
                "home fallback must land on the documented dir, got {}",
                path.display()
            );
        }
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
        // SOPs always empty (no on-disk source). Workflows come from cwd;
        // this brief has no cwd so they're empty here too.
        assert!(prep.sops.is_empty());
        assert!(prep.workflows.is_empty());
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
        assert!(empty
            .prepare(&TurnBrief::from_prompt("anything"))
            .is_empty());

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

    // ---- OCEAN-283: better deterministic ranking ----

    #[test]
    fn lone_description_word_hit_is_below_the_floor() {
        // A single description-only term match (weighted 1 + 1 coverage = 2) sits
        // below MIN_RELEVANCE_SCORE (3) and must NOT surface — default-on means we
        // only inject genuinely-relevant briefs, never a one-word incidental hit.
        let index = SkillIndex::from_briefs(vec![brief(
            "Database Migrations",
            "Author and run schema migrations against Postgres",
        )]);
        // "schema" hits the description once and nothing else → dropped.
        let prep = index.prepare(&TurnBrief::from_prompt("rename a schema thing"));
        assert!(
            prep.skills.is_empty(),
            "a lone description-only hit must fall below the floor, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_single_name_hit_clears_the_floor() {
        // A name-term hit (weighted 2 + 1 coverage = 3) clears the floor: the name
        // is the strongest "what this skill is for" signal, so one is enough.
        let index = SkillIndex::from_briefs(vec![brief(
            "Postgres",
            "everything about the database engine",
        )]);
        let prep = index.prepare(&TurnBrief::from_prompt("connect to postgres"));
        assert_eq!(prep.skills.len(), 1, "a name hit should surface the skill");
        assert_eq!(prep.skills[0].name, "Postgres");
    }

    #[test]
    fn broader_distinct_coverage_outranks_a_single_repeated_word() {
        // One skill matches a single prompt term, repeated all over its blurb; the
        // other matches two *distinct* prompt terms. Distinct coverage must win —
        // the ranker rewards breadth, not repetition.
        let index = SkillIndex::from_briefs(vec![
            brief(
                "Repeater",
                "video video video video video video clips and more video",
            ),
            brief("Coverer", "render a programmatic video composition"),
        ]);
        let prep = index.prepare(&TurnBrief::from_prompt("render a programmatic video"));
        assert_eq!(
            prep.skills[0].name,
            "Coverer",
            "two distinct matched terms beat one term repeated, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn duplicate_skill_across_roots_is_deduped() {
        // The SAME skill (name + source_path) reachable twice must collapse to one
        // brief, never occupy two of the capped slots.
        let dup = brief("Remotion Video", "Build programmatic videos in React");
        let index = SkillIndex::from_briefs(vec![dup.clone(), dup.clone()]);
        let prep = index.prepare(&TurnBrief::from_prompt("programmatic video in react"));
        assert_eq!(prep.skills.len(), 1, "duplicate skill must be deduped");
        assert_eq!(prep.skills[0].name, "Remotion Video");
    }

    #[test]
    fn stop_words_do_not_drive_spurious_matches() {
        // A generic, filler-heavy prompt that shares only stop-words ("help",
        // "with", "the", "please") with a skill description must not match.
        let index = SkillIndex::from_briefs(vec![brief(
            "Invoicing",
            "Use this to help you with the billing please",
        )]);
        let prep = index.prepare(&TurnBrief::from_prompt("please help me with the thing"));
        assert!(
            prep.skills.is_empty(),
            "stop-word-only overlap must not surface a skill, got {:?}",
            prep.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ---- OCEAN-283: process-wide TTL cache ----

    #[test]
    fn cached_index_serves_without_rewalking_disk() {
        // Warm the cache against an on-disk skill, then DELETE the skill dir. While
        // the cache is fresh, a second `cached_index_for` must still return the
        // skill — proving it served the cached copy and did NOT re-walk disk. Then
        // bust the cache (TTL=0) and confirm the re-walk now sees the deletion.
        let guard = ttl_env_guard();
        clear_index_cache();

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        write(
            repo,
            "skills/zorp/skill.yaml",
            "name: Zorptastic\ndescription: build a zorptastic widget\n",
        );
        let roots = SkillRoots {
            ocean: None,
            spawner: None,
            codex: None,
            repo: Some(repo.join("skills")),
        };

        // Default TTL (60s) → cache is live across calls.
        std::env::remove_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS");
        let first = cached_index_for(&roots);
        assert_eq!(first.len(), 1, "warm load sees the planted skill");

        // Remove the source from disk. A cache HIT must not notice.
        fs::remove_dir_all(repo.join("skills")).unwrap();
        let second = cached_index_for(&roots);
        assert_eq!(
            second.len(),
            1,
            "fresh cache must serve the skill WITHOUT re-walking disk (the dir is gone)"
        );
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "a cache hit returns the very same Arc, proving no reload happened"
        );

        // Now disable caching → the next call re-walks and sees the deletion.
        std::env::set_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS", "0");
        let third = cached_index_for(&roots);
        assert_eq!(
            third.len(),
            0,
            "with caching off, the re-walk sees the gone dir"
        );

        clear_index_cache();
        drop(guard);
    }

    #[test]
    fn ttl_zero_disables_cache_and_always_reloads() {
        let guard = ttl_env_guard();
        clear_index_cache();
        std::env::set_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS", "0");

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        write(
            repo,
            "skills/a/skill.yaml",
            "name: Alpha\ndescription: alpha skill\n",
        );
        let roots = SkillRoots {
            ocean: None,
            spawner: None,
            codex: None,
            repo: Some(repo.join("skills")),
        };

        let a = cached_index_for(&roots);
        let b = cached_index_for(&roots);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // Distinct Arcs: TTL=0 means each call loads a fresh index (no caching).
        assert!(
            !std::sync::Arc::ptr_eq(&a, &b),
            "TTL=0 must not cache: each call is a fresh load"
        );

        drop(guard);
    }

    /// Serialize the `OCEAN_LONGHOUSE_SKILL_TTL_SECS` env mutation + the shared
    /// process-wide cache across the cache tests (both are global).
    fn ttl_env_guard() -> std::sync::MutexGuard<'static, ()> {
        static TTL_ENV_LOCK: Mutex<()> = Mutex::new(());
        let g = TTL_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS");
        g
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

    // ---- OCEAN-338: WorkflowIndex loader ----

    /// Plant a workflow .md with YAML frontmatter under a tempdir's
    /// `docs/orchestrator/workflows/` and confirm WorkflowIndex parses it.
    #[test]
    fn workflow_index_loads_md_frontmatter() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "docs/orchestrator/workflows/my-flow.md",
            "---\nname: my-test-workflow\ndescription: does something useful in tests\n---\n\n# Body\n",
        );
        let roots = WorkflowRoots::for_cwd(tmp.path());
        let index = WorkflowIndex::load_from(&roots);
        assert_eq!(index.len(), 1, "one workflow should parse");
        assert_eq!(index.workflows()[0].name, "my-test-workflow");
        assert!(index.workflows()[0]
            .description
            .contains("does something useful"));
    }

    /// A missing workflow dir must yield an empty index, never an error.
    #[test]
    fn workflow_missing_dir_is_fail_open() {
        let roots = WorkflowRoots {
            dir: Some(PathBuf::from("/nonexistent/docs/orchestrator/workflows")),
        };
        let index = WorkflowIndex::load_from(&roots);
        assert!(
            index.is_empty(),
            "missing dir must yield empty, not an error"
        );
    }

    /// A workflow .md with no frontmatter (or a `name:` that's empty) is skipped.
    #[test]
    fn workflow_without_name_is_skipped() {
        let tmp = TempDir::new().unwrap();
        // No frontmatter at all.
        write(
            tmp.path(),
            "docs/orchestrator/workflows/no-front.md",
            "# Just a heading\nno frontmatter here\n",
        );
        // Has frontmatter but no `name:`.
        write(
            tmp.path(),
            "docs/orchestrator/workflows/no-name.md",
            "---\ndescription: has description but no name\n---\n",
        );
        let roots = WorkflowRoots::for_cwd(tmp.path());
        let index = WorkflowIndex::load_from(&roots);
        assert!(
            index.is_empty(),
            "workflow docs without a name must be skipped"
        );
    }

    /// `prepare()` with a cwd that contains a matching workflow doc populates
    /// `TurnPrep::workflows` with a brief for that workflow.
    #[test]
    fn prepare_populates_workflows_from_cwd() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "docs/orchestrator/workflows/factory-tick.md",
            "---\nname: ocean-os-factory-tick\ndescription: Ocean-native factory loop for keeping ocean-os moving\n---\n",
        );

        let skill_index = SkillIndex::default(); // no skills needed for this test
        let brief = TurnBrief {
            prompt: "run the factory tick workflow".to_string(),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        // Disable caching so the tempdir scan is always fresh.
        let guard = ttl_env_guard();
        clear_workflow_cache();
        std::env::set_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS", "0");

        let prep = skill_index.prepare(&brief);

        assert!(
            prep.workflows
                .iter()
                .any(|wf| wf.name == "ocean-os-factory-tick"),
            "planted workflow must surface in prep.workflows, got {:?}",
            prep.workflows.iter().map(|w| &w.name).collect::<Vec<_>>()
        );
        assert!(prep.sops.is_empty(), "SOPs must remain empty");

        clear_workflow_cache();
        drop(guard);
    }

    /// `prepare()` with no cwd returns empty workflows (fail-open, not an error).
    #[test]
    fn prepare_no_cwd_returns_empty_workflows() {
        let index = sample_index();
        let prep = index.prepare(&TurnBrief::from_prompt("run the factory workflow"));
        assert!(
            prep.workflows.is_empty(),
            "no cwd → no workflow scan → empty workflows"
        );
    }

    #[test]
    fn inspection_has_exact_prepare_parity_and_deterministic_explanations() {
        let guard = ttl_env_guard();
        clear_workflow_cache();
        std::env::set_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS", "0");

        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "docs/orchestrator/workflows/zeta.md",
            "---\nname: zorpquok-zeta\ndescription: alpha workflow\n---\n",
        );
        write(
            tmp.path(),
            "docs/orchestrator/workflows/alpha.md",
            "---\nname: zorpquok-alpha\ndescription: zeta workflow\n---\n",
        );
        let index = SkillIndex::from_briefs(vec![
            brief("Zorpquok Zeta", "alpha skill"),
            brief("Zorpquok Alpha", "zeta skill"),
            brief("Unrelated", "does not match"),
        ]);
        let turn = TurnBrief {
            prompt: "zeta zorpquok alpha zeta".to_string(),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        };

        let inspection = index.inspect_top_n(&turn, 1);
        assert_eq!(inspection.prep, index.prepare_top_n(&turn, 1));
        assert_eq!(inspection.skills_indexed, 3);
        assert_eq!(inspection.skill_candidates, 2);
        assert_eq!(inspection.workflows_indexed, 2);
        assert_eq!(inspection.workflow_candidates, 2);
        assert_eq!(inspection.selected_skills.len(), 1);
        assert_eq!(inspection.selected_workflows.len(), 1);
        assert_eq!(inspection.selected_skills[0].brief.name, "Zorpquok Alpha");
        assert_eq!(
            inspection.selected_workflows[0].brief.name,
            "zorpquok-alpha"
        );
        assert_eq!(
            inspection.selected_skills[0].matched_prompt_terms,
            ["zeta", "zorpquok", "alpha"]
        );
        assert_eq!(inspection.selected_skills[0].score, 8);
        assert_eq!(inspection, index.inspect_top_n(&turn, 1));
        assert_eq!(
            serde_json::to_value(&inspection.prep).unwrap(),
            serde_json::to_value(index.prepare_top_n(&turn, 1)).unwrap(),
            "inspection prep must remain serde-equivalent to ordinary prepare"
        );

        clear_workflow_cache();
        drop(guard);
    }

    #[test]
    fn inspection_zero_cap_reports_candidates_but_selects_empty_prep() {
        let index = sample_index();
        let turn = TurnBrief::from_prompt("postgres remotion slack video schema messaging");
        let inspection = index.inspect_top_n(&turn, 0);
        assert!(inspection.prep.is_empty());
        assert!(inspection.selected_skills.is_empty());
        assert!(inspection.skill_candidates > 0);
        assert_eq!(inspection.prep, index.prepare_top_n(&turn, 0));
    }

    /// The workflow cache serves stale data on a cache hit and re-scans on TTL=0.
    #[test]
    fn workflow_cache_hit_avoids_disk_rescan() {
        let guard = ttl_env_guard();
        clear_workflow_cache();
        std::env::remove_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS");

        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "docs/orchestrator/workflows/wf.md",
            "---\nname: cache-test-flow\ndescription: workflow for cache test\n---\n",
        );
        let roots = WorkflowRoots::for_cwd(tmp.path());

        let first = cached_workflows_for(&roots);
        assert_eq!(first.len(), 1, "warm load sees the planted workflow");

        // Remove the source. A cache HIT must still serve it.
        fs::remove_dir_all(tmp.path().join("docs")).unwrap();
        let second = cached_workflows_for(&roots);
        assert_eq!(second.len(), 1, "cache hit must serve without re-scanning");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "cache hit returns the same Arc"
        );

        // TTL=0 → re-scan sees the deletion.
        std::env::set_var("OCEAN_LONGHOUSE_SKILL_TTL_SECS", "0");
        let third = cached_workflows_for(&roots);
        assert_eq!(third.len(), 0, "TTL=0 re-scan sees the gone dir");

        clear_workflow_cache();
        drop(guard);
    }
}
