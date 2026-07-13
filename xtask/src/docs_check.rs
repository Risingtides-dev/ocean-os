//! Repository documentation and workspace-index integrity checks.
//! Dependency-free so documentation validation cannot burden production crates.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use crate::paths::workspace_root;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CheckStats {
    pub packages: usize,
    pub markdown_files: usize,
    pub local_links: usize,
}

pub fn run(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("docs-check: no arguments are supported");
        return ExitCode::FAILURE;
    }
    match check_repo(&workspace_root()) {
        Ok(stats) => {
            println!(
                "docs-check: PASS ({} packages, {} active Markdown files, {} local links)",
                stats.packages, stats.markdown_files, stats.local_links
            );
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("docs-check: FAILED ({} issue(s))", errors.len());
            for error in errors {
                eprintln!("  - {error}");
            }
            ExitCode::FAILURE
        }
    }
}

pub fn check_repo(root: &Path) -> Result<CheckStats, Vec<String>> {
    let mut errors = Vec::new();
    let cargo = read(&root.join("Cargo.toml"), &mut errors);
    let members = parse_array(&cargo, "members");
    let defaults = parse_array(&cargo, "default-members");
    if members.is_empty() {
        errors.push("Cargo.toml workspace members could not be parsed".into());
    }

    let package_names = names_for_member_paths(root, &members, &mut errors);
    let index = read(&root.join("crates/AGENTS.md"), &mut errors);
    let indexed_names = parse_index_packages(&index);
    compare_sets(
        "workspace package index",
        &package_names,
        &indexed_names,
        &mut errors,
    );

    let default_names = names_for_member_paths(root, &defaults, &mut errors);
    let non_default_section = section(&index, "## Non-default Members");
    for name in package_names.difference(&default_names) {
        if !non_default_section.contains(&format!("`{name}`")) {
            errors.push(format!(
                "non-default package `{name}` has no rationale in crates/AGENTS.md"
            ));
        }
    }

    if !read(&root.join("AGENTS.md"), &mut errors).contains("crates/AGENTS.md") {
        errors.push("root AGENTS.md does not point to crates/AGENTS.md".into());
    }

    let mut markdown = Vec::new();
    collect_markdown(root, root, &mut markdown, &mut errors);
    let mut local_links = 0;
    for path in &markdown {
        let text = read(path, &mut errors);
        for target in markdown_targets(&text) {
            if is_external_target(&target) {
                continue;
            }
            if target.contains(".agentarchive") {
                errors.push(format!(
                    "{} links into opt-in .agentarchive: {target}",
                    display(root, path)
                ));
                continue;
            }
            let path_part = target.split('#').next().unwrap_or("");
            if path_part.is_empty() || Path::new(path_part).is_absolute() {
                continue;
            }
            let candidate = normalize(&path.parent().unwrap_or(root).join(path_part));
            if !candidate.starts_with(root) {
                continue;
            }
            local_links += 1;
            if !candidate.exists() {
                errors.push(format!(
                    "{} has broken local link `{target}`",
                    display(root, path)
                ));
            }
        }
    }

    let stats = CheckStats {
        packages: package_names.len(),
        markdown_files: markdown.len(),
        local_links,
    };
    if errors.is_empty() {
        Ok(stats)
    } else {
        Err(errors)
    }
}

fn read(path: &Path, errors: &mut Vec<String>) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            errors.push(format!("could not read {}: {error}", path.display()));
            String::new()
        }
    }
}

fn parse_array(input: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut active = false;
    for raw in input.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if !active {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let rest = rest.trim_start();
            if !rest.starts_with('=') || !rest.contains('[') {
                continue;
            }
            active = true;
        }
        values.extend(quoted_values(line));
        if line.contains(']') {
            break;
        }
    }
    values
}

fn quoted_values(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('"') else { break };
        out.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    out
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && line.starts_with('[') {
            return None;
        }
        if in_package && line.starts_with("name") {
            return quoted_values(line).into_iter().next();
        }
    }
    None
}

fn names_for_member_paths(
    root: &Path,
    members: &[String],
    errors: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for member in members {
        let manifest = root.join(member).join("Cargo.toml");
        let text = read(&manifest, errors);
        match package_name(&text) {
            Some(name) => {
                names.insert(name);
            }
            None => errors.push(format!(
                "{} has no [package] name",
                display(root, &manifest)
            )),
        }
    }
    names
}

fn parse_index_packages(index: &str) -> BTreeSet<String> {
    index
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("| `")?;
            let end = rest.find("` |")?;
            Some(rest[..end].to_string())
        })
        .collect()
}

fn compare_sets(
    label: &str,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let missing: Vec<_> = expected.difference(actual).cloned().collect();
    let extra: Vec<_> = actual.difference(expected).cloned().collect();
    if !missing.is_empty() {
        errors.push(format!("{label} missing: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        errors.push(format!("{label} has unknown entries: {}", extra.join(", ")));
    }
}

fn section<'a>(input: &'a str, heading: &str) -> &'a str {
    let Some(start) = input.find(heading) else {
        return "";
    };
    let tail = &input[start + heading.len()..];
    let end = tail.find("\n## ").unwrap_or(tail.len());
    &tail[..end]
}

fn collect_markdown(root: &Path, dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        errors.push(format!("could not list {}", display(root, dir)));
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if excluded_dir(relative) {
                continue;
            }
            collect_markdown(root, &path, out, errors);
        } else if path.extension().and_then(|value| value.to_str()) == Some("md")
            && path.file_name().and_then(|value| value.to_str()) != Some("events.md")
        {
            out.push(path);
        }
    }
}

fn excluded_dir(relative: &Path) -> bool {
    let text = relative.to_string_lossy();
    text == ".git"
        || text == "target"
        || text.starts_with("target/")
        || text == ".pi-subagents"
        || text.starts_with(".pi-subagents/")
        || text == ".claude/worktrees"
        || text.starts_with(".claude/worktrees/")
        || text == ".agentignore"
        || text.starts_with(".agentignore/")
        || text == "docs/.agentarchive"
        || text.starts_with("docs/.agentarchive/")
}

fn markdown_targets(input: &str) -> Vec<String> {
    let inline = input
        .split("](")
        .skip(1)
        .filter_map(|tail| tail.split(')').next())
        .filter_map(link_destination);
    let references = input.lines().filter_map(reference_destination);
    inline.chain(references).collect()
}

fn link_destination(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let target = if let Some(rest) = raw.strip_prefix('<') {
        rest.split('>').next().unwrap_or("")
    } else {
        raw.split_whitespace().next().unwrap_or("")
    };
    (!target.is_empty()).then(|| target.to_string())
}

fn reference_destination(line: &str) -> Option<String> {
    let line = line.trim_start();
    let rest = line.strip_prefix('[')?;
    let (label, destination) = rest.split_once("]:")?;
    // Footnote definitions use the same punctuation but contain prose rather
    // than a link destination.
    if label.starts_with('^') || label.is_empty() {
        return None;
    }
    link_destination(destination)
}

fn is_external_target(target: &str) -> bool {
    target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("obsidian:")
        || target.starts_with('#')
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_toml_arrays() {
        let input = r#"
[workspace]
members = [
  "crates/alpha",
  "xtask", # comment
]
default-members = ["crates/alpha"]
"#;
        assert_eq!(parse_array(input, "members"), ["crates/alpha", "xtask"]);
        assert_eq!(parse_array(input, "default-members"), ["crates/alpha"]);
    }

    #[test]
    fn parses_only_package_index_rows() {
        let input = "| Package | Role |\n| `ocean-core` | Core |\n| prose | text |\n";
        assert_eq!(
            parse_index_packages(input),
            BTreeSet::from(["ocean-core".to_string()])
        );
    }

    #[test]
    fn extracts_inline_and_reference_markdown_targets() {
        let input = "[local](../AGENTS.md) [web](https://example.com) [ref][core]\n\
                     [core]: <../crates/ocean-core/src/lib.rs> \"Core\"\n\
                     [^note]: prose, not a link\n";
        assert_eq!(
            markdown_targets(input),
            [
                "../AGENTS.md",
                "https://example.com",
                "../crates/ocean-core/src/lib.rs",
            ]
        );
    }

    fn sample_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ocean-docs-check-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("crates/alpha/src")).expect("create crate");
        fs::create_dir_all(root.join("docs")).expect("create docs");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/alpha\"]\ndefault-members = [\"crates/alpha\"]\n",
        )
        .expect("write workspace");
        fs::write(
            root.join("crates/alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
        )
        .expect("write package");
        fs::write(
            root.join("crates/AGENTS.md"),
            "| Package | Role |\n|---|---|\n| `alpha` | fixture |\n\n## Non-default Members\n",
        )
        .expect("write index");
        fs::write(
            root.join("AGENTS.md"),
            "See [the package index](crates/AGENTS.md).\n",
        )
        .expect("write root contract");
        fs::write(root.join("docs/guide.md"), "See [root](../AGENTS.md).\n").expect("write guide");
        root
    }

    #[test]
    fn repository_check_accepts_consistent_fixture() {
        let root = sample_repo("pass");
        let stats = check_repo(&root).expect("consistent fixture should pass");
        assert_eq!(stats.packages, 1);
        assert_eq!(stats.markdown_files, 3);
        assert_eq!(stats.local_links, 2);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn repository_check_reports_index_link_and_archive_drift() {
        let root = sample_repo("fail");
        fs::write(
            root.join("crates/AGENTS.md"),
            "| Package | Role |\n|---|---|\n| `beta` | stale |\n\n## Non-default Members\n",
        )
        .expect("replace index");
        fs::write(
            root.join("docs/guide.md"),
            "[missing][m] [archive][a]\n[m]: missing.md\n[a]: ../docs/.agentarchive/old.md\n",
        )
        .expect("replace guide");

        let errors = check_repo(&root).expect_err("drift must fail").join("\n");
        assert!(errors.contains("workspace package index missing: alpha"));
        assert!(errors.contains("workspace package index has unknown entries: beta"));
        assert!(errors.contains("broken local link `missing.md`"));
        assert!(errors.contains("links into opt-in .agentarchive"));
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
