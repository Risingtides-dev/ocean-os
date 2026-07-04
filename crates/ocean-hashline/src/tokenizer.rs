//! Line-oriented parser: turns hashline patch text into a [`Patch`].
//!
//! Faithful to oh-my-pi's `tokenizer.ts` + `parser.ts` for the in-scope verbs
//! (`SWAP`, `DEL`, `INS.PRE|POST|HEAD|TAIL`). The tree-sitter block verbs
//! (`SWAP.BLK`, `DEL.BLK`, `INS.BLK.POST`) and the file-level ops (`REM`, `MV`)
//! are recognized and **rejected** with a clear error — they are out of scope
//! for v1.

use crate::format::{
    InsertPos, Op, Patch, Section, KW_DEL, KW_INS, KW_INS_HEAD, KW_INS_POST, KW_INS_PRE,
    KW_INS_TAIL, KW_SWAP, PAYLOAD_SIGIL,
};
use std::fmt;

/// Error raised while parsing hashline patch text. Carries the 1-indexed source
/// line for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

/// A verb-header target recognized on a hunk line.
enum Target {
    Swap { start: usize, end: usize },
    Del { start: usize, end: usize },
    InsPre(usize),
    InsPost(usize),
    InsHead,
    InsTail,
}

/// In-progress hunk: a target plus its accumulating body.
struct Pending {
    target: Target,
    line: usize,
    body: Vec<String>,
    /// Blank body rows seen after content started; committed only when a later
    /// non-blank row proves they were interior, discarded on flush otherwise.
    deferred_blanks: Vec<String>,
}

/// Parse a section header line `[path#HASH]`. Returns `(path, hash)` or `None`
/// when the line is not a well-formed header. Mirrors `tryParseHeader`: the tag
/// is the trailing `#XXXX` (exactly 4 hex, uppercased); `#` is not allowed in
/// the path.
fn parse_header(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_end();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() {
        return None;
    }
    // Trailing `#XXXX` tag detection.
    let bytes = inner.as_bytes();
    if bytes.len() >= 5 {
        let hash_start = bytes.len() - 4;
        let sep = hash_start - 1;
        if bytes[sep] == b'#' {
            let tag = &inner[hash_start..];
            if tag.chars().all(|c| c.is_ascii_hexdigit()) {
                let path = &inner[..sep];
                if path.is_empty() || path.contains('#') {
                    return None;
                }
                return Some((path.to_string(), tag.to_ascii_uppercase()));
            }
        }
    }
    None
}

/// Parse a signed-free decimal line number (must be a non-zero positive
/// integer). Returns `(value, rest)`.
fn scan_number(s: &str) -> Option<(usize, &str)> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let n: usize = s[..end].parse().ok()?;
    if n == 0 {
        return None;
    }
    Some((n, &s[end..]))
}

/// Consume a range separator (`.=`, `..`, `-`, or whitespace) if present.
/// Returns the remaining slice after the separator, or `None` if no separator.
fn scan_range_sep(s: &str) -> Option<&str> {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix(".=") {
        return Some(rest);
    }
    if let Some(rest) = t.strip_prefix("..") {
        return Some(rest);
    }
    if let Some(rest) = t.strip_prefix('-') {
        return Some(rest);
    }
    // Bare whitespace separator: `SWAP 5 7:` — only if leading whitespace was
    // consumed and a digit follows.
    if t.len() < s.len() && t.starts_with(|c: char| c.is_ascii_digit()) {
        return Some(t);
    }
    None
}

/// Parse a `start[sep end]` range from the text after a verb keyword.
/// `allow_single` permits a lone number (range of one line).
fn scan_range(s: &str, allow_single: bool) -> Option<(usize, usize, &str)> {
    let (start, after_start) = scan_number(s)?;
    match scan_range_sep(after_start) {
        Some(after_sep) => {
            let (end, rest) = scan_number(after_sep)?;
            Some((start, end, rest))
        }
        None => {
            if allow_single {
                Some((start, start, after_start))
            } else {
                None
            }
        }
    }
}

/// Strip an optional trailing `:` (with surrounding whitespace). Returns the
/// text with a leading `:` consumed if present.
fn consume_optional_colon(s: &str) -> &str {
    let t = s.trim_start();
    t.strip_prefix(':').map(|r| r.trim_start()).unwrap_or(t)
}

/// Recognize a hunk-header verb on `line`. Returns `Some(Target)` when `line` is
/// a valid in-scope header, `Err` when it is a recognized-but-rejected verb, and
/// `Ok(None)` when it is not a hunk header at all.
fn try_parse_hunk(line: &str, line_num: usize) -> Result<Option<Target>, ParseError> {
    let t = line.trim();
    // Rejected block / file-level verbs first (longest-prefix wins).
    for (kw, why) in [
        ("SWAP.BLK", "`SWAP.BLK` (tree-sitter block replace) is out of scope for hashline v1; use `SWAP N.=M:` with explicit lines."),
        ("DEL.BLK", "`DEL.BLK` (tree-sitter block delete) is out of scope for hashline v1; use `DEL N.=M` with explicit lines."),
        ("INS.BLK.POST", "`INS.BLK.POST` (tree-sitter block insert) is out of scope for hashline v1; use `INS.POST N:` with an explicit line."),
        ("REM", "`REM` (whole-file delete) is out of scope for hashline v1."),
        ("MV", "`MV` (file move/rename) is out of scope for hashline v1."),
    ] {
        if t == kw || t.starts_with(&format!("{kw} ")) || t.starts_with(&format!("{kw}:")) {
            return Err(err(line_num, why));
        }
    }

    // SWAP N[.=M]:
    if let Some(rest) = t.strip_prefix(KW_SWAP) {
        if !starts_boundary(rest) {
            return Ok(None);
        }
        let (start, end, after) = scan_range(rest, true).ok_or_else(|| {
            err(
                line_num,
                "`SWAP` needs a line number, e.g. `SWAP 5:` or `SWAP 5.=7:`.",
            )
        })?;
        validate_range(start, end, line_num)?;
        // A trailing `:` is expected but tolerated-if-absent (body still follows).
        let _ = consume_optional_colon(after);
        return Ok(Some(Target::Swap { start, end }));
    }

    // DEL N[.=M]  (no colon, no body)
    if let Some(rest) = t.strip_prefix(KW_DEL) {
        if !starts_boundary(rest) {
            return Ok(None);
        }
        let (start, end, after) = scan_range(rest, true).ok_or_else(|| {
            err(
                line_num,
                "`DEL` needs a line number, e.g. `DEL 5` or `DEL 5.=7`.",
            )
        })?;
        validate_range(start, end, line_num)?;
        if !after.trim().is_empty() {
            return Err(err(
                line_num,
                "`DEL N.=M` takes no colon and no body. Use `SWAP N.=M:` to replace.",
            ));
        }
        return Ok(Some(Target::Del { start, end }));
    }

    // INS.PRE|POST|HEAD|TAIL
    if let Some(rest) = t.strip_prefix(KW_INS) {
        let rest = rest.strip_prefix('.').ok_or_else(|| {
            err(
                line_num,
                "`INS` requires a position: `INS.PRE`, `INS.POST`, `INS.HEAD`, or `INS.TAIL`.",
            )
        })?;
        if let Some(after) = rest.strip_prefix(KW_INS_PRE) {
            let (n, tail) = scan_number(after).ok_or_else(|| {
                err(
                    line_num,
                    "`INS.PRE` needs a line number, e.g. `INS.PRE 5:`.",
                )
            })?;
            let _ = consume_optional_colon(tail);
            return Ok(Some(Target::InsPre(n)));
        }
        if let Some(after) = rest.strip_prefix(KW_INS_POST) {
            let (n, tail) = scan_number(after).ok_or_else(|| {
                err(
                    line_num,
                    "`INS.POST` needs a line number, e.g. `INS.POST 5:`.",
                )
            })?;
            let _ = consume_optional_colon(tail);
            return Ok(Some(Target::InsPost(n)));
        }
        if rest.starts_with(KW_INS_HEAD) {
            return Ok(Some(Target::InsHead));
        }
        if rest.starts_with(KW_INS_TAIL) {
            return Ok(Some(Target::InsTail));
        }
        return Err(err(
            line_num,
            "unknown `INS` position; use `INS.PRE`, `INS.POST`, `INS.HEAD`, or `INS.TAIL`.",
        ));
    }

    Ok(None)
}

/// True when `rest` (the text after a verb keyword) begins a valid header
/// continuation: whitespace, a `:`, a `.`, or end-of-line.
fn starts_boundary(rest: &str) -> bool {
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || c == ':' || c == '.',
    }
}

fn validate_range(start: usize, end: usize, line_num: usize) -> Result<(), ParseError> {
    if end < start {
        return Err(err(
            line_num,
            format!("range {start}.={end} ends before it starts."),
        ));
    }
    Ok(())
}

/// True when the line begins a section header.
fn looks_like_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

/// True when the line begins a hunk-header verb.
fn looks_like_hunk(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with(KW_SWAP)
        || t.starts_with(KW_DEL)
        || t.starts_with(KW_INS)
        || t.starts_with("REM")
        || t.starts_with("MV")
}

impl Patch {
    /// Parse hashline patch text into a [`Patch`]. Rejects malformed headers,
    /// unknown verbs, out-of-scope block/file ops, empty inserts, bodies on
    /// deletes, `-` rows, overlapping ranges, and payload rows without a
    /// preceding op.
    pub fn parse(input: &str) -> Result<Patch, ParseError> {
        let mut sections: Vec<Section> = Vec::new();
        let mut cur: Option<Section> = None;
        let mut pending: Option<Pending> = None;

        // Split on '\n', dropping a trailing '\r' on each line (CRLF tolerance).
        let lines: Vec<&str> = split_lines(input);

        for (i, raw) in lines.iter().enumerate() {
            let line_num = i + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);

            // Blank line.
            if line.is_empty() {
                handle_blank(&mut pending);
                continue;
            }

            // Section header.
            if looks_like_header(line) {
                if let Some((path, hash)) = parse_header(line) {
                    flush_pending(&mut pending, &mut cur, &mut sections)?;
                    // start a fresh section; close the previous one.
                    if let Some(prev) = cur.take() {
                        validate_no_overlap(&prev)?;
                        sections.push(prev);
                    }
                    cur = Some(Section {
                        path,
                        expected_hash: hash,
                        ops: Vec::new(),
                    });
                    continue;
                }
                // Falls through: a `[` line that is not a valid header is treated
                // as a body/raw row below.
            }

            // Hunk header.
            if looks_like_hunk(line) {
                if let Some(target) = try_parse_hunk(line, line_num)? {
                    flush_pending(&mut pending, &mut cur, &mut sections)?;
                    if cur.is_none() {
                        return Err(err(
                            line_num,
                            "hunk op appears before any `[path#hash]` section header.",
                        ));
                    }
                    pending = Some(Pending {
                        target,
                        line: line_num,
                        body: Vec::new(),
                        deferred_blanks: Vec::new(),
                    });
                    continue;
                }
            }

            // Payload row (`+TEXT`) or bare body row.
            if let Some(text) = line.strip_prefix(PAYLOAD_SIGIL) {
                push_body(&mut pending, text.to_string(), line_num)?;
                continue;
            }

            // Bare row: auto-pipe into a pending body, or error.
            handle_bare(&mut pending, line, line_num)?;
        }

        flush_pending(&mut pending, &mut cur, &mut sections)?;
        if let Some(prev) = cur.take() {
            validate_no_overlap(&prev)?;
            sections.push(prev);
        }

        Ok(Patch { sections })
    }
}

fn split_lines(input: &str) -> Vec<&str> {
    if input.is_empty() {
        return vec![""];
    }
    input.split('\n').collect()
}

fn handle_blank(pending: &mut Option<Pending>) {
    if let Some(p) = pending {
        // Blanks are only meaningful for body-carrying targets and only after
        // content has started.
        if matches!(p.target, Target::Del { .. }) {
            return;
        }
        if p.body.is_empty() {
            return;
        }
        p.deferred_blanks.push(String::new());
    }
}

fn push_body(
    pending: &mut Option<Pending>,
    text: String,
    line_num: usize,
) -> Result<(), ParseError> {
    let p = pending
        .as_mut()
        .ok_or_else(|| err(line_num, "payload row `+…` has no preceding hunk header."))?;
    if matches!(p.target, Target::Del { .. }) {
        return Err(err(
            line_num,
            "`DEL` takes no body rows. Use `SWAP N.=M:` to replace.",
        ));
    }
    commit_deferred_blanks(p);
    p.body.push(text);
    Ok(())
}

fn handle_bare(
    pending: &mut Option<Pending>,
    line: &str,
    line_num: usize,
) -> Result<(), ParseError> {
    match pending.as_mut() {
        Some(p) if !matches!(p.target, Target::Del { .. }) => {
            if line.trim().is_empty() {
                handle_blank(pending);
                return Ok(());
            }
            if line.trim_start().starts_with('-') {
                return Err(err(
                    line_num,
                    "`-` rows are not valid; the range already names the lines being changed. Prefix a literal `-` line with `+`.",
                ));
            }
            // Auto-pipe: treat the bare row as body content.
            commit_deferred_blanks(p);
            p.body.push(line.to_string());
            Ok(())
        }
        Some(_) => Err(err(line_num, "`DEL` takes no body rows.")),
        None => {
            if line.trim().is_empty() {
                Ok(())
            } else {
                Err(err(
                    line_num,
                    "payload row has no preceding hunk header. Use `SWAP N.=M:`, `DEL N.=M`, or `INS.PRE|POST|HEAD|TAIL:`.",
                ))
            }
        }
    }
}

fn commit_deferred_blanks(p: &mut Pending) {
    if p.deferred_blanks.is_empty() {
        return;
    }
    let blanks = std::mem::take(&mut p.deferred_blanks);
    p.body.extend(blanks);
}

fn flush_pending(
    pending: &mut Option<Pending>,
    cur: &mut Option<Section>,
    _sections: &mut [Section],
) -> Result<(), ParseError> {
    let Some(p) = pending.take() else {
        return Ok(());
    };
    let Pending {
        target, line, body, ..
    } = p;
    let section = cur.as_mut().ok_or_else(|| {
        err(
            line,
            "hunk op appears before any `[path#hash]` section header.",
        )
    })?;

    let op = match target {
        Target::Del { start, end } => Op::Del { start, end },
        Target::Swap { start, end } => {
            if body.is_empty() {
                // A SWAP with no body degrades to a range delete (OMP semantics).
                Op::Del { start, end }
            } else {
                Op::Swap { start, end, body }
            }
        }
        Target::InsPre(l) => {
            if body.is_empty() {
                return Err(err(line, "`INS.PRE` needs at least one `+TEXT` body row."));
            }
            Op::Ins {
                pos: InsertPos::Pre(l),
                body,
            }
        }
        Target::InsPost(l) => {
            if body.is_empty() {
                return Err(err(line, "`INS.POST` needs at least one `+TEXT` body row."));
            }
            Op::Ins {
                pos: InsertPos::Post(l),
                body,
            }
        }
        Target::InsHead => {
            if body.is_empty() {
                return Err(err(line, "`INS.HEAD` needs at least one `+TEXT` body row."));
            }
            Op::Ins {
                pos: InsertPos::Head,
                body,
            }
        }
        Target::InsTail => {
            if body.is_empty() {
                return Err(err(line, "`INS.TAIL` needs at least one `+TEXT` body row."));
            }
            Op::Ins {
                pos: InsertPos::Tail,
                body,
            }
        }
    };
    section.ops.push(op);
    Ok(())
}

/// Reject two ops whose delete/replace ranges share any line.
fn validate_no_overlap(section: &Section) -> Result<(), ParseError> {
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    for op in &section.ops {
        let range = match op {
            Op::Swap { start, end, .. } | Op::Del { start, end } => Some((*start, *end)),
            Op::Ins { .. } => None,
        };
        if let Some((s, e)) = range {
            for &(cs, ce) in &claimed {
                if s <= ce && cs <= e {
                    return Err(err(
                        0,
                        format!(
                            "overlapping ranges: {s}.={e} collides with {cs}.={ce}. Issue one hunk per range."
                        ),
                    ));
                }
            }
            claimed.push((s, e));
        }
    }
    Ok(())
}
