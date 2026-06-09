//! `SubagentSpec` + assembler — the read-only **subagent-spec** step for the
//! Longhouse "Subagent future" (`docs/LONGHOUSE.md` §"Subagent future", lines
//! 138-154).
//!
//! Longhouse should be able to take a desired role/intent ("a security reviewer
//! for this PR", "a Postgres schema specialist") and **assemble a subagent
//! spec** from the skill library + sensible defaults: which skills the subagent
//! should carry, what tools it's allowed, where its memory lives, what shape its
//! output takes, and its turn/budget ceilings. The spec is *advisory*: it
//! RETURNS a description of a subagent; it never spawns one and never bypasses a
//! daemon permission gate (local side effects still route through the daemon —
//! `docs/LONGHOUSE.md` line 154).
//!
//! ## What this builds on (OCEAN-282, builds on OCEAN-281)
//!
//! The skill-id half is **not reimplemented**: skill selection reuses the same
//! [`SkillIndex`](crate::SkillIndex) the prep loop (OCEAN-226) and the skill
//! librarian (OCEAN-281) use. We rank skills against the role/intent with the
//! library's own deterministic keyword overlap (`SkillIndex::prepare_top_n`),
//! and the resulting [`SubagentSpec::skill_ids`] are exactly the `source_path`
//! ids the OCEAN-281 `POST /v1/skills/fetch` endpoint can resolve to full
//! bodies. So a spec → fetch each skill body → assemble the subagent prompt is a
//! coherent downstream flow.
//!
//! ## What the spec carries
//!
//! Every field named in `docs/LONGHOUSE.md` §"Subagent future":
//!
//! | field | source |
//! |---|---|
//! | `role` | the request's role/intent, normalized |
//! | `objective` | the request's objective (falls back to the role) |
//! | `model_policy` | request override, else a default keyed off the role |
//! | `skill_ids` | top-N skills ranked from the [`SkillIndex`] |
//! | `allowed_tools` | derived from the role + matched skills (a conservative,
//!   read-leaning default set; never a blanket allow) |
//! | `memory_namespace` | a stable slug derived from the role |
//! | `output_schema` | request override, else `"text"` |
//! | `max_turns` | request override, else a per-policy default |
//! | `budget` | request override, else a per-policy default |
//!
//! ## Fail-open + deterministic
//!
//! Like `prepare`, this is **deterministic** (same request → same spec) and
//! **fail-open**: an empty/garbled role yields a *minimal valid spec* (a
//! generic assistant role, no skills, the conservative default tool set) rather
//! than an error, so a missing skill library or a blank intent can never produce
//! an unusable spec. The disk scan is the caller's concern (run it on
//! `spawn_blocking`); the assembler itself takes an already-loaded `SkillIndex`.

use serde::{Deserialize, Serialize};

use crate::prepare::{SkillIndex, TurnBrief};

/// How many skills an assembled spec carries by default. Matches the librarian's
/// "3–7 compact briefs" guidance (`docs/LONGHOUSE.md` line 136) at its low end —
/// a subagent wants a *focused* skill set, not the whole library, so we lean
/// smaller than the prep loop's [`crate::DEFAULT_TOP_N`].
pub const DEFAULT_SKILL_COUNT: usize = 3;

/// Default per-spec turn ceiling when the request doesn't pin one. A subagent is
/// a *bounded* worker; this keeps an assembled spec provably finite by default
/// (the same "must terminate" discipline the quorum engine enforces).
pub const DEFAULT_MAX_TURNS: u32 = 12;

/// Default token budget when the request doesn't pin one. Conservative ceiling
/// for a single focused subagent run; callers raise it explicitly when needed.
pub const DEFAULT_BUDGET_TOKENS: u64 = 100_000;

/// The model tier a subagent should run on. Longhouse already runs its council
/// workers on **cheap** models (`agent.rs` — deepseek/kimi); a subagent spec
/// expresses the *policy* (which tier) rather than hard-coding a provider id, so
/// the daemon's provider registry resolves the concrete model at spawn time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicy {
    /// Cheapest fast model — the default for routine/bounded subagent work, and
    /// the same posture the Longhouse council takes for its workers.
    Cheap,
    /// A mid/standard model — for roles whose intent reads as needing more
    /// reasoning headroom (review, audit, architecture, planning).
    Standard,
    /// The strongest available model — only when the request explicitly asks for
    /// it; the assembler never auto-escalates to this tier.
    Frontier,
}

impl Default for ModelPolicy {
    /// Cheap by default — bounded subagents should not reach for a frontier
    /// model unasked (cost + the Longhouse "cheap worker" posture).
    fn default() -> Self {
        ModelPolicy::Cheap
    }
}

impl ModelPolicy {
    /// Parse a free-text policy hint from the request (`"cheap"`, `"standard"`,
    /// `"frontier"`, plus a few friendly synonyms). Unknown text → `None` so the
    /// caller can fall back to the role-derived default rather than guessing.
    fn parse(hint: &str) -> Option<Self> {
        match hint.trim().to_ascii_lowercase().as_str() {
            "cheap" | "fast" | "small" | "mini" => Some(ModelPolicy::Cheap),
            "standard" | "mid" | "medium" | "balanced" | "default" => Some(ModelPolicy::Standard),
            "frontier" | "strong" | "large" | "max" | "best" => Some(ModelPolicy::Frontier),
            _ => None,
        }
    }

    /// The per-policy default turn ceiling. Heavier policies get a little more
    /// room because they're chosen for harder, multi-step work.
    fn default_max_turns(self) -> u32 {
        match self {
            ModelPolicy::Cheap => DEFAULT_MAX_TURNS,
            ModelPolicy::Standard => DEFAULT_MAX_TURNS + 6,
            ModelPolicy::Frontier => DEFAULT_MAX_TURNS + 12,
        }
    }

    /// The per-policy default token budget, scaled the same way as turns.
    fn default_budget(self) -> u64 {
        match self {
            ModelPolicy::Cheap => DEFAULT_BUDGET_TOKENS,
            ModelPolicy::Standard => DEFAULT_BUDGET_TOKENS * 2,
            ModelPolicy::Frontier => DEFAULT_BUDGET_TOKENS * 4,
        }
    }
}

/// The request describing a subagent to assemble a spec for. Only `role` carries
/// real weight; everything else is an optional override of an assembler default,
/// so the minimal request is `{ "role": "..." }`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// The desired role / intent — the text the assembler ranks skills against
    /// and derives the namespace + tool set from. An empty role yields the
    /// minimal generic spec (fail-open), not an error.
    #[serde(default)]
    pub role: String,
    /// What the subagent is for, if distinct from the role. Carried onto the
    /// spec verbatim; falls back to the role when omitted.
    #[serde(default)]
    pub objective: Option<String>,
    /// Optional model-policy override (`"cheap"` | `"standard"` | `"frontier"`,
    /// plus synonyms). Unrecognized/omitted → the role-derived default.
    #[serde(default)]
    pub model_policy: Option<String>,
    /// Optional working directory, so repo-local `./skills` are ranked alongside
    /// the home libraries (mirrors the skill librarian's `cwd`). Used by the
    /// caller to pick `SkillRoots`; the assembler itself just takes the index.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Cap on how many skill ids the spec carries. Defaults to
    /// [`DEFAULT_SKILL_COUNT`] when omitted.
    #[serde(default)]
    pub skill_count: Option<usize>,
    /// Optional output-schema hint carried onto the spec (e.g. `"json"`,
    /// `"markdown"`, a schema name). Defaults to `"text"`.
    #[serde(default)]
    pub output_schema: Option<String>,
    /// Optional explicit turn ceiling override. Defaults per model policy.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Optional explicit token-budget override. Defaults per model policy.
    #[serde(default)]
    pub budget: Option<u64>,
    /// Optional extra tools to allow on top of the role-derived set. Lets a
    /// caller widen the conservative default deliberately (never silently).
    #[serde(default)]
    pub extra_tools: Vec<String>,
}

impl SubagentRequest {
    /// Convenience constructor for the common "just a role" case.
    pub fn from_role(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            ..Default::default()
        }
    }
}

/// An assembled subagent spec — every field from `docs/LONGHOUSE.md` §"Subagent
/// future". Advisory: this DESCRIBES a subagent; it does not spawn one. Local
/// side effects a spawned subagent might take still route through daemon
/// permission gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSpec {
    /// The subagent's role, normalized from the request (whitespace-collapsed).
    /// Empty role → a generic `"assistant"` role (fail-open).
    pub role: String,
    /// What the subagent is for. The request's objective, else the role.
    pub objective: String,
    /// Which model tier to run on.
    pub model_policy: ModelPolicy,
    /// The skills this subagent carries — each id is a `source_path` the
    /// OCEAN-281 `POST /v1/skills/fetch` endpoint resolves to a full body.
    /// Ranked from the [`SkillIndex`] against the role; may be empty (fail-open
    /// when no library/match), never padded with irrelevant skills.
    pub skill_ids: Vec<String>,
    /// The tools the subagent is allowed to use — a conservative, role-derived
    /// default set (read-leaning), plus any `extra_tools` the request widened it
    /// with. Never a blanket allow; the daemon still gates each actual call.
    pub allowed_tools: Vec<String>,
    /// Where the subagent's scratch memory lives — a stable slug derived from the
    /// role (e.g. `subagent/security-reviewer`), so two specs for the same role
    /// share a namespace and distinct roles stay isolated.
    pub memory_namespace: String,
    /// The shape the subagent's output should take. Request override, else
    /// `"text"`.
    pub output_schema: String,
    /// Hard turn ceiling — keeps the subagent provably finite. Per-policy default
    /// unless the request pinned one.
    pub max_turns: u32,
    /// Token budget ceiling. Per-policy default unless the request pinned one.
    pub budget: u64,
}

/// Assemble a [`SubagentSpec`] from a [`SubagentRequest`] + an already-loaded
/// [`SkillIndex`].
///
/// **Deterministic + fail-open.** The same request against the same index always
/// yields the same spec. An empty/garbled role does NOT error — it yields a
/// minimal valid spec (generic role, no skills, the conservative default tool
/// set, default policy/turns/budget), so the assembler can never hand back an
/// unusable spec.
///
/// **Read-only.** Skill selection reuses [`SkillIndex::prepare_top_n`] (the same
/// ranking OCEAN-281's `query` uses); nothing here mutates the hive or spawns
/// anything. The caller is responsible for loading the index off disk on a
/// blocking thread — this function is pure given the index.
pub fn assemble_spec(req: &SubagentRequest, index: &SkillIndex) -> SubagentSpec {
    let role = normalize_role(&req.role);

    // Model policy: an explicit (recognized) override wins; otherwise infer a
    // sensible tier from the role text; otherwise the Cheap default.
    let model_policy = req
        .model_policy
        .as_deref()
        .and_then(ModelPolicy::parse)
        .unwrap_or_else(|| infer_policy(&role));

    // Skill ids: rank the library against the role/objective using the SAME
    // SkillIndex ranking the skill librarian uses — we do not reimplement
    // selection. The ranking text is the objective if present (more specific),
    // else the role. Returned ids are source_paths fetch can resolve.
    let ranking_text = req
        .objective
        .as_deref()
        .filter(|o| !o.trim().is_empty())
        .unwrap_or(&role);
    let skill_count = req.skill_count.unwrap_or(DEFAULT_SKILL_COUNT);
    let brief = TurnBrief {
        prompt: ranking_text.to_string(),
        cwd: req.cwd.clone(),
        ..Default::default()
    };
    let ranked = index.prepare_top_n(&brief, skill_count);
    let skill_ids: Vec<String> = ranked
        .skills
        .iter()
        .map(|s| s.source_path.to_string_lossy().into_owned())
        .collect();

    // Allowed tools: a conservative read-leaning baseline every subagent gets,
    // widened by capability keywords detected in the role + matched skill names
    // (e.g. a "review/audit" role gets nothing extra-destructive; a "build/edit"
    // role gains file-write/exec). Plus any extra_tools the request asked for.
    // Deduped + sorted for a deterministic, stable spec.
    let allowed_tools = derive_allowed_tools(&role, &ranked, &req.extra_tools);

    let memory_namespace = memory_namespace_for(&role);

    let output_schema = req
        .output_schema
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("text")
        .to_string();

    let max_turns = req
        .max_turns
        .unwrap_or_else(|| model_policy.default_max_turns());
    let budget = req.budget.unwrap_or_else(|| model_policy.default_budget());

    let objective = req
        .objective
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&role)
        .to_string();

    SubagentSpec {
        role,
        objective,
        model_policy,
        skill_ids,
        allowed_tools,
        memory_namespace,
        output_schema,
        max_turns,
        budget,
    }
}

/// Collapse whitespace + trim; an empty role becomes the generic `"assistant"`
/// (fail-open — the spec stays valid for a blank/garbled intent).
fn normalize_role(role: &str) -> String {
    let collapsed = role.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "assistant".to_string()
    } else {
        collapsed
    }
}

/// Infer a model tier from the role text. Roles that read as heavier reasoning
/// work (review, audit, security, architecture, planning, debugging) lean
/// `Standard`; everything else stays `Cheap`. We never auto-escalate to
/// `Frontier` — that requires an explicit request override.
fn infer_policy(role: &str) -> ModelPolicy {
    let r = role.to_ascii_lowercase();
    const HEAVY: &[&str] = &[
        "review",
        "audit",
        "security",
        "architect",
        "architecture",
        "design",
        "plan",
        "planning",
        "debug",
        "analyze",
        "analysis",
        "reason",
    ];
    if HEAVY.iter().any(|kw| r.contains(kw)) {
        ModelPolicy::Standard
    } else {
        ModelPolicy::Cheap
    }
}

/// A stable, filesystem-ish slug under the `subagent/` prefix, derived from the
/// role. Lowercase alphanumerics, runs of other chars collapse to a single `-`.
/// Same role → same namespace (shared scratch); distinct roles stay isolated.
fn memory_namespace_for(role: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // suppress leading dash
    for ch in role.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("assistant");
    }
    format!("subagent/{slug}")
}

/// The conservative tool set every subagent gets: read-only inspection + the
/// advisory skill-librarian endpoints (so a subagent can fetch the bodies of the
/// very skills its spec lists). Deliberately read-leaning — no file writes, no
/// shell, no destructive ops in the baseline.
const BASE_TOOLS: &[&str] = &["read_file", "list_dir", "skills_query", "skills_fetch"];

/// Capability keyword → the extra tools a role/skill that mentions it unlocks.
/// This is how a "build"/"edit" role gains write/exec while a "review" role does
/// not — derived, conservative, and explicit (the daemon still gates each call).
const CAPABILITY_TOOLS: &[(&str, &[&str])] = &[
    // editing / building / implementing → file writes + shell. These are ACTION
    // verbs: a role that says it will *build*/*write*/*edit* earns write tools.
    // Deliberately NOT a noun like "code" — "audit the code"/"review the code"
    // are read-only and must keep the read-leaning baseline.
    ("edit", &["edit_file", "write_file"]),
    ("write", &["edit_file", "write_file"]),
    ("build", &["edit_file", "write_file", "run_command"]),
    ("implement", &["edit_file", "write_file", "run_command"]),
    ("fix", &["edit_file", "write_file", "run_command"]),
    ("refactor", &["edit_file", "write_file"]),
    // running / testing → shell
    ("test", &["run_command"]),
    ("run", &["run_command"]),
    ("deploy", &["run_command"]),
    // search / research → web + grep
    ("search", &["web_search", "grep"]),
    ("research", &["web_search", "grep"]),
    ("find", &["grep"]),
    // version control
    ("git", &["run_command"]),
    ("commit", &["run_command"]),
    ("pr", &["run_command"]),
];

/// Build the allowed-tools list: the read-leaning [`BASE_TOOLS`] baseline, plus
/// any [`CAPABILITY_TOOLS`] unlocked by keywords in the role or a matched skill
/// name, plus the request's `extra_tools`. Deduped + sorted → deterministic.
///
/// Capability matching is on **word boundaries**, not raw substrings: a keyword
/// matches a haystack *token* only when the token equals it or begins with it
/// (so `"deploy"` matches `"deployment"` and `"build"` matches `"building"`).
/// This is what stops the classic false positive where `"audit"` would trip the
/// `"edit"` keyword and silently hand a *review* role file-write tools.
fn derive_allowed_tools(
    role: &str,
    ranked: &crate::prepare::TurnPrep,
    extra_tools: &[String],
) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut tools: BTreeSet<String> = BASE_TOOLS.iter().map(|t| t.to_string()).collect();

    // Tokens = the words of the role + the names of the skills the spec carries.
    // Skill names are a strong capability signal (e.g. "Supabase Postgres" → db
    // work). Splitting into words is what makes the keyword match word-bounded.
    let mut tokens: Vec<String> = role
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect();
    for s in &ranked.skills {
        for w in s.name.split(|c: char| !c.is_alphanumeric()) {
            if !w.is_empty() {
                tokens.push(w.to_ascii_lowercase());
            }
        }
    }

    for (keyword, unlocked) in CAPABILITY_TOOLS {
        // A keyword fires when some token *is* it, or — for keywords long enough
        // (≥4 chars) to make a prefix unambiguous — is an inflection of it (token
        // starts with the keyword: "build"→"building", "deploy"→"deployment").
        // Short keywords ("pr", "run", "fix", "git") require an EXACT token so a
        // 2–3-char prefix can't greedily match "prepare"/"runtime"/"fixture".
        // Either way the match is word-bounded, never a mid-word substring — which
        // is what stops "audit" from tripping "edit".
        let hit = tokens.iter().any(|tok| {
            tok == keyword || (keyword.len() >= 4 && tok.starts_with(keyword))
        });
        if hit {
            for tool in *unlocked {
                tools.insert((*tool).to_string());
            }
        }
    }

    // Explicit widen: caller-requested tools are always honored.
    for t in extra_tools {
        let t = t.trim();
        if !t.is_empty() {
            tools.insert(t.to_string());
        }
    }

    tools.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepare::{SkillBrief, SkillSource};
    use std::path::PathBuf;

    fn brief(name: &str, desc: &str) -> SkillBrief {
        SkillBrief {
            name: name.to_string(),
            description: desc.to_string(),
            source_path: PathBuf::from(format!("/skills/{name}/skill.yaml")),
            source: SkillSource::Repo,
        }
    }

    fn sample_index() -> SkillIndex {
        SkillIndex::from_briefs(vec![
            brief(
                "Security Hardening",
                "Audit code for vulnerabilities, secrets, and injection risks",
            ),
            brief(
                "Supabase Postgres",
                "Write and optimize Postgres queries, schema, and RLS policies",
            ),
            brief(
                "Remotion Video",
                "Build programmatic videos in React with Remotion compositions",
            ),
        ])
    }

    #[test]
    fn assembles_well_formed_spec_with_relevant_skill_ids() {
        let index = sample_index();
        let req = SubagentRequest::from_role("security reviewer auditing code for vulnerabilities");
        let spec = assemble_spec(&req, &index);

        assert_eq!(
            spec.role,
            "security reviewer auditing code for vulnerabilities"
        );
        // The most relevant skill must surface and its id is a fetchable path.
        assert!(
            spec.skill_ids
                .iter()
                .any(|id| id.contains("Security Hardening")),
            "security role should pull the Security Hardening skill, got {:?}",
            spec.skill_ids
        );
        assert!(
            spec.skill_ids.iter().all(|id| id.ends_with("skill.yaml")),
            "skill ids are fetchable source_paths"
        );
        // A review/security role infers the Standard tier (no explicit override).
        assert_eq!(spec.model_policy, ModelPolicy::Standard);
        // Sensible defaults for the rest.
        assert_eq!(spec.output_schema, "text");
        assert_eq!(spec.max_turns, ModelPolicy::Standard.default_max_turns());
        assert_eq!(spec.budget, ModelPolicy::Standard.default_budget());
        assert_eq!(
            spec.memory_namespace,
            "subagent/security-reviewer-auditing-code-for-vulnerabilities"
        );
        assert_eq!(spec.objective, spec.role, "objective falls back to role");
    }

    #[test]
    fn skill_count_caps_the_skill_ids() {
        let index = sample_index();
        // A query that hits multiple skills, capped to 1.
        let req = SubagentRequest {
            role: "build and audit a postgres-backed video app".to_string(),
            skill_count: Some(1),
            ..Default::default()
        };
        let spec = assemble_spec(&req, &index);
        assert_eq!(spec.skill_ids.len(), 1, "skill_count caps the ids");
    }

    #[test]
    fn review_role_gets_read_leaning_tools_no_write() {
        let index = sample_index();
        let spec = assemble_spec(&SubagentRequest::from_role("review this pull request"), &index);
        // Baseline read tools present.
        assert!(spec.allowed_tools.iter().any(|t| t == "read_file"));
        assert!(spec.allowed_tools.iter().any(|t| t == "skills_fetch"));
        // A pure review role must NOT gain destructive write/exec tools.
        assert!(
            !spec.allowed_tools.iter().any(|t| t == "write_file"),
            "review role must not unlock write_file, got {:?}",
            spec.allowed_tools
        );
    }

    #[test]
    fn audit_role_does_not_trip_the_edit_keyword() {
        // Regression: "audit" contains the substring "edit" — a raw substring
        // match would silently hand an *audit/review* role file-write tools. The
        // word-boundary match must NOT unlock edit_file/write_file here, even with
        // a matched skill named "...Audit" reinforcing the substring.
        let index = sample_index();
        let spec = assemble_spec(
            &SubagentRequest::from_role("audit the code for security issues"),
            &index,
        );
        assert!(
            !spec.allowed_tools.iter().any(|t| t == "edit_file"),
            "'audit' must not trip 'edit', got {:?}",
            spec.allowed_tools
        );
        assert!(
            !spec.allowed_tools.iter().any(|t| t == "write_file"),
            "'audit' must not unlock write_file, got {:?}",
            spec.allowed_tools
        );
    }

    #[test]
    fn deploy_inflection_unlocks_exec_but_short_keywords_need_exact_token() {
        let index = sample_index();
        // "deployment" (an inflection of "deploy", ≥4 chars) DOES unlock exec.
        let deploy = assemble_spec(
            &SubagentRequest::from_role("manage the production deployment"),
            &index,
        );
        assert!(
            deploy.allowed_tools.iter().any(|t| t == "run_command"),
            "'deployment' should unlock run_command via the deploy keyword"
        );
        // "prepare" must NOT match the short "pr" keyword (exact-token rule).
        let prep = assemble_spec(
            &SubagentRequest::from_role("prepare a summary of the project"),
            &index,
        );
        assert!(
            !prep.allowed_tools.iter().any(|t| t == "run_command"),
            "'prepare'/'project' must not trip the short 'pr' keyword, got {:?}",
            prep.allowed_tools
        );
    }

    #[test]
    fn build_role_unlocks_write_and_exec() {
        let index = sample_index();
        let spec = assemble_spec(
            &SubagentRequest::from_role("build and implement the feature"),
            &index,
        );
        assert!(
            spec.allowed_tools.iter().any(|t| t == "write_file"),
            "build role unlocks write_file"
        );
        assert!(
            spec.allowed_tools.iter().any(|t| t == "run_command"),
            "build role unlocks run_command"
        );
    }

    #[test]
    fn explicit_overrides_win() {
        let index = sample_index();
        let req = SubagentRequest {
            role: "anything".to_string(),
            objective: Some("a very specific objective".to_string()),
            model_policy: Some("frontier".to_string()),
            output_schema: Some("json".to_string()),
            max_turns: Some(3),
            budget: Some(42),
            extra_tools: vec!["custom_tool".to_string()],
            ..Default::default()
        };
        let spec = assemble_spec(&req, &index);
        assert_eq!(spec.model_policy, ModelPolicy::Frontier);
        assert_eq!(spec.objective, "a very specific objective");
        assert_eq!(spec.output_schema, "json");
        assert_eq!(
            spec.max_turns, 3,
            "explicit max_turns wins over policy default"
        );
        assert_eq!(spec.budget, 42, "explicit budget wins over policy default");
        assert!(spec.allowed_tools.iter().any(|t| t == "custom_tool"));
    }

    #[test]
    fn empty_role_yields_minimal_valid_spec_fail_open() {
        let index = sample_index();
        let spec = assemble_spec(&SubagentRequest::from_role("   "), &index);
        // Fail-open: a generic but VALID spec, not an error.
        assert_eq!(spec.role, "assistant");
        assert_eq!(spec.objective, "assistant");
        assert_eq!(spec.memory_namespace, "subagent/assistant");
        assert_eq!(spec.model_policy, ModelPolicy::Cheap);
        assert_eq!(spec.output_schema, "text");
        assert!(spec.skill_ids.is_empty(), "blank role matches no skills");
        // Still carries the conservative baseline tools.
        assert!(spec.allowed_tools.iter().any(|t| t == "read_file"));
        assert_eq!(spec.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(spec.budget, DEFAULT_BUDGET_TOKENS);
    }

    #[test]
    fn empty_index_yields_spec_with_no_skills() {
        let empty = SkillIndex::default();
        let spec = assemble_spec(&SubagentRequest::from_role("security reviewer"), &empty);
        assert!(
            spec.skill_ids.is_empty(),
            "no library → no skills, still a valid spec"
        );
        // The rest of the spec is still well-formed.
        assert_eq!(spec.role, "security reviewer");
        assert!(!spec.allowed_tools.is_empty());
    }

    #[test]
    fn assembly_is_deterministic() {
        let index = sample_index();
        let req = SubagentRequest::from_role("optimize the postgres schema and rls policies");
        let a = assemble_spec(&req, &index);
        let b = assemble_spec(&req, &index);
        assert_eq!(a, b, "same request + index → identical spec");
    }

    #[test]
    fn model_policy_parses_synonyms_and_ignores_garbage() {
        assert_eq!(ModelPolicy::parse("Cheap"), Some(ModelPolicy::Cheap));
        assert_eq!(ModelPolicy::parse("fast"), Some(ModelPolicy::Cheap));
        assert_eq!(ModelPolicy::parse("standard"), Some(ModelPolicy::Standard));
        assert_eq!(ModelPolicy::parse("frontier"), Some(ModelPolicy::Frontier));
        assert_eq!(ModelPolicy::parse("best"), Some(ModelPolicy::Frontier));
        assert_eq!(ModelPolicy::parse("gobbledygook"), None);
    }

    #[test]
    fn objective_drives_skill_ranking_when_present() {
        let index = sample_index();
        // Role is generic; the objective is what's specific. Ranking should use
        // the objective and surface the Postgres skill.
        let req = SubagentRequest {
            role: "helper".to_string(),
            objective: Some("optimize postgres schema and rls".to_string()),
            ..Default::default()
        };
        let spec = assemble_spec(&req, &index);
        assert!(
            spec.skill_ids
                .iter()
                .any(|id| id.contains("Supabase Postgres")),
            "objective should drive skill selection, got {:?}",
            spec.skill_ids
        );
    }
}
