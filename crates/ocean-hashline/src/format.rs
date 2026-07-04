//! Hashline patch data types and their wire-format display.
//!
//! A patch is one or more file **sections**. Each section names a path plus the
//! content-hash tag the edit is bound to, followed by verb ops whose line
//! numbers are 1-indexed positions into the (LF, un-stripped) file.
//!
//! Wire format (faithful to oh-my-pi `packages/hashline/src/format.ts`):
//!
//! ```text
//! [path/to/file.rs#1A2B]
//! SWAP 5.=7:
//! +replacement line one
//! +replacement line two
//! DEL 10
//! INS.POST 12:
//! +appended line
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Wire sigils / separators (single source of truth) ──────────────────────

/// File-section header opening delimiter.
pub const FILE_PREFIX: char = '[';
/// File-section header closing delimiter.
pub const FILE_SUFFIX: char = ']';
/// Separator between a hashline file path and its snapshot tag.
pub const FILE_HASH_SEP: char = '#';
/// Payload sigil for a literal body row.
pub const PAYLOAD_SIGIL: char = '+';
/// Separator between two line numbers in a range, e.g. `5.=10`.
pub const RANGE_SEP: &str = ".=";
/// Separator between a header verb/anchor and an optional body.
pub const HEADER_COLON: char = ':';

/// Verb keywords.
pub const KW_SWAP: &str = "SWAP";
pub const KW_DEL: &str = "DEL";
pub const KW_INS: &str = "INS";
pub const KW_INS_PRE: &str = "PRE";
pub const KW_INS_POST: &str = "POST";
pub const KW_INS_HEAD: &str = "HEAD";
pub const KW_INS_TAIL: &str = "TAIL";

// ── Types ──────────────────────────────────────────────────────────────────

/// Where an insert op lands relative to existing content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsertPos {
    /// Insert before a concrete 1-indexed line (`INS.PRE N:`).
    Pre(usize),
    /// Insert after a concrete 1-indexed line (`INS.POST N:`).
    Post(usize),
    /// Insert at the start of the file (`INS.HEAD:`).
    Head,
    /// Insert at the end of the file (`INS.TAIL:`).
    Tail,
}

/// A single verb op within a section. Line numbers are 1-indexed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Replace lines `start..=end` inclusive with `body`. `SWAP N.=M:`.
    Swap {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    /// Delete lines `start..=end` inclusive. `DEL N` / `DEL N.=M`.
    Del { start: usize, end: usize },
    /// Insert `body` at `pos`. `INS.PRE|POST|HEAD|TAIL:`.
    Ins { pos: InsertPos, body: Vec<String> },
}

impl Op {
    /// The 1-indexed anchor lines this op references (used for stale-mismatch
    /// context rendering and recovery remapping). `Head`/`Tail` inserts have no
    /// concrete anchor.
    pub fn anchor_lines(&self) -> Vec<usize> {
        match self {
            Op::Swap { start, end, .. } | Op::Del { start, end } => (*start..=*end).collect(),
            Op::Ins { pos, .. } => match pos {
                InsertPos::Pre(l) | InsertPos::Post(l) => vec![*l],
                InsertPos::Head | InsertPos::Tail => vec![],
            },
        }
    }
}

/// One file section: a path, the content-hash tag the edit is bound to, and the
/// ordered ops to apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub path: String,
    pub expected_hash: String,
    pub ops: Vec<Op>,
}

/// A parsed patch: one or more file sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub sections: Vec<Section>,
}

// ── Display (round-trips through the parser) ────────────────────────────────

/// Format a section header line, e.g. `[src/foo.rs#1A2B]`.
pub fn format_header(path: &str, file_hash: &str) -> String {
    format!("{FILE_PREFIX}{path}{FILE_HASH_SEP}{file_hash}{FILE_SUFFIX}")
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::Swap { start, end, body } => {
                if start == end {
                    writeln!(f, "{KW_SWAP} {start}{HEADER_COLON}")?;
                } else {
                    writeln!(f, "{KW_SWAP} {start}{RANGE_SEP}{end}{HEADER_COLON}")?;
                }
                for line in body {
                    writeln!(f, "{PAYLOAD_SIGIL}{line}")?;
                }
                Ok(())
            }
            Op::Del { start, end } => {
                if start == end {
                    writeln!(f, "{KW_DEL} {start}")
                } else {
                    writeln!(f, "{KW_DEL} {start}{RANGE_SEP}{end}")
                }
            }
            Op::Ins { pos, body } => {
                match pos {
                    InsertPos::Pre(l) => writeln!(f, "{KW_INS}.{KW_INS_PRE} {l}{HEADER_COLON}")?,
                    InsertPos::Post(l) => writeln!(f, "{KW_INS}.{KW_INS_POST} {l}{HEADER_COLON}")?,
                    InsertPos::Head => writeln!(f, "{KW_INS}.{KW_INS_HEAD}{HEADER_COLON}")?,
                    InsertPos::Tail => writeln!(f, "{KW_INS}.{KW_INS_TAIL}{HEADER_COLON}")?,
                }
                for line in body {
                    writeln!(f, "{PAYLOAD_SIGIL}{line}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", format_header(&self.path, &self.expected_hash))?;
        for op in &self.ops {
            write!(f, "{op}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Patch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for section in &self.sections {
            write!(f, "{section}")?;
        }
        Ok(())
    }
}
