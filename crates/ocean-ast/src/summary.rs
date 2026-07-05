//! The summarization mechanism: elidable-span forest + BFS unfold + segment
//! assembly. Ported from oh-my-pi's `pi-ast` (MIT); see the crate-level docs.

use std::collections::{HashSet, VecDeque};

use tree_sitter::{Node, Parser};

use crate::lang::Lang;

/// Minimum lines a block comment must span before its interior folds.
const DEFAULT_MIN_COMMENT_LINES: usize = 6;

/// Tuning knobs for [`summarize_code`].
#[derive(Debug, Clone)]
pub struct SummaryOptions {
    /// Target visible-line count for the BFS unfold. Starting from every
    /// elidable span folded, outer→inner spans are progressively revealed until
    /// the visible-line count reaches this target. `0` disables BFS and keeps
    /// only the outermost elisions (every nested span stays hidden behind its
    /// parent).
    pub unfold_until_lines: usize,
    /// Hard ceiling for the BFS unfold. A candidate unfold whose revealed lines
    /// would push the visible count past this value is skipped, leaving that
    /// span folded while the BFS keeps exploring queued siblings — so one huge
    /// body can't starve the rest. Defaults to `2 × unfold_until_lines`.
    pub unfold_limit_lines: Option<usize>,
    /// Minimum total node lines before a body/container is eligible to fold.
    pub min_body_lines: usize,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            unfold_until_lines: 120,
            unfold_limit_lines: None,
            min_body_lines: 4,
        }
    }
}

/// Whether a [`SummarySegment`] is kept verbatim or collapsed to a placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Source lines preserved byte-for-byte.
    Kept,
    /// A folded span, represented by a one-line placeholder.
    Elided,
}

/// A contiguous run of source classified as kept or elided. Line numbers are
/// 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySegment {
    pub kind: SegmentKind,
    pub start_line: usize,
    pub end_line: usize,
    /// For [`SegmentKind::Kept`], the verbatim source lines joined by `\n`. For
    /// [`SegmentKind::Elided`], the one-line placeholder.
    pub text: String,
}

/// Result of [`summarize_code`].
#[derive(Debug, Clone)]
pub struct Summary {
    pub segments: Vec<SummarySegment>,
    /// Total lines in the source.
    pub total_lines: usize,
    /// Lines that survive folding (the sum of every kept segment's line count).
    pub visible_lines: usize,
}

impl Summary {
    /// Render the folded text: kept segments verbatim, elided segments as their
    /// placeholder line, joined by `\n`. Deterministic for a given input.
    pub fn render(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 1-based inclusive line span plus an optional placeholder label.
#[derive(Debug, Clone)]
struct FoldSpan {
    start: usize,
    end: usize,
    label: Option<String>,
}

impl FoldSpan {
    fn lines(&self) -> usize {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

/// One elidable region plus its directly-nested elidable descendants.
#[derive(Debug)]
struct SpanNode {
    start: usize,
    end: usize,
    label: Option<String>,
    children: Vec<usize>,
}

impl SpanNode {
    fn lines(&self) -> usize {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

/// Flat arena of elidable spans organized as a forest.
#[derive(Debug, Default)]
struct ElidableForest {
    nodes: Vec<SpanNode>,
    roots: Vec<usize>,
}

impl ElidableForest {
    fn push(
        &mut self,
        parent: Option<usize>,
        start: usize,
        end: usize,
        label: Option<String>,
    ) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(SpanNode {
            start,
            end,
            label,
            children: Vec::new(),
        });
        match parent {
            Some(p) => self.nodes[p].children.push(idx),
            None => self.roots.push(idx),
        }
        idx
    }
}

/// Summarize `source` for `lang`, folding elidable bodies per `opts`.
///
/// Total and panic-free: an empty source, a parse failure, or a tree with
/// syntax errors returns the input unsummarized (a single kept segment).
pub fn summarize_code(source: &str, lang: Lang, opts: &SummaryOptions) -> Summary {
    let lines = split_lines(source);
    let total_lines = lines.len();
    if total_lines == 0 {
        return Summary {
            segments: Vec::new(),
            total_lines: 0,
            visible_lines: 0,
        };
    }

    let Some(tree) = parse(source, lang) else {
        return passthrough(&lines, total_lines);
    };
    let root = tree.root_node();
    if root.has_error() {
        return passthrough(&lines, total_lines);
    }

    let min_body = opts.min_body_lines.max(2);
    let min_comment = DEFAULT_MIN_COMMENT_LINES.max(4);
    let until = opts.unfold_until_lines;
    let limit = opts
        .unfold_limit_lines
        .unwrap_or_else(|| until.saturating_mul(2));

    let mut forest = ElidableForest::default();
    collect_elidable_tree(
        root,
        None,
        lang,
        min_body,
        min_comment,
        source.as_bytes(),
        &mut forest,
    );

    let folded = select_folded_spans(&forest, total_lines, until, limit);
    let spans = folded
        .into_iter()
        .map(|i| FoldSpan {
            start: forest.nodes[i].start,
            end: forest.nodes[i].end,
            label: forest.nodes[i].label.clone(),
        })
        .collect();
    let spans = normalize_spans(spans, total_lines);
    let segments = build_segments(&lines, total_lines, &spans);

    let elided_lines: usize = segments
        .iter()
        .filter(|s| s.kind == SegmentKind::Elided)
        .map(|s| s.end_line.saturating_sub(s.start_line).saturating_add(1))
        .sum();
    let visible_lines = total_lines.saturating_sub(elided_lines);

    Summary {
        segments,
        total_lines,
        visible_lines,
    }
}

fn parse(source: &str, lang: Lang) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    parser.parse(source, None)
}

/// The whole source as one kept segment (unsummarized fallback).
fn passthrough(lines: &[&str], total_lines: usize) -> Summary {
    let text = lines.join("\n");
    Summary {
        segments: vec![SummarySegment {
            kind: SegmentKind::Kept,
            start_line: 1,
            end_line: total_lines,
            text,
        }],
        total_lines,
        visible_lines: total_lines,
    }
}

/// Split on `\n`, dropping the trailing empty segment produced by a final
/// newline so the count matches `str::lines()`. `\r` is preserved so a rejoin
/// of kept lines is byte-identical to CRLF source.
fn split_lines(source: &str) -> Vec<&str> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = source.split('\n').collect();
    if source.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn collect_elidable_tree(
    node: Node<'_>,
    elidable_parent: Option<usize>,
    lang: Lang,
    min_body_lines: usize,
    min_comment_lines: usize,
    src: &[u8],
    forest: &mut ElidableForest,
) {
    let total_lines = node_line_count(node);
    if lang.is_comment_kind(node.kind()) {
        if total_lines >= min_comment_lines {
            let start_line = node_start_line(node) + 2;
            let end_line = node_end_line(node).saturating_sub(1);
            if start_line <= end_line {
                forest.push(
                    elidable_parent,
                    start_line,
                    end_line,
                    Some("comment".to_string()),
                );
            }
        }
        return;
    }

    let mut current_parent = elidable_parent;
    if lang.is_elidable_kind(node.kind()) && total_lines >= min_body_lines {
        let start_line = node_start_line(node) + 1;
        let end_line = node_end_line(node).saturating_sub(1);
        if start_line <= end_line {
            // Recurse into the elided node so nested elisions are recorded as
            // children; the BFS unfold pass decides which level actually fires.
            let label = describe_body(node, src);
            current_parent = Some(forest.push(elidable_parent, start_line, end_line, label));
        }
    }

    // Detect consecutive runs of groupable siblings (import statements). When a
    // run's total line span meets `min_body_lines`, elide the lines strictly
    // between the first and last sibling, leaving the boundaries visible.
    let child_count = node.child_count() as u32;
    let mut run_first: Option<Node<'_>> = None;
    let mut run_last: Option<Node<'_>> = None;
    let mut run_count: usize = 0;
    for index in 0..child_count {
        let Some(child) = node.child(index) else {
            continue;
        };
        if lang.is_groupable_kind(child.kind()) {
            if run_first.is_none() {
                run_first = Some(child);
            }
            run_last = Some(child);
            run_count += 1;
        } else {
            flush_groupable_run(
                run_first,
                run_last,
                run_count,
                min_body_lines,
                forest,
                current_parent,
            );
            run_first = None;
            run_last = None;
            run_count = 0;
        }
    }
    flush_groupable_run(
        run_first,
        run_last,
        run_count,
        min_body_lines,
        forest,
        current_parent,
    );

    for index in 0..child_count {
        if let Some(child) = node.child(index) {
            collect_elidable_tree(
                child,
                current_parent,
                lang,
                min_body_lines,
                min_comment_lines,
                src,
                forest,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_groupable_run(
    first: Option<Node<'_>>,
    last: Option<Node<'_>>,
    count: usize,
    min_body_lines: usize,
    forest: &mut ElidableForest,
    parent: Option<usize>,
) {
    if count < 2 {
        return;
    }
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    let first_start = node_start_line(first);
    let last_start = node_start_line(last);
    let last_end = node_end_line(last);
    let span_lines = last_end.saturating_sub(first_start).saturating_add(1);
    if span_lines < min_body_lines {
        return;
    }
    // Use the first node's last content line as the lower bound (some grammars
    // include a trailing newline in the node range, which would otherwise place
    // `end_line` on the next sibling's first line).
    let first_content_end = node_content_end_line(first).min(last_start.saturating_sub(1));
    let start = first_content_end.saturating_add(1);
    let end = last_start.saturating_sub(1);
    if start <= end {
        forest.push(parent, start, end, Some("imports".to_string()));
    }
}

/// Best-effort `(<kind> <name>)` label for an elided body, derived from the
/// body node's parent. `None` when no cheap label is available (then the
/// placeholder shows only the line count).
fn describe_body(node: Node<'_>, src: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let word = Lang::parent_word(parent.kind())?;
    match parent
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => Some(format!("{word} {name}")),
        None => Some(word.to_string()),
    }
}

fn node_start_line(node: Node<'_>) -> usize {
    node.start_position().row.saturating_add(1)
}

fn node_end_line(node: Node<'_>) -> usize {
    node.end_position().row.saturating_add(1)
}

/// Last source line containing a content byte from `node`. Tree-sitter reports
/// `end_position` one past the last byte; when that byte is a newline the naive
/// `row + 1` overshoots by one, which this corrects.
fn node_content_end_line(node: Node<'_>) -> usize {
    let pos = node.end_position();
    let row = if pos.column == 0 && pos.row > 0 {
        pos.row - 1
    } else {
        pos.row
    };
    row.saturating_add(1)
}

fn node_line_count(node: Node<'_>) -> usize {
    node_end_line(node)
        .saturating_sub(node_start_line(node))
        .saturating_add(1)
}

/// BFS unfold. Start with every root span folded and progressively replace
/// folded spans with their elidable children, breadth-first, until the visible
/// line count reaches `unfold_until`. A candidate whose revealed lines would
/// push the visible count past `unfold_limit` is skipped (stays folded, subtree
/// unexplored) while the BFS keeps unfolding queued siblings. `unfold_until == 0`
/// short-circuits to the outermost-only behavior. Returns folded node indices.
fn select_folded_spans(
    forest: &ElidableForest,
    total_lines: usize,
    unfold_until: usize,
    unfold_limit: usize,
) -> Vec<usize> {
    let nodes = &forest.nodes;
    let mut folded: HashSet<usize> = forest.roots.iter().copied().collect();
    if unfold_until == 0 || folded.is_empty() {
        return folded.into_iter().collect();
    }

    let folded_line_total: usize = folded.iter().map(|&i| nodes[i].lines()).sum();
    let mut visible = total_lines.saturating_sub(folded_line_total);
    let mut queue: VecDeque<usize> = forest.roots.iter().copied().collect();

    while let Some(idx) = queue.pop_front() {
        if visible >= unfold_until {
            break;
        }
        if !folded.contains(&idx) {
            continue;
        }
        let node = &nodes[idx];
        let child_line_total: usize = node.children.iter().map(|&c| nodes[c].lines()).sum();
        let revealed = node.lines().saturating_sub(child_line_total);
        let new_visible = visible.saturating_add(revealed);
        if new_visible > unfold_limit {
            continue;
        }
        folded.remove(&idx);
        for &c in &node.children {
            folded.insert(c);
            queue.push_back(c);
        }
        visible = new_visible;
    }

    folded.into_iter().collect()
}

/// Clamp, sort, and merge overlapping/adjacent spans. Merged spans lose their
/// label (the combined region has no single name) and show only a line count.
fn normalize_spans(mut spans: Vec<FoldSpan>, total_lines: usize) -> Vec<FoldSpan> {
    if total_lines == 0 {
        return Vec::new();
    }
    spans.retain(|span| span.start <= span.end && span.start <= total_lines);
    for span in &mut spans {
        span.end = span.end.min(total_lines);
    }
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<FoldSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut() {
            if span.start <= last.end.saturating_add(1) {
                if span.end > last.end {
                    last.end = span.end;
                }
                last.label = None;
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

fn build_segments(lines: &[&str], total_lines: usize, spans: &[FoldSpan]) -> Vec<SummarySegment> {
    if total_lines == 0 {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut cursor = 1usize; // next 1-based line awaiting emission
    for span in spans {
        if span.start > cursor {
            push_kept(&mut segments, lines, cursor, span.start - 1);
        }
        let n = span.lines();
        segments.push(SummarySegment {
            kind: SegmentKind::Elided,
            start_line: span.start,
            end_line: span.end,
            text: placeholder(n, span.label.as_deref()),
        });
        cursor = span.end + 1;
    }
    if cursor <= total_lines {
        push_kept(&mut segments, lines, cursor, total_lines);
    }
    segments
}

fn push_kept(segments: &mut Vec<SummarySegment>, lines: &[&str], start: usize, end: usize) {
    let text = lines[start - 1..end].join("\n");
    segments.push(SummarySegment {
        kind: SegmentKind::Kept,
        start_line: start,
        end_line: end,
        text,
    });
}

fn placeholder(n: usize, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("… {n} lines elided ({label}) …"),
        None => format!("… {n} lines elided …"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy outermost-only fold (BFS off) — deterministic single fold per body.
    fn fold_opts() -> SummaryOptions {
        SummaryOptions {
            unfold_until_lines: 0,
            unfold_limit_lines: None,
            min_body_lines: 4,
        }
    }

    fn kinds(summary: &Summary) -> Vec<SegmentKind> {
        summary.segments.iter().map(|s| s.kind).collect()
    }

    fn elided_text(summary: &Summary) -> String {
        summary
            .segments
            .iter()
            .find(|s| s.kind == SegmentKind::Elided)
            .map(|s| s.text.clone())
            .unwrap_or_default()
    }

    // ---- per-language body folding -------------------------------------

    #[test]
    fn rust_folds_fn_body_keeps_signature() {
        let src = "fn greet(name: &str) -> String {\n    let clean = name.trim();\n    let label = if clean.is_empty() { \"world\" } else { clean };\n    format!(\"hello {label}\")\n}\n";
        let s = summarize_code(src, Lang::Rust, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert_eq!(s.segments[0].text, "fn greet(name: &str) -> String {");
        assert_eq!(s.segments[2].text, "}");
        assert!(
            elided_text(&s).contains("greet"),
            "label should name the fn: {}",
            elided_text(&s)
        );
    }

    #[test]
    fn rust_method_keeps_impl_boundaries() {
        let src = "struct Greeter;\n\nimpl Greeter {\n    fn greet(&self) -> String {\n        let name = \"world\";\n        let label = name.to_uppercase();\n        format!(\"hello {label}\")\n    }\n}\n";
        let s = summarize_code(src, Lang::Rust, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert!(s.segments[0].text.contains("impl Greeter {"));
        assert_eq!(s.segments[2].text, "}");
    }

    #[test]
    fn typescript_folds_function_body() {
        let src = "export function greet(name: string): string {\n\tconst clean = name.trim();\n\tconst label = clean || 'world';\n\treturn `hello ${label}`;\n}\n";
        let s = summarize_code(src, Lang::TypeScript, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert_eq!(
            s.segments[0].text,
            "export function greet(name: string): string {"
        );
        assert_eq!(s.segments[1].start_line, 2);
        assert_eq!(s.segments[1].end_line, 4);
        assert_eq!(s.segments[2].text, "}");
    }

    #[test]
    fn typescript_class_body() {
        let src = "export class Greeter {\n\tname: string = \"world\";\n\tlength(): number { return this.name.length; }\n\tgreet(): string { return this.name; }\n\tshout(): string { return this.name.toUpperCase(); }\n}\n";
        let s = summarize_code(src, Lang::TypeScript, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert!(s.segments[0].text.contains("class Greeter"));
        assert_eq!(s.segments[2].text, "}");
    }

    #[test]
    fn tsx_folds_function_body() {
        let src = "export function View(): JSX.Element {\n\tconst a = 1;\n\tconst b = 2;\n\tconst c = 3;\n\treturn <div>{a + b + c}</div>;\n}\n";
        let s = summarize_code(src, Lang::Tsx, &fold_opts());
        assert!(s.segments.iter().any(|seg| seg.kind == SegmentKind::Elided));
        assert!(s.segments[0].text.contains("function View"));
    }

    #[test]
    fn javascript_folds_function_body() {
        let src = "export function greet(name) {\n\tconst clean = name.trim();\n\tconst label = clean || 'world';\n\treturn `hello ${label}`;\n}\n";
        let s = summarize_code(src, Lang::JavaScript, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert!(s.segments[0].text.contains("function greet"));
    }

    #[test]
    fn python_folds_function_body() {
        let src = "class Greeter:\n    def greet(self, name: str) -> str:\n        clean = name.strip()\n        label = clean or 'world'\n        return f'hello {label}'\n";
        let s = summarize_code(src, Lang::Python, &fold_opts());
        assert!(s.segments.iter().any(|seg| seg.kind == SegmentKind::Elided));
        assert!(s.segments[0].text.contains("def greet"));
        assert!(s.segments.last().unwrap().text.contains("return"));
    }

    #[test]
    fn go_folds_function_body() {
        let src = "func greet(name string) string {\n\tclean := strings.TrimSpace(name)\n\tlabel := clean\n\tif label == \"\" {\n\t\tlabel = \"world\"\n\t}\n\treturn \"hello \" + label\n}\n";
        let s = summarize_code(src, Lang::Go, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert!(s.segments[0].text.contains("func greet"));
        assert_eq!(s.segments[2].text, "}");
        assert!(elided_text(&s).contains("greet"));
    }

    #[test]
    fn bash_folds_function_body() {
        let src = "greet() {\n\tlocal name=\"$1\"\n\tlocal label=\"${name:-world}\"\n\techo \"hello $label\"\n\treturn 0\n}\n";
        let s = summarize_code(src, Lang::Bash, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert!(s.segments[0].text.contains("greet()"));
        assert_eq!(s.segments[2].text, "}");
    }

    #[test]
    fn json_folds_object_body() {
        let body = (0..30)
            .map(|i| format!("\t\"key{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let src = format!("{{\n{body}\n}}\n");
        let s = summarize_code(&src, Lang::Json, &fold_opts());
        assert_eq!(
            kinds(&s),
            vec![SegmentKind::Kept, SegmentKind::Elided, SegmentKind::Kept]
        );
        assert_eq!(s.segments[0].text, "{");
        assert_eq!(s.segments[2].text, "}");
    }

    #[test]
    fn toml_passthrough_no_elision() {
        // TOML has no closing-token anchor, so it parses but never folds.
        let src = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[deps]\na = 1\nb = 2\nc = 3\n";
        let s = summarize_code(src, Lang::Toml, &SummaryOptions::default());
        assert!(s.segments.iter().all(|seg| seg.kind == SegmentKind::Kept));
        assert_eq!(s.visible_lines, s.total_lines);
    }

    // ---- import-run folding --------------------------------------------

    #[test]
    fn rust_folds_use_run() {
        let src = "use std::fs;\nuse std::path::Path;\nuse std::collections::HashMap;\nuse std::sync::Arc;\nuse std::io;\n\nfn main() {}\n";
        let s = summarize_code(src, Lang::Rust, &fold_opts());
        let elided = s
            .segments
            .iter()
            .find(|seg| seg.kind == SegmentKind::Elided)
            .expect("elided");
        assert_eq!(elided.start_line, 2);
        assert_eq!(elided.end_line, 4);
        assert!(elided.text.contains("imports"));
    }

    #[test]
    fn typescript_folds_import_run() {
        let src = "import a from \"a\";\nimport b from \"b\";\nimport c from \"c\";\nimport d from \"d\";\nimport e from \"e\";\nimport f from \"f\";\n\nexport function main() {}\n";
        let s = summarize_code(src, Lang::TypeScript, &fold_opts());
        let elided = s
            .segments
            .iter()
            .find(|seg| seg.kind == SegmentKind::Elided)
            .expect("elided");
        assert_eq!(elided.start_line, 2);
        assert_eq!(elided.end_line, 5);
        assert!(s.segments[0].text.starts_with("import a from"));
    }

    // ---- thresholds & fallbacks ----------------------------------------

    #[test]
    fn min_body_lines_respected() {
        let src = "function small() {\n\treturn 1;\n}\n";
        let default = summarize_code(src, Lang::TypeScript, &fold_opts());
        assert!(default
            .segments
            .iter()
            .all(|seg| seg.kind == SegmentKind::Kept));

        let lowered = SummaryOptions {
            unfold_until_lines: 0,
            unfold_limit_lines: None,
            min_body_lines: 3,
        };
        let folded = summarize_code(src, Lang::TypeScript, &lowered);
        assert!(folded
            .segments
            .iter()
            .any(|seg| seg.kind == SegmentKind::Elided));
    }

    #[test]
    fn parse_failure_falls_back_to_kept() {
        let src = "export function broken( {\n";
        let s = summarize_code(src, Lang::TypeScript, &fold_opts());
        assert_eq!(s.segments.len(), 1);
        assert_eq!(s.segments[0].kind, SegmentKind::Kept);
        assert_eq!(s.visible_lines, s.total_lines);
    }

    #[test]
    fn unknown_extension_is_none() {
        assert_eq!(Lang::from_extension("txt"), None);
        assert_eq!(Lang::from_extension("md"), None);
        assert_eq!(Lang::from_extension("rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_extension(".TS"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_extension("tsx"), Some(Lang::Tsx));
    }

    #[test]
    fn empty_source_yields_empty_summary() {
        let s = summarize_code("", Lang::Rust, &SummaryOptions::default());
        assert!(s.segments.is_empty());
        assert_eq!(s.total_lines, 0);
        assert_eq!(s.visible_lines, 0);
        assert_eq!(s.render(), "");
    }

    // ---- BFS unfold ----------------------------------------------------

    #[test]
    fn bfs_unfolds_outer_before_inner() {
        // Root object unfolds so top-level keys stay visible; the nested `deps`
        // object stays folded — outer-before-inner.
        let body = (0..30)
            .map(|i| format!("\t\"key{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let nested =
            "\t\"deps\": {\n\t\t\"a\": 1,\n\t\t\"b\": 2,\n\t\t\"c\": 3,\n\t\t\"d\": 4\n\t}";
        let src = format!("{{\n{body},\n{nested}\n}}\n");
        let opts = SummaryOptions {
            unfold_until_lines: 20,
            unfold_limit_lines: Some(100),
            min_body_lines: 4,
        };
        let s = summarize_code(&src, Lang::Json, &opts);
        assert!(
            s.segments.iter().any(|seg| seg.kind == SegmentKind::Elided),
            "deps stays folded"
        );
        let kept: String = s
            .segments
            .iter()
            .filter(|seg| seg.kind == SegmentKind::Kept)
            .map(|seg| seg.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(kept.contains("\"key0\""));
        assert!(kept.contains("\"key29\""));
        assert!(kept.contains("\"deps\""));
        assert!(!kept.contains("\"a\": 1"), "nested body must stay folded");
    }

    #[test]
    fn bfs_keeps_all_bodies_folded_when_target_already_met() {
        // 10 small functions: initial visible count already exceeds a low target,
        // so no body unfolds — 10 elided segments remain.
        let src = (0..10)
            .map(|i| format!("export function fn{i}(): number {{\n\tconst a = {i};\n\tconst b = {i};\n\tconst c = {i};\n\treturn a + b + c;\n}}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let opts = SummaryOptions {
            unfold_until_lines: 10,
            unfold_limit_lines: Some(100),
            min_body_lines: 4,
        };
        let s = summarize_code(&src, Lang::TypeScript, &opts);
        let elided = s
            .segments
            .iter()
            .filter(|seg| seg.kind == SegmentKind::Elided)
            .count();
        assert_eq!(elided, 10);
    }

    #[test]
    fn bfs_reverts_when_unfold_overflows_limit() {
        // One huge body whose unfold would overshoot the limit stays folded.
        let body = (0..40)
            .map(|i| format!("\tconst x{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("export function big(): void {{\n{body}\n}}\n");
        let opts = SummaryOptions {
            unfold_until_lines: 10,
            unfold_limit_lines: Some(30),
            min_body_lines: 4,
        };
        let s = summarize_code(&src, Lang::TypeScript, &opts);
        assert_eq!(
            s.segments
                .iter()
                .filter(|seg| seg.kind == SegmentKind::Elided)
                .count(),
            1
        );
    }

    #[test]
    fn budget_respected_visible_within_limit() {
        // Deeply-nested JSON: the fully-folded skeleton is tiny (2 brace lines),
        // so the BFS unfolds toward the target. Because every accepted unfold
        // keeps `new_visible <= limit`, the final visible count also stays
        // within the limit, and the pass keeps some nested objects folded.
        let groups = (0..12)
            .map(|i| format!("\t\"g{i}\": {{\n\t\t\"a\": 1,\n\t\t\"b\": 2,\n\t\t\"c\": 3,\n\t\t\"d\": 4\n\t}}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let src = format!("{{\n{groups}\n}}\n");
        let opts = SummaryOptions {
            unfold_until_lines: 30,
            unfold_limit_lines: Some(60),
            min_body_lines: 4,
        };
        let s = summarize_code(&src, Lang::Json, &opts);
        assert!(
            s.visible_lines <= 60,
            "visible {} exceeded limit",
            s.visible_lines
        );
        assert!(
            s.visible_lines >= 30,
            "visible {} did not reach target",
            s.visible_lines
        );
        assert!(
            s.visible_lines < s.total_lines,
            "something must stay folded"
        );
        assert!(s.segments.iter().any(|seg| seg.kind == SegmentKind::Elided));
    }

    // ---- render stability & fidelity -----------------------------------

    #[test]
    fn render_is_stable() {
        let src = "fn greet() -> u32 {\n\tlet a = 1;\n\tlet b = 2;\n\tlet c = 3;\n\ta + b + c\n}\n";
        let a = summarize_code(src, Lang::Rust, &fold_opts());
        let b = summarize_code(src, Lang::Rust, &fold_opts());
        assert_eq!(a.render(), b.render());
        assert_eq!(a.segments, b.segments);
    }

    #[test]
    fn kept_text_is_byte_identical_to_source_lines() {
        let src = "use std::fs;\n\nfn greet(name: &str) -> String {\n    let a = name.trim();\n    let b = a.to_uppercase();\n    let c = format!(\"{b}!\");\n    c\n}\n";
        let s = summarize_code(src, Lang::Rust, &fold_opts());
        // Reconstruct the source line vector the same way the summarizer does.
        let mut src_lines: Vec<&str> = src.split('\n').collect();
        if src.ends_with('\n') {
            src_lines.pop();
        }
        for seg in s
            .segments
            .iter()
            .filter(|seg| seg.kind == SegmentKind::Kept)
        {
            let expected = src_lines[seg.start_line - 1..seg.end_line].join("\n");
            assert_eq!(
                seg.text, expected,
                "kept lines {}-{} must be verbatim",
                seg.start_line, seg.end_line
            );
        }
        assert!(s.segments.iter().any(|seg| seg.kind == SegmentKind::Elided));
    }

    #[test]
    fn crlf_kept_text_preserves_carriage_returns() {
        // A parse-failure passthrough on CRLF source must round-trip byte-for-byte.
        let src = "not valid\r\nrust code {{{\r\n";
        let s = summarize_code(src, Lang::Rust, &fold_opts());
        assert_eq!(s.segments.len(), 1);
        assert!(
            s.segments[0].text.contains('\r'),
            "carriage returns preserved"
        );
    }
}
