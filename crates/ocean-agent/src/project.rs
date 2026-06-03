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
            find_by_workspace(&dir, "/dev/ocean-os").unwrap().unwrap().id,
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
        assert_eq!(all[0].name, "new-name");
        assert_eq!(all[0].updated_ms, 3000);
    }

    #[test]
    fn malformed_file_is_error() {
        let dir = tmp_dir();
        std::fs::write(projects_path(&dir), "{ not valid json").unwrap();
        assert!(load_all(&dir).is_err());
    }
}
