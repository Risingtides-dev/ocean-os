//! Project persistence — the named, directory-bound workspaces sessions belong
//! to.
//!
//! A project is a name + a `workspace_root` + optional config. It does **not**
//! own a separate session store: a project's sessions are simply the sessions in
//! its `workspace_root` bucket under the existing per-workspace session layout
//! (`session::list(config_dir, Some(&workspace_root))`). So projects are a thin
//! index on top of the session store, not a parallel one.
//!
//! Storage is a single `<config_dir>/projects.json` array, written with the same
//! atomic temp-file + rename discipline as [`crate::session::save`] so a crash
//! mid-write can never corrupt the index. A missing file is an empty list (never
//! an error); a present-but-malformed file is a hard error, matching how the
//! daemon treats its other config.

use std::path::{Path, PathBuf};

use anyhow::Context;
use ocean_core::{Project, ProjectId};

/// `<config_dir>/projects.json`.
fn projects_path(config_dir: &Path) -> PathBuf {
    config_dir.join("projects.json")
}

/// Load every project. Missing file ⇒ empty list. Malformed file ⇒ error.
pub fn load_all(config_dir: &Path) -> anyhow::Result<Vec<Project>> {
    let path = projects_path(config_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Write the full project list, atomically (temp sibling + rename).
pub fn save_all(config_dir: &Path, projects: &[Project]) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("mkdir {}", config_dir.display()))?;
    let path = projects_path(config_dir);
    let json = serde_json::to_string_pretty(projects)?;
    let tmp = config_dir.join(".projects.json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// One project by id, if present.
pub fn find_by_id(config_dir: &Path, id: ProjectId) -> anyhow::Result<Option<Project>> {
    Ok(load_all(config_dir)?.into_iter().find(|p| p.id == id))
}

/// One project by its bound directory, if any project claims it. Used to map a
/// session's `workspace_root` back to its owning project.
pub fn find_by_workspace(
    config_dir: &Path,
    workspace_root: &str,
) -> anyhow::Result<Option<Project>> {
    Ok(load_all(config_dir)?
        .into_iter()
        .find(|p| p.workspace_root == workspace_root))
}

/// Insert or replace a project by id, refreshing `updated_ms`. Returns the
/// stored project.
pub fn upsert(config_dir: &Path, mut project: Project, now_ms: i64) -> anyhow::Result<Project> {
    let mut all = load_all(config_dir)?;
    project.updated_ms = now_ms;
    match all.iter_mut().find(|p| p.id == project.id) {
        Some(existing) => *existing = project.clone(),
        None => all.push(project.clone()),
    }
    save_all(config_dir, &all)?;
    Ok(project)
}

/// Remove a project by id. Returns `false` if no such project existed. Does NOT
/// touch the project's sessions — they keep their workspace bucket on disk.
pub fn delete(config_dir: &Path, id: ProjectId) -> anyhow::Result<bool> {
    let mut all = load_all(config_dir)?;
    let before = all.len();
    all.retain(|p| p.id != id);
    let removed = all.len() != before;
    if removed {
        save_all(config_dir, &all)?;
    }
    Ok(removed)
}

/// Lightweight WorktreeInfo: path + branch (or None when unresolvable).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Pure-filesystem HEAD reader — no subprocess.
///
/// Returns `(is_repo, branch_or_short_sha)`.  Handles:
/// - `.git` directory → read `HEAD`, parse `ref: refs/heads/<branch>` → branch
/// - detached HEAD (raw SHA) → first 8 chars as the label
/// - `.git` as a **file** (linked worktree) → parse `gitdir: <path>`, resolve
///   relative paths against the containing directory, read `HEAD` there
/// - any malformed or missing state → `(false, None)`
pub fn git_head_info(dir: &Path) -> (bool, Option<String>) {
    let dot_git = dir.join(".git");
    let head_path = if dot_git.is_dir() {
        dot_git.join("HEAD")
    } else if dot_git.is_file() {
        match std::fs::read_to_string(&dot_git) {
            Ok(content) => {
                let gitdir_line = content.lines().next().unwrap_or("");
                let inner = gitdir_line
                    .strip_prefix("gitdir:")
                    .map(|s| s.trim())
                    .unwrap_or("");
                if inner.is_empty() {
                    return (false, None);
                }
                let git_dir = Path::new(inner);
                let resolved = if git_dir.is_absolute() {
                    git_dir.to_path_buf()
                } else {
                    dot_git.parent().unwrap_or(Path::new(".")).join(git_dir)
                };
                resolved.join("HEAD")
            }
            Err(_) => return (false, None),
        }
    } else {
        // No .git at all
        return (false, None);
    };

    let head_content = match std::fs::read_to_string(&head_path) {
        Ok(c) => c,
        Err(_) => return (true, None), // .git exists but HEAD is unreadable
    };

    let head_line = head_content.lines().next().unwrap_or("").trim().to_string();

    if head_line.is_empty() {
        return (true, None);
    }

    if let Some(ref_path) = head_line.strip_prefix("ref: ") {
        // ref: refs/heads/branch-name → extract branch
        let branch = ref_path
            .trim()
            .strip_prefix("refs/heads/")
            .map(|b| b.to_string())
            .unwrap_or_else(|| ref_path.trim().to_string());
        (true, Some(branch))
    } else {
        // Detached HEAD (raw SHA) → first 8 chars
        let label = head_line.chars().take(8).collect::<String>();
        if label.is_empty() {
            (true, None)
        } else {
            (true, Some(label))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::ProjectConfig;
    use uuid::Uuid;

    fn project(name: &str, root: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            workspace_root: root.to_string(),
            config: ProjectConfig::default(),
            created_ms: 1000,
            updated_ms: 1000,
        }
    }

    fn tmp_dir() -> PathBuf {
        // Deterministic-ish unique dir without touching the wall clock helpers
        // the engine forbids — process id + a counter file is enough here.
        let base = std::env::temp_dir().join(format!(
            "ocean-project-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn missing_file_is_empty_not_error() {
        let dir = tmp_dir();
        assert!(load_all(&dir).unwrap().is_empty());
    }

    #[test]
    fn upsert_list_find_delete_roundtrip() {
        let dir = tmp_dir();
        let p = project("ocean-os", "/dev/ocean-os");
        let id = p.id;

        let stored = upsert(&dir, p, 2000).unwrap();
        assert_eq!(stored.updated_ms, 2000, "upsert refreshes updated_ms");

        let all = load_all(&dir).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "ocean-os");

        assert_eq!(find_by_id(&dir, id).unwrap().unwrap().id, id);
        assert_eq!(
            find_by_workspace(&dir, "/dev/ocean-os")
                .unwrap()
                .unwrap()
                .id,
            id
        );
        assert!(find_by_workspace(&dir, "/nope").unwrap().is_none());

        assert!(delete(&dir, id).unwrap(), "delete reports removal");
        assert!(load_all(&dir).unwrap().is_empty());
        assert!(!delete(&dir, id).unwrap(), "second delete is a no-op");
    }

    #[test]
    fn upsert_replaces_same_id() {
        let dir = tmp_dir();
        let mut p = project("old-name", "/dev/x");
        let id = p.id;
        upsert(&dir, p.clone(), 1000).unwrap();
        p.name = "new-name".into();
        upsert(&dir, p, 3000).unwrap();
        let all = load_all(&dir).unwrap();
        assert_eq!(all.len(), 1, "same id replaces, not appends");
        assert_eq!(all[0].id, id, "replacement keeps the original id");
        assert_eq!(all[0].name, "new-name");
        assert_eq!(all[0].updated_ms, 3000);
    }

    #[test]
    fn malformed_file_is_error() {
        let dir = tmp_dir();
        std::fs::write(projects_path(&dir), "{ not valid json").unwrap();
        assert!(load_all(&dir).is_err());
    }

    // OCEAN-228: `find_by_workspace` is the reverse of project→sessions — it maps
    // a session's bound `workspace_root` back to its owning project. This pins
    // the exact-match semantics the daemon's session-detail enrichment relies on
    // (a project owns its root, but NOT sub-dirs or unrelated paths), so the
    // session→project binding can't silently broaden or break.
    #[test]
    fn find_by_workspace_matches_exact_root_only() {
        let dir = tmp_dir();
        let p = project("ocean-os", "/dev/ocean-os");
        let id = p.id;
        upsert(&dir, p, 1000).unwrap();

        // Exact workspace root resolves to the owning project.
        assert_eq!(
            find_by_workspace(&dir, "/dev/ocean-os")
                .unwrap()
                .expect("exact root binds")
                .id,
            id,
        );
        // A sub-directory of the root is NOT the project root, so no binding —
        // session keying is by the canonical workspace_root, not any cwd within.
        assert!(
            find_by_workspace(&dir, "/dev/ocean-os/crates")
                .unwrap()
                .is_none(),
            "a sub-dir of the root must not resolve to the project"
        );
        // An unrelated directory never binds.
        assert!(find_by_workspace(&dir, "/dev/other").unwrap().is_none());
        // No projects at all ⇒ no binding (the project-less session case).
        let empty = tmp_dir();
        assert!(find_by_workspace(&empty, "/dev/ocean-os")
            .unwrap()
            .is_none());
    }

    // -- git_head_info tests -------------------------------------------------

    fn make_git_dir(path: &std::path::Path, head_content: &str) {
        let dot_git = path.join(".git");
        std::fs::create_dir_all(dot_git.join("refs").join("heads")).unwrap();
        std::fs::write(dot_git.join("HEAD"), head_content).unwrap();
    }

    fn make_worktree_git_file(path: &std::path::Path, gitdir_target: &str) {
        std::fs::write(path.join(".git"), format!("gitdir: {gitdir_target}\n")).unwrap();
    }

    #[test]
    fn git_head_info_normal_branch() {
        let dir = tmp_dir();
        make_git_dir(&dir, "ref: refs/heads/main\n");

        let (is_repo, branch) = git_head_info(&dir);
        assert!(is_repo);
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_head_info_detached_sha() {
        let dir = tmp_dir();
        let sha = "a1b2c3d4e5f67890abcdef1234567890abcdef00\n";
        make_git_dir(&dir, sha);

        let (is_repo, label) = git_head_info(&dir);
        assert!(is_repo);
        assert_eq!(label.as_deref(), Some("a1b2c3d4"));
    }

    #[test]
    fn git_head_info_worktree_gitdir_file_absolute() {
        let main_dir = tmp_dir();
        make_git_dir(&main_dir, "ref: refs/heads/feature\n");

        // Create a worktree dir whose .git file points at the main repo.
        let wt_dir = tmp_dir();
        let gitdir_target = main_dir.join(".git");
        make_worktree_git_file(&wt_dir, &gitdir_target.to_string_lossy());

        let (is_repo, branch) = git_head_info(&wt_dir);
        assert!(is_repo);
        assert_eq!(branch.as_deref(), Some("feature"));
    }

    #[test]
    fn git_head_info_worktree_gitdir_file_relative() {
        let main_dir = tmp_dir();
        make_git_dir(&main_dir, "ref: refs/heads/bugfix\n");

        // The worktree .git file uses a relative gitdir path.
        let wt_dir = tmp_dir();
        // Construct a relative path manually: ../<parent_dir>/<main_dir>/.git
        let parent = wt_dir.parent().unwrap();
        let rel = std::path::Path::new("..")
            .join(main_dir.strip_prefix(parent).unwrap_or(&main_dir))
            .join(".git");
        make_worktree_git_file(&wt_dir, &rel.to_string_lossy());

        let (is_repo, branch) = git_head_info(&wt_dir);
        assert!(is_repo);
        assert_eq!(branch.as_deref(), Some("bugfix"));
    }

    #[test]
    fn git_head_info_no_git_dir() {
        let dir = tmp_dir();

        let (is_repo, branch) = git_head_info(&dir);
        assert!(!is_repo);
        assert!(branch.is_none());
    }
    #[test]
    fn git_head_info_garbage_git_file() {
        let dir = tmp_dir();
        std::fs::write(dir.join(".git"), "not a valid gitdir line\n").unwrap();

        let (is_repo, branch) = git_head_info(&dir);
        assert!(!is_repo);
        assert!(branch.is_none());
    }

    #[test]
    fn git_head_info_empty_head_file() {
        let dir = tmp_dir();
        make_git_dir(&dir, "\n");

        let (is_repo, branch) = git_head_info(&dir);
        assert!(is_repo); // .git dir exists
        assert!(branch.is_none());
    }

    #[test]
    fn git_head_info_short_sha_detached() {
        let dir = tmp_dir();
        make_git_dir(&dir, "abc\n");

        let (is_repo, label) = git_head_info(&dir);
        assert!(is_repo);
        assert_eq!(label.as_deref(), Some("abc"));
    }
}
