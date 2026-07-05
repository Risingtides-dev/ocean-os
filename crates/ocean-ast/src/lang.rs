//! Supported languages and their per-grammar elidable node-kind tables.
//!
//! The node-kind strings are ported from oh-my-pi's `pi-ast`, trimmed to the
//! grammars Ocean bundles. Each language contributes three predicates used by
//! the collector:
//!
//! * [`Lang::is_elidable_kind`] — bodies/blocks/containers whose interior folds
//!   (keeping the opening/closing signature lines).
//! * [`Lang::is_comment_kind`] — multi-line block comments that fold once they
//!   exceed the comment threshold.
//! * [`Lang::is_groupable_kind`] — consecutive sibling statements (imports/uses)
//!   whose middle collapses, leaving the boundary statements visible.

use tree_sitter::Language;

/// A language Ocean can structurally summarize. Unknown / unsupported inputs are
/// represented by [`Lang::from_extension`] returning `None`; callers then treat
/// the source as unsummarizable and keep it verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Bash,
    Toml,
    Json,
}

impl Lang {
    /// Resolve a language from a bare file extension (no leading dot), case
    /// insensitively. Returns `None` for anything outside the bundled grammar
    /// set — the signal to keep the source unsummarized.
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "sh" | "bash" | "zsh" => Self::Bash,
            "toml" => Self::Toml,
            "json" => Self::Json,
            _ => return None,
        })
    }

    /// Canonical lowercase name (used in diagnostics / tests).
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Bash => "bash",
            Self::Toml => "toml",
            Self::Json => "json",
        }
    }

    /// The tree-sitter grammar for this language.
    pub(crate) fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }

    /// AST node kinds whose *interior* lines are elidable while the boundary
    /// lines (signature + closing token) stay visible.
    ///
    /// TOML is intentionally empty: its tables have no closing-token anchor, so
    /// eliding a table body would delete the only content worth reading. TOML
    /// therefore parses but passes through unchanged (matching upstream).
    pub(crate) fn is_elidable_kind(self, kind: &str) -> bool {
        match self {
            Self::TypeScript | Self::Tsx | Self::JavaScript => matches!(
                kind,
                "statement_block"
                    | "function_body"
                    | "object"
                    | "array"
                    | "template_string"
                    | "class_body"
                    | "interface_body"
                    | "enum_body"
                    | "object_type"
                    | "switch_body"
            ),
            Self::Rust => matches!(
                kind,
                "block"
                    | "array_expression"
                    | "tuple_expression"
                    | "struct_expression"
                    | "match_block"
                    | "raw_string_literal"
                    | "declaration_list"
                    | "field_declaration_list"
                    | "ordered_field_declaration_list"
                    | "enum_variant_list"
                    | "where_clause"
                    | "use_list"
                    | "token_tree"
            ),
            Self::Python => matches!(
                kind,
                "block"
                    | "dictionary"
                    | "list"
                    | "set"
                    | "string"
                    | "tuple"
                    | "list_comprehension"
                    | "set_comprehension"
                    | "dictionary_comprehension"
                    | "generator_expression"
            ),
            Self::Go => matches!(
                kind,
                "block"
                    | "composite_literal"
                    | "raw_string_literal"
                    | "import_spec_list"
                    | "const_declaration"
                    | "var_declaration"
                    | "field_declaration_list"
                    | "interface_type"
                    | "expression_switch_statement"
                    | "type_switch_statement"
                    | "select_statement"
            ),
            Self::Bash => matches!(
                kind,
                "compound_statement"
                    | "if_statement"
                    | "case_statement"
                    | "do_group"
                    | "subshell"
                    | "array"
                    | "heredoc_body"
            ),
            Self::Json => matches!(kind, "object" | "array"),
            // See doc comment: TOML tables have no closing-token anchor.
            Self::Toml => false,
        }
    }

    /// Multi-line block comment kinds. Line-comment-only grammars (Bash, TOML,
    /// JSON) return `false` — there is no multi-line block comment to fold.
    pub(crate) fn is_comment_kind(self, kind: &str) -> bool {
        match self {
            Self::TypeScript | Self::Tsx | Self::JavaScript => kind == "comment",
            Self::Rust => kind == "block_comment",
            Self::Python => kind == "comment",
            Self::Go => kind == "comment",
            Self::Bash | Self::Toml | Self::Json => false,
        }
    }

    /// Sibling statement kinds that form collapsible runs (imports/uses). A run
    /// of two or more spanning at least the body threshold folds its middle,
    /// keeping the first and last statement visible.
    pub(crate) fn is_groupable_kind(self, kind: &str) -> bool {
        match self {
            Self::TypeScript | Self::Tsx | Self::JavaScript => kind == "import_statement",
            Self::Rust => matches!(kind, "use_declaration" | "extern_crate_declaration"),
            Self::Python => matches!(
                kind,
                "import_statement" | "import_from_statement" | "future_import_statement"
            ),
            Self::Go => kind == "import_declaration",
            Self::Bash | Self::Toml | Self::Json => false,
        }
    }

    /// A short human word for a *parent* node kind whose body we are eliding,
    /// used to build the `(<kind> <name>)` placeholder label. Returns `None`
    /// when the parent has no cheap, meaningful label.
    pub(crate) fn parent_word(kind: &str) -> Option<&'static str> {
        Some(match kind {
            "function_item" => "fn",
            "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "method_definition"
            | "method_declaration"
            | "arrow_function" => "fn",
            "function_definition" => "def",
            "struct_item" | "struct_type" => "struct",
            "enum_item" | "enum_declaration" | "enum_specifier" => "enum",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "class_declaration" | "class_definition" => "class",
            "interface_declaration" => "interface",
            "type_declaration" | "type_spec" => "type",
            "type_alias_declaration" => "type",
            _ => return None,
        })
    }
}
