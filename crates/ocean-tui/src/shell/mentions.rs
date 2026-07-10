//! `@`-file mentions — a fuzzy file index over the active project, so typing
//! `@` in the composer opens a picker of cwd files (like Claude Code / pi / omp
//! / codex). Scans once per project (gitignore-respecting), then ranks with the
//! same subsequence scorer the `/` palette uses, biased toward basename hits.

use std::path::Path;

use ignore::WalkBuilder;

/// Hard cap on the indexed file count — keeps the scan bounded on huge trees.
/// If a repo exceeds this, the picker still works over the first `CAP` files
/// (sorted), which covers the overwhelming majority of real projects.
pub const CAP: usize = 6000;

/// Walk `root` for files, honoring `.gitignore` and skipping the usual noise
/// (`.git`, and whatever the project ignores). Returns project-relative,
/// forward-slash paths, sorted. Directories are excluded — you mention files.
pub fn scan(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dent in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build()
        .flatten()
    {
        if out.len() >= CAP {
            break;
        }
        if dent.file_type().is_some_and(|t| t.is_file()) {
            if let Ok(rel) = dent.path().strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

/// Rank `index` against `query` (the text after `@`), returning the top `limit`
/// paths. Scores against both the basename and the full path and keeps the
/// better — so `main` surfaces `src/main.rs` above `src/domain/other.rs`. An
/// empty query returns the first `limit` paths (the sorted head).
pub fn filter<'a>(index: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    if query.is_empty() {
        return index.iter().take(limit).map(String::as_str).collect();
    }
    let mut scored: Vec<(&str, i32)> = index
        .iter()
        .filter_map(|p| {
            let base = p.rsplit('/').next().unwrap_or(p.as_str());
            // Basename match gets a bonus — that's what people are usually after.
            let by_base = crate::shell::slash::subseq_score(query, base).map(|s| s + 6);
            let by_path = crate::shell::slash::subseq_score(query, p);
            by_base
                .into_iter()
                .chain(by_path)
                .max()
                .map(|s| (p.as_str(), s))
        })
        .collect();
    // Best score first; break ties toward shorter (shallower) paths, then lexicographic.
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then_with(|| a.0.cmp(b.0))
    });
    scored.into_iter().take(limit).map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> Vec<String> {
        [
            "src/main.rs",
            "src/domain/user.rs",
            "README.md",
            "docs/main-notes.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn empty_query_returns_head() {
        let i = idx();
        assert_eq!(filter(&i, "", 2).len(), 2);
    }

    #[test]
    fn basename_hit_outranks_path_hit() {
        // "main" is a basename of src/main.rs; it's only a mid-path match in
        // docs/main-notes.md's dir — main.rs should win.
        let i = idx();
        let top = filter(&i, "main", 10);
        assert_eq!(top.first(), Some(&"src/main.rs"));
    }

    #[test]
    fn non_subsequence_drops() {
        let i = idx();
        assert!(filter(&i, "zzzq", 10).is_empty());
    }
}
