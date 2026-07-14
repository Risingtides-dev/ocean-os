use super::{
    filesystem::{expand_tilde, try_canonicalize},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use ocean_core::{Project, ProjectConfig, ProjectId, ProjectResponse};
use serde_json::json;

#[derive(serde::Deserialize)]
pub(super) struct CreateProjectRequest {
    pub(super) name: String,
    pub(super) workspace_root: String,
    #[serde(default)]
    pub(super) config: ProjectConfig,
}

#[derive(serde::Deserialize)]
pub(super) struct PatchProjectRequest {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) config: Option<ProjectConfig>,
}

/// Pagination query for `GET /v1/projects` (OCEAN-250).
#[derive(Debug, serde::Deserialize, Default)]
pub(super) struct ProjectsListQuery {
    /// Max projects to return in this page. Omitted ⇒ the default cap
    /// (`DEFAULT_LIST_LIMIT`); any value is clamped to `MAX_LIST_LIMIT`.
    #[serde(default)]
    pub(super) limit: Option<usize>,
    /// Cursor: the `id` of the last project from the previous page. Omitted ⇒
    /// the first page. Replay `next_cursor` here for the following page.
    #[serde(default)]
    pub(super) cursor: Option<String>,
}

/// `GET /v1/projects?limit=&cursor=` — list registered projects, one bounded
/// page at a time (OCEAN-250). Projects are ordered newest-first; the `projects`
/// array shape is unchanged except for additive git fields (`git_branch`,
/// `git_dirty`, `worktrees`) computed at response time on each project's
/// `workspace_root`. Fields are additive; clients that don't know them ignore
/// them. Pagination fields `next_cursor`/`has_more` are unchanged.
pub(super) async fn projects_list(
    State(state): State<AppState>,
    Query(q): Query<ProjectsListQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .runtime
        .list_projects_page(q.cursor.as_deref(), q.limit)
    {
        Ok(page) => {
            let mut projects_json: Vec<serde_json::Value> = Vec::with_capacity(page.items.len());
            for p in &page.items {
                projects_json.push(enriched_project_json(p).await);
            }
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "projects": projects_json,
                    "next_cursor": page.next_cursor,
                    "has_more": page.has_more,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "ok": false,
                "projects": [],
                "error": e.to_string(),
                "next_cursor": null,
                "has_more": false,
            })),
        ),
    }
}

/// `POST /v1/projects` — create a project bound to a directory.
pub(super) async fn project_create(
    State(state): State<AppState>,
    Json(req): Json<CreateProjectRequest>,
) -> (StatusCode, Json<ProjectResponse>) {
    let now = Utc::now().timestamp_millis();

    // Expand ~ → $HOME, create_dir_all, canonicalize. An empty workspace_root
    // passes through unchanged (existing behavior — the project is created with
    // an empty string and the daemon treats it as project-less).
    let workspace_root = if req.workspace_root.is_empty() {
        req.workspace_root
    } else {
        let expanded = expand_tilde(&req.workspace_root);
        if let Err(e) = std::fs::create_dir_all(&expanded) {
            return (
                StatusCode::BAD_REQUEST,
                Json(ProjectResponse {
                    ok: false,
                    project: None,
                    error: Some(format!("cannot create workspace directory: {e}")),
                }),
            );
        }
        match try_canonicalize(&expanded) {
            Ok(canon) => canon,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ProjectResponse {
                        ok: false,
                        project: None,
                        error: Some(e),
                    }),
                );
            }
        }
    };

    let project = Project {
        id: uuid::Uuid::new_v4(),
        name: req.name,
        workspace_root,
        config: req.config,
        created_ms: now,
        updated_ms: now,
    };
    match state.runtime.upsert_project(project, now) {
        Ok(project) => (
            StatusCode::CREATED,
            Json(ProjectResponse {
                ok: true,
                project: Some(project),
                error: None,
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse {
                ok: false,
                project: None,
                error: Some(e.to_string()),
            }),
        ),
    }
}

/// Build a project JSON value with live git fields computed on its
/// `workspace_root`.  Non-repo or any failure → nulls/empty vec — the surface
/// hides git chrome when the fields are absent.
async fn enriched_project_json(project: &Project) -> serde_json::Value {
    let mut j = serde_json::to_value(project).unwrap_or(json!({}));

    let proj_root = &project.workspace_root;
    let root_path = std::path::Path::new(proj_root);

    // -- git_branch (pure filesystem) ------------------------------------
    let (is_repo, git_branch) = if !proj_root.is_empty() {
        ocean_agent::git_head_info(root_path)
    } else {
        (false, None)
    };
    j["git_branch"] = json!(git_branch);

    // -- git_dirty (subprocess, ~1.5s timeout) ---------------------------
    let git_dirty: Option<bool> = if is_repo && !proj_root.is_empty() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(proj_root)
                .arg("status")
                .arg("--porcelain")
                .output(),
        )
        .await;
        match result {
            Ok(Ok(out)) if out.status.success() => Some(!out.stdout.is_empty()),
            _ => None,
        }
    } else {
        None
    };
    j["git_dirty"] = json!(git_dirty);

    // -- worktrees (subprocess) ------------------------------------------
    let worktrees: Vec<ocean_agent::WorktreeInfo> = if is_repo && !proj_root.is_empty() {
        match tokio::process::Command::new("git")
            .arg("-C")
            .arg(proj_root)
            .arg("worktree")
            .arg("list")
            .arg("--porcelain")
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                parse_worktree_list(&String::from_utf8_lossy(&out.stdout))
                    .into_iter()
                    .filter(|wt| &wt.path != proj_root)
                    .collect()
            }
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    j["worktrees"] = serde_json::to_value(&worktrees).unwrap_or(json!([]));

    j
}

pub(super) async fn discover_project_worktrees(
    project_root: &str,
) -> Result<Vec<ocean_agent::WorktreeInfo>, String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()
        .await
        .map_err(|e| format!("git worktree discovery failed: {e}"))?;
    if !out.status.success() {
        return Err("git worktree discovery failed".into());
    }
    Ok(parse_worktree_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git worktree list --porcelain` output into WorktreeInfo entries.
/// Strips `refs/heads/` from branch refs.
pub(super) fn parse_worktree_list(raw: &str) -> Vec<ocean_agent::WorktreeInfo> {
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(path) = current_path.take() {
                out.push(ocean_agent::WorktreeInfo {
                    path,
                    branch: current_branch.take(),
                });
            }
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("worktree ") {
            current_path = Some(path.trim().to_string());
        } else if let Some(branch) = trimmed.strip_prefix("branch ") {
            let b = branch.trim();
            current_branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
    }
    // Flush last entry if no trailing blank line.
    if let Some(path) = current_path {
        out.push(ocean_agent::WorktreeInfo {
            path,
            branch: current_branch,
        });
    }
    out
}

/// `GET /v1/projects/{id}` — one project plus its sessions (the sessions in the
/// project's `workspace_root` bucket).
pub(super) async fn project_get(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.runtime.find_project(id) {
        Ok(Some(project)) => {
            let sessions = state
                .runtime
                .list_sessions(Some(&project.workspace_root))
                .unwrap_or_default();
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "project": project, "sessions": sessions })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("unknown project {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

/// `PATCH /v1/projects/{id}` — update name and/or config (partial).
pub(super) async fn project_patch(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(req): Json<PatchProjectRequest>,
) -> (StatusCode, Json<ProjectResponse>) {
    let now = Utc::now().timestamp_millis();
    let existing = match state.runtime.find_project(id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ProjectResponse {
                    ok: false,
                    project: None,
                    error: Some(format!("unknown project {id}")),
                }),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProjectResponse {
                    ok: false,
                    project: None,
                    error: Some(e.to_string()),
                }),
            )
        }
    };
    let updated = Project {
        name: req.name.unwrap_or(existing.name),
        config: req.config.unwrap_or(existing.config),
        ..existing
    };
    match state.runtime.upsert_project(updated, now) {
        Ok(project) => (
            StatusCode::OK,
            Json(ProjectResponse {
                ok: true,
                project: Some(project),
                error: None,
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ProjectResponse {
                ok: false,
                project: None,
                error: Some(e.to_string()),
            }),
        ),
    }
}

/// `DELETE /v1/projects/{id}` — remove a project. Its sessions are NOT deleted;
/// they keep their workspace bucket and simply become project-less.
pub(super) async fn project_delete(
    State(state): State<AppState>,
    Path(id): Path<ProjectId>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.runtime.delete_project(id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("unknown project {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}
