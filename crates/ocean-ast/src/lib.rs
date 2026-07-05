//! Read-time structural code summarization powered by tree-sitter.
//!
//! [`summarize_code`] folds function/method/struct/enum bodies (and long
//! comment / import runs) into one-line elided placeholders while keeping the
//! surrounding signatures and imports verbatim, so reading a large file costs a
//! fraction of the tokens. It builds an *elidable-span forest* per language and
//! runs a **BFS unfold**: every root span starts folded and outer→inner spans
//! are progressively revealed until a visible-line budget
//! ([`SummaryOptions::unfold_until_lines`]) is met, skipping any single unfold
//! that would blow past [`SummaryOptions::unfold_limit_lines`] so one huge
//! function can't starve its siblings.
//!
//! Everything is total and panic-free: a parse failure, a tree with syntax
//! errors, or an empty source returns the input unsummarized (a single kept
//! segment). Rendering is stable (same input → same output) and lossless — the
//! text of every kept segment is byte-identical to the corresponding source
//! lines.
//!
//! ## Attribution
//!
//! The summarization mechanism (elidable forest, BFS unfold, per-language
//! node-kind tables) is ported from oh-my-pi's `crates/pi-ast/src/summary.rs`
//! by can1357 (<https://github.com/can1357/oh-my-pi>), MIT licensed. The public
//! API here is reshaped for Ocean OS and the grammar set is trimmed to the
//! languages Ocean ingests.

mod lang;
mod summary;

pub use lang::Lang;
pub use summary::{summarize_code, SegmentKind, Summary, SummaryOptions, SummarySegment};
