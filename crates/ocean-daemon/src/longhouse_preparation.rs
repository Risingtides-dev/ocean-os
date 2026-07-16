//! State-free Longhouse prepare, inspect, and workflow HTTP adapters.
//!
//! Route composition, turn-time prompt preparation, librarian/spec compatibility,
//! and Longhouse governance remain in `main.rs`; ranking and cache authority remain
//! in `ocean-longhouse`.

use super::longhouse_turn_preparation::longhouse_prepare_enabled;
use axum::Json;
use serde_json::json;

/// Request body for `POST /v1/longhouse/prepare`.
///
/// Mirrors the [`ocean_longhouse::TurnBrief`] the prep loop ranks against — the
/// daemon's own turn shape minus the heavy bits. Only `prompt` is required; the
/// rest scope the skill index (`cwd`) or are reserved for future SOP/workflow
/// selection. `top_n` overrides how many compact skill briefs come back.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghousePrepareRequest {
    /// The upcoming turn's prompt — the text Longhouse ranks skills against.
    pub(super) prompt: String,
    /// Opaque daemon session id this turn belongs to (carried through to the
    /// brief; unused in v1 ranking).
    #[serde(default)]
    pub(super) session_id: Option<String>,
    /// Working directory of the turn. When set, the skill index also scans the
    /// repo-local `./skills` dir under it (`SkillRoots::for_cwd`), on top of the
    /// documented home libraries.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// Which client is steering ("tui", "surface", "voice"). Reserved for future
    /// client-aware SOP reminders; unused in v1 ranking.
    #[serde(default)]
    pub(super) client_type: Option<String>,
    /// Cap on how many compact skill briefs to return. Defaults to
    /// [`ocean_longhouse::DEFAULT_TOP_N`] when omitted.
    #[serde(default)]
    pub(super) top_n: Option<usize>,
}

/// Path-redacted wire projection for explicit ranking inspection. Internal
/// preparation retains source paths for later body fetches; this diagnostic does
/// not disclose the caller's cwd or home-library layout.
#[derive(serde::Serialize)]
struct LonghouseInspectSkillBrief {
    name: String,
    description: String,
    source: ocean_longhouse::SkillSource,
}

impl From<ocean_longhouse::SkillBrief> for LonghouseInspectSkillBrief {
    fn from(brief: ocean_longhouse::SkillBrief) -> Self {
        Self {
            name: brief.name,
            description: brief.description,
            source: brief.source,
        }
    }
}

#[derive(serde::Serialize)]
struct LonghouseInspectWorkflowBrief {
    name: String,
    description: String,
}

impl From<ocean_longhouse::WorkflowBrief> for LonghouseInspectWorkflowBrief {
    fn from(brief: ocean_longhouse::WorkflowBrief) -> Self {
        Self {
            name: brief.name,
            description: brief.description,
        }
    }
}

#[derive(serde::Serialize)]
struct LonghouseInspectSopBrief {
    name: String,
    description: String,
}

impl From<ocean_longhouse::SopBrief> for LonghouseInspectSopBrief {
    fn from(brief: ocean_longhouse::SopBrief) -> Self {
        Self {
            name: brief.name,
            description: brief.description,
        }
    }
}

#[derive(serde::Serialize)]
struct LonghouseInspectSkillMatch {
    brief: LonghouseInspectSkillBrief,
    score: u32,
    matched_prompt_terms: Vec<String>,
    exact_name_phrase: bool,
}

impl From<ocean_longhouse::ExplainedSkillMatch> for LonghouseInspectSkillMatch {
    fn from(selected: ocean_longhouse::ExplainedSkillMatch) -> Self {
        Self {
            brief: selected.brief.into(),
            score: selected.score,
            matched_prompt_terms: selected.matched_prompt_terms,
            exact_name_phrase: selected.exact_name_phrase,
        }
    }
}

#[derive(serde::Serialize)]
struct LonghouseInspectWorkflowMatch {
    brief: LonghouseInspectWorkflowBrief,
    score: u32,
    matched_prompt_terms: Vec<String>,
    exact_name_phrase: bool,
}

impl From<ocean_longhouse::ExplainedWorkflowMatch> for LonghouseInspectWorkflowMatch {
    fn from(selected: ocean_longhouse::ExplainedWorkflowMatch) -> Self {
        Self {
            brief: selected.brief.into(),
            score: selected.score,
            matched_prompt_terms: selected.matched_prompt_terms,
            exact_name_phrase: selected.exact_name_phrase,
        }
    }
}

#[derive(serde::Serialize)]
struct LonghouseInspectPrep {
    skills: Vec<LonghouseInspectSkillBrief>,
    sops: Vec<LonghouseInspectSopBrief>,
    workflows: Vec<LonghouseInspectWorkflowBrief>,
}

impl From<ocean_longhouse::TurnPrep> for LonghouseInspectPrep {
    fn from(prep: ocean_longhouse::TurnPrep) -> Self {
        Self {
            skills: prep.skills.into_iter().map(Into::into).collect(),
            sops: prep.sops.into_iter().map(Into::into).collect(),
            workflows: prep.workflows.into_iter().map(Into::into).collect(),
        }
    }
}

/// `POST /v1/longhouse/prepare` — the **read-only pre-turn preparation step**,
/// the "first safe integration slice" from `docs/LONGHOUSE.md` §"First safe
/// integration slice" (lines 101-115). This is the first real consumer of
/// [`ocean_longhouse::SkillIndex::prepare`] (OCEAN-226): the library capability
/// shipped in OCEAN-215 ph1 had no caller until now.
///
/// The daemon hands Longhouse a compact [`ocean_longhouse::TurnBrief`] (the
/// prompt + a little session context) and gets back a [`ocean_longhouse::TurnPrep`]:
/// the handful of skills (plus, in later phases, SOPs/workflows) most relevant to
/// that prompt, each as a *compact* brief — name + one-line when-to-use, never a
/// full body. A client may call this before submitting a turn and fold the briefs
/// into its own guidance.
///
/// **Advisory only — Longhouse recommends, it never acts (per the repo's Longhouse
/// rule + `docs/LONGHOUSE.md` line 115).** This endpoint performs no local side
/// effects, executes nothing, and touches no permission gate: it loads the skill
/// index off disk and ranks it. The returned `advisory: true` makes that contract
/// explicit on the wire. The main agent still routes every real action back
/// through the daemon's permission gates.
///
/// **Fail-open** (matches `prepare` itself): a missing/garbled skill library, an
/// empty index, or an irrelevant prompt yields `ok: true` with an empty `prep` —
/// it never errors, so consulting Longhouse can never block a would-be turn.
///
/// The disk scan runs on a blocking thread (`spawn_blocking`) so the index walk
/// never stalls the async scheduler; the cheap keyword ranking then runs inline.
pub(super) async fn longhouse_prepare(
    Json(req): Json<LonghousePrepareRequest>,
) -> Json<serde_json::Value> {
    let brief = ocean_longhouse::TurnBrief {
        session_id: req.session_id.unwrap_or_default(),
        prompt: req.prompt,
        cwd: req.cwd.clone(),
        client_type: req.client_type,
    };
    let top_n = req.top_n;

    // Rank against the CACHED skill index on a blocking thread: a cold/stale load
    // walks ~/.spawner/skills, ~/.codex/skills (+ repo-local ./skills when a cwd
    // is given), which is filesystem I/O we must not run on the async scheduler;
    // a warm cache hit just ranks an already-loaded index (OCEAN-283). Both load
    // and rank are fail-open, so a JoinError (the only way this can fail)
    // collapses to an empty prep — never a 500 — preserving the contract that
    // consulting Longhouse can't block a turn.
    let prep = tokio::task::spawn_blocking(move || {
        let roots = match brief.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => ocean_longhouse::SkillRoots::for_cwd(cwd),
            _ => ocean_longhouse::SkillRoots::default(),
        };
        let index = ocean_longhouse::cached_index_for(&roots);
        let skills_indexed = index.len();
        let prep = match top_n {
            Some(n) => index.prepare_top_n(&brief, n),
            None => index.prepare(&brief),
        };
        (prep, skills_indexed)
    })
    .await;

    let (prep, skills_indexed) = prep.unwrap_or_else(|err| {
        // spawn_blocking only errors if the closure panicked; the loader and
        // ranker don't panic, but stay fail-open here regardless.
        tracing::warn!(error = %err, "longhouse prepare task failed; returning empty prep");
        (ocean_longhouse::TurnPrep::default(), 0)
    });

    Json(json!({
        "ok": true,
        // Advisory contract: Longhouse only recommends. This endpoint executes
        // nothing and bypasses no permission gate.
        "advisory": true,
        // How many skills the index held this call (diagnostic: distinguishes
        // "no library on disk" from "library present, nothing matched").
        "skills_indexed": skills_indexed,
        "prep": prep,
    }))
}

/// `POST /v1/longhouse/inspect` — explain the exact deterministic selection that
/// ordinary preparation would return. This is advisory and read-only: it does
/// not run a turn, grant capabilities, invoke a model, or change prompt injection.
/// It returns only contributing prompt terms and path-redacted compact metadata,
/// never the raw submitted prompt, session id, cwd, or any skill/workflow body.
pub(super) async fn longhouse_inspect(
    Json(req): Json<LonghousePrepareRequest>,
) -> Json<serde_json::Value> {
    let brief = ocean_longhouse::TurnBrief {
        session_id: req.session_id.unwrap_or_default(),
        prompt: req.prompt,
        cwd: req.cwd.clone(),
        client_type: req.client_type,
    };
    let top_n = req.top_n;

    // Identical cwd-confined roots and process-wide cache as `/prepare`. Only
    // `cwd/skills` and `cwd/docs/orchestrator/workflows` are considered locally;
    // source paths remain internal and are redacted from the response below.
    let inspection = tokio::task::spawn_blocking(move || {
        let roots = match brief.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => ocean_longhouse::SkillRoots::for_cwd(cwd),
            _ => ocean_longhouse::SkillRoots::default(),
        };
        let index = ocean_longhouse::cached_index_for(&roots);
        match top_n {
            Some(n) => index.inspect_top_n(&brief, n),
            None => index.inspect(&brief),
        }
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(error = %err, "longhouse inspect task failed; returning empty inspection");
        ocean_longhouse::TurnPrepInspection::default()
    });

    Json(json!({
        "ok": true,
        "advisory": true,
        "consult_enabled": longhouse_prepare_enabled(),
        "skills_indexed": inspection.skills_indexed,
        "skill_candidates": inspection.skill_candidates,
        "workflows_indexed": inspection.workflows_indexed,
        "workflow_candidates": inspection.workflow_candidates,
        "selected_skills": inspection
            .selected_skills
            .into_iter()
            .map(LonghouseInspectSkillMatch::from)
            .collect::<Vec<_>>(),
        "selected_workflows": inspection
            .selected_workflows
            .into_iter()
            .map(LonghouseInspectWorkflowMatch::from)
            .collect::<Vec<_>>(),
        "prep": LonghouseInspectPrep::from(inspection.prep),
    }))
}

// --- Workflow-brief endpoint: POST /v1/workflows/prepare (OCEAN-340) ----------
//
// Surfaces the OCEAN-338 WorkflowBrief loader's `workflows` field from
// `TurnPrep` over HTTP as a thin, advisory, read-only, fail-open shell.
// Mirrors `longhouse_prepare` exactly: same `spawn_blocking` pattern, same
// fail-open JoinError collapse, same `advisory: true` wire contract.  Returns
// `{ ok: true, advisory: true, workflows: [...WorkflowBrief] }`.  Until a
// `docs/orchestrator/workflows/` dir exists in the cwd the array is empty —
// that is the expected, correct behaviour.

/// `POST /v1/workflows/prepare` — the **read-only workflow-brief step** from
/// the Longhouse discovery wave (OCEAN-340).  Runs the same prepare path as
/// `longhouse_prepare` but surfaces only the `workflows` field populated by the
/// OCEAN-338 WorkflowBrief loader.
///
/// **Advisory only** — Longhouse recommends, it never acts.  This endpoint
/// performs no local side effects, executes nothing, and touches no permission
/// gate.  The `advisory: true` field makes that contract explicit on the wire.
///
/// **Fail-open**: a missing `docs/orchestrator/workflows/` dir, a garbled
/// loader, or a JoinError all collapse to `workflows: []` — never a 5xx —
/// so consulting this endpoint can never block a would-be turn.
///
/// The disk scan runs on a blocking thread (`spawn_blocking`) so the workflow
/// dir walk never stalls the async scheduler, matching `longhouse_prepare`.
pub(super) async fn workflows_prepare(
    Json(req): Json<LonghousePrepareRequest>,
) -> Json<serde_json::Value> {
    let brief = ocean_longhouse::TurnBrief {
        session_id: req.session_id.unwrap_or_default(),
        prompt: req.prompt,
        cwd: req.cwd.clone(),
        client_type: req.client_type,
    };
    let top_n = req.top_n;

    // Scan the workflow dir on a blocking thread — same rationale as
    // `longhouse_prepare`: filesystem I/O must not run on the async scheduler.
    // A missing dir returns an empty index (fail-open), and a JoinError (the
    // only way this can fail) collapses to workflows:[] — never a 500.
    let workflows = tokio::task::spawn_blocking(move || {
        let roots = match brief.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => ocean_longhouse::SkillRoots::for_cwd(cwd),
            _ => ocean_longhouse::SkillRoots::default(),
        };
        let index = ocean_longhouse::cached_index_for(&roots);
        let prep = match top_n {
            Some(n) => index.prepare_top_n(&brief, n),
            None => index.prepare(&brief),
        };
        prep.workflows
    })
    .await;

    let workflows = workflows.unwrap_or_else(|err| {
        // spawn_blocking only errors if the closure panicked; the loader
        // doesn't panic, but stay fail-open here regardless.
        tracing::warn!(error = %err, "workflows prepare task failed; returning empty list");
        Vec::new()
    });

    Json(json!({
        "ok": true,
        // Advisory contract: Longhouse only recommends. This endpoint executes
        // nothing and bypasses no permission gate.
        "advisory": true,
        "workflows": workflows,
    }))
}
