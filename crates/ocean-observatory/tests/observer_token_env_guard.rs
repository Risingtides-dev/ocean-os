//! Observatory Task 9 review, rollout recommendation 6: no workspace code may
//! inject `OCEAN_OBSERVER_TOKEN` into a child process environment. Children
//! obtain the observer token from the rotating mode-0600 file (or an
//! environment an OPERATOR set); a daemon that exported it would hand every
//! tool subprocess — `bash` included — a live Observatory credential.
//!
//! The scan covers every crate's production source (test modules excluded).
//! Only `ocean-observatory/src/auth.rs` may name the variable, and there only
//! to READ it; any `.env(` / `.envs(` / `set_var(` of it outside test code
//! fails, as does the literal anywhere else.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == "tests" || name == "benches" || name.starts_with('.') {
                continue;
            }
            rust_sources(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The production half of a source file: everything before its first
/// `#[cfg(test)]`-gated test module.
fn production(source: &str) -> &str {
    source
        .find("#[cfg(test)]\nmod tests")
        .or_else(|| source.find("#[cfg(test)]\nmod "))
        .map_or(source, |end| &source[..end])
}

#[test]
fn no_code_injects_the_observer_token_into_a_child_environment() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_sources(&crates, &mut files);
    assert!(
        files.len() > 100,
        "the scan stopped finding sources: {}",
        files.len()
    );

    let auth = Path::new("ocean-observatory").join("src").join("auth.rs");
    let mut offenders = Vec::new();
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let code = production(&source);
        let is_auth = path.ends_with(&auth);
        for (number, line) in code.lines().enumerate() {
            let names_it = line.contains("OCEAN_OBSERVER_TOKEN")
                || (is_auth && line.contains("OBSERVER_TOKEN_ENV"));
            if !names_it || line.trim_start().starts_with("//") {
                continue;
            }
            let writes =
                line.contains(".env(") || line.contains(".envs(") || line.contains("set_var(");
            let allowed = is_auth && !writes;
            if !allowed {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "OCEAN_OBSERVER_TOKEN may only be READ, in ocean-observatory/src/auth.rs:\n{}",
        offenders.join("\n")
    );
}
