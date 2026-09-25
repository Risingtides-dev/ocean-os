//! Gate 1 manifest §9.4 (Task 9 review, rollout recommendation 5): the
//! admission/binding seam is record-only in V1. The first production caller
//! of any admission function outside this crate must arrive together with the
//! wire-level test `observation_binding_never_reaches_the_wire`, so binding
//! material can never ride a provider request unnoticed.

use std::fs;
use std::path::{Path, PathBuf};

const ADMISSION_FNS: [&str; 4] = [
    "validate_admission(",
    "consume_binding(",
    "strip_binding(",
    "validate_topology_edge(",
];
/// The required test's NAME. The definition searched for is built at runtime
/// (`fn <name>(`) so this file, which must mention the name, never matches.
const REQUIRED_TEST: &str = "observation_binding_never_reaches_the_wire";

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn production(source: &str) -> &str {
    source
        .find("#[cfg(test)]\nmod tests")
        .map_or(source, |end| &source[..end])
}

#[test]
fn admission_cannot_be_wired_without_the_wire_level_binding_test() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_sources(&crates, &mut files);
    assert!(
        files.len() > 100,
        "the scan stopped finding sources: {}",
        files.len()
    );

    let own_crate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("crate dir");
    let definition = format!("fn {REQUIRED_TEST}(");
    let mut callers = Vec::new();
    let mut has_wire_test = false;
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        if source.contains(&definition) {
            has_wire_test = true;
        }
        let in_own_crate = path
            .canonicalize()
            .map(|p| p.starts_with(&own_crate))
            .unwrap_or(false);
        let is_test_file = path.components().any(|c| c.as_os_str() == "tests");
        if in_own_crate || is_test_file {
            continue;
        }
        for (number, line) in production(&source).lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if ADMISSION_FNS.iter().any(|f| line.contains(f)) {
                callers.push(format!("{}:{}", path.display(), number + 1));
            }
        }
    }
    assert!(
        callers.is_empty() || has_wire_test,
        "admission is now wired at {callers:?} but no `{REQUIRED_TEST}` test exists; \
         see §9.4 of the Observatory Gate 1 manifest"
    );
}
