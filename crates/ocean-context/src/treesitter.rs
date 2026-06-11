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
//! | known language + symbol, symbol found, hash matches/none | `Resolves(1.0)` |
//! | known language + symbol, symbol found, hash differs     | `Stale`         |
//! | known language + symbol, symbol gone (clean parse)      | `Dead`          |
//! | known language + symbol, revision doesn't parse         | `Unresolvable`  |
//! | other language + symbol                                 | `Unresolvable`  |
//! | no symbol, recorded file hash matches / differs         | `Resolves(1.0)` / `Stale` |
//! | no symbol, no recorded hash, file present               | `Resolves(1.0)` |
//!
//! Known languages (the spec's B1 row names rust/typescript grammars):
//! `.rs` (tree-sitter-rust), `.ts`/`.mts`/`.cts` and `.tsx`
//! (tree-sitter-typescript, grammar picked by extension), `.toml` (structured
//! key probe, below). `.js`/`.jsx` are NOT in B1 scope and stay Unresolvable.
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
//! Rust and TypeScript go through tree-sitter grammars. For `.toml` anchors
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
use crate::seams::{is_repo_relative, Baseline, Resolution, Resolver, WORKTREE};
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
            Some(symbol) => probe_symbol(language_of(file)?, &text, symbol).found(),
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
                match probe_symbol(lang, &text, symbol) {
                    // The revision doesn't parse: no evidence either way —
                    // a mid-edit or merge-artifact blob must never read as
                    // the symbol having been removed.
                    SymbolProbe::Unparseable => Resolution::Unresolvable,
                    SymbolProbe::Absent => Resolution::Dead, // file parsed, symbol gone
                    SymbolProbe::Found(cur) => match anchor.sig_hash.as_deref() {
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

    /// Write-time baseline for a symbol-bearing anchor at its claim's anchor
    /// commit — what `reverify` stamps on first pass so a later shape change
    /// can flag (Codex round-6 P2: baselines are part of the claim
    /// lifecycle, not a replay-CLI courtesy).
    ///
    /// Anchors this resolver can't baseline ANYWHERE (no file, traversal,
    /// unknown language, directory, no symbol) are `Unsupported` — `resolve`
    /// already gives those their honest verdict. A missing file or absent
    /// symbol AT BIRTH is `Unattestable` (the birth-check analog: the claim
    /// was never reproducible — it must not verify by name later); a bare
    /// basename that matches elsewhere keeps its weak ambiguous-location
    /// semantics instead. An unparseable birth revision is `Unparseable`.
    fn baseline_at(&self, anchor: &Anchor, at_commit: &str) -> Baseline {
        let Some(file) = anchor.file.as_deref() else { return Baseline::Unsupported };
        if !is_repo_relative(file) {
            return Baseline::Unsupported;
        }
        let Some(symbol) = anchor.symbol.as_deref() else { return Baseline::Unsupported };
        let Some(lang) = language_of(file) else { return Baseline::Unsupported };
        match self.load(file, at_commit) {
            Source::Missing => {
                if self.basename_resolves(file, at_commit) {
                    // Ambiguous location: weak file-level evidence only —
                    // defer to `resolve`'s 0.5 semantics, don't assert death.
                    Baseline::Unsupported
                } else {
                    Baseline::Unattestable
                }
            }
            Source::Opaque => Baseline::Unsupported,
            Source::Text(text) => match probe_symbol(lang, &text, symbol) {
                SymbolProbe::Found(hash) => Baseline::Stamped(hash),
                SymbolProbe::Absent => Baseline::Unattestable,
                SymbolProbe::Unparseable => Baseline::Unparseable,
            },
        }
    }
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Toml,
    /// `.ts`/`.mts`/`.cts` — the `typescript` grammar.
    TypeScript,
    /// `.tsx` — the `tsx` grammar (JSX-bearing).
    Tsx,
}

fn language_of(file: &str) -> Option<Lang> {
    let ext = std::path::Path::new(file).extension()?.to_str()?;
    match ext {
        "rs" => Some(Lang::Rust),
        "toml" => Some(Lang::Toml),
        "ts" | "mts" | "cts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        // `.js`/`.jsx` are NOT in the spec's B1 row — honestly uncheckable.
        _ => None,
    }
}

fn probe_symbol(lang: Lang, text: &str, symbol: &str) -> SymbolProbe {
    match lang {
        Lang::Rust => rust_sig_hash(text, symbol),
        Lang::Toml => toml_sig_hash(text, symbol),
        Lang::TypeScript => ts_sig_hash(text, symbol, false),
        Lang::Tsx => ts_sig_hash(text, symbol, true),
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

/// Three-way outcome of probing a source blob for a symbol: presence with a
/// shape hash, attested absence, or a blob we cannot parse — which is no
/// evidence at all (absence-of-evidence must never read as removal).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SymbolProbe {
    Found(String),
    Absent,
    Unparseable,
}

impl SymbolProbe {
    fn found(self) -> Option<String> {
        match self {
            SymbolProbe::Found(h) => Some(h),
            _ => None,
        }
    }
}

/// Probe for the signature shapes of every item named `symbol` in `source`,
/// in document order. `Absent` = parsed clean, no such symbol; `Unparseable`
/// = the source can't be parsed well enough to attest absence. A
/// path-qualified symbol (`module::name`) matches on its last segment.
fn rust_sig_hash(source: &str, symbol: &str) -> SymbolProbe {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust grammar version matches tree-sitter core");
    let Some(tree) = parser.parse(source, None) else {
        return SymbolProbe::Unparseable;
    };
    // `a::run` anchors `run` inside a scope named `a` — the qualifier is
    // matched against enclosing mod/impl/trait names, not discarded, so an
    // unrelated same-named item in another module can never corrupt the
    // hash (leading `crate`/`self` are path noise, not scopes).
    let segs: Vec<&str> = symbol
        .split("::")
        .filter(|s| !s.is_empty() && *s != "crate" && *s != "self")
        .collect();
    let (want, qual) = match segs.split_last() {
        Some((w, q)) => (*w, q),
        None => return SymbolProbe::Absent,
    };
    let mut sigs: Vec<String> = Vec::new();
    let mut scope: Vec<String> = Vec::new();
    collect_rust_sigs(tree.root_node(), source, want, qual, &mut scope, &mut sigs);
    if sigs.is_empty() {
        // tree-sitter recovers around syntax errors; a miss in a broken tree
        // can be the breakage hiding the item, not evidence of removal.
        if tree.root_node().has_error() {
            return SymbolProbe::Unparseable;
        }
        return SymbolProbe::Absent;
    }
    SymbolProbe::Found(fnv1a64(&sigs.join("\n")))
}

fn collect_rust_sigs(
    node: Node,
    source: &str,
    want: &str,
    qual: &[&str],
    scope: &mut Vec<String>,
    sigs: &mut Vec<String>,
) {
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
    // A qualified anchor must sit in a scope whose innermost names spell the
    // qualifier (`a::run` = `run` directly inside something named `a`).
    if matched && scope_matches(scope, qual) {
        sigs.push(signature_text(node, source));
    }
    // Entering a named scope? mods, impls, and traits qualify their contents.
    let entered = match node.kind() {
        "mod_item" | "trait_item" => node
            .child_by_field_name("name")
            .map(|n| node_text(n, source).to_string()),
        "impl_item" => ["type", "trait"].iter().find_map(|f| {
            node.child_by_field_name(f)
                .map(|n| base_type_name(node_text(n, source)).to_string())
        }),
        _ => None,
    };
    let pushed = entered.is_some();
    if let Some(name) = entered {
        scope.push(name);
    }
    // Always recurse: methods inside impls, items inside mods, nested fns.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_sigs(child, source, want, qual, scope, sigs);
    }
    if pushed {
        scope.pop();
    }
}

/// True when `qual` is the innermost suffix of the current scope chain — an
/// unqualified anchor (empty `qual`) matches anywhere, preserving the
/// existing all-same-named-items hashing for bare symbols.
fn scope_matches(scope: &[String], qual: &[&str]) -> bool {
    if qual.is_empty() {
        return true;
    }
    if qual.len() > scope.len() {
        return false;
    }
    scope[scope.len() - qual.len()..]
        .iter()
        .zip(qual.iter())
        .all(|(s, q)| s == q)
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
        // `line_comment`/`block_comment` are Rust's kinds; `comment` is
        // TypeScript's. Prose churn is not structure in either grammar.
        if !matches!(node.kind(), "line_comment" | "block_comment" | "comment") {
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
// TypeScript / TSX: tree-sitter symbol location + signature shape
// ---------------------------------------------------------------------------

/// TS declaration kinds whose `name` field can match an anchor symbol.
/// `variable_declarator`/`public_field_definition` cover the
/// `const requiresPermission = (...) => ...` and class-property-arrow
/// patterns; `method_definition` covers methods, getters and setters.
const TS_NAMED_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_signature",
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
    "method_definition",
    "method_signature",
    "abstract_method_signature",
    "variable_declarator",
    "public_field_definition",
    "internal_module",
    "module",
];

/// TS kinds whose `body` is excluded from the shape — implementation churn
/// inside a function/method/class/namespace must not read as a signature
/// change. Interfaces, type aliases, enums and method signatures hash in
/// full: their contents ARE the shape.
const TS_BODYLESS_SIG_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "class_declaration",
    "abstract_class_declaration",
    "internal_module",
    "module",
];

/// TS scopes that qualify their contents for qualified anchors:
/// `Class.method` / `ns.fn` (namespaces, modules, classes, interfaces).
const TS_SCOPE_KINDS: &[&str] = &[
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
    "internal_module",
    "module",
];

/// Probe TypeScript/TSX source for `symbol` — the exact mirror of
/// [`rust_sig_hash`]'s three-state epistemics. Qualified anchors accept both
/// `.` and `::` separators (handoffs write `Class.method`; the extractor
/// emits `ns::name`); the qualifier matches enclosing namespace/module/class
/// names via the same scope-chain walk as Rust.
fn ts_sig_hash(source: &str, symbol: &str, tsx: bool) -> SymbolProbe {
    let mut parser = Parser::new();
    let grammar = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    parser
        .set_language(&grammar.into())
        .expect("tree-sitter-typescript grammar version matches tree-sitter core");
    let Some(tree) = parser.parse(source, None) else {
        return SymbolProbe::Unparseable;
    };
    let dotted = symbol.replace("::", ".");
    let segs: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    let (want, qual) = match segs.split_last() {
        Some((w, q)) => (*w, q),
        None => return SymbolProbe::Absent,
    };
    let mut sigs: Vec<String> = Vec::new();
    let mut scope: Vec<String> = Vec::new();
    collect_ts_sigs(tree.root_node(), source, want, qual, &mut scope, &mut sigs);
    if sigs.is_empty() {
        // Same epistemics as Rust: a miss inside an error-bearing tree can be
        // the breakage hiding the item, not evidence of removal.
        if tree.root_node().has_error() {
            return SymbolProbe::Unparseable;
        }
        return SymbolProbe::Absent;
    }
    SymbolProbe::Found(fnv1a64(&sigs.join("\n")))
}

fn collect_ts_sigs(
    node: Node,
    source: &str,
    want: &str,
    qual: &[&str],
    scope: &mut Vec<String>,
    sigs: &mut Vec<String>,
) {
    let matched = TS_NAMED_KINDS.contains(&node.kind())
        && node.child_by_field_name("name").is_some_and(|n| node_text(n, source) == want);
    if matched && scope_matches(scope, qual) {
        sigs.push(ts_signature_text(node, source));
    }
    let entered = if TS_SCOPE_KINDS.contains(&node.kind()) {
        node.child_by_field_name("name").map(|n| node_text(n, source).to_string())
    } else {
        None
    };
    let pushed = entered.is_some();
    if let Some(name) = entered {
        scope.push(name);
    }
    // Always recurse: export statements wrap declarations, methods sit
    // inside class bodies, namespaces nest.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ts_sigs(child, source, want, qual, scope, sigs);
    }
    if pushed {
        scope.pop();
    }
}

/// TS shape end: like Rust's, the body is cut for function-like and
/// container kinds; a declarator whose value is a function expression /
/// arrow keeps the parameter list + return type but drops the function body
/// (`const f = (a: A): B => …` hashes as `const f = (a: A): B =>`).
fn ts_signature_text(node: Node, source: &str) -> String {
    let mut end_limit = node.end_byte();
    if TS_BODYLESS_SIG_KINDS.contains(&node.kind()) {
        if let Some(body) = node.child_by_field_name("body") {
            end_limit = body.start_byte();
        }
    } else if matches!(node.kind(), "variable_declarator" | "public_field_definition") {
        if let Some(value) = node.child_by_field_name("value") {
            if matches!(value.kind(), "arrow_function" | "function_expression" | "generator_function")
            {
                if let Some(body) = value.child_by_field_name("body") {
                    end_limit = body.start_byte();
                }
            }
        }
    }
    let mut tokens: Vec<&str> = Vec::new();
    collect_tokens(node, source, end_limit, &mut tokens);
    tokens.join(" ")
}

// ---------------------------------------------------------------------------
// TOML: dotted-key probe over the parsed value tree
// ---------------------------------------------------------------------------

/// Probe for the value at dotted key path `symbol` (e.g. `workspace.members`):
/// `Found(hash)` when present, `Absent` when the document parses but the path
/// is missing, `Unparseable` when the document isn't valid TOML.
fn toml_sig_hash(source: &str, symbol: &str) -> SymbolProbe {
    let Ok(doc) = toml::from_str::<toml::Value>(source) else {
        // Invalid TOML at this revision: cannot attest presence OR absence.
        return SymbolProbe::Unparseable;
    };
    let mut cur = &doc;
    for seg in symbol.split('.') {
        match cur.get(seg) {
            Some(v) => cur = v,
            None => return SymbolProbe::Absent,
        }
    }
    // The parsed value's Display is already canonical: two documents that
    // spell the same value differently (section vs dotted key, reflowed
    // arrays) print identically, so surface syntax never reads as change.
    SymbolProbe::Found(fnv1a64(&cur.to_string()))
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
        let a = rust_sig_hash(RUST_SRC, "requires_permission").found().unwrap();
        let b = rust_sig_hash(RUST_SRC, "requires_permission").found().unwrap();
        assert_eq!(a, b);
        assert_eq!(rust_sig_hash(RUST_SRC, "no_such_symbol"), SymbolProbe::Absent);
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
    fn rust_qualified_symbol_scopes_to_its_module() {
        // Codex round-4 P2 scenario: same-named `run` in two modules. The
        // qualified anchor must hash ONLY its own module's item, so an
        // unrelated change to `b::run` never flips an `a::run` claim.
        let two = "mod a { pub fn run(x: u8) -> bool { x > 0 } }\n\
                   mod b { pub fn run(x: u8) -> bool { x > 0 } }\n";
        let b_changed = "mod a { pub fn run(x: u8) -> bool { x > 0 } }\n\
                         mod b { pub fn run(x: u8, strict: bool) -> bool { x > 0 } }\n";
        let a_hash = rust_sig_hash(two, "a::run");
        assert!(matches!(a_hash, SymbolProbe::Found(_)));
        assert_eq!(a_hash, rust_sig_hash(b_changed, "a::run"), "b::run noise must not leak into a::run");
        assert_ne!(rust_sig_hash(two, "b::run"), rust_sig_hash(b_changed, "b::run"));
        // The unqualified anchor still hashes every same-named item.
        assert_ne!(rust_sig_hash(two, "run"), rust_sig_hash(b_changed, "run"));
        // A qualifier naming a scope that doesn't contain the item: Absent.
        assert_eq!(rust_sig_hash(RUST_SRC, "gate::free_standing"), SymbolProbe::Absent);
        // Leading `crate::` is path noise, not a scope.
        assert_eq!(rust_sig_hash(two, "crate::a::run"), rust_sig_hash(two, "a::run"));
        // Impl-qualified methods: `Gate::requires_permission` matches the
        // method inside `impl Gate`, and only that.
        assert!(matches!(rust_sig_hash(RUST_SRC, "Gate::requires_permission"), SymbolProbe::Found(_)));
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
        let h = toml_sig_hash(TOML_SRC, "workspace.members").found().unwrap();
        let grown = TOML_SRC.replace(r#""crates/b"]"#, r#""crates/b", "crates/c"]"#);
        assert_ne!(h, toml_sig_hash(&grown, "workspace.members").found().unwrap());
        assert_eq!(toml_sig_hash(TOML_SRC, "workspace.nope"), SymbolProbe::Absent);
        assert_eq!(toml_sig_hash(TOML_SRC, "nope"), SymbolProbe::Absent);
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

    const TS_SRC: &str = r#"
export interface PermissionCheck {
    action: string;
    level: number;
}

export class Gate {
    requiresPermission(action: string): boolean {
        return action !== "read";
    }
    get level(): number {
        return 1;
    }
}

export class Portal {
    requiresPermission(action: string): boolean {
        return action !== "read";
    }
}

export const checkAll = (actions: string[]): boolean => {
    return actions.length > 0;
};

export function freeStanding(x: number): number {
    return x + 1;
}

export type GateLevel = "low" | "high";

export enum Mode { Open, Closed }
"#;

    #[test]
    fn ts_symbol_found_and_hash_is_stable() {
        let a = ts_sig_hash(TS_SRC, "requiresPermission", false).found().unwrap();
        let b = ts_sig_hash(TS_SRC, "requiresPermission", false).found().unwrap();
        assert_eq!(a, b);
        assert_eq!(ts_sig_hash(TS_SRC, "noSuchSymbol", false), SymbolProbe::Absent);
        // every named-kind class is reachable
        for sym in ["Gate", "PermissionCheck", "checkAll", "freeStanding", "GateLevel", "Mode", "level"] {
            assert!(
                matches!(ts_sig_hash(TS_SRC, sym, false), SymbolProbe::Found(_)),
                "{sym} should be found"
            );
        }
    }

    #[test]
    fn ts_body_edit_does_not_change_hash() {
        let edited = TS_SRC.replace("action !== \"read\"", "action !== \"read\" && action !== \"list\"");
        assert_eq!(
            ts_sig_hash(TS_SRC, "Gate.requiresPermission", false),
            ts_sig_hash(&edited, "Gate.requiresPermission", false),
        );
        // arrow-function body edit: declarator shape unchanged
        let arrow_edited = TS_SRC.replace("actions.length > 0", "actions.length > 1");
        assert_eq!(
            ts_sig_hash(TS_SRC, "checkAll", false),
            ts_sig_hash(&arrow_edited, "checkAll", false),
        );
    }

    #[test]
    fn ts_signature_edit_changes_hash() {
        let edited = TS_SRC.replace(
            "requiresPermission(action: string): boolean",
            "requiresPermission(action: string, strict: boolean): boolean",
        );
        assert_ne!(
            ts_sig_hash(TS_SRC, "requiresPermission", false),
            ts_sig_hash(&edited, "requiresPermission", false),
        );
        // arrow param change flips the declarator shape
        let arrow_edited =
            TS_SRC.replace("(actions: string[]): boolean =>", "(actions: string[], max: number): boolean =>");
        assert_ne!(
            ts_sig_hash(TS_SRC, "checkAll", false),
            ts_sig_hash(&arrow_edited, "checkAll", false),
        );
        // interface member change flips the interface shape (hashes in full)
        let iface_edited = TS_SRC.replace("level: number;", "level: bigint;");
        assert_ne!(
            ts_sig_hash(TS_SRC, "PermissionCheck", false),
            ts_sig_hash(&iface_edited, "PermissionCheck", false),
        );
    }

    #[test]
    fn ts_symbol_gone_is_absent_and_broken_source_is_unparseable() {
        let gone = TS_SRC.replace("requiresPermission", "renamedGate");
        assert_eq!(ts_sig_hash(&gone, "requiresPermission", false), SymbolProbe::Absent);
        assert_eq!(
            ts_sig_hash("export function broken( ((((\n", "requiresPermission", false),
            SymbolProbe::Unparseable
        );
    }

    #[test]
    fn ts_qualified_anchor_scopes_to_its_class() {
        // Same-named method on two classes: the qualified anchor must hash
        // ONLY its own class's method — changing Portal's never flips Gate's.
        let portal_changed = TS_SRC.replace(
            "export class Portal {\n    requiresPermission(action: string): boolean {",
            "export class Portal {\n    requiresPermission(action: string, strict: boolean): boolean {",
        );
        assert_ne!(TS_SRC, portal_changed, "replace must hit");
        let gate = ts_sig_hash(TS_SRC, "Gate.requiresPermission", false);
        assert!(matches!(gate, SymbolProbe::Found(_)));
        assert_eq!(gate, ts_sig_hash(&portal_changed, "Gate.requiresPermission", false));
        assert_ne!(
            ts_sig_hash(TS_SRC, "Portal.requiresPermission", false),
            ts_sig_hash(&portal_changed, "Portal.requiresPermission", false),
        );
        // unqualified anchor hashes both, so it DOES flip
        assert_ne!(
            ts_sig_hash(TS_SRC, "requiresPermission", false),
            ts_sig_hash(&portal_changed, "requiresPermission", false),
        );
        // `::` is accepted as a separator alias for `.`
        assert_eq!(gate, ts_sig_hash(TS_SRC, "Gate::requiresPermission", false));
        // a qualifier that names the wrong scope: Absent
        assert_eq!(ts_sig_hash(TS_SRC, "Nope.requiresPermission", false), SymbolProbe::Absent);
    }

    #[test]
    fn ts_namespace_qualifies_like_a_module() {
        let src = "namespace auth {\n  export function run(x: number): boolean { return x > 0; }\n}\n\
                   namespace billing {\n  export function run(x: number): boolean { return x > 0; }\n}\n";
        let billing_changed = src.replace(
            "namespace billing {\n  export function run(x: number): boolean",
            "namespace billing {\n  export function run(x: number, strict: boolean): boolean",
        );
        let auth = ts_sig_hash(src, "auth.run", false);
        assert!(matches!(auth, SymbolProbe::Found(_)));
        assert_eq!(auth, ts_sig_hash(&billing_changed, "auth.run", false));
        assert_ne!(
            ts_sig_hash(src, "billing.run", false),
            ts_sig_hash(&billing_changed, "billing.run", false),
        );
    }

    #[test]
    fn tsx_variant_parses_jsx() {
        let tsx = "export function Banner({ title }: { title: string }) {\n\
                   return <div className=\"banner\">{title}</div>;\n}\n";
        // the tsx grammar parses it…
        assert!(matches!(ts_sig_hash(tsx, "Banner", true), SymbolProbe::Found(_)));
        // …and a body-only JSX edit doesn't change the shape
        let edited = tsx.replace("banner", "banner wide");
        assert_eq!(ts_sig_hash(tsx, "Banner", true), ts_sig_hash(&edited, "Banner", true));
        let sig_edited = tsx.replace("{ title }: { title: string }", "{ title, big }: { title: string; big: boolean }");
        assert_ne!(ts_sig_hash(tsx, "Banner", true), ts_sig_hash(&sig_edited, "Banner", true));
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
        // .js/.jsx are NOT in B1 scope: a symbol there stays Unresolvable.
        std::fs::write(dir.path().join("app.js"), "function gate() {}\n").unwrap();
        assert_eq!(
            r.resolve(&anchor(Some("app.js"), Some("gate"), None), WORKTREE),
            Resolution::Unresolvable
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
