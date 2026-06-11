//! B1 — the tree-sitter resolver (OCEAN-307). Fills the `Resolver` seam with
//! symbol presence + signature-shape hashing, replacing file-exists blindness:
//! a claim anchored at `Cargo.toml#workspace.members` must FLAG when the
//! members list changes, not hold forever because the file still exists.
//!
//! ## Resolution semantics (per anchor)
//!
//! | anchor state                                            | resolution      |
//! |---------------------------------------------------------|-----------------|
//! | no file (symbol-only), path outside repo                | `Unresolvable`  |
//! | file missing (bare-basename match elsewhere)            | `Dead` (`Resolves(0.5)`) |
//! | `.rs`/`.toml` + symbol, symbol found, hash matches/none | `Resolves(1.0)` |
//! | `.rs`/`.toml` + symbol, symbol found, hash differs      | `Stale`         |
//! | `.rs`/`.toml` + symbol, symbol gone                     | `Dead`          |
//! | other language + symbol                                 | `Unresolvable`  |
//! | no symbol, recorded file hash matches / differs         | `Resolves(1.0)` / `Stale` |
//! | no symbol, no recorded hash, file present               | `Resolves(1.0)` |
//!
//! **Why `Stale` and not `Resolves(partial)` for a changed shape:** the spec's
//! `S(c)` is *structural reproducibility* — "does the AST anchor still
//! resolve?" — where what must reproduce is the structure the claim attested
//! at write time, not merely a name. A symbol whose signature changed is a
//! name that resolves to *demonstrably different structure*: the recorded
//! state does not reproduce, so the claim is stale (exactly the schema's
//! `ClaimStatus::Stale`, which `reverify` already maps `Resolution::Stale`
//! onto). `Resolves(partial)` would conflate "weak evidence of the same
//! thing" (e.g. the 0.5 bare-basename match) with "strong evidence of a
//! different thing" — and a changed anchor must flag for re-verification,
//! never quietly keep partial trust.
//!
//! ## TOML: structured key probe, not a second grammar
//!
//! Rust goes through tree-sitter (`tree-sitter-rust`). For `.toml` anchors
//! (dotted key paths like `workspace.members`) we deliberately use the `toml`
//! crate the workspace already depends on, parsed to a real value tree,
//! instead of adding a `tree-sitter-toml` grammar crate:
//!
//! - it is *more* semantic than a CST probe: `[workspace] members = [...]`,
//!   `workspace.members = [...]` and inline-table spellings all normalize to
//!   the same value tree, so the hash tracks meaning, not surface syntax;
//! - dotted-path lookup is native (`Value` indexing) instead of hand-walking
//!   table headers / dotted keys / inline tables in a CST;
//! - zero new dependencies for a strictly stronger check. The grammar crate
//!   can still slot in later behind this same function without touching the
//!   `Resolver` trait.
//!
//! This catches the demonstrated Cargo.toml case: the hash of the value at
//! `workspace.members` changes the moment the members array changes.

use crate::claim::{Anchor, Claim};
use crate::seams::{is_repo_relative, Resolution, Resolver, WORKTREE};
use std::path::PathBuf;
use std::process::Command;
use tree_sitter::{Node, Parser};

/// Symbol-presence + signature-hash resolver. Layer B1 organ behind the
/// `Resolver` seam — same trait, same path-traversal guard, same
/// working-tree/at-commit duality as [`crate::seams::FileExistsResolver`].
pub struct TreeSitterResolver {
    pub repo_root: PathBuf,
}

/// What the repo has at (commit, path).
enum Source {
    /// Not in the tree / working copy.
    Missing,
    /// Present with blob content.
    Text(String),
    /// Present but not a readable blob (a directory). File-exists evidence
    /// only — nothing to parse or hash.
    Opaque,
}

impl TreeSitterResolver {
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git").arg("-C").arg(&self.repo_root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Fetch the anchor's blob at `at_commit` (or the working tree). At-commit
    /// reads go through `git show <rev>:<path>` — never a checkout.
    fn load(&self, file: &str, at_commit: &str) -> Source {
        if at_commit == WORKTREE {
            let path = self.repo_root.join(file);
            if !path.exists() {
                return Source::Missing;
            }
            return match std::fs::read(&path) {
                Ok(bytes) => Source::Text(String::from_utf8_lossy(&bytes).into_owned()),
                Err(_) => Source::Opaque, // exists but unreadable as a blob (directory)
            };
        }
        let spec = format!("{at_commit}:{file}");
        // Only blobs are hashable content; a tracked directory resolves as a
        // tree object — present, but opaque to parsing.
        match self.git(&["cat-file", "-t", &spec]).map(|t| t.trim().to_string()) {
            Some(t) if t == "blob" => match self.git(&["show", &spec]) {
                Some(text) => Source::Text(text),
                None => Source::Missing,
            },
            Some(_) => Source::Opaque,
            None => Source::Missing,
        }
    }

    /// Same bare-basename fallback as the file-exists resolver: corpus claims
    /// often anchor bare filenames (`input.rs`). Ambiguous location ⇒ weak
    /// file-level evidence only — no symbol probe against a guessed file.
    fn basename_resolves(&self, file: &str, at_commit: &str) -> bool {
        if file.contains('/') {
            return false;
        }
        let listing = if at_commit == WORKTREE {
            self.git(&["ls-files"])
        } else {
            self.git(&["ls-tree", "-r", "--name-only", at_commit])
        };
        let Some(listing) = listing else { return false };
        let needle = format!("/{file}");
        listing.lines().any(|l| l == file || l.ends_with(&needle))
    }

    /// Current signature/shape hash for `anchor` at `at_commit`:
    /// the symbol's shape when the anchor names one, the file content hash
    /// otherwise. `None` = nothing hashable there (missing file, missing
    /// symbol, unparseable language).
    pub fn sig_hash_at(&self, anchor: &Anchor, at_commit: &str) -> Option<String> {
        let file = anchor.file.as_deref()?;
        if !is_repo_relative(file) {
            return None;
        }
        let Source::Text(text) = self.load(file, at_commit) else { return None };
        match anchor.symbol.as_deref() {
            Some(symbol) => match language_of(file)? {
                Lang::Rust => rust_sig_hash(&text, symbol),
                Lang::Toml => toml_sig_hash(&text, symbol),
            },
            None => Some(fnv1a64(&text)),
        }
    }

    /// Seed write-time baselines before a replay: every SYMBOL-bearing anchor
    /// without a recorded `sig_hash` gets the hash of its symbol's shape at
    /// the claim's anchor commit. Symbol-less anchors are deliberately NOT
    /// seeded: a file-only anchor is an existence claim (file-exists
    /// semantics), and auto-recording a content hash would flip every later
    /// doc edit to Stale — false flags file-exists rightly never raised.
    pub fn seed_sig_hashes(&self, claims: &mut [Claim]) {
        for claim in claims.iter_mut() {
            let commit = claim.provenance.commit_sha.clone();
            for anchor in &mut claim.provenance.anchors {
                if anchor.symbol.is_some() && anchor.sig_hash.is_none() {
                    anchor.sig_hash = self.sig_hash_at(anchor, &commit);
                }
            }
        }
    }
}

impl Resolver for TreeSitterResolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution {
        // Identical guard semantics to FileExistsResolver: symbol-only
        // anchors carry no file evidence, and absolute/`..` paths must never
        // reach the filesystem or git (Unresolvable, never Stale/Dead).
        let Some(file) = anchor.file.as_deref() else {
            return Resolution::Unresolvable;
        };
        if !is_repo_relative(file) {
            return Resolution::Unresolvable;
        }
        let text = match self.load(file, at_commit) {
            Source::Missing => {
                if self.basename_resolves(file, at_commit) {
                    return Resolution::Resolves(0.5);
                }
                return Resolution::Dead;
            }
            Source::Text(text) => text,
            // Present but opaque (directory): pure existence evidence.
            Source::Opaque => {
                return match (&anchor.symbol, &anchor.sig_hash) {
                    (None, None) => Resolution::Resolves(1.0),
                    _ => Resolution::Unresolvable,
                };
            }
        };
        match anchor.symbol.as_deref() {
            Some(symbol) => {
                let Some(lang) = language_of(file) else {
                    // A symbol in a language we cannot parse: can't check —
                    // never evidence of life or death.
                    return Resolution::Unresolvable;
                };
                let current = match lang {
                    Lang::Rust => rust_sig_hash(&text, symbol),
                    Lang::Toml => toml_sig_hash(&text, symbol),
                };
                match current {
                    None => Resolution::Dead, // file parsed, symbol gone
                    Some(cur) => match anchor.sig_hash.as_deref() {
                        // No recorded baseline: presence is all we can attest.
                        None => Resolution::Resolves(1.0),
                        Some(recorded) if recorded == cur => Resolution::Resolves(1.0),
                        // Name resolves, shape doesn't reproduce: stale.
                        Some(_) => Resolution::Stale,
                    },
                }
            }
            None => match anchor.sig_hash.as_deref() {
                // File-level: recorded content hash vs the blob.
                Some(recorded) if recorded == fnv1a64(&text) => Resolution::Resolves(1.0),
                Some(_) => Resolution::Stale,
                // No recorded state: file-exists semantics.
                None => Resolution::Resolves(1.0),
            },
        }
    }
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Toml,
}

fn language_of(file: &str) -> Option<Lang> {
    let ext = std::path::Path::new(file).extension()?.to_str()?;
    match ext {
        "rs" => Some(Lang::Rust),
        "toml" => Some(Lang::Toml),
        _ => None,
    }
}

/// FNV-1a 64 — tiny, dependency-free and stable across Rust releases (unlike
/// `DefaultHasher`, whose algorithm is unspecified). Sig-hashes persist
/// inside handoffs, so the function must never drift.
fn fnv1a64(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}


// ---------------------------------------------------------------------------
// Rust: tree-sitter symbol location + signature shape
// ---------------------------------------------------------------------------

/// Item kinds whose `name` field can match an anchor symbol.
const NAMED_ITEM_KINDS: &[&str] = &[
    "function_item",
    "function_signature_item",
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "mod_item",
    "const_item",
    "static_item",
    "type_item",
    "macro_definition",
];

/// Kinds whose signature excludes the `body`: a function's body, a module's
/// contents and an impl's items may churn freely without changing what the
/// anchor attests (the declaration shape). Everything else (struct fields,
/// enum variants, a trait's method set, const/static/type values) IS the
/// shape and hashes in full.
const BODYLESS_SIG_KINDS: &[&str] = &["function_item", "mod_item", "impl_item"];

/// Hash of the signature shapes of every item named `symbol` in `source`,
/// in document order. `None` = no such symbol. A path-qualified symbol
/// (`module::name`) matches on its last segment.
fn rust_sig_hash(source: &str, symbol: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust grammar version matches tree-sitter core");
    let tree = parser.parse(source, None)?;
    let want = symbol.rsplit("::").next().unwrap_or(symbol);
    let mut sigs: Vec<String> = Vec::new();
    collect_rust_sigs(tree.root_node(), source, want, &mut sigs);
    if sigs.is_empty() {
        return None;
    }
    Some(fnv1a64(&sigs.join("\n")))
}

fn collect_rust_sigs(node: Node, source: &str, want: &str, sigs: &mut Vec<String>) {
    let mut matched = false;
    if NAMED_ITEM_KINDS.contains(&node.kind()) {
        if let Some(name) = node.child_by_field_name("name") {
            matched = node_text(name, source) == want;
        }
    } else if node.kind() == "impl_item" {
        // `impl Foo` / `impl Trait for Foo` anchors by the type or trait name.
        matched = ["type", "trait"].iter().any(|f| {
            node.child_by_field_name(f)
                .is_some_and(|n| base_type_name(node_text(n, source)) == want)
        });
    }
    if matched {
        sigs.push(signature_text(node, source));
    }
    // Always recurse: methods inside impls, items inside mods, nested fns.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_sigs(child, source, want, sigs);
    }
}

/// `FileExistsResolver` from `FileExistsResolver<'a, T>` / `crate::seams::FileExistsResolver`.
fn base_type_name(type_text: &str) -> &str {
    let head = type_text.split('<').next().unwrap_or(type_text).trim();
    head.rsplit("::").next().unwrap_or(head)
}

/// The item's shape as a canonical token stream — minus its `body` for kinds
/// where the body is not part of the attested shape. Token-level (tree-sitter
/// leaves joined by single spaces) rather than text-level, so formatting
/// churn (rustfmt reflow, re-indents, newline placement) can never read as a
/// shape change while string-literal contents stay byte-exact. Comments are
/// skipped: prose churn is not structure.
fn signature_text(node: Node, source: &str) -> String {
    let end_limit = if BODYLESS_SIG_KINDS.contains(&node.kind()) {
        node.child_by_field_name("body").map_or(node.end_byte(), |b| b.start_byte())
    } else {
        node.end_byte()
    };
    let mut tokens: Vec<&str> = Vec::new();
    collect_tokens(node, source, end_limit, &mut tokens);
    tokens.join(" ")
}

fn collect_tokens<'s>(node: Node, source: &'s str, end_limit: usize, tokens: &mut Vec<&'s str>) {
    if node.start_byte() >= end_limit {
        return;
    }
    if node.child_count() == 0 {
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            tokens.push(node_text(node, source));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_tokens(child, source, end_limit, tokens);
    }
}

fn node_text<'s>(node: Node, source: &'s str) -> &'s str {
    &source[node.start_byte()..node.end_byte()]
}

// ---------------------------------------------------------------------------
// TOML: dotted-key probe over the parsed value tree
// ---------------------------------------------------------------------------

/// Hash of the value at dotted key path `symbol` (e.g. `workspace.members`).
/// `None` = path absent or the document doesn't parse as TOML.
fn toml_sig_hash(source: &str, symbol: &str) -> Option<String> {
    let doc: toml::Value = toml::from_str(source).ok()?;
    let mut cur = &doc;
    for seg in symbol.split('.') {
        cur = cur.get(seg)?;
    }
    // The parsed value's Display is already canonical: two documents that
    // spell the same value differently (section vs dotted key, reflowed
    // arrays) print identically, so surface syntax never reads as change.
    Some(fnv1a64(&cur.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = r#"
pub struct Gate { pub level: u8 }

impl Gate {
    pub fn requires_permission(&self, action: &str) -> bool {
        action != "read"
    }
}

pub fn free_standing(x: i32) -> i32 { x + 1 }
"#;

    #[test]
    fn rust_symbol_found_and_hash_is_stable() {
        let a = rust_sig_hash(RUST_SRC, "requires_permission").unwrap();
        let b = rust_sig_hash(RUST_SRC, "requires_permission").unwrap();
        assert_eq!(a, b);
        assert!(rust_sig_hash(RUST_SRC, "no_such_symbol").is_none());
    }

    #[test]
    fn rust_body_edit_does_not_change_a_function_signature_hash() {
        let edited = RUST_SRC.replace("action != \"read\"", "action != \"read\" && self.level > 0");
        assert_eq!(
            rust_sig_hash(RUST_SRC, "requires_permission"),
            rust_sig_hash(&edited, "requires_permission"),
        );
    }

    #[test]
    fn rust_signature_edit_changes_the_hash() {
        let edited = RUST_SRC
            .replace("requires_permission(&self, action: &str)", "requires_permission(&self)");
        assert_ne!(
            rust_sig_hash(RUST_SRC, "requires_permission"),
            rust_sig_hash(&edited, "requires_permission"),
        );
    }

    #[test]
    fn rust_formatting_churn_does_not_change_the_hash() {
        let reflowed = RUST_SRC.replace(
            "pub fn requires_permission(&self, action: &str) -> bool {",
            "pub fn requires_permission(\n        &self, action: &str\n    ) -> bool {",
        );
        assert_eq!(
            rust_sig_hash(RUST_SRC, "requires_permission"),
            rust_sig_hash(&reflowed, "requires_permission"),
        );
    }

    #[test]
    fn rust_struct_field_change_changes_the_hash() {
        let edited = RUST_SRC.replace("pub level: u8", "pub level: u32");
        assert_ne!(rust_sig_hash(RUST_SRC, "Gate"), rust_sig_hash(&edited, "Gate"));
    }

    #[test]
    fn rust_path_qualified_symbol_matches_last_segment() {
        assert_eq!(
            rust_sig_hash(RUST_SRC, "free_standing"),
            rust_sig_hash(RUST_SRC, "gate::free_standing"),
        );
    }

    #[test]
    fn rust_impl_anchor_matches_by_type_name() {
        // `Gate` matches both the struct and the impl header — adding a
        // method to the impl must NOT change the hash (impl body excluded).
        let with_method =
            RUST_SRC.replace("}\n\npub fn", "    pub fn extra(&self) {}\n}\n\npub fn");
        assert_eq!(rust_sig_hash(RUST_SRC, "Gate"), rust_sig_hash(&with_method, "Gate"));
    }

    const TOML_SRC: &str = r#"
[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
edition = "2021"
"#;

    #[test]
    fn toml_dotted_path_found_and_value_change_changes_hash() {
        let h = toml_sig_hash(TOML_SRC, "workspace.members").unwrap();
        let grown = TOML_SRC.replace(r#""crates/b"]"#, r#""crates/b", "crates/c"]"#);
        assert_ne!(h, toml_sig_hash(&grown, "workspace.members").unwrap());
        assert!(toml_sig_hash(TOML_SRC, "workspace.nope").is_none());
        assert!(toml_sig_hash(TOML_SRC, "nope").is_none());
    }

    #[test]
    fn toml_spelling_is_normalized_semantically() {
        // Same value, different surface syntax — identical hash.
        let dotted = "workspace.members = [\"crates/a\", \"crates/b\"]\n";
        let sect = "[workspace]\nmembers = [\n  \"crates/a\",\n  \"crates/b\",\n]\n";
        assert_eq!(
            toml_sig_hash(dotted, "workspace.members"),
            toml_sig_hash(sect, "workspace.members"),
        );
    }

    #[test]
    fn toml_sibling_key_change_does_not_change_hash() {
        let edited = TOML_SRC.replace("resolver = \"2\"", "resolver = \"3\"");
        assert_eq!(
            toml_sig_hash(TOML_SRC, "workspace.members"),
            toml_sig_hash(&edited, "workspace.members"),
        );
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(""), "cbf29ce484222325");
        assert_eq!(fnv1a64("a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64("foobar"), "85944171f73967e8");
    }

    // ---- worktree-mode resolver semantics (no git history needed) ----

    fn resolver_in(dir: &std::path::Path) -> TreeSitterResolver {
        TreeSitterResolver { repo_root: dir.to_path_buf() }
    }

    fn anchor(file: Option<&str>, symbol: Option<&str>, sig_hash: Option<&str>) -> Anchor {
        Anchor {
            file: file.map(Into::into),
            symbol: symbol.map(Into::into),
            lines: vec![],
            sig_hash: sig_hash.map(Into::into),
        }
    }

    #[test]
    fn guard_semantics_match_file_exists_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver_in(dir.path());
        // symbol-only, escape paths, absolute paths: Unresolvable, never Dead
        for a in [
            anchor(None, Some("workspace.members"), None),
            anchor(Some("../escape.rs"), None, None),
            anchor(Some("/etc/hosts"), None, None),
            anchor(Some(""), Some("x"), None),
        ] {
            assert_eq!(r.resolve(&a, WORKTREE), Resolution::Unresolvable, "{:?}", a.file);
        }
    }

    #[test]
    fn missing_file_is_dead_and_symbol_in_unknown_language_is_unresolvable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "# Proof / tuning method\n").unwrap();
        let r = resolver_in(dir.path());
        assert_eq!(r.resolve(&anchor(Some("gone.rs"), None, None), WORKTREE), Resolution::Dead);
        // .md with a symbol: can't check the symbol — honest non-verdict.
        assert_eq!(
            r.resolve(&anchor(Some("notes.md"), Some("Proof / tuning method"), None), WORKTREE),
            Resolution::Unresolvable
        );
        // .md without a symbol: plain existence.
        assert_eq!(
            r.resolve(&anchor(Some("notes.md"), None, None), WORKTREE),
            Resolution::Resolves(1.0)
        );
    }

    #[test]
    fn rust_symbol_lifecycle_present_changed_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, RUST_SRC).unwrap();
        let r = resolver_in(dir.path());
        let plain = anchor(Some("lib.rs"), Some("requires_permission"), None);

        // present, no recorded baseline → presence attests
        assert_eq!(r.resolve(&plain, WORKTREE), Resolution::Resolves(1.0));

        // record the live baseline → still resolves
        let recorded = r.sig_hash_at(&plain, WORKTREE).unwrap();
        let pinned = anchor(Some("lib.rs"), Some("requires_permission"), Some(&recorded));
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Resolves(1.0));

        // signature changes → Stale
        std::fs::write(&path, RUST_SRC.replace("action: &str", "action: &Action")).unwrap();
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Stale);

        // symbol gone → Dead
        std::fs::write(&path, RUST_SRC.replace("requires_permission", "renamed_gate")).unwrap();
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Dead);
        assert_eq!(r.resolve(&plain, WORKTREE), Resolution::Dead);
    }

    #[test]
    fn toml_key_lifecycle_present_changed_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo.toml");
        std::fs::write(&path, TOML_SRC).unwrap();
        let r = resolver_in(dir.path());
        let plain = anchor(Some("Cargo.toml"), Some("workspace.members"), None);
        assert_eq!(r.resolve(&plain, WORKTREE), Resolution::Resolves(1.0));

        let recorded = r.sig_hash_at(&plain, WORKTREE).unwrap();
        let pinned = anchor(Some("Cargo.toml"), Some("workspace.members"), Some(&recorded));
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Resolves(1.0));

        // THE Cargo.toml case: members content changes, file still exists.
        std::fs::write(&path, TOML_SRC.replace(r#""crates/b"]"#, r#""crates/b", "crates/c"]"#))
            .unwrap();
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Stale);

        // key gone → Dead
        std::fs::write(&path, "[workspace]\nresolver = \"2\"\n").unwrap();
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Dead);
    }

    #[test]
    fn file_level_recorded_hash_compares_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, "original\n").unwrap();
        let r = resolver_in(dir.path());
        let plain = anchor(Some("doc.md"), None, None);
        let recorded = r.sig_hash_at(&plain, WORKTREE).unwrap();
        let pinned = anchor(Some("doc.md"), None, Some(&recorded));
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Resolves(1.0));
        std::fs::write(&path, "rewritten\n").unwrap();
        assert_eq!(r.resolve(&pinned, WORKTREE), Resolution::Stale);
    }
}
