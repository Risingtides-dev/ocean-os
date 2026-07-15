//! ChatComponent — the native agent surface. Re-houses the PM room's streaming
//! model (structured blocks: text, thinking, tool calls) onto the component
//! architecture, plus: permission approval cards (⌃Y allow / ⌃N deny, the
//! OCEAN-185 gated flow), streaming markdown with prefix-freeze (via
//! `shell::markdown` — headings, syntax-highlighted fences, lists, blockquotes,
//! inline `code`/**bold**/*italic*), tool cards with ⌃O collapse/expand,
//! multi-line input (⌃J newline), and wheel/PageUp scrollback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ocean_agent_sdk::{AgentTurnEvent, ThinkingLevel, ToolCallId};
use ocean_core::{OceanEvent, PermissionId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::shell::{
    action::{Action, LoginTarget, Nav},
    component::Component,
    diff::{self, DiffKind, DiffRow},
    errfmt,
    history::PromptHistory,
    markdown::Markdown,
    panel, slash,
    theme::{self, g},
};

/// An open tool drawer shows at most this many logical body rows (newest
/// first); older rows collapse behind a "… N earlier lines" marker. Bounds
/// live-streamed output so one huge result never floods the transcript. This
/// is a per-drawer tail; ⌃O remains the global open-all override.
const DRAWER_BODY_ROWS: usize = 40;

/// Kill-ring depth (⌃U / ⌃K push, ⌃Y yanks the newest).
const KILL_RING_CAP: usize = 10;

/// One rendered unit of transcript.
enum Turn {
    /// Operator's prompt.
    User(String),
    /// Assistant visible text (accumulates deltas).
    Assistant(String),
    /// Extended-thinking text (accumulates deltas).
    Thinking(String),
    /// A tool call: keyed by call id, with name + lossless raw args + streamed
    /// output + status. Each call is an independent drawer — `expanded` is the
    /// per-call open state (⌃O / settings still globally open every drawer via
    /// `tools_expanded`; a body is open when either is true). When the tool is
    /// an edit tool, `diff` carries pre-computed diff-card rows and the open
    /// drawer renders those instead of raw output. Raw `args_json` is retained
    /// so the expanded body can show lossless arguments.
    Tool {
        id: ToolCallId,
        name: String,
        args_json: serde_json::Value,
        output: String,
        status: ToolStatus,
        diff: Option<Vec<DiffRow>>,
        expanded: bool,
    },
    /// An advisor aside — a note from the observer/advisor extension. Rendered as
    /// a set-off amber card, clearly not the agent's own output.
    Advisor {
        note: String,
        severity: String,
        model: String,
    },
    /// A terminal-level notice about a turn that failed or was cancelled.
    /// Rendered as a plain notice line ("✗ turn failed — …"), NOT as an advisor
    /// card — no severity label, no model attribution.
    ErrorNotice { note: String },
    /// A gated tool waiting on the operator (OCEAN-185). `resolved` is `None`
    /// while waiting, then Some(allowed).
    Permission {
        permission_id: PermissionId,
        tool: String,
        reason: String,
        resolved: Option<bool>,
    },
    /// A surface-neutral `component_render` artifact projected into terminal
    /// cells. The payload remains canonical JSON; this client supplies only the
    /// btop-style visual interpretation.
    Component {
        id: String,
        kind: String,
        props: serde_json::Value,
        /// For `confirm` components: None while waiting, Some(true/false) after
        /// the operator answers. Non-confirm components leave this None.
        resolved: Option<bool>,
    },
}

#[derive(PartialEq)]
enum ToolStatus {
    Running,
    Ok,
    Err,
}

/// One visible tool-drawer header row on screen, built per frame for mouse
/// hit-testing. Coordinates are absolute terminal cells (after wrapping and
/// scroll), so a click maps to exactly the drawer whose header painted there.
#[derive(Debug)]
struct DrawerHit {
    id: ToolCallId,
    row: u16,
    col_start: u16,
    col_end: u16,
}

/// The ⌃R fuzzy history-search overlay state (present only while open).
#[derive(Default)]
struct HistorySearch {
    /// Typed query, fuzzy-matched against history entries.
    query: String,
    /// Highlighted row in the match list.
    sel: usize,
}

#[derive(Default)]
pub struct ChatComponent {
    /// Throughput of the LAST finished turn, exactly as the daemon reported
    /// it (provider usage when available, its estimate otherwise). Cleared on
    /// `TurnStarted` — never a stale rate dressed up as current.
    last_tok_per_s: Option<f64>,
    turns: Vec<Turn>,
    input: String,
    model: Option<String>,
    busy: bool,
    /// Byte-index cursor position in the composer. `None` means trailing
    /// (at `input.len()`), which is also the Default so `chat_with` and
    /// direct `input = …` writes all self-seat at the end.
    cursor: Option<usize>,
    /// Scrollback offset in lines from the bottom (0 = stick to live tail).
    scroll_back: usize,
    /// Highlighted row in the `/` command palette (see `slash_matches`).
    menu_sel: usize,
    /// Streaming markdown renderer + frozen-block cache for assistant text.
    md: Markdown,
    /// When true, tool cards render their full output instead of a tail window.
    tools_expanded: bool,
    /// Focused tool drawer (Alt-↑/↓ traverse in transcript order with wrapping;
    /// Alt-Space / Alt-Enter toggles). `None` when no drawer is focused.
    focused_drawer: Option<ToolCallId>,
    /// Per-frame map of visible drawer-header screen rows → tool id, rebuilt
    /// on every draw for mouse hit-testing. Consumed by `handle_mouse`.
    drawer_hits: Vec<DrawerHit>,
    /// Drawer header armed by a left-button Down, committed on Up. A Drag in
    /// between disarms it — so the app-level drag-to-select-text sweep across
    /// a header never toggles the drawer.
    pending_drawer_click: Option<ToolCallId>,
    /// Persisted prompt history (↑/↓ recall, ⌃R search). Loaded at startup.
    history: PromptHistory,
    /// Cursor into `history` while navigating with ↑/↓; `None` when editing a
    /// fresh line rather than walking history.
    history_idx: Option<usize>,
    /// The in-progress composer text saved when ↑ first enters history, restored
    /// when ↓ walks back past the newest entry.
    draft: String,
    /// Kill ring: ⌃U / ⌃K push, ⌃Y yanks the newest (cap [`KILL_RING_CAP`]).
    kill_ring: Vec<String>,
    /// The ⌃R history-search overlay, when open.
    search: Option<HistorySearch>,
    /// Project root for `@`-file mentions; set by the app (follows the active
    /// project). `None` until first set.
    mention_root: Option<std::path::PathBuf>,
    /// Lazy, per-project file index for the `@` picker (scanned on first `@`,
    /// invalidated when `mention_root` changes).
    mention_index: Option<Vec<String>>,
    /// Highlighted row in the `@` mention picker.
    mention_sel: usize,
    pub focused: bool,
    /// Optional provider-status line shown in the welcome empty-state only.
    /// Set by the app after construction.
    pub welcome_provider_line: Option<String>,
    /// Optional pinned component rendered in a fixed footer row above the
    /// composer. Set by `component_render` with `pinned: true`; cleared by
    /// `component_unmount` targeting the pinned id. Only one slot is supported.
    pinned: Option<Turn>,
    /// Operator-controlled visibility. `/pinned hide` preserves the artifact so
    /// `/pinned show` can restore it without asking the agent to re-render.
    pinned_visible: bool,
}

/// Tool-aware salient preview for a drawer header: command for bash, pattern +
/// path for grep/glob, path for file tools, url for fetch/nav, and a readable
/// scalar `key: value` fallback for unknown/MCP tools. `write`/`edit` are
/// PATH-FOCUSED here — the full payload lives in the expanded args body and the
/// approval card is a separate surface; do NOT echo file content into the
/// collapsed header. Returns a single clean line (whitespace flattened, control
/// bytes stripped); the caller truncates by terminal-cell width at render time.
fn humanize_preview(name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let joined = |parts: Vec<&str>| {
        parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let summary = match name {
        "bash" => s("command").to_string(),
        "glob" | "grep" => joined(vec![s("pattern"), s("path")]),
        "read" | "ls" => s("path").to_string(),
        // Mutating file tools: path only in the collapsed preview. Content and
        // old→new land in the expanded args body; the approval card is a
        // separate surface that shows the payload.
        // Path ONLY, unconditionally: even when "path" is empty/missing this
        // must NEVER fall through to the scalar fallback below, which would
        // echo `content: <file payload>` into the collapsed header.
        "write" | "edit" => return flatten_oneline(s("path")),
        "web_fetch" | "browser_navigate" => s("url").to_string(),
        _ => String::new(),
    };
    let summary = if summary.is_empty() {
        // Fallback for unknown/MCP tools and known tools whose salient arg came
        // through empty: render scalar args as `key: value` pairs.
        args.as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(key, value)| match value {
                        serde_json::Value::String(st) if !st.is_empty() => {
                            Some(format!("{key}: {st}"))
                        }
                        serde_json::Value::Number(n) => Some(format!("{key}: {n}")),
                        serde_json::Value::Bool(b) => Some(format!("{key}: {b}")),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default()
    } else {
        summary
    };
    flatten_oneline(&summary)
}

/// Flatten whitespace to single spaces and drop control bytes WITHOUT
/// truncating. Used for the drawer preview before width-based truncation.
fn flatten_oneline(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Truncate `s` to at most `max_width` terminal cells, appending a disclosure
/// ellipsis (`…` / `...`) when something is dropped. Unlike [`clamp_line`]
/// (char-based) this budgets by display width via `UnicodeWidthStr`, so CJK or
/// emoji arguments cannot wrap the one-row drawer header or blow the width
/// budget. The result is guaranteed to fit in `max_width` cells: if the
/// ellipsis itself is wider than `max_width`, a hard cell cut is used instead.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    // Zero budget first: returning the whole string here would defeat the
    // one-row width guarantee (the early `|| max_width == 0` returned `s`
    // verbatim — a CJK/emoji argument would then wrap the header).
    if max_width == 0 {
        return String::new();
    }
    let total = UnicodeWidthStr::width(s);
    if total <= max_width {
        return s.to_string();
    }
    let ell = g("…", "...");
    let ell_w = UnicodeWidthStr::width(ell);
    let mut out = String::new();
    if max_width >= ell_w {
        let budget = max_width - ell_w;
        let mut w = 0usize;
        for c in s.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > budget {
                break;
            }
            out.push(c);
            w += cw;
        }
        out.push_str(ell);
    } else {
        // Absurdly narrow: hard cell cut, no ellipsis room.
        let mut w = 0usize;
        for c in s.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > max_width {
                break;
            }
            out.push(c);
            w += cw;
        }
    }
    out
}

/// Pad terminal-safe text to an exact display-cell width. Unlike format width,
/// this treats CJK and emoji as multi-cell glyphs.
fn pad_to_width(s: &str, width: usize) -> String {
    let safe = sanitize_line(s);
    let clipped = truncate_to_width(&safe, width);
    let used = UnicodeWidthStr::width(clipped.as_str());
    format!("{clipped}{}", " ".repeat(width.saturating_sub(used)))
}

/// Make a line of tool/diff output terminal-safe. ratatui does NOT expand
/// tabs: a raw `\t` makes the terminal jump to its own tab stop, every cell
/// after it paints misaligned, and ratatui's diffing (which believes its own
/// cell math) leaves smeared "bleed" that never clears. Other control chars
/// (ESC sequences, `\r`) can recolor or move the cursor under ratatui's feet
/// the same way. Tabs become 4 spaces; other control chars drop.
pub(crate) fn sanitize_line(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Project the portable `component_render` contract into a compact terminal
/// artifact. This is intentionally a pure local projection: the JSON is not
/// added to model context and no terminal layout data crosses the daemon.
fn component_lines(kind: &str, props: &serde_json::Value, width: usize) -> Vec<Line<'static>> {
    let title = props
        .get("title")
        .or_else(|| props.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or(kind);
    let inner = width.saturating_sub(6).clamp(8, 48);
    let title = truncate_to_width(&sanitize_line(title), inner.saturating_sub(2));
    let title_width = UnicodeWidthStr::width(title.as_str());
    let rule = "─".repeat(inner.saturating_sub(title_width + 1));
    let header = format!("  ╭─{title} {rule}");
    let mut lines = vec![Line::from(Span::styled(
        header,
        Style::default().fg(theme::EDGE),
    ))];
    let row = |text: String, color| {
        let safe = sanitize_line(&text);
        let clipped = truncate_to_width(&safe, inner);
        Line::from(vec![
            Span::styled("  │ ", Style::default().fg(theme::EDGE)),
            Span::styled(clipped, Style::default().fg(color)),
        ])
    };
    match kind {
        "progress" => {
            let value = props.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let max = props
                .get("max")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0)
                .max(f64::EPSILON);
            let ratio = (value / max).clamp(0.0, 1.0);
            let bar_w = inner.saturating_sub(9).max(4);
            let filled = (ratio * bar_w as f64).round() as usize;
            lines.push(row(
                format!(
                    "{} {} {:>3}%",
                    "█".repeat(filled),
                    "░".repeat(bar_w - filled),
                    (ratio * 100.0).round()
                ),
                theme::CYAN,
            ));
        }
        "stat" => {
            for stat in props
                .get("stats")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .take(4)
            {
                let label = stat
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("metric");
                let value = stat
                    .get("value")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "—".into())
                    .trim_matches('"')
                    .to_string();
                let delta = stat.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                lines.push(row(format!("{label:<14} {value:>10}  {delta}"), theme::FG));
            }
        }
        "chart" => {
            let series = props
                .get("series")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let values: Vec<f64> = series
                .iter()
                .filter_map(|point| point.get("value").and_then(|v| v.as_f64()))
                .collect();
            if values.is_empty() {
                lines.push(row("(no data)".into(), theme::COMMENT));
            } else {
                let max = values
                    .iter()
                    .copied()
                    .fold(0.0_f64, f64::max)
                    .max(f64::EPSILON);
                let glyphs = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
                let start = values.len().saturating_sub(inner);
                let graph: String = values[start..]
                    .iter()
                    .map(|v| glyphs[((v / max * 7.0).round() as usize).min(7)])
                    .collect();
                let last = values.last().copied().unwrap_or_default();
                lines.push(row(format!("{graph}  {last:.2}"), theme::CYAN));
            }
        }
        "timeline" => {
            for step in props
                .get("steps")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .take(6)
            {
                let label = step.get("label").and_then(|v| v.as_str()).unwrap_or("step");
                let status = step
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                let (mark, color) = match status {
                    "done" => ("●", theme::GREEN),
                    "active" => ("◉", theme::CYAN),
                    "error" => ("✗", theme::RED),
                    _ => ("○", theme::COMMENT),
                };
                lines.push(row(format!("{mark} {label}  {status}"), color));
            }
        }
        "callout" => {
            let body = props.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let color = match props.get("variant").and_then(|v| v.as_str()) {
                Some("success") => theme::GREEN,
                Some("warn") => theme::YELLOW,
                Some("error") => theme::RED,
                _ => theme::CYAN,
            };
            for text in body.lines().take(3) {
                lines.push(row(truncate_to_width(&sanitize_line(text), inner), color));
            }
        }
        "confirm" => {
            let body = props.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let variant = props.get("variant").and_then(|v| v.as_str());
            let color = match variant {
                Some("error") => theme::RED,
                _ => theme::YELLOW,
            };
            for text in body.lines().take(2) {
                lines.push(row(truncate_to_width(&sanitize_line(text), inner), color));
            }
            if let Some(confirm_label) = props.get("confirm_label").and_then(|v| v.as_str()) {
                let cancel_label = props
                    .get("cancel_label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("n");
                lines.push(row(
                    format!("[Y] {confirm_label}  [N] {cancel_label}"),
                    theme::FG,
                ));
            } else {
                lines.push(row("[Y] confirm  [N] cancel".into(), theme::FG));
            }
        }
        "table" => {
            let columns = props
                .get("columns")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            let rows = props
                .get("rows")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_array()).collect::<Vec<_>>())
                .unwrap_or_default();
            if columns.is_empty() || rows.is_empty() {
                lines.push(row("(empty table)".into(), theme::COMMENT));
            } else {
                let col_w = (inner.saturating_sub(columns.len() * 2)) / columns.len().max(1);
                let fit = |s: &str| pad_to_width(s, col_w);
                // Header row
                let hdr: String = columns
                    .iter()
                    .map(|c| fit(c))
                    .collect::<Vec<_>>()
                    .join("  ");
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(theme::EDGE)),
                    Span::styled(
                        hdr,
                        Style::default()
                            .fg(theme::CYAN)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                // Separator
                let sep: String = columns
                    .iter()
                    .map(|_| "─".repeat(col_w))
                    .collect::<Vec<_>>()
                    .join("  ");
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(theme::EDGE)),
                    Span::styled(sep, Style::default().fg(theme::EDGE)),
                ]));
                // Data rows
                for row_data in rows.iter().take(8) {
                    let r: String = columns
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            let cell = row_data.get(i).and_then(|v| v.as_str()).unwrap_or("");
                            fit(cell)
                        })
                        .collect::<Vec<_>>()
                        .join("  ");
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(theme::EDGE)),
                        Span::styled(r, Style::default().fg(theme::FG)),
                    ]));
                }
            }
        }
        "code" => {
            let code = props
                .get("code")
                .or_else(|| props.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lang = props.get("language").and_then(|v| v.as_str()).unwrap_or("");
            if !lang.is_empty() {
                lines.push(row(format!("language: {lang}"), theme::COMMENT));
            }
            for text in code.lines().take(12) {
                lines.push(row(
                    truncate_to_width(&sanitize_line(text), inner),
                    theme::FG,
                ));
            }
            if code.lines().count() > 12 {
                lines.push(row(
                    format!("… {} more lines", code.lines().count() - 12),
                    theme::COMMENT,
                ));
            }
        }
        "diff" => {
            let unified = props
                .get("unified")
                .or_else(|| props.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let filename = props.get("filename").and_then(|v| v.as_str());
            if let Some(f) = filename {
                lines.push(row(format!("file: {f}"), theme::COMMENT));
            }
            for text in unified.lines().take(16) {
                let (color, prefix) = if text.starts_with('+') {
                    (theme::GREEN, text)
                } else if text.starts_with('-') {
                    (theme::RED, text)
                } else if text.starts_with("@@") {
                    (theme::CYAN, text)
                } else {
                    (theme::COMMENT, text)
                };
                lines.push(row(truncate_to_width(&sanitize_line(prefix), inner), color));
            }
        }
        "file_tree" => {
            let entries = props
                .get("entries")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            fn walk_tree(
                entries: &[serde_json::Value],
                prefix: &str,
                out: &mut Vec<(String, Color)>,
            ) {
                for (i, entry) in entries.iter().enumerate() {
                    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let is_dir = entry.get("type").and_then(|v| v.as_str()) == Some("dir");
                    let last = i + 1 == entries.len();
                    let (conn, child_prefix) = if last {
                        ("└── ", "    ")
                    } else {
                        ("├── ", "│   ")
                    };
                    let display =
                        format!("{prefix}{conn}{}{}", name, if is_dir { "/" } else { "" });
                    out.push((display, if is_dir { theme::CYAN } else { theme::FG }));
                    if is_dir {
                        if let Some(children) = entry.get("children").and_then(|v| v.as_array()) {
                            walk_tree(children, &format!("{prefix}{child_prefix}"), out);
                        }
                    }
                }
            }
            let mut tree_lines: Vec<(String, Color)> = Vec::new();
            walk_tree(&entries, "", &mut tree_lines);
            for (text, color) in tree_lines.iter().take(14) {
                lines.push(row(truncate_to_width(&sanitize_line(text), inner), *color));
            }
            if tree_lines.len() > 14 {
                lines.push(row(
                    format!("… {} more entries", tree_lines.len() - 14),
                    theme::COMMENT,
                ));
            }
        }
        "gallery" => {
            let images = props
                .get("images")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if images.is_empty() {
                lines.push(row("(no images)".into(), theme::COMMENT));
            } else {
                for img in images.iter().take(4) {
                    let caption = img
                        .get("caption")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image");
                    let src = img.get("src").and_then(|v| v.as_str()).unwrap_or("");
                    let local = src.strip_prefix("file://").unwrap_or(src);
                    lines.push(row(format!("🖼 {caption}  {local}"), theme::CYAN));
                }
                if images.len() > 4 {
                    lines.push(row(
                        format!("… {} more images", images.len() - 4),
                        theme::COMMENT,
                    ));
                }
            }
        }
        _ => lines.push(row(
            format!("{kind} component (web-only projection)"),
            theme::COMMENT,
        )),
    }
    lines.push(Line::from(Span::styled(
        format!("  ╰{}", "─".repeat(inner + 3)),
        Style::default().fg(theme::EDGE),
    )));
    lines
}

/// Render one diff-card row: a coloured gutter sigil + the (possibly word-diffed)
/// body on the dark bed. Changed word runs carry `Modifier::REVERSED` (SGR
/// inverse), matching OMP's intra-line diff highlight.
fn diff_line(row: &DiffRow, body_width: usize) -> Line<'static> {
    let (gutter, gutter_fg, body_fg, dim) = match row.kind {
        DiffKind::Del => (g("-", "-"), theme::RED, theme::RED, false),
        DiffKind::Add => (g("+", "+"), theme::GREEN, theme::GREEN, false),
        DiffKind::Context => (" ", theme::EDGE, theme::COMMENT, false),
        DiffKind::Header => (g("┆", ":"), theme::COMMENT, theme::COMMENT, true),
    };
    let prefix = format!("    {gutter} ");
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
    let budget = body_width.saturating_sub(prefix_w);
    let mut spans: Vec<Span> = vec![Span::styled(prefix, Style::default().fg(gutter_fg))];
    let seg_style = |changed: bool| {
        let mut style = Style::default().fg(body_fg).bg(theme::BG_DARK);
        if dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if changed {
            style = style.add_modifier(Modifier::REVERSED);
        }
        style
    };
    // Fast path: the whole row fits within the budget — keep per-segment
    // styling verbatim (no ellipsis).
    let total_body: usize = row
        .segs
        .iter()
        .map(|seg| UnicodeWidthStr::width(sanitize_line(&seg.text).as_str()))
        .sum();
    if total_body <= budget {
        for seg in &row.segs {
            spans.push(Span::styled(
                sanitize_line(&seg.text),
                seg_style(seg.changed),
            ));
        }
        return Line::from(spans);
    }
    // Truncation path: walk segments cell-by-cell; reserve an ellipsis (or hard
    // cut when even the ellipsis won't fit) and stop once the budget is
    // exhausted. Colouring is preserved on the kept prefix.
    let ell = g("…", "...");
    let ell_w = UnicodeWidthStr::width(ell);
    let (limit, use_ellipsis) = if budget >= ell_w {
        (budget - ell_w, true)
    } else {
        (budget, false)
    };
    let mut used = 0usize;
    'outer: for seg in &row.segs {
        let style = seg_style(seg.changed);
        let clean = sanitize_line(&seg.text);
        let mut buf = String::new();
        for c in clean.chars() {
            let cw = c.width().unwrap_or(0);
            if used + cw > limit {
                if !buf.is_empty() {
                    spans.push(Span::styled(buf, style));
                }
                if use_ellipsis {
                    spans.push(Span::styled(
                        ell.to_string(),
                        Style::default().fg(body_fg).bg(theme::BG_DARK),
                    ));
                }
                break 'outer;
            }
            buf.push(c);
            used += cw;
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
    }
    Line::from(spans)
}

/// Flatten whitespace to single spaces and truncate to `max` chars with an
/// ellipsis.
fn one_line(s: &str, max: usize) -> String {
    // Flatten whitespace, then drop any remaining control bytes: this string
    // lands in a ratatui `Span`, and one raw ESC/CSI sequence repaints the
    // terminal underneath ratatui's cell math (the collapsed-card screen
    // smear). `split_whitespace` already eats tabs/newlines/CRs; ESC is NOT
    // whitespace and would sail through without the filter.
    let flat: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}{}", g("…", "..."))
    } else {
        flat
    }
}

/// Truncate a single line to `max` chars with an ellipsis, preserving internal
/// whitespace (unlike [`one_line`], which flattens it — wrong for tool-output
/// previews where indentation is signal). Used by collapsed tool cards so each
/// tail line costs exactly one screen row: without this, a tool returning one
/// giant single-line blob (JSON from recall/lsp/MCP results) wraps into
/// hundreds of rows and defeats the tail window entirely.
fn clamp_line(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}{}", g("…", "..."))
    } else {
        s.to_string()
    }
}

/// Normalize a bracketed paste for the composer: CRLF/CR become plain
/// newlines (kept — the composer is multi-line via ⌃J), tabs become four
/// spaces, and every other control byte drops (an ESC sequence must never
/// reach a `Span`). Pasted newlines are CONTENT, never synthetic Enter
/// presses — the pre-bracketed-paste terminal replayed them as real key
/// events and auto-submitted mid-paste.
pub(crate) fn paste_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

impl ChatComponent {
    /// Construct the chat surface with prompt history loaded from disk (for
    /// ↑/↓ recall and ⌃R search). `Default` leaves history empty — used in
    /// tests that don't touch disk.
    pub fn new() -> Self {
        Self {
            history: PromptHistory::load(),
            pinned_visible: true,
            ..Default::default()
        }
    }

    /// Inject visual-harness components when `OCEAN_TUI_COMPONENT_DEMO` is set.
    /// Call this AFTER session resume so `load_history` doesn't wipe them.
    /// Each component is contextualized to the ocean-os project — real file
    /// names, real PR history, real build metrics.
    pub fn maybe_inject_demo(&mut self) {
        if std::env::var_os("OCEAN_TUI_COMPONENT_DEMO").is_none() {
            return;
        }
        self.turns.extend([
            // ── callout: what this is ──────────────────────────────────────
            Turn::Component {
                id: "demo-intro".into(),
                kind: "callout".into(),
                props: serde_json::json!({
                    "title": "Component projection demo",
                    "variant": "info",
                    "body": "ocean-os · main · 31 MB daemon · 274+10 TUI tests\n\
                             11 of 17 render-protocol kinds projected into the terminal.\n\
                             All data below is live project state — real files, real PRs."
                }),
                resolved: None,
            },
            // ── stat: build metrics ───────────────────────────────────────
            Turn::Component {
                id: "demo-stats".into(),
                kind: "stat".into(),
                props: serde_json::json!({
                    "title": "ocean-os build · main",
                    "stats": [
                        {"label": "daemon",    "value": "31 MB",   "delta": "release"},
                        {"label": "ocean-tui",  "value": "11 MB",   "delta": "release"},
                        {"label": "tests",      "value": "284",     "delta": "▲ 10"},
                        {"label": "open PRs",   "value": "0",       "delta": "clean"}
                    ]
                }),
                resolved: None,
            },
            // ── timeline: recent PR merges ────────────────────────────────
            Turn::Component {
                id: "demo-pr-timeline".into(),
                kind: "timeline".into(),
                props: serde_json::json!({
                    "title": "Recent PR merges → main",
                    "steps": [
                        {"label": "#277 shell halt CI window",       "status": "done"},
                        {"label": "#276 TUI launch chooser",         "status": "done"},
                        {"label": "#275 build compat enforcement",   "status": "done"},
                        {"label": "readline composer follow-ups",    "status": "done"},
                        {"label": "TUI shell rebuild + lifeline",    "status": "done"},
                        {"label": "component-projection spike",     "status": "active"}
                    ]
                }),
                resolved: None,
            },
            // ── table: TUI crate file map ────────────────────────────────
            Turn::Component {
                id: "demo-table".into(),
                kind: "table".into(),
                props: serde_json::json!({
                    "title": "crates/ocean-tui/src/shell/ layout",
                    "columns": ["file", "lines", "role"],
                    "rows": [
                        ["app.rs",        "3895",  "Elm loop + dispatch"],
                        ["chat.rs",       "5271",  "transcript + composer + components"],
                        ["session_rail.rs","~340", "session browser sidebar"],
                        ["file_tree.rs",  "~220", "project file browser"],
                        ["editor.rs",     "~190", "text editor pane"],
                        ["graph.rs",      "~700", "agent graph viewer"],
                        ["slash.rs",      "~110", "/command dispatch"]
                    ]
                }),
                resolved: None,
            },
            // ── code: component renderer entry point ─────────────────────
            Turn::Component {
                id: "demo-code".into(),
                kind: "code".into(),
                props: serde_json::json!({
                    "title": "ComponentRender handler in chat.rs",
                    "language": "rust",
                    "code": "AgentTurnEvent::ComponentRender {\n    component_id, kind, props, replace, ..\n} => {\n    let pinned = props.get(\"pinned\")\n        .and_then(|v| v.as_bool()).unwrap_or(false);\n    if pinned { self.pinned = Some(/* ... */); }\n    if *replace { self.turns.retain(/* ... */); }\n    self.turns.push(Turn::Component { id, kind, props });\n}"
                }),
                resolved: None,
            },
            // ── diff: hook change we just made ───────────────────────────
            Turn::Component {
                id: "demo-diff".into(),
                kind: "diff".into(),
                props: serde_json::json!({
                    "title": "Fix: demo inject after session resume (app.rs)",
                    "unified": "@@ -483,6 +483,9 @@\n             app.bind_session_with(id, false);\n             app.rail.live_id = Some(id.0.to_string());\n         }\n+        // Inject visual-harness components AFTER session resume\n+        // so load_history doesn't overwrite them.\n+        app.chat.maybe_inject_demo();\n         app"
                }),
                resolved: None,
            },
            // ── file_tree: TUI crate structure ───────────────────────────
            Turn::Component {
                id: "demo-ftree".into(),
                kind: "file_tree".into(),
                props: serde_json::json!({
                    "title": "crates/ocean-tui/",
                    "entries": [
                        {"name": "src/", "type": "dir", "children": [
                            {"name": "main.rs", "type": "file"},
                            {"name": "splash.rs", "type": "file"},
                            {"name": "shell/", "type": "dir", "children": [
                                {"name": "mod.rs", "type": "file"},
                                {"name": "app.rs", "type": "file"},
                                {"name": "action.rs", "type": "file"},
                                {"name": "components/", "type": "dir", "children": [
                                    {"name": "chat.rs", "type": "file"},
                                    {"name": "editor.rs", "type": "file"},
                                    {"name": "file_tree.rs", "type": "file"},
                                    {"name": "graph.rs", "type": "file"},
                                    {"name": "pty_pane.rs", "type": "file"},
                                    {"name": "session_rail.rs", "type": "file"}
                                ]},
                                {"name": "client.rs", "type": "file"},
                                {"name": "daemon_boot.rs", "type": "file"},
                                {"name": "sessions.rs", "type": "file"},
                                {"name": "slash.rs", "type": "file"}
                            ]}
                        ]}
                    ]
                }),
                resolved: None,
            },
            // ── chart: keep provider latency ─────────────────────────────
            Turn::Component {
                id: "demo-chart".into(),
                kind: "chart".into(),
                props: serde_json::json!({
                    "title": "Provider latency (ms)",
                    "type": "line",
                    "series": [
                        {"label":"-9","value":12},{"label":"-8","value":18},
                        {"label":"-7","value":15},{"label":"-6","value":28},
                        {"label":"-5","value":34},{"label":"-4","value":21},
                        {"label":"-3","value":39},{"label":"-2","value":31},
                        {"label":"-1","value":48},{"label":"now","value":42}
                    ]
                }),
                resolved: None,
            },
            // ── progress: component projection coverage ──────────────────
            Turn::Component {
                id: "demo-progress".into(),
                kind: "progress".into(),
                props: serde_json::json!({
                    "label": "Component projection coverage",
                    "value": 11,
                    "max": 17
                }),
                resolved: None,
            },
            // ── confirm: interactive test ────────────────────────────────
            Turn::Component {
                id: "demo-confirm".into(),
                kind: "confirm".into(),
                props: serde_json::json!({
                    "title": "Land this branch?",
                    "body": "11 of 17 kinds projected.\n\
                             All 284 tests pass. Ready to rebase onto main.",
                    "confirm_label": "land it",
                    "cancel_label": "more work"
                }),
                resolved: None,
            },
        ]);
    }

    // ── prompt history: ↑/↓ recall ───────────────────────────────────────────

    /// Leave history-navigation mode (any edit or submit resets it).
    fn reset_history_nav(&mut self) {
        self.history_idx = None;
        self.draft.clear();
    }

    /// ↑ — walk one step back through history. Only fires when the composer is
    /// empty or already navigating, so it never hijacks an in-progress draft.
    fn history_prev(&mut self) {
        if self.history_idx.is_none() && !self.input.is_empty() {
            return; // non-history text in the composer — don't hijack ↑
        }
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.draft = self.input.clone(); // stash the (empty) draft
                self.history.len() - 1
            }
            Some(0) => 0, // already at the oldest entry — stay put
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.input = self.history.get(idx).unwrap_or("").to_string();
        self.cursor = None;
        self.menu_sel = 0;
    }

    /// ↓ — walk one step forward through history. Only fires while navigating;
    /// walking past the newest entry restores the stashed draft.
    fn history_next(&mut self) {
        let Some(i) = self.history_idx else {
            return; // not navigating — don't hijack ↓
        };
        if i + 1 < self.history.len() {
            self.history_idx = Some(i + 1);
            self.input = self.history.get(i + 1).unwrap_or("").to_string();
            self.cursor = None;
        } else {
            self.input = std::mem::take(&mut self.draft);
            self.cursor = None;
            self.history_idx = None;
        }
        self.menu_sel = 0;
    }

    // ── kill ring: ⌃U / ⌃K / ⌃Y ───────────────────────────────────────────────

    /// Push killed text onto the ring (newest last), capped at [`KILL_RING_CAP`].
    fn push_kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.kill_ring.push(text);
        if self.kill_ring.len() > KILL_RING_CAP {
            self.kill_ring.remove(0);
        }
    }

    /// ⌃U — kill from the start of the current line to the cursor onto the ring.
    fn kill_to_start(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let c = self.cursor_byte();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if line_start == c {
            return;
        }
        let killed = self.input[line_start..c].to_string();
        self.input.drain(line_start..c);
        self.push_kill(killed);
        self.cursor = (line_start < self.input.len()).then_some(line_start);
        self.reset_history_nav();
        self.menu_sel = 0;
    }

    /// ⌃K — kill from the cursor to the end of the current line. At end of
    /// buffer → no-op. At a non-final line end (cursor sits on `\n`) the
    /// newline is killed, joining the lines. Mid-line kills through end of
    /// line but preserves the newline.
    fn kill_to_end(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let c = self.cursor_byte();
        if c == self.input.len() {
            return;
        } // at buffer end → no-op
        let line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());
        if c == line_end {
            // Cursor sits on a newline (non-final line). Kill the newline to join lines.
            let end = c + 1; // skip the '\n'
            let killed = self.input[c..end].to_string();
            self.input.drain(c..end);
            self.push_kill(killed);
        } else {
            let killed = self.input[c..line_end].to_string();
            self.input.drain(c..line_end);
            self.push_kill(killed);
        }
        if self.cursor.is_some_and(|cur| cur >= self.input.len()) {
            self.cursor = None;
        }
        self.reset_history_nav();
        self.menu_sel = 0;
    }

    /// ⌃Y — yank the newest kill at the cursor.
    fn yank(&mut self) {
        if let Some(kill) = self.kill_ring.last().cloned() {
            let c = self.cursor_byte();
            self.input.insert_str(c, &kill);
            let new_cursor = c + kill.len();
            self.cursor = if new_cursor == self.input.len() {
                None
            } else {
                Some(new_cursor)
            };
            self.reset_history_nav();
            self.menu_sel = 0;
        }
    }

    // ── cursor movement / helpers ──────────────────────────────────────────

    /// Effective byte offset of the cursor. `None` → input end.
    fn cursor_byte(&self) -> usize {
        self.cursor
            .map_or(self.input.len(), |c| c.min(self.input.len()))
    }

    /// Move the cursor one Unicode scalar to the left.
    fn cursor_left(&mut self) {
        let c = self.cursor_byte();
        if c > 0 {
            let mut prev = c - 1;
            while prev > 0 && !self.input.is_char_boundary(prev) {
                prev -= 1;
            }
            self.cursor = Some(prev);
        }
    }

    /// Move the cursor one Unicode scalar to the right.
    fn cursor_right(&mut self) {
        let c = self.cursor_byte();
        if c < self.input.len() {
            let mut next = c + 1;
            while next < self.input.len() && !self.input.is_char_boundary(next) {
                next += 1;
            }
            if next == self.input.len() {
                self.cursor = None;
            } else {
                self.cursor = Some(next);
            }
        }
    }

    /// Move the cursor to the start of the current line.
    fn cursor_line_start(&mut self) {
        let c = self.cursor_byte();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if line_start != c {
            self.cursor = Some(line_start);
        }
    }

    /// Move the cursor to the end of the current line.
    fn cursor_line_end(&mut self) {
        let c = self.cursor_byte();
        let line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());
        if line_end == self.input.len() {
            self.cursor = None;
        } else {
            self.cursor = Some(line_end);
        }
    }

    /// Insert a `&str` at the cursor and advance past it. Leaves cursor at
    /// `None` when the insertion reaches the end.
    fn insert_at_cursor(&mut self, s: &str) {
        let c = self.cursor_byte();
        self.input.insert_str(c, s);
        let new_cursor = c + s.len();
        self.cursor = if new_cursor == self.input.len() {
            None
        } else {
            Some(new_cursor)
        };
    }

    // ── word-kill helper (⌃W) ──────────────────────────────────────────────

    /// Kill the word before the cursor onto the ring. Two-phase: first consume
    /// the trailing whitespace run, then the preceding non-whitespace word.
    /// Each phase uses its own reversed iterator so no char is lost at the
    /// phase boundary. Both phases step by whole UTF-8 scalars. The killed
    /// region includes trailing whitespace so yank reconstructs cleanly.
    fn kill_word_backward(&mut self) {
        let c = self.cursor_byte();
        if c == 0 {
            return;
        }
        // Phase 1: consume trailing whitespace run.
        let mut start = c;
        for ch in self.input[..c].chars().rev() {
            if ch.is_whitespace() {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }
        // Phase 2: consume the preceding non-whitespace word.
        // Fresh iterator on the remaining prefix — no shared state with phase 1.
        for ch in self.input[..start].chars().rev() {
            if !ch.is_whitespace() {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }
        if start == c {
            return;
        }
        let killed = self.input[start..c].to_string();
        self.input.drain(start..c);
        self.push_kill(killed);
        self.cursor = (start < self.input.len()).then_some(start);
        self.reset_history_nav();
        self.menu_sel = 0;
    }

    // ── ⌃R history search ──────────────────────────────────────────────────────

    /// History entries matching the current search query, best-first then newest-
    /// first, as indices into the history ring. Empty query matches everything.
    fn search_matches(&self) -> Vec<usize> {
        let Some(s) = self.search.as_ref() else {
            return Vec::new();
        };
        let mut scored: Vec<(usize, i32)> = self
            .history
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| slash::subseq_score(&s.query, e).map(|sc| (i, sc)))
            .collect();
        // Best score first; ties break to the newer entry (higher index).
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// Drive the ⌃R search overlay. Enter inserts the selection into the
    /// composer; Esc dismisses; ↑/↓ move the cursor; typing filters.
    fn handle_search_key(&mut self, key: KeyEvent) -> Option<Action> {
        let matches = self.search_matches();
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                let sel = self.search.as_ref().map(|s| s.sel).unwrap_or(0);
                if let Some(&idx) = matches.get(sel) {
                    if let Some(entry) = self.history.get(idx) {
                        self.input = entry.to_string();
                        self.cursor = None;
                    }
                }
                self.search = None;
                self.reset_history_nav();
                self.menu_sel = 0;
            }
            KeyCode::Up => {
                if let Some(s) = self.search.as_mut() {
                    s.sel = s.sel.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(s) = self.search.as_mut() {
                    if !matches.is_empty() {
                        s.sel = (s.sel + 1).min(matches.len() - 1);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = self.search.as_mut() {
                    s.query.pop();
                    s.sel = 0;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = self.search.as_mut() {
                    s.query.push(c);
                    s.sel = 0;
                }
            }
            _ => {}
        }
        None
    }

    /// Replace the transcript with a resumed session's history (from disk).
    pub fn load_history(&mut self, msgs: Vec<crate::shell::sessions::HistoryMsg>) {
        self.turns = msgs
            .into_iter()
            .map(|m| {
                if m.role == "user" {
                    Turn::User(m.text)
                } else {
                    Turn::Assistant(m.text)
                }
            })
            .collect();
        self.md.clear();
        self.pinned = None;
        self.pinned_visible = true;
        self.busy = false;
    }

    /// Append an assistant text delta, coalescing into the trailing Assistant
    /// block when the last turn is already assistant text.
    fn push_assistant(&mut self, delta: &str) {
        match self.turns.last_mut() {
            Some(Turn::Assistant(s)) => s.push_str(delta),
            _ => self.turns.push(Turn::Assistant(delta.to_string())),
        }
    }

    fn push_thinking(&mut self, delta: &str) {
        match self.turns.last_mut() {
            Some(Turn::Thinking(s)) => s.push_str(delta),
            _ => self.turns.push(Turn::Thinking(delta.to_string())),
        }
    }

    fn tool_by_id(&mut self, id: &ToolCallId) -> Option<&mut Turn> {
        self.turns
            .iter_mut()
            .rev()
            .find(|t| matches!(t, Turn::Tool { id: tid, .. } if tid == id))
    }

    /// All tool-call ids in transcript order — the Alt-↑/↓ traversal sequence.
    fn drawer_ids(&self) -> Vec<ToolCallId> {
        self.turns
            .iter()
            .filter_map(|t| match t {
                Turn::Tool { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Alt-↓ — focus the next tool drawer in transcript order, wrapping from
    /// the last back to the first. With no current focus, start at the first.
    fn drawer_focus_next(&mut self) {
        let ids = self.drawer_ids();
        if ids.is_empty() {
            return;
        }
        let next = match &self.focused_drawer {
            None => 0,
            Some(cur) => ids
                .iter()
                .position(|i| i == cur)
                .map(|p| (p + 1) % ids.len())
                .unwrap_or(0),
        };
        self.focused_drawer = Some(ids[next].clone());
    }

    /// Alt-↑ — focus the previous tool drawer in transcript order, wrapping
    /// from the first back to the last. With no current focus, start at the
    /// last (so the first press from nothing lands on the newest tool).
    fn drawer_focus_prev(&mut self) {
        let ids = self.drawer_ids();
        if ids.is_empty() {
            return;
        }
        let prev = match &self.focused_drawer {
            None => ids.len() - 1,
            Some(cur) => ids
                .iter()
                .position(|i| i == cur)
                .map(|p| (p + ids.len() - 1) % ids.len())
                .unwrap_or(ids.len() - 1),
        };
        self.focused_drawer = Some(ids[prev].clone());
    }

    /// Alt-Space / Alt-Enter — flip the focused drawer's local open state.
    /// No-op when nothing is focused (global ⌃O is a separate control).
    fn toggle_focused_drawer(&mut self) {
        if let Some(id) = self.focused_drawer.clone() {
            self.toggle_drawer(&id);
        }
    }

    /// Flip one drawer's local `expanded` state by call id.
    fn toggle_drawer(&mut self, id: &ToolCallId) {
        if let Some(Turn::Tool { expanded, .. }) = self.tool_by_id(id) {
            *expanded = !*expanded;
        }
    }

    /// Whether any composer overlay — the ⌃R history search, the `/` command
    /// palette, or the `@` mention picker — is currently painted over the
    /// transcript. Mouse drawer hit-handling is suppressed while one is up:
    /// overlay rows cover transcript rows, and a click there must not fall
    /// through to the hidden drawer header underneath.
    fn overlay_active(&self) -> bool {
        self.search.is_some() || !self.slash_matches().is_empty() || self.mention_query().is_some()
    }

    /// Fold an `advisor` extension event into an [`Turn::Advisor`]. Tolerates
    /// missing fields (sensible defaults), skips empty notes, and never panics
    /// on a malformed payload.
    fn push_advisor(&mut self, payload: &serde_json::Value) {
        let note = payload.get("note").and_then(|v| v.as_str()).unwrap_or("");
        if note.trim().is_empty() {
            return;
        }
        let severity = payload
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string();
        let model = payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.turns.push(Turn::Advisor {
            note: note.to_string(),
            severity,
            model,
        });
    }

    /// The newest unresolved permission card, if any — the ⌃Y/⌃N target.
    fn pending_permission(&self) -> Option<PermissionId> {
        self.turns.iter().rev().find_map(|t| match t {
            Turn::Permission {
                permission_id,
                resolved: None,
                ..
            } => Some(*permission_id),
            _ => None,
        })
    }

    fn resolve_permission(&mut self, id: PermissionId, allowed: bool) {
        for t in self.turns.iter_mut().rev() {
            if let Turn::Permission {
                permission_id,
                resolved,
                ..
            } = t
            {
                if *permission_id == id {
                    *resolved = Some(allowed);
                    return;
                }
            }
        }
    }

    /// The ranked command palette for the current composer text, or empty when
    /// the composer isn't in `/`-command mode. Active only when the input starts
    /// with `/` and the query (everything after it) has no whitespace — so a
    /// normal message that merely mentions a slash never triggers the menu.
    fn slash_matches(&self) -> Vec<&'static slash::SlashCommand> {
        match self.input.strip_prefix('/') {
            Some(q) if !q.contains(char::is_whitespace) => {
                slash::filter(q).into_iter().map(|(c, _)| c).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether Tab should route to the composer instead of cycling focus — true
    /// when the `/` palette or `@` mention picker is open (both handle Tab for
    /// completion, but app.rs swallows it for focus-cycling first).
    pub fn wants_tab(&self) -> bool {
        // `/` palette: input starts with `/`, no whitespace in query.
        if let Some(q) = self.input.strip_prefix('/') {
            if !q.contains(char::is_whitespace) {
                return true;
            }
        }
        // `@` mention picker: the cursor-relative token starts with `@`.
        self.mention_query().is_some()
    }

    /// Whether a turn is currently streaming (submit → TurnFinished). The app's
    /// render loop animates at tick rate only while this (or a live PTY) is
    /// true, instead of redrawing 60Hz forever.
    pub fn is_busy(&self) -> bool {
        self.busy
    }
    /// Live activity for the bottom status row, derived from transcript state
    /// (never an action-driven copy, so it cannot go stale): the newest
    /// running tool's name while one runs, `working` for a busy turn between
    /// tools, `None` when idle. Gated on `busy` — `TurnFinished` clears the
    /// flag without rewriting every `ToolStatus::Running`, so an unguarded
    /// transcript scan could leak a dead tool as live activity.
    pub fn activity(&self) -> Option<&str> {
        if !self.busy {
            return None;
        }
        self.turns
            .iter()
            .rev()
            .find_map(|t| match t {
                Turn::Tool {
                    name,
                    status: ToolStatus::Running,
                    ..
                } => Some(name.as_str()),
                _ => None,
            })
            .or(Some("working"))
    }

    /// Last finished turn's tokens/sec for the status row. `None` until a
    /// turn completes or when the daemon reported no rate.
    pub fn tok_per_s(&self) -> Option<f64> {
        self.last_tok_per_s
    }

    /// The model driving turns (the header pill), for the status bar. `None`
    /// until the first `TurnStarted` names it.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Newest image path referenced anywhere in the transcript (user or
    /// assistant text), for the bare `/image` viewer. Scans turns newest-first,
    /// then lines within a turn newest-first. Uses the same `![](path)` parse
    /// as the markdown card so what shows as an image is what `/image` opens.
    pub fn latest_image(&self) -> Option<String> {
        self.turns.iter().rev().find_map(|t| {
            let text = match t {
                Turn::Assistant(s) | Turn::User(s) => s.as_str(),
                _ => return None,
            };
            text.lines().rev().find_map(|l| {
                crate::shell::markdown::parse_image_ref(l.trim_start()).map(|(_, p)| p)
            })
        })
    }

    /// Whether tool/diff cards render fully expanded (the ⌃O toggle). Exposed
    /// for the settings overlay.
    pub fn tools_expanded(&self) -> bool {
        self.tools_expanded
    }

    /// Flip the ⌃O expand toggle from outside (settings overlay row).
    pub fn toggle_tools_expanded(&mut self) {
        self.tools_expanded = !self.tools_expanded;
    }

    // ── `@` file mentions ────────────────────────────────────────────────────

    /// Point `@`-mentions at a project root. Invalidates the file index when the
    /// root actually changes (the app calls this on every project re-root).
    pub fn set_mention_root(&mut self, root: std::path::PathBuf) {
        if self.mention_root.as_deref() != Some(root.as_path()) {
            self.mention_root = Some(root);
            self.mention_index = None; // rescan lazily on next `@`
        }
    }

    /// The active `@` mention in the composer: the trailing token, when it
    /// starts with `@`. Returns (byte offset of the `@`, query after it).
    /// Token = everything after the last whitespace, so `fix @src/ma` matches
    /// and `email me a@b.com` does not (the `@` must LEAD the token).
    /// Token = whitespace-delimited word containing the character immediately
    /// left of the cursor (`cursor_byte()`). Cursor at 0 closes the picker
    /// (nothing left of the caret to match). Returns `(at, end, query)` where
    /// `at..end` is the byte range of the `@` token (exclusive end) and
    /// `query` is the text after the `@` sigil.
    fn mention_query(&self) -> Option<(usize, usize, &str)> {
        let cursor = self.cursor_byte();
        if cursor == 0 {
            return None;
        }
        // Walk back from just left of cursor to find the last whitespace,
        // then skip past it — that's where this token starts.
        let start = self.input[..cursor]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(i, ch)| i + ch.len_utf8())
            .unwrap_or(0);
        // Walk forward from cursor to find the next whitespace — that's
        // where this token ends.
        let end = self.input[cursor..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(i, _)| cursor + i)
            .unwrap_or(self.input.len());
        let token = &self.input[start..end];
        token.starts_with('@').then(|| (start, end, &token[1..]))
    }

    /// Ranked file matches for the current `@` query (empty when not in mention
    /// mode). Scans the project lazily on first use.
    fn mention_matches(&mut self) -> Vec<String> {
        let Some((_, _, query)) = self.mention_query() else {
            return Vec::new();
        };
        let query = query.to_string();
        if self.mention_index.is_none() {
            let root = self
                .mention_root
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            self.mention_index = Some(crate::shell::mentions::scan(&root));
        }
        let index = self.mention_index.as_deref().unwrap_or(&[]);
        crate::shell::mentions::filter(index, &query, 8)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Replace the active `@token` with the picked path, preserving any text
    /// and whitespace that follows it. Cursor lands after the suffix's leading
    /// whitespace so the next keystroke inserts before its content; when the
    /// completion ends the buffer, one trailing space is appended and cursor
    /// normalizes to `None`.
    fn insert_mention(&mut self, path: &str) {
        if let Some((at, end, _)) = self.mention_query() {
            let suffix = self.input[end..].to_string();
            self.input.truncate(at);
            self.input.push('@');
            self.input.push_str(path);
            if suffix.is_empty() {
                self.input.push(' ');
                self.cursor = None;
            } else {
                let leading_whitespace = suffix
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
                let cursor_pos = self.input.len() + leading_whitespace;
                self.input.push_str(&suffix);
                self.cursor = (cursor_pos < self.input.len()).then_some(cursor_pos);
            }
            self.mention_sel = 0;
        }
    }

    fn login_target(args: &str) -> Result<LoginTarget, String> {
        let target = args.trim();
        if target.is_empty() {
            return Ok(LoginTarget::Claude);
        }
        match target.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "anthropic" => Ok(LoginTarget::Claude),
            "codex" | "openai-codex" | "chatgpt" | "openai" => Ok(LoginTarget::Codex),
            _ => Err("usage: /login [claude|codex]".into()),
        }
    }

    fn parse_thinking(args: &str) -> Result<Option<ThinkingLevel>, String> {
        let level = args.trim();
        if level.is_empty() {
            return Err("usage: /thinking default|off|minimal|low|medium|high|xhigh".into());
        }
        match level.to_ascii_lowercase().as_str() {
            "default" | "auto" | "daemon" => Ok(None),
            "off" | "none" => Ok(Some(ThinkingLevel::Off)),
            "minimal" | "min" => Ok(Some(ThinkingLevel::Minimal)),
            "low" => Ok(Some(ThinkingLevel::Low)),
            "medium" | "med" => Ok(Some(ThinkingLevel::Medium)),
            "high" => Ok(Some(ThinkingLevel::High)),
            "xhigh" | "x-high" | "extra-high" => Ok(Some(ThinkingLevel::Xhigh)),
            _ => Err("usage: /thinking default|off|minimal|low|medium|high|xhigh".into()),
        }
    }

    /// Execute a slash command by name, with any trailing `args` (empty for a
    /// palette pick; the tail of a typed `/name args` line otherwise). Clears
    /// the composer, then either mutates the transcript locally (`/clear`,
    /// `/help`), emits an [`Action`], or — for a `soon` roadmap command —
    /// surfaces an honest "not wired on this branch" hint. Pane focus rides
    /// [`Action::Navigate`]; chat never reaches into the app's Focus/Center.
    fn run_slash(&mut self, name: &str, args: &str) -> Option<Action> {
        self.input.clear();
        self.cursor = None;
        self.menu_sel = 0;
        let args = args.trim();
        match name {
            "/quit" => Some(Action::Quit),
            "/clear" => {
                self.turns.clear();
                self.md.clear();
                self.scroll_back = 0;
                self.busy = false;
                None
            }
            "/pinned" => match args {
                "hide" | "off" | "clear" => {
                    self.pinned_visible = false;
                    Some(Action::Status(
                        "pinned component hidden; /pinned show restores it".into(),
                    ))
                }
                "show" | "on" => {
                    if self.pinned.is_some() {
                        self.pinned_visible = true;
                        Some(Action::Status("pinned component shown".into()))
                    } else {
                        Some(Action::Status("no pinned component to show".into()))
                    }
                }
                "" => {
                    self.pinned_visible = !self.pinned_visible;
                    let state = if self.pinned_visible {
                        "shown"
                    } else {
                        "hidden"
                    };
                    Some(Action::Status(format!("pinned component {state}")))
                }
                _ => Some(Action::Status("usage: /pinned [show|hide]".into())),
            },
            "/help" => {
                self.push_help();
                self.scroll_back = 0;
                None
            }
            "/new" => {
                // Fresh session: wipe the transcript locally, then let the app
                // unbind so the next turn mints a new session id.
                self.turns.clear();
                self.md.clear();
                self.pinned = None;
                self.pinned_visible = true;
                self.scroll_back = 0;
                self.busy = false;
                Some(Action::NewSession)
            }
            "/model" => {
                if args.is_empty() {
                    // Bare `/model` opens the picker — nobody memorizes ids.
                    Some(Action::OpenModels)
                } else {
                    Some(Action::SetModel(args.to_string()))
                }
            }
            "/thinking" => match Self::parse_thinking(args) {
                Ok(level) => Some(Action::SetThinking(level)),
                Err(usage) => Some(Action::Status(usage)),
            },
            "/models" => Some(Action::OpenModels),
            "/advisor" => Some(Action::OpenAdvisor),
            "/memory" => Some(Action::OpenMemory),
            "/lsp" => Some(Action::OpenLsp),
            "/image" => {
                let path = if args.trim().is_empty() {
                    self.latest_image()
                } else {
                    Some(args.trim().to_string())
                };
                match path {
                    Some(p) => Some(Action::ViewImage(p)),
                    None => Some(Action::Status(
                        "no image in this chat yet (agents show one with ![](path))".into(),
                    )),
                }
            }
            "/login" => {
                // Bare `/login` opens the provider popup (OAuth + API keys);
                // `/login claude|codex` keeps the direct browser flow.
                if args.trim().is_empty() {
                    Some(Action::OpenProviders)
                } else {
                    match Self::login_target(args) {
                        Ok(target) => Some(Action::Login(target)),
                        Err(usage) => Some(Action::Status(usage)),
                    }
                }
            }
            "/providers" => Some(Action::OpenProviders),
            "/copy" => match self.last_reply() {
                Some(text) => Some(Action::CopyToClipboard(text)),
                None => Some(Action::Status("nothing to copy yet".into())),
            },
            // Pane/center navigation — the app owns Focus/Center, so emit a
            // targeted Navigate and let it move there.
            "/sessions" | "/resume" => Some(Action::Navigate(Nav::Sessions)),
            "/files" => Some(Action::Navigate(Nav::Files)),
            "/graph" => Some(Action::Navigate(Nav::Graph)),
            "/terminal" => Some(Action::Navigate(Nav::Terminal)),
            "/settings" => Some(Action::OpenSettings),
            "/permissions" => Some(Action::OpenPermissions),
            // Roadmap commands: present in the palette as a discoverability map,
            // but honest that the backend isn't on this branch yet.
            _ => {
                let hint = slash::COMMANDS
                    .iter()
                    .find(|c| c.name == name)
                    .filter(|c| c.soon)
                    .map(|c| format!("{} — {} · not wired on this branch yet", c.name, c.desc))
                    .unwrap_or_else(|| format!("unknown command: {name}"));
                Some(Action::Status(hint))
            }
        }
    }

    /// The text of the newest assistant reply, for `/copy`.
    fn last_reply(&self) -> Option<String> {
        self.turns.iter().rev().find_map(|t| match t {
            Turn::Assistant(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        })
    }

    /// Push `/help` output into the transcript as an assistant block — the
    /// markdown-lite renderer styles the headings, bullets, and inline `code`.
    /// Sections follow the registry's breadcrumb groups. Commands only: no
    /// shortcut list is printed anywhere, per the no-instructions house rule.
    fn push_help(&mut self) {
        let mut body = String::from("# commands\n");
        let mut last_group = "";
        for c in slash::COMMANDS {
            if c.group != last_group {
                body.push_str(&format!("\n## {}\n", c.group));
                last_group = c.group;
            }
            body.push_str(&format!("- `{}` — {}\n", c.name, c.desc));
        }
        self.turns.push(Turn::Assistant(body));
    }

    /// Render the floating command palette just above the composer. On the bare
    /// `/` the registry renders as breadcrumbed group SECTIONS (session /
    /// workspace / chat / roadmap groups) with muted headers, like the file
    /// tree's grouping; while filtering, rows are flat ranked matches carrying a
    /// muted `group ›` breadcrumb so you still see where a command lives. The
    /// selection rides on a `BG_HL` bed. Overlaid last so it sits on top of the
    /// transcript.
    fn draw_menu(&self, frame: &mut Frame, composer: Rect, matches: &[&slash::SlashCommand]) {
        if matches.is_empty() {
            return;
        }
        // Grouped mode on the bare `/` (matches are in registry order there).
        let grouped = self.input == "/";

        // Build the visible row list: command rows indexed into `matches`, plus
        // (in grouped mode) a header row at each group boundary.
        enum Row<'a> {
            Header(&'static str),
            Cmd(usize, &'a slash::SlashCommand),
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut last_group = "";
        for (i, c) in matches.iter().enumerate() {
            if grouped && c.group != last_group {
                rows.push(Row::Header(c.group));
                last_group = c.group;
            }
            rows.push(Row::Cmd(i, c));
        }

        let cap = (composer.y as usize).saturating_sub(3).clamp(1, 26);
        let shown = rows.len().min(cap);
        let sel = self.menu_sel.min(matches.len() - 1);

        // Width fits the widest ACTUAL row, capped to the composer width. A
        // command row renders as ` {marker} ` (3) + breadcrumb (filtered mode)
        // + `{:<11}` name + ` — ` (3) + desc + the soon badge; mirror that
        // exactly or long descriptions clip mid-word.
        const BADGE_W: usize = 7; // " · soon"
        let crumb_w = |c: &slash::SlashCommand| {
            if grouped {
                0
            } else {
                c.group.chars().count() + 3 // "group › "
            }
        };
        let content_w = rows
            .iter()
            .take(shown)
            .map(|r| match r {
                Row::Header(gname) => 2 + gname.chars().count(),
                Row::Cmd(_, c) => {
                    3 + crumb_w(c)
                        + c.name.chars().count().max(11)
                        + 3
                        + c.desc.chars().count()
                        + if c.soon { BADGE_W } else { 0 }
                }
            })
            .max()
            .unwrap_or(24);
        let width = ((content_w as u16) + 2/* borders */)
            .min(composer.width)
            .max(24);
        let height = shown as u16 + 2; // top+bottom border
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " commands ",
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Scroll the row window to keep the SELECTED row visible — with the
        // registry grown past the overlay cap, the cursor used to walk off the
        // bottom (or the top groups were simply unreachable) while the window
        // stayed pinned to the first rows.
        let sel_row = rows
            .iter()
            .position(|r| matches!(r, Row::Cmd(i, _) if *i == sel))
            .unwrap_or(0);
        let scroll = sel_row
            .saturating_sub(shown.saturating_sub(1))
            .min(rows.len().saturating_sub(shown));

        for (row_i, row) in rows.iter().enumerate().skip(scroll).take(shown) {
            let row_y = (row_i - scroll) as u16;
            match row {
                Row::Header(gname) => {
                    // Muted section header, file-tree style: `▾ group`.
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(
                                format!(" {} ", g("▾", "v")),
                                Style::default().fg(theme::DEEPBLUE),
                            ),
                            Span::styled(
                                gname.to_string(),
                                Style::default()
                                    .fg(theme::BLUE)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]))
                        .style(Style::default().bg(theme::SLATE)),
                        Rect::new(inner.x, inner.y + row_y, inner.width, 1),
                    );
                }
                Row::Cmd(i, c) => {
                    let selected = *i == sel;
                    let bed = if selected { theme::BG_HL } else { theme::SLATE };
                    // Live rows read bold in FG/CYAN; roadmap (soon) rows are
                    // muted so the working commands stay visually dominant.
                    let (name_fg, name_mod) = match (selected, c.soon) {
                        (true, false) => (theme::CYAN, Modifier::BOLD),
                        (true, true) => (theme::YELLOW, Modifier::empty()),
                        (false, false) => (theme::FG, Modifier::BOLD),
                        (false, true) => (theme::COMMENT, Modifier::empty()),
                    };
                    let marker = if selected { g("❯", ">") } else { " " };
                    let mut spans = vec![Span::styled(
                        format!(" {marker} "),
                        Style::default().fg(theme::CYAN),
                    )];
                    if !grouped {
                        // Filtered rows carry their breadcrumb: `group › /name`.
                        spans.push(Span::styled(
                            format!("{} {} ", c.group, g("›", ">")),
                            Style::default().fg(theme::DEEPBLUE),
                        ));
                    }
                    spans.push(Span::styled(
                        format!("{:<11}", c.name),
                        Style::default().fg(name_fg).add_modifier(name_mod),
                    ));
                    spans.push(Span::styled(
                        format!(" {} {}", g("—", "-"), c.desc),
                        Style::default().fg(theme::COMMENT),
                    ));
                    if c.soon {
                        let left_w = 3
                            + crumb_w(c)
                            + c.name.chars().count().max(11)
                            + 3
                            + c.desc.chars().count();
                        let target = inner.width as usize;
                        if target > left_w + BADGE_W {
                            spans.push(Span::raw(" ".repeat(target - left_w - BADGE_W)));
                        }
                        spans.push(Span::styled(
                            format!(" {} soon", g("·", "-")),
                            Style::default().fg(theme::YELLOW),
                        ));
                    }
                    frame.render_widget(
                        Paragraph::new(Line::from(spans)).style(Style::default().bg(bed)),
                        Rect::new(inner.x, inner.y + row_y, inner.width, 1),
                    );
                }
            }
        }
        // No footer legend: overlays carry no printed instructions, and the
        // selection window already scrolls to keep the cursor visible.
    }

    /// Render the `@` file-mention picker just above the composer: ranked file
    /// paths with the basename emphasized, selection on a `BG_HL` bed. Same
    /// floating idiom as the `/` palette.
    fn draw_mentions(&self, frame: &mut Frame, composer: Rect, matches: &[String]) {
        if matches.is_empty() {
            return;
        }
        let cap = (composer.y as usize).saturating_sub(3).clamp(1, 8);
        let shown = matches.len().min(cap);
        let sel = self.mention_sel.min(shown - 1);

        let content_w = matches
            .iter()
            .take(shown)
            .map(|p| 3 + 1 + p.chars().count()) // " ❯ " + "@" + path
            .max()
            .unwrap_or(24);
        let width = ((content_w as u16) + 2).min(composer.width).max(24);
        let height = shown as u16 + 2;
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " files ",
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        for (i, p) in matches.iter().take(shown).enumerate() {
            let selected = i == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let marker = if selected { g("❯", ">") } else { " " };
            // dir prefix muted, basename bright — you pick by basename.
            let (dir, base) = match p.rsplit_once('/') {
                Some((d, b)) => (format!("{d}/"), b.to_string()),
                None => (String::new(), p.clone()),
            };
            let spans = vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme::CYAN)),
                Span::styled("@", Style::default().fg(theme::CYAN)),
                Span::styled(dir, Style::default().fg(theme::COMMENT)),
                Span::styled(
                    base,
                    Style::default()
                        .fg(if selected { theme::CYAN } else { theme::FG })
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
    }

    /// Render the ⌃R history-search overlay above the composer: a title row
    /// carrying the live query, then fuzzy-matched history entries (newest
    /// first) with the selection on a `BG_HL` bed. Same popup skin as the `/`
    /// palette.
    fn draw_search(&self, frame: &mut Frame, composer: Rect) {
        let matches = self.search_matches();
        let query = self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("");
        let sel = self.search.as_ref().map(|s| s.sel).unwrap_or(0);
        let shown = matches.len().min(8);
        let sel = sel.min(shown.saturating_sub(1));

        // Width fits the widest entry (capped), with a sane floor.
        let content_w = matches
            .iter()
            .take(shown)
            .filter_map(|&i| self.history.get(i))
            .map(|e| e.chars().take(60).count())
            .max()
            .unwrap_or(24)
            .max(query.chars().count() + 4);
        let width = ((content_w as u16) + 6).min(composer.width).max(24);
        let height = shown.max(1) as u16 + 2; // top+bottom border
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" {} history: {query}", g("⌕", "?")),
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " no matching history",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        for (i, &hidx) in matches.iter().take(shown).enumerate() {
            let selected = i == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let fg = if selected { theme::CYAN } else { theme::FG };
            let marker = if selected { g("❯", ">") } else { " " };
            // Flatten newlines (multi-line prompts) into the one-row preview.
            let preview = self
                .history
                .get(hidx)
                .map(|e| one_line(e, inner.width.saturating_sub(4) as usize))
                .unwrap_or_default();
            let row = Line::from(vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme::CYAN)),
                Span::styled(preview, Style::default().fg(fg)),
            ]);
            frame.render_widget(
                Paragraph::new(row).style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
    }
}

impl Component for ChatComponent {
    /// Bracketed paste into the chat surface. While the ⌃R search overlay is
    /// open the paste feeds its single-line query; otherwise it inserts into
    /// the composer verbatim (newlines included) and NEVER submits.
    fn handle_paste(&mut self, text: &str) -> Option<Action> {
        if let Some(s) = self.search.as_mut() {
            s.query.extend(text.chars().filter(|c| !c.is_control()));
            s.sel = 0;
            return None;
        }
        let clean = paste_text(text);
        if clean.is_empty() {
            return None;
        }
        self.insert_at_cursor(&clean);
        self.reset_history_nav();
        self.menu_sel = 0;
        self.mention_sel = 0;
        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The ⌃R history-search overlay is modal: while open, all keys drive it.
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        // Confirm component resolution: a pending confirm in the transcript
        // intercepts Y/N before they reach the composer. Find the most-recent
        // unresolved confirm and resolve it.
        if let Some(code) = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
            _ => None,
        } {
            // Without modifiers: don't hijack Ctrl-Y (permission) or Ctrl-N.
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                for turn in self.turns.iter_mut().rev() {
                    if let Turn::Component { kind, resolved, .. } = turn {
                        if kind == "confirm" && resolved.is_none() {
                            *resolved = Some(code);
                            self.scroll_back = 0;
                            // In a future phase, POST to /v1/component/event here.
                            return None;
                        }
                    }
                }
            }
        }
        // Permission decisions work even mid-stream (legacy ⌃Y/⌃N bindings).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') => {
                    if let Some(id) = self.pending_permission() {
                        return Some(Action::PermissionDecided {
                            permission_id: id,
                            allow: true,
                        });
                    }
                    self.yank();
                    return None;
                }
                KeyCode::Char('n') => {
                    if let Some(id) = self.pending_permission() {
                        return Some(Action::PermissionDecided {
                            permission_id: id,
                            allow: false,
                        });
                    }
                    return None;
                }
                // ⌃J: newline in the composer at the cursor.
                KeyCode::Char('j') => {
                    self.insert_at_cursor("\n");
                    return None;
                }
                // ⌃O: toggle tool-card expansion.
                KeyCode::Char('o') => {
                    self.tools_expanded = !self.tools_expanded;
                    return None;
                }
                // ⌃R: open the fuzzy prompt-history search overlay.
                KeyCode::Char('r') => {
                    self.search = Some(HistorySearch::default());
                    return None;
                }
                // ⌃U: kill from line start to cursor.
                KeyCode::Char('u') => {
                    self.kill_to_start();
                    return None;
                }
                // ⌃K: kill from cursor to line end.
                KeyCode::Char('k') => {
                    self.kill_to_end();
                    return None;
                }
                // ⌃W: kill word before cursor.
                KeyCode::Char('w') => {
                    self.kill_word_backward();
                    return None;
                }
                // ⌃B / ⌃F: cursor left / right.
                KeyCode::Char('b') => {
                    self.cursor_left();
                    return None;
                }
                KeyCode::Char('f') => {
                    self.cursor_right();
                    return None;
                }
                // ⌃A / ⌃E: line start / line end.
                KeyCode::Char('a') => {
                    self.cursor_line_start();
                    return None;
                }
                KeyCode::Char('e') => {
                    self.cursor_line_end();
                    return None;
                }
                // ⌃D: delete the char at cursor (or end-of-input no-op).
                KeyCode::Char('d') => {
                    let c = self.cursor_byte();
                    if c < self.input.len() {
                        let ch = self.input[c..].chars().next().unwrap();
                        let end = c + ch.len_utf8();
                        self.input.drain(c..end);
                        if self.cursor.is_some_and(|cur| cur >= self.input.len()) {
                            self.cursor = None;
                        }
                        self.reset_history_nav();
                        self.menu_sel = 0;
                        self.mention_sel = 0;
                    }
                    return None;
                }
                // ⌃L: clear transcript when idle, no-op while busy. Preserves
                //      composer text and cursor position.
                KeyCode::Char('l') => {
                    if !self.busy {
                        self.turns.clear();
                        self.md.clear();
                        self.scroll_back = 0;
                    }
                    return None;
                }
                _ => {}
            }
        }
        // ── Alt drawer navigation: traverse tool drawers in transcript order ─
        // Alt-↓/↑ move focus across tool drawers (wrapping end-to-end); Alt-Space
        // or Alt-Enter flips the focused drawer's open state. These NEVER touch
        // the composer: plain ↓/↑ recall history and plain Space/Enter edit/send,
        // so the disclosure controls stay out of the typing path.
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Down => {
                    self.drawer_focus_next();
                    return None;
                }
                KeyCode::Up => {
                    self.drawer_focus_prev();
                    return None;
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.toggle_focused_drawer();
                    return None;
                }
                _ => {}
            }
        }
        // ── `/` command palette: nav + execute intercept ─────────────────────
        // Active only while the composer is in command mode (input starts with
        // `/`). These keys drive the menu instead of the composer. NOTE: Tab is
        // swallowed by the app for focus-cycling before it reaches us, so Enter
        // is the primary execute key; Tab-to-complete is handled defensively in
        // case that routing ever changes.
        let matches = self.slash_matches();
        if !matches.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.menu_sel = self.menu_sel.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.menu_sel = (self.menu_sel + 1).min(matches.len() - 1);
                    return None;
                }
                KeyCode::Tab => {
                    let sel = self.menu_sel.min(matches.len() - 1);
                    self.input = matches[sel].name.to_string();
                    self.menu_sel = 0;
                    self.cursor = None;
                    return None;
                }
                KeyCode::Enter => {
                    let sel = self.menu_sel.min(matches.len() - 1);
                    let name = matches[sel].name;
                    return self.run_slash(name, "");
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.menu_sel = 0;
                    self.cursor = None;
                    return None;
                }
                _ => {}
            }
        }
        // ── `@` file-mention picker: nav + insert intercept ──────────────────
        // Active while the trailing token starts with `@` and files match. Enter
        // / Tab insert the selected path into the composer (send stays a second
        // Enter); Esc drops the `@` sigil, closing the picker but keeping the
        // typed text.
        let mentions = self.mention_matches();
        if !mentions.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.mention_sel = self.mention_sel.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.mention_sel = (self.mention_sel + 1).min(mentions.len() - 1);
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    let sel = self.mention_sel.min(mentions.len() - 1);
                    let path = mentions[sel].clone();
                    self.insert_mention(&path);
                    return None;
                }
                KeyCode::Esc => {
                    if let Some((at, _, _)) = self.mention_query() {
                        self.input.remove(at); // drop the `@`, keep the text
                                               // Shift cursor left if it was past the removed sigil.
                        if let Some(c) = self.cursor {
                            if c > at {
                                self.cursor = Some(c - 1);
                            }
                        }
                    }
                    self.mention_sel = 0;
                    return None;
                }
                _ => {}
            }
        }
        match (key.code, key.modifiers) {
            (KeyCode::PageUp, _) => {
                self.scroll_back += 10;
                None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_back = self.scroll_back.saturating_sub(10);
                None
            }
            // ↑/↓ recall history when the composer is empty or already
            // navigating; otherwise they no-op (scrolling is wheel/PgUp).
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.history_prev();
                None
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.history_next();
                None
            }
            // Left / Right: cursor movement.
            (KeyCode::Left, _) => {
                self.cursor_left();
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Right, _) => {
                self.cursor_right();
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            // Home / End: line boundaries.
            (KeyCode::Home, _) => {
                self.cursor_line_start();
                None
            }
            (KeyCode::End, _) => {
                self.cursor_line_end();
                None
            }
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                if text.starts_with('/') {
                    let (name, args) = match text.split_once(char::is_whitespace) {
                        Some((n, a)) => (n, a),
                        None => (text.as_str(), ""),
                    };
                    if slash::is_command(name) {
                        return self.run_slash(name, args);
                    }
                    let looks_like_cmd = name
                        .strip_prefix('/')
                        .map(|n| {
                            !n.is_empty()
                                && n.chars().all(|c| {
                                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
                                })
                        })
                        .unwrap_or(false);
                    if looks_like_cmd {
                        let hint = if let Some(nearest) = slash::nearest(name) {
                            format!("unknown command {name} — did you mean {nearest}?  /help lists commands")
                        } else {
                            format!("unknown command {name} — /help lists commands")
                        };
                        self.turns.push(Turn::Assistant(hint.clone()));
                        return Some(Action::Status(hint));
                    }
                }
                self.history.push(&text);
                self.reset_history_nav();
                self.input.clear();
                self.cursor = None;
                self.scroll_back = 0;
                self.turns.push(Turn::User(text.clone()));
                self.busy = true;
                Some(Action::SubmitPrompt(text))
            }
            (KeyCode::Backspace, _) => {
                let c = self.cursor_byte();
                if c > 0 {
                    let ch = self.input[..c].chars().next_back().unwrap();
                    let start = c - ch.len_utf8();
                    self.input.drain(start..c);
                    self.cursor = (start < self.input.len()).then_some(start);
                }
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Delete, _) => {
                let c = self.cursor_byte();
                if c < self.input.len() {
                    let ch = self.input[c..].chars().next().unwrap();
                    let end = c + ch.len_utf8();
                    self.input.drain(c..end);
                    if self.cursor.is_some_and(|cur| cur >= self.input.len()) {
                        self.cursor = None;
                    }
                }
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.insert_at_cursor(c.encode_utf8(&mut [0u8; 4]));
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            _ => None,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_back += 3,
            MouseEventKind::ScrollDown => self.scroll_back = self.scroll_back.saturating_sub(3),
            // Left click on a rendered drawer header focuses and toggles that
            // one call. `drawer_hits` is rebuilt every draw from the exact
            // wrapped + scroll-adjusted geometry, so the hit routes to the
            // header actually painted under the cursor — correct even after a
            // preceding turn wraps, after scrolling, or after a resize. Wheel
            // scrolling is untouched. The toggle commits on a CLEAN click
            // only: the hit is armed on Down
            // and committed on Up, and any Drag in between disarms it (the
            // app-level drag-to-select-text gesture starts on the same Down,
            // so a selection sweep across a header must not toggle it). While
            // a composer overlay (⌃R search, `/` palette, `@` mentions) is up,
            // drawer hit-handling is skipped entirely: overlay rows paint OVER
            // transcript rows, and a click there must not fall through to the
            // hidden header underneath.
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending_drawer_click = if self.overlay_active() {
                    None
                } else {
                    self.drawer_hits
                        .iter()
                        .find(|h| {
                            h.row == mouse.row
                                && mouse.column >= h.col_start
                                && mouse.column < h.col_end
                        })
                        .map(|h| h.id.clone())
                };
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Text selection, not a click — disarm the pending toggle.
                self.pending_drawer_click = None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(id) = self.pending_drawer_click.take() {
                    if !self.overlay_active() {
                        self.focused_drawer = Some(id.clone());
                        self.toggle_drawer(&id);
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn update(&mut self, action: &Action) -> Option<Action> {
        // The turn (or its session mint) never reached the daemon, even after
        // the blip-retry window: unwind the spinner, say so in the transcript,
        // and put the prompt back in the composer so nothing typed is lost.
        if let Action::TurnSendFailed { prompt, err } = action {
            self.busy = false;
            let msg = errfmt::humanize(err);
            let prefix = if errfmt::is_connect_shaped(err) {
                "couldn't reach the daemon"
            } else {
                "turn could not start"
            };
            self.turns.push(Turn::Assistant(format!(
                "{} {prefix} — {msg}\n\nYour prompt is back in the composer.",
                g("⚠", "!")
            )));
            if self.input.is_empty() {
                self.input = prompt.clone();
            }
            self.scroll_back = 0;
            return None;
        }
        // The request connected, but the final HTTP outcome was lost. The
        // daemon may already have accepted/executed it, so restoring the prompt
        // would invite a duplicate browser action, write, or purchase.
        if let Action::TurnOutcomeUnknown { err } = action {
            self.busy = false;
            self.turns.push(Turn::Assistant(format!(
                "{} turn connection ended before confirmation — {}\n\nThe prompt was not restored because the turn may already be running. Watch the session stream before resubmitting.",
                g("⚠", "!"),
                errfmt::humanize(err)
            )));
            self.scroll_back = 0;
            return None;
        }
        // Permission traffic rides the GLOBAL event stream, not the agent one.
        if let Action::OceanEvent(env) = action {
            match &env.event {
                OceanEvent::PermissionRequest { tool, reason, .. } => {
                    if let Some(pid) = env.permission_id {
                        self.turns.push(Turn::Permission {
                            permission_id: pid,
                            tool: tool.clone(),
                            reason: reason.clone(),
                            resolved: None,
                        });
                        self.scroll_back = 0; // surface the prompt immediately
                    }
                }
                OceanEvent::PermissionDecision { allowed, .. } => {
                    if let Some(pid) = env.permission_id {
                        self.resolve_permission(pid, *allowed);
                    }
                }
                _ => {}
            }
            return None;
        }
        if let Action::AgentEvent(evt) = action {
            match evt.as_ref() {
                AgentTurnEvent::TurnStarted { model, .. } => {
                    // A fresh turn invalidates the previous throughput reading.
                    self.last_tok_per_s = None;
                    if let Some(m) = model {
                        self.model = Some(m.clone());
                    }
                    self.busy = true;
                }
                // Failover honesty (OCEAN-275): the daemon rerouted this turn to
                // a different model than requested. Render it as a concern card
                // in the transcript (the status line alone is easy to miss) and
                // update the pill to the model actually answering.
                AgentTurnEvent::ModelRerouted {
                    requested,
                    effective,
                    reason,
                    ..
                } => {
                    self.model = Some(effective.clone());
                    self.turns.push(Turn::Advisor {
                        note: format!(
                            "{requested} unavailable — turn running on {effective}. {reason}"
                        ),
                        severity: "concern".into(),
                        model: effective.clone(),
                    });
                }
                AgentTurnEvent::AssistantTextDelta { delta, .. } => self.push_assistant(delta),
                AgentTurnEvent::ThinkingDelta { delta, .. } => self.push_thinking(delta),
                AgentTurnEvent::ToolCallStarted { call, .. } => {
                    let name = call.name.to_string();
                    // Edit tools render as diff cards; a malformed payload yields
                    // `None` and falls back to the plain output card.
                    let diff = diff::is_edit_tool(&name)
                        .then(|| diff::edit_tool_diff(&name, &call.args_json))
                        .flatten();
                    self.turns.push(Turn::Tool {
                        id: call.id.clone(),
                        name,
                        args_json: call.args_json.clone(),
                        output: String::new(),
                        status: ToolStatus::Running,
                        diff,
                        expanded: false,
                    });
                }
                AgentTurnEvent::ToolCallChunk { call_id, chunk, .. } => {
                    if let Some(Turn::Tool { output, .. }) = self.tool_by_id(call_id) {
                        output.push_str(chunk);
                    }
                }
                AgentTurnEvent::ToolCallFinished {
                    call_id, result, ..
                } => {
                    let ok = result.ok;
                    if let Some(Turn::Tool { status, output, .. }) = self.tool_by_id(call_id) {
                        *status = if ok { ToolStatus::Ok } else { ToolStatus::Err };
                        if output.is_empty() {
                            *output = result.output.clone();
                        }
                    }
                }
                AgentTurnEvent::ComponentRender {
                    component_id,
                    kind,
                    props,
                    replace,
                    ..
                } => {
                    let pinned = props
                        .get("pinned")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if *replace {
                        if self.pinned.as_ref().is_some_and(
                            |turn| matches!(turn, Turn::Component { id, .. } if id == component_id),
                        ) {
                            self.pinned = None;
                        }
                        self.turns.retain(|turn| {
                            !matches!(turn, Turn::Component { id, .. } if id == component_id)
                        });
                    }
                    if pinned {
                        self.pinned = Some(Turn::Component {
                            id: component_id.clone(),
                            kind: kind.clone(),
                            props: props.clone(),
                            resolved: None,
                        });
                        // A fresh agent pin is intentional; reveal it even if a
                        // previous pinned artifact was hidden by the operator.
                        self.pinned_visible = true;
                    } else {
                        self.turns.push(Turn::Component {
                            id: component_id.clone(),
                            kind: kind.clone(),
                            props: props.clone(),
                            resolved: None,
                        });
                    }
                    self.scroll_back = 0;
                }
                AgentTurnEvent::ComponentUnmount { component_id, .. } => {
                    if self.pinned.as_ref().is_some_and(
                        |p| matches!(p, Turn::Component { id, .. } if id == component_id),
                    ) {
                        self.pinned = None;
                    }
                    self.turns.retain(
                        |turn| !matches!(turn, Turn::Component { id, .. } if id == component_id),
                    );
                }
                AgentTurnEvent::TurnFinished {
                    status,
                    error,
                    tokens_per_second,
                    ..
                } => {
                    self.busy = false;
                    self.last_tok_per_s = *tokens_per_second;
                    let is_failure = matches!(
                        status,
                        ocean_agent_sdk::AgentTurnStatus::Failed
                            | ocean_agent_sdk::AgentTurnStatus::Cancelled
                    );
                    if is_failure || error.is_some() {
                        let note = if let Some(e) = error {
                            format!("{} turn failed — {}", g("✗", "X"), errfmt::humanize(e))
                        } else {
                            format!("{} turn failed — no error detail", g("✗", "X"))
                        };
                        self.turns.push(Turn::ErrorNotice { note });
                    }
                }
                AgentTurnEvent::Extension {
                    extension, payload, ..
                } if extension == "advisor" => self.push_advisor(payload),
                _ => {}
            }
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Composer grows with its content — explicit ⌃J lines AND soft-wrapped
        // rows of long lines (typing past the right edge used to just clip:
        // the words kept landing but never showed). Capped so the transcript
        // keeps the room; past the cap the composer scrolls to the cursor.
        let usable = (area.width.saturating_sub(2)).max(1) as usize;
        let c = self.cursor_byte();
        let cursor_line_idx = self.input[..c].matches('\n').count();
        let input_rows: u16 = self
            .input
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                // The block cursor occupies one extra cell on its logical line, so
                // a line exactly at the width still wraps its cursor visibly.
                let visual_w = UnicodeWidthStr::width(l) + usize::from(i == cursor_line_idx);
                (visual_w.max(1)).div_ceil(usable) as u16
            })
            .sum::<u16>()
            .max(1);
        let input_lines = input_rows.min(8).min((area.height / 2).max(1));
        let pinned_lines: u16 = if self.pinned.is_some() && self.pinned_visible {
            // A pinned component gets 3-5 rows depending on kind
            match self.pinned.as_ref() {
                Some(Turn::Component { kind, .. }) => match kind.as_str() {
                    "timeline" => 6u16.min(area.height / 4),
                    "table" | "file_tree" => 8u16.min(area.height / 3),
                    "chart" | "stat" => 5u16.min(area.height / 4),
                    _ => 3u16.min(area.height / 5),
                },
                _ => 3u16.min(area.height / 5),
            }
        } else {
            0
        };
        let constraints: Vec<Constraint> = if pinned_lines > 0 {
            vec![
                Constraint::Min(3),
                Constraint::Length(pinned_lines),
                Constraint::Length(input_lines + 1),
            ]
        } else {
            vec![Constraint::Min(3), Constraint::Length(input_lines + 1)]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        let (transcript_area, composer_area) = if pinned_lines > 0 {
            (chunks[0], chunks[2])
        } else {
            (chunks[0], chunks[1])
        };

        // Command palette state for this frame (empty unless in `/`-mode, and
        // suppressed while the ⌃R search overlay owns the screen). Clamp the
        // selection cursor in case the match set shrank since last keypress.
        let menu = if self.search.is_some() {
            Vec::new()
        } else {
            self.slash_matches()
        };
        if !menu.is_empty() {
            self.menu_sel = self.menu_sel.min(menu.len() - 1);
        }

        // ── transcript panel in the CTRL skin ────────────────────────────────
        // Title-less, pill-less chrome: the app title bar + breadcrumb already
        // identify this pane, and the bound model lives on the bottom status
        // row — a panel pill duplicated it.
        let body = panel::draw(frame, transcript_area, "", None, self.focused);

        // ── pinned component (between transcript and composer) ───────────────
        if let Some(Turn::Component { kind, props, .. }) = &self.pinned {
            if pinned_lines > 0 {
                let pinned_area = chunks[1];
                let pinned_rows = component_lines(kind, props, pinned_area.width as usize);
                let max_rows = (pinned_lines as usize).saturating_sub(1).max(1);
                let trimmed: Vec<Line> = if pinned_rows.len() > max_rows {
                    let mut t = pinned_rows[..max_rows].to_vec();
                    t.push(Line::from(Span::styled(
                        format!("  {} pinned, scroll for more", g("┄", "..")),
                        Style::default().fg(theme::COMMENT),
                    )));
                    t
                } else {
                    pinned_rows
                };
                frame.render_widget(
                    Paragraph::new(trimmed)
                        .block(Block::default().style(Style::default().bg(theme::BG_DARK))),
                    pinned_area,
                );
            }
        }

        // Transcript lines (bottom-anchored via scroll offset). Split the
        // borrow: the markdown cache (`md`) is a distinct field from `turns`, so
        // the loop can read turns while `md.render` mutates its cache.
        let md = &mut self.md;
        let tools_expanded = self.tools_expanded;
        let busy = self.busy;
        let n_turns = self.turns.len();
        let mut lines: Vec<Line> = Vec::new();
        // Per-frame record of each drawer header's logical-line index, used
        // after wrapping + scroll to build the mouse hit map (`self.drawer_hits`).
        let mut drawer_header_lines: Vec<(ToolCallId, usize)> = Vec::new();
        // ── welcome empty-state: blank except a terse configuration condition
        // (set by the app only when no provider is configured). No branding,
        // no printed instructions — the `/` palette and /help carry discovery.
        if self.turns.is_empty() {
            if let Some(pline) = &self.welcome_provider_line {
                let vpad = (body.height.saturating_sub(1)) / 2;
                for _ in 0..vpad {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(
                    format!("  {pline}"),
                    Style::default().fg(theme::YELLOW),
                )));
            }
        }
        for (ti, turn) in self.turns.iter().enumerate() {
            match turn {
                Turn::User(s) => {
                    lines.push(Line::from(vec![
                        Span::styled(g("❯ ", "> "), Style::default().fg(theme::CYAN)),
                        Span::styled(
                            sanitize_line(s),
                            Style::default()
                                .fg(theme::CYAN)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                Turn::Assistant(s) => {
                    // Streaming markdown with prefix-freeze: frozen head blocks
                    // are served from cache, only the growing tail re-renders.
                    lines.extend(md.render(s));
                }
                Turn::Component {
                    kind,
                    props,
                    resolved,
                    ..
                } => {
                    if kind == "confirm" && resolved.is_some() {
                        let confirmed = resolved.unwrap_or(false);
                        let (mark, color) = if confirmed {
                            (g("✓", "+"), theme::GREEN)
                        } else {
                            (g("✗", "x"), theme::RED)
                        };
                        let text = props
                            .get("body")
                            .or_else(|| props.get("title"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let inner = (body.width as usize).saturating_sub(6).clamp(8, 48);
                        lines.push(Line::from(Span::styled(
                            format!(
                                "  ╭─ {} {}",
                                truncate_to_width(&sanitize_line(text), inner / 2),
                                "─".repeat(inner)
                            ),
                            Style::default().fg(theme::EDGE),
                        )));
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(theme::EDGE)),
                            Span::styled(
                                format!(
                                    "{mark} {} — {}",
                                    if confirmed { "confirmed" } else { "cancelled" },
                                    sanitize_line(text)
                                ),
                                Style::default().fg(color),
                            ),
                        ]));
                        lines.push(Line::from(Span::styled(
                            format!("  ╰{}", "─".repeat(inner + 3)),
                            Style::default().fg(theme::EDGE),
                        )));
                    } else {
                        lines.extend(component_lines(kind, props, body.width as usize));
                    }
                }
                Turn::Permission {
                    tool,
                    reason,
                    resolved,
                    ..
                } => {
                    // Approval card: loud while pending, quiet once decided.
                    match resolved {
                        None => {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("  {} approval needed: ", g("⚠", "!")),
                                    Style::default()
                                        .fg(theme::YELLOW)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    sanitize_line(tool),
                                    Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("  {} ", g("▎", "|")),
                                    Style::default().fg(theme::YELLOW),
                                ),
                                Span::styled(sanitize_line(reason), Style::default().fg(theme::FG)),
                            ]));
                        }
                        Some(true) => {
                            let st = sanitize_line(tool);
                            lines.push(Line::from(Span::styled(
                                format!("  {} allowed: {st}", g("✓", "+")),
                                Style::default().fg(theme::GREEN),
                            )));
                        }
                        Some(false) => {
                            let st = sanitize_line(tool);
                            lines.push(Line::from(Span::styled(
                                format!("  {} denied: {st}", g("✗", "x")),
                                Style::default().fg(theme::RED),
                            )));
                        }
                    }
                }
                Turn::Thinking(_) => {
                    // Collapsed mode shows thinking ONLY while it's the live
                    // tail of a busy turn (feedback that the model is working).
                    // Historical thinking markers between every tool call were
                    // half the transcript spam; ⌃O brings them all back.
                    if tools_expanded || (busy && ti + 1 == n_turns) {
                        lines.push(Line::from(Span::styled(
                            format!("  {} thinking", g("◌", "~")),
                            Style::default()
                                .fg(theme::COMMENT)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    } else {
                        continue; // no separator row either
                    }
                }
                Turn::Tool {
                    id,
                    name,
                    args_json,
                    output,
                    status,
                    diff,
                    expanded,
                } => {
                    // Each tool call is an independent drawer. A body is open
                    // when the per-call `expanded` OR the global ⌃O override is
                    // on; toggling one drawer never touches another.
                    let open = tools_expanded || *expanded;
                    let disc = if open { g("▾", "v") } else { g("▸", ">") };
                    let (status_word, status_color) = match status {
                        ToolStatus::Running => ("running", theme::YELLOW),
                        ToolStatus::Ok => ("done", theme::GREEN),
                        ToolStatus::Err => ("error", theme::RED),
                    };
                    // One NON-wrapping header row:
                    //   "  {disc} {name}[ · {preview}][ · {status}]"
                    // Reserve disclosure / name / status by terminal-cell width
                    // first, then let ONLY the preview truncate — so CJK/emoji
                    // args can never wrap the header (a wrap would invalidate
                    // the click hit map and break the one-row guarantee).
                    let width = body.width as usize;
                    let prefix = format!("  {disc} ");
                    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
                    let sep = " · ";
                    let sep_w = UnicodeWidthStr::width(sep);
                    let status_w = UnicodeWidthStr::width(status_word);
                    let name_budget = width.saturating_sub(prefix_w + sep_w + status_w);
                    let name_disp = truncate_to_width(name, name_budget);
                    let name_w = UnicodeWidthStr::width(name_disp.as_str());
                    let reserved = prefix_w + name_w + sep_w + status_w + sep_w;
                    let preview_budget = width.saturating_sub(reserved);
                    let ell = g("…", "...");
                    let ell_w = UnicodeWidthStr::width(ell);
                    let preview_raw = humanize_preview(name, args_json);
                    let show_preview = !preview_raw.is_empty() && preview_budget >= ell_w;
                    let preview_disp = if show_preview {
                        truncate_to_width(&preview_raw, preview_budget)
                    } else {
                        String::new()
                    };
                    let mut header: Vec<Span> = Vec::new();
                    header.push(Span::styled(prefix, Style::default().fg(theme::EDGE)));
                    header.push(Span::styled(
                        name_disp,
                        Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                    ));
                    if show_preview {
                        header.push(Span::styled(
                            format!("{sep}{preview_disp}"),
                            Style::default().fg(theme::COMMENT),
                        ));
                    }
                    header.push(Span::styled(
                        format!("{sep}{status_word}"),
                        Style::default().fg(status_color),
                    ));
                    // Hard width clamp: on very narrow panes the fixed parts
                    // (prefix + separators + status) can alone exceed the body
                    // width, and the status span is appended after budgeting —
                    // truncate the assembled spans by cell budget so the header
                    // can NEVER wrap to a second row (a wrap would invalidate
                    // the click hit map and break the one-row guarantee).
                    let mut cells_left = width;
                    for sp in header.iter_mut() {
                        let w = UnicodeWidthStr::width(sp.content.as_ref());
                        if w <= cells_left {
                            cells_left -= w;
                        } else {
                            sp.content = truncate_to_width(sp.content.as_ref(), cells_left).into();
                            cells_left = 0;
                        }
                    }
                    header.retain(|sp| !sp.content.is_empty());
                    // Focus highlight: reverse the complete header row.
                    if self.focused_drawer.as_ref() == Some(id) {
                        for sp in header.iter_mut() {
                            sp.style = sp.style.add_modifier(Modifier::REVERSED);
                        }
                    }
                    // Record the header's logical-line index for the mouse hit
                    // map BEFORE pushing, so the index points at the header row.
                    drawer_header_lines.push((id.clone(), lines.len()));
                    lines.push(Line::from(header));

                    if open {
                        // Lossless, terminal-sanitized args section. Lines may
                        // wrap; the exact per-line hit map accounts for those
                        // wrapped rows. Skip null / empty-object payloads.
                        let has_args = !args_json.is_null()
                            && !(matches!(args_json, serde_json::Value::Object(o) if o.is_empty()));
                        if has_args {
                            let pretty = serde_json::to_string_pretty(args_json)
                                .unwrap_or_else(|_| args_json.to_string());
                            for l in pretty.lines() {
                                lines.push(Line::from(vec![
                                    Span::styled(
                                        "    ".to_string(),
                                        Style::default().fg(theme::EDGE),
                                    ),
                                    Span::styled(
                                        sanitize_line(l),
                                        Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
                                    ),
                                ]));
                            }
                        }
                        // Body rail bounded to the newest DRAWER_BODY_ROWS rows
                        // (diff for edit tools, streamed output otherwise), each
                        // clamped to one screen row. The earlier-rows marker
                        // paints ABOVE the tail — the omitted rows are
                        // chronologically older than every visible one.
                        match diff {
                            Some(rows) => {
                                let total = rows.len();
                                let hidden = total.saturating_sub(DRAWER_BODY_ROWS);
                                if hidden > 0 {
                                    lines.push(Line::from(Span::styled(
                                        format!("    {} {} earlier lines", g("┄", ".."), hidden),
                                        Style::default().fg(theme::COMMENT),
                                    )));
                                }
                                for row in &rows[hidden..] {
                                    lines.push(diff_line(row, width));
                                }
                            }
                            None => {
                                let body_rows: Vec<&str> = output.lines().collect();
                                if body_rows.is_empty() {
                                    if matches!(status, ToolStatus::Running) {
                                        lines.push(Line::from(Span::styled(
                                            "    (no output yet)".to_string(),
                                            Style::default().fg(theme::COMMENT),
                                        )));
                                    }
                                } else {
                                    let hidden = body_rows.len().saturating_sub(DRAWER_BODY_ROWS);
                                    // "    │ " gutter = 6 cells; clamp each line
                                    // to the rest so it costs exactly one row.
                                    let rail_w = width.saturating_sub(6);
                                    if hidden > 0 {
                                        lines.push(Line::from(Span::styled(
                                            format!(
                                                "    {} {} earlier lines",
                                                g("┄", ".."),
                                                hidden
                                            ),
                                            Style::default().fg(theme::COMMENT),
                                        )));
                                    }
                                    for l in &body_rows[hidden..] {
                                        let text = truncate_to_width(&sanitize_line(l), rail_w);
                                        lines.push(Line::from(vec![
                                            Span::styled(
                                                format!("    {} ", g("│", "|")),
                                                Style::default().fg(theme::EDGE),
                                            ),
                                            Span::styled(
                                                text,
                                                Style::default()
                                                    .fg(theme::COMMENT)
                                                    .bg(theme::BG_DARK),
                                            ),
                                        ]));
                                    }
                                }
                            }
                        }
                    }
                }
                Turn::Advisor {
                    note,
                    severity,
                    model,
                } => {
                    // Severity → theme accent: blocker red, concern amber,
                    // info muted. Rendered as a set-off card with a │ gutter.
                    let accent = match severity.as_str() {
                        "blocker" => theme::RED,
                        "concern" => theme::YELLOW,
                        _ => theme::COMMENT,
                    };
                    let sev = sanitize_line(severity);
                    let mut header: Vec<Span> = vec![Span::styled(
                        format!("  {} advisor ({sev})", g("⚑", "!")),
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )];
                    if !model.is_empty() {
                        let san_model = sanitize_line(model);
                        header.push(Span::styled(
                            format!("  · {san_model}"),
                            Style::default()
                                .fg(theme::COMMENT)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    let notice_w = (body.width as usize).saturating_sub(2);
                    for l in note.lines() {
                        let sane = sanitize_line(l);
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {} ", g("▎", "|")),
                                Style::default().fg(accent),
                            ),
                            Span::styled(
                                clamp_line(&sane, notice_w),
                                Style::default().fg(theme::FG),
                            ),
                        ]));
                    }
                }
                Turn::ErrorNotice { note } => {
                    let notice_w = (body.width as usize).saturating_sub(2);
                    for l in note.lines() {
                        let sane = sanitize_line(l);
                        lines.push(Line::from(Span::styled(
                            format!("  {}", clamp_line(&sane, notice_w)),
                            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
                        )));
                    }
                }
            }
            // Single-space tool runs: a suppressed Thinking turn between tool
            // calls emits nothing (its arm `continue`s), so the NEXT VISIBLE
            // turn decides the gap — Tool -> hidden Thinking -> Tool stays
            // tight. Every other visible boundary keeps its blank separator.
            let next_visible_is_tool = self.turns[ti + 1..]
                .iter()
                .enumerate()
                .find_map(|(off, t)| match t {
                    Turn::Thinking(_) if !(tools_expanded || (busy && ti + 2 + off == n_turns)) => {
                        None
                    }
                    Turn::Tool { .. } => Some(true),
                    _ => Some(false),
                })
                .unwrap_or(false);
            if !(matches!(turn, Turn::Tool { .. }) && next_visible_is_tool) {
                lines.push(Line::from(""));
            }
        }
        // Bottom-anchor on the WRAPPED row count, not the raw line count — long
        // streamed lines reflow into multiple rows, and Paragraph's scroll
        // offset is in wrapped rows. Counting unwrapped lines made the live
        // tail jitter/scroll off as text arrived. `line_count` uses the exact
        // same wrap algorithm the render will.
        // Per-line wrapped-row counts (ratatui's exact algorithm) and the
        // cumulative wrapped-row offset where each drawer header first paints.
        // Computed before `lines` moves into the Paragraph; the scroll window
        // is applied once it is known. A header at logical line i lands on
        // wrapped row Σ(rows[0..i)), so a long assistant turn that wraps above
        // shifts the header down on screen — the hit map MUST follow that shift
        // or a click toggles the wrong drawer (the logical-index-as-row bug).
        let drawer_wrapped: Vec<(ToolCallId, u16, u16)> = if drawer_header_lines.is_empty() {
            Vec::new()
        } else {
            let per_line: Vec<u16> = lines
                .iter()
                .map(|l| {
                    Paragraph::new(vec![l.clone()])
                        .wrap(Wrap { trim: false })
                        .line_count(body.width) as u16
                })
                .collect();
            let mut mapped: Vec<(ToolCallId, u16, u16)> = Vec::new();
            let mut cum: u16 = 0;
            // `drawer_header_lines` is in ascending logical-index order (headers
            // are pushed while walking turns top-to-bottom), so one advancing
            // cursor matches every header in O(lines + drawers) instead of the
            // O(lines × drawers) a per-line rescan would cost.
            let mut headers = drawer_header_lines.iter().peekable();
            for (i, &rows) in per_line.iter().enumerate() {
                if headers.peek().is_some_and(|(_, li)| *li == i) {
                    let (id, _) = headers.next().unwrap();
                    mapped.push((id.clone(), cum, cum.saturating_add(rows)));
                }
                cum = cum.saturating_add(rows);
            }
            mapped
        };
        let para = Paragraph::new(lines)
            .style(Style::default().bg(theme::SLATE))
            .wrap(Wrap { trim: false });
        let wrapped = para.line_count(body.width) as u16;
        let max_back = wrapped.saturating_sub(body.height) as usize;
        self.scroll_back = self.scroll_back.min(max_back);
        let scroll = wrapped
            .saturating_sub(body.height)
            .saturating_sub(self.scroll_back as u16);
        // Apply the scroll window and body origin to the wrapped header rows,
        // yielding absolute screen rows for the mouse hit map. Only headers
        // inside the visible wrapped-row window are clickable; the vec is
        // always reassigned so stale entries from a prior frame never linger.
        let vis_lo = scroll;
        let vis_hi = scroll.saturating_add(body.height);
        self.drawer_hits = drawer_wrapped
            .iter()
            .filter_map(|(id, start, end)| {
                let row = (*start..*end).find(|r| *r >= vis_lo && *r < vis_hi)?;
                Some(DrawerHit {
                    id: id.clone(),
                    row: body.y + (row - vis_lo),
                    col_start: body.x,
                    col_end: body.x + body.width,
                })
            })
            .collect();
        frame.render_widget(para.scroll((scroll, 0)), body);
        // Footer: always blank. No key legends, no counters; activity lives on
        // the bottom status row (derived from this component's state), never
        // duplicated here. The reserved row stays so panel geometry is stable.
        panel::footer(frame, transcript_area, "");

        // ── composer: highlight bed, accent bar, multi-line, block cursor ────
        let comp = composer_area;
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_HL)),
            comp,
        );
        for k in 0..comp.height {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    g("▎", "|"),
                    Style::default().fg(if self.busy {
                        theme::COMMENT
                    } else {
                        theme::CYAN
                    }),
                ))
                .style(Style::default().bg(theme::BG_HL)),
                Rect::new(comp.x, comp.y + k, 1, 1),
            );
        }
        let input_fg = if self.busy { theme::COMMENT } else { theme::FG };
        let cursor_glyph = g("▏", "_");
        let cursor_style = Style::default().fg(theme::CYAN);

        // Cursor position within input
        let c = self.cursor_byte();
        let cursor_line_idx = self.input[..c].matches('\n').count();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let after_cursor_line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());

        // Build render lines: cursor line gets before+glyph+after spans
        let mut input_render: Vec<Line> = Vec::new();
        for (li, line_text) in self.input.split('\n').enumerate() {
            if li == cursor_line_idx {
                let before = &self.input[line_start..c];
                let after = &self.input[c..after_cursor_line_end];
                let mut spans: Vec<Span> = Vec::with_capacity(3);
                if !before.is_empty() {
                    spans.push(Span::styled(
                        before.to_string(),
                        Style::default().fg(input_fg),
                    ));
                }
                spans.push(Span::styled(cursor_glyph, cursor_style));
                if !after.is_empty() {
                    spans.push(Span::styled(
                        after.to_string(),
                        Style::default().fg(input_fg),
                    ));
                }
                input_render.push(Line::from(spans));
            } else {
                input_render.push(Line::from(Span::styled(
                    line_text.to_string(),
                    Style::default().fg(input_fg),
                )));
            }
        }
        // Preserve trailing empty line when input ends with '\n'
        if input_render.is_empty() || self.input.ends_with('\n') {
            input_render.push(Line::from(""));
        }

        // Wrap and scroll: keep the cursor's visual row visible, not always bottom.
        let input_w = (comp.width.saturating_sub(2)).max(1) as usize;
        let before_cursor = &self.input[line_start..c];
        let vis_before = UnicodeWidthStr::width(before_cursor);
        // Visual rows occupied by lines before the cursor's logical line
        let prior_rows: u16 = self
            .input
            .split('\n')
            .take(cursor_line_idx)
            .map(|l| {
                let vw = UnicodeWidthStr::width(l).max(1);
                vw.div_ceil(input_w) as u16
            })
            .sum();
        // Cursor's visual row within its own line (0-indexed)
        let cursor_row_in_line = vis_before.checked_div(input_w).unwrap_or(0) as u16;
        let cursor_vis_row = prior_rows + cursor_row_in_line;

        let input_para = Paragraph::new(input_render)
            .style(Style::default().bg(theme::BG_HL))
            .wrap(Wrap { trim: false });
        let wrapped_rows = input_para.line_count(input_w as u16) as u16;
        let max_scroll = wrapped_rows.saturating_sub(comp.height);
        // When cursor is below the visible area, scroll so it's on the last visible row
        let input_scroll = cursor_vis_row
            .saturating_sub(comp.height.saturating_sub(1))
            .min(max_scroll);
        frame.render_widget(
            input_para.scroll((input_scroll, 0)),
            Rect::new(comp.x + 2, comp.y, input_w as u16, comp.height),
        );

        // ── `/` command palette overlay, floated just above the composer ─────
        if !menu.is_empty() {
            self.draw_menu(frame, comp, &menu);
        }
        // ── `@` file-mention picker, floated just above the composer ─────────
        let mentions = self.mention_matches();
        if !mentions.is_empty() {
            self.draw_mentions(frame, comp, &mentions);
        }
        // ── ⌃R history-search overlay, floated just above the composer ───────
        if self.search.is_some() {
            self.draw_search(frame, comp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::action::LoginTarget;
    use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnId, AgentTurnStatus};
    use serde_json::json;
    use uuid::Uuid;

    /// A chat with the composer pre-filled — avoids the `field_reassign_with_default`
    /// clippy lint that fires on `let mut c = default(); c.input = …`.
    fn chat_with(input: &str) -> ChatComponent {
        ChatComponent {
            input: input.to_string(),
            ..Default::default()
        }
    }
    fn render_chat_to_string(chat: &mut ChatComponent, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| chat.draw(frame, frame.area()))
            .expect("draw chat");
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn extension(extension: &str, payload: serde_json::Value) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::Extension {
            extension: extension.to_string(),
            payload,
            scope: None,
        }))
    }

    #[test]
    fn component_render_replaces_and_unmounts_by_id() {
        let mut chat = ChatComponent::default();
        let sid = AgentSessionId(Uuid::new_v4());
        chat.update(&Action::AgentEvent(Box::new(
            AgentTurnEvent::ComponentRender {
                session_id: sid,
                component_id: "health".into(),
                kind: "progress".into(),
                props: json!({ "label": "build", "value": 0.25, "max": 1.0 }),
                replace: false,
            },
        )));
        chat.update(&Action::AgentEvent(Box::new(
            AgentTurnEvent::ComponentRender {
                session_id: sid,
                component_id: "health".into(),
                kind: "progress".into(),
                props: json!({ "label": "build", "value": 1.0, "max": 1.0 }),
                replace: true,
            },
        )));
        assert_eq!(
            chat.turns
                .iter()
                .filter(|t| matches!(t, Turn::Component { .. }))
                .count(),
            1
        );
        chat.update(&Action::AgentEvent(Box::new(
            AgentTurnEvent::ComponentUnmount {
                session_id: sid,
                component_id: "health".into(),
            },
        )));
        assert!(!chat
            .turns
            .iter()
            .any(|t| matches!(t, Turn::Component { .. })));
    }

    #[test]
    fn component_replace_moves_id_between_inline_and_pinned_without_duplicates() {
        let mut chat = ChatComponent::default();
        let sid = AgentSessionId(Uuid::new_v4());
        let render = |pinned| {
            Action::AgentEvent(Box::new(AgentTurnEvent::ComponentRender {
                session_id: sid,
                component_id: "health".into(),
                kind: "stat".into(),
                props: json!({ "title": "health", "pinned": pinned }),
                replace: true,
            }))
        };

        chat.update(&render(false));
        chat.update(&render(true));
        assert!(chat.pinned.is_some());
        assert!(!chat
            .turns
            .iter()
            .any(|turn| matches!(turn, Turn::Component { id, .. } if id == "health")));

        chat.update(&render(false));
        assert!(chat.pinned.is_none());
        assert_eq!(
            chat.turns
                .iter()
                .filter(|turn| matches!(turn, Turn::Component { id, .. } if id == "health"))
                .count(),
            1
        );
    }

    #[test]
    fn new_session_and_history_load_clear_pinned_components() {
        let mut chat = ChatComponent {
            pinned: Some(Turn::Component {
                id: "pin".into(),
                kind: "stat".into(),
                props: json!({}),
                resolved: None,
            }),
            ..Default::default()
        };
        chat.run_slash("/new", "");
        assert!(chat.pinned.is_none());

        chat.pinned = Some(Turn::Component {
            id: "pin".into(),
            kind: "stat".into(),
            props: json!({}),
            resolved: None,
        });
        chat.load_history(Vec::new());
        assert!(chat.pinned.is_none());
    }

    #[test]
    fn chart_projection_is_compact_and_terminal_safe() {
        let lines = component_lines(
            "chart",
            &json!({ "title": "load\ttrend", "series": [{"value": 1}, {"value": 4}] }),
            40,
        );
        assert!(lines.len() >= 3);
        assert!(lines.iter().any(|line| line.to_string().contains('█')));
        assert!(!lines.iter().any(|line| line.to_string().contains('\t')));

        let hostile = component_lines(
            "gallery",
            &json!({ "title": "x\u{1b}[31m", "images": [{"caption": "a\tb", "src": "x\r.png"}] }),
            30,
        );
        assert!(hostile.iter().all(|line| {
            let text = line.to_string();
            !text.contains('\u{1b}') && !text.contains('\t') && !text.contains('\r')
        }));
    }

    #[test]
    fn advisor_extension_appends_advisor_turn() {
        let mut chat = ChatComponent::default();
        chat.update(&extension(
            "advisor",
            json!({ "note": "consider a smaller diff", "severity": "concern", "model": "opus" }),
        ));
        assert_eq!(chat.turns.len(), 1);
        match &chat.turns[0] {
            Turn::Advisor {
                note,
                severity,
                model,
            } => {
                assert_eq!(note, "consider a smaller diff");
                assert_eq!(severity, "concern");
                assert_eq!(model, "opus");
            }
            _ => panic!("expected an advisor turn"),
        }
    }

    #[test]
    fn advisor_defaults_missing_fields() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("advisor", json!({ "note": "heads up" })));
        assert_eq!(chat.turns.len(), 1);
        match &chat.turns[0] {
            Turn::Advisor {
                severity, model, ..
            } => {
                assert_eq!(severity, "info");
                assert_eq!(model, "");
            }
            _ => panic!("expected an advisor turn"),
        }
    }

    #[test]
    fn empty_note_is_skipped() {
        let mut chat = ChatComponent::default();
        chat.update(&extension(
            "advisor",
            json!({ "note": "   ", "severity": "info" }),
        ));
        chat.update(&extension("advisor", json!({ "severity": "blocker" })));
        assert!(chat.turns.is_empty());
    }

    fn perm_envelope(pid: PermissionId, event: OceanEvent) -> Action {
        Action::OceanEvent(Box::new(ocean_core::EventEnvelope {
            id: ocean_core::EventId::new_v4(),
            at: chrono::Utc::now(),
            session_id: None,
            request_id: Some(ocean_core::RequestId::new_v4()),
            permission_id: Some(pid),
            origin: None,
            event,
        }))
    }

    #[test]
    fn permission_request_then_decision_resolves_card() {
        let mut chat = ChatComponent::default();
        let pid = PermissionId::new_v4();
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionRequest {
                tool: "bash".into(),
                reason: "rm -rf build".into(),
                args: json!({}),
            },
        ));
        assert_eq!(
            chat.pending_permission(),
            Some(pid),
            "card should be pending"
        );
        // ⌃Y targets the pending card.
        let act = chat.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(matches!(
            act,
            Some(Action::PermissionDecided { permission_id, allow: true }) if permission_id == pid
        ));
        // The daemon's decision event resolves it.
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionDecision {
                allowed: true,
                reason: None,
            },
        ));
        assert_eq!(chat.pending_permission(), None, "card should be resolved");
    }

    #[test]
    fn ctrl_o_toggles_tool_expansion() {
        let mut chat = ChatComponent::default();
        assert!(!chat.tools_expanded);
        chat.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(chat.tools_expanded);
        chat.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!chat.tools_expanded);
    }

    #[test]
    fn sanitize_line_expands_tabs_and_drops_controls() {
        // Raw tabs make the terminal jump to ITS tab stops while ratatui
        // paints at its own cell math — the smeared-bleed bug John hit with
        // the read tool's `<lineno>\t<code>` output lines.
        assert_eq!(sanitize_line("1108\t}"), "1108    }");
        // ESC/CR and friends drop; plain text passes through untouched.
        assert_eq!(sanitize_line("a\x1b[31mred\rb"), "a[31mredb");
        assert_eq!(sanitize_line("plain text"), "plain text");
    }

    #[test]
    fn clamp_line_truncates_but_preserves_whitespace() {
        // Unlike one_line, indentation must survive — it's signal in tool
        // output previews.
        assert_eq!(clamp_line("  indented ok", 40), "  indented ok");
        // A giant single line hard-stops at max chars (one screen row), with
        // an ellipsis marking the cut.
        let blob = "x".repeat(500);
        let clamped = clamp_line(&blob, 40);
        assert_eq!(clamped.chars().count(), 40);
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn humanize_preview_picks_salient_arg_and_flattens_to_one_line() {
        // bash → the command, whitespace flattened so it can't wrap a header.
        let bash = humanize_preview(
            "bash",
            &json!({ "command": "echo hi\nrm x", "cwd": "/tmp" }),
        );
        assert!(!bash.contains('\n'), "preview must be one line");
        assert!(bash.contains("echo hi"));
        assert!(bash.contains("rm x"));
        // grep → pattern + path joined; write → path only, NEVER content.
        assert_eq!(
            humanize_preview("grep", &json!({ "pattern": "TODO", "path": "/src" })),
            "TODO /src"
        );
        let write = humanize_preview("write", &json!({ "path": "/a.txt", "content": "secret" }));
        assert_eq!(write, "/a.txt");
        assert!(
            !write.contains("secret"),
            "header must not echo file content"
        );
        // Pathless write/edit: NEVER fall through to the scalar fallback —
        // that would print `content: <payload>` into the collapsed header.
        let pathless = humanize_preview("write", &json!({ "content": "secret" }));
        assert!(
            !pathless.contains("secret"),
            "pathless write must not echo content: {pathless}"
        );
        assert!(pathless.is_empty(), "pathless write previews as empty");
        // Unknown/MCP tool with scalar args → readable `key: value` fallback.
        let unk = humanize_preview("mcp_foo", &json!({ "foo": 1, "bar": "baz" }));
        assert!(unk.contains("foo: 1"), "scalar fallback: {unk}");
        assert!(unk.contains("bar: baz"), "string fallback: {unk}");
    }

    #[test]
    fn slash_prefix_opens_palette() {
        let chat = chat_with("/mod");
        assert!(ChatComponent::default().slash_matches().is_empty());
        let m = chat.slash_matches();
        assert_eq!(m.first().map(|c| c.name), Some("/model"));
        // A message that merely contains a slash mid-text is not command mode.
        assert!(chat_with("/model with args").slash_matches().is_empty());
    }

    #[test]
    fn enter_on_quit_command_emits_quit() {
        let mut chat = chat_with("/quit");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::Quit)));
        assert!(chat.input.is_empty(), "composer clears after a command");
    }

    #[test]
    fn enter_on_clear_command_clears_transcript_and_does_not_submit() {
        let mut chat = chat_with("/clear");
        chat.turns.push(Turn::User("hello".into()));
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // No SubmitPrompt — the menu intercepts Enter.
        assert!(!matches!(act, Some(Action::SubmitPrompt(_))));
        assert!(chat.turns.is_empty(), "/clear empties the transcript");
    }

    #[test]
    fn help_command_pushes_transcript_block() {
        let mut chat = chat_with("/help");
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(chat.turns.len(), 1);
        assert!(matches!(&chat.turns[0], Turn::Assistant(s) if s.contains("/quit")));
    }

    #[test]
    fn arrows_move_palette_selection() {
        let mut chat = chat_with("/"); // all commands shown
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(chat.menu_sel, 1);
        chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(chat.menu_sel, 0);
    }

    #[test]
    fn esc_dismisses_palette() {
        let mut chat = chat_with("/quit");
        chat.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(chat.input.is_empty());
    }

    #[test]
    fn slash_new_clears_and_requests_new_session() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::User("hi".into()));
        let act = chat.run_slash("/new", "");
        assert!(chat.turns.is_empty(), "/new wipes the transcript");
        assert!(matches!(act, Some(Action::NewSession)));
    }

    #[test]
    fn slash_model_routes_arg_and_usage() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/model", "anthropic/claude-opus-4-8") {
            Some(Action::SetModel(id)) => assert_eq!(id, "anthropic/claude-opus-4-8"),
            other => panic!("expected SetModel, got {other:?}"),
        }
        // Bare `/model` (and `/models`) open the picker — nobody memorizes ids.
        assert!(matches!(
            chat.run_slash("/model", ""),
            Some(Action::OpenModels)
        ));
        assert!(matches!(
            chat.run_slash("/models", ""),
            Some(Action::OpenModels)
        ));
    }

    #[test]
    fn slash_thinking_routes_levels_and_usage() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/thinking", "high") {
            Some(Action::SetThinking(Some(ThinkingLevel::High))) => {}
            other => panic!("expected SetThinking(high), got {other:?}"),
        }
        match chat.run_slash("/thinking", "default") {
            Some(Action::SetThinking(None)) => {}
            other => panic!("expected SetThinking(default), got {other:?}"),
        }
        match chat.run_slash("/thinking", "") {
            Some(Action::Status(s)) => assert!(s.contains("usage: /thinking"), "got: {s}"),
            other => panic!("expected usage Status, got {other:?}"),
        }
    }

    #[test]
    fn slash_login_without_args_opens_providers_popup() {
        // Bare `/login` opens the provider popup (OAuth + API keys); the
        // direct browser flow is still reachable via `/login claude|codex`.
        let mut chat = ChatComponent::default();

        let act = chat.run_slash("/login", "");

        assert!(matches!(act, Some(Action::OpenProviders)));
    }

    #[test]
    fn slash_providers_opens_popup() {
        let mut chat = ChatComponent::default();

        let act = chat.run_slash("/providers", "");

        assert!(matches!(act, Some(Action::OpenProviders)));
    }

    #[test]
    fn slash_permissions_opens_picker_even_while_a_turn_is_blocked() {
        let mut chat = chat_with("/permissions");
        chat.busy = true;

        let act = chat.handle_key(key(KeyCode::Enter));

        assert!(matches!(act, Some(Action::OpenPermissions)));
        assert!(chat.busy, "opening policy must not falsify turn lifecycle");
    }

    #[test]
    fn slash_copy_uses_last_reply() {
        let mut chat = ChatComponent::default();
        assert!(matches!(
            chat.run_slash("/copy", ""),
            Some(Action::Status(_))
        )); // nothing yet
        chat.turns.push(Turn::Assistant("the answer".into()));
        match chat.run_slash("/copy", "") {
            Some(Action::CopyToClipboard(t)) => assert_eq!(t, "the answer"),
            other => panic!("expected CopyToClipboard, got {other:?}"),
        }
    }

    #[test]
    fn soon_command_surfaces_honest_hint() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/compact", "") {
            Some(Action::Status(s)) => assert!(s.contains("not wired"), "got: {s}"),
            other => panic!("expected a Status hint, got {other:?}"),
        }
    }

    #[test]
    fn typed_command_line_routes_not_submits() {
        // A typed `/model <id>\n` line invokes the command instead of sending.
        let mut chat = chat_with("/model anthropic/claude-opus-4-8");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::SetModel(_))));
        assert!(
            chat.turns.is_empty(),
            "command line must not become a User turn"
        );
        // A `/`-path that is NOT a command still sends as a normal message.
        let mut chat2 = chat_with("/etc/hosts is a file");
        let act2 = chat2.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act2, Some(Action::SubmitPrompt(_))));
    }

    #[test]
    fn typed_login_codex_routes_login_action_not_user_turn() {
        let mut chat = chat_with("/login codex");

        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(act, Some(Action::Login(LoginTarget::Codex))));
        assert!(
            chat.turns.is_empty(),
            "command line must not become a User turn"
        );
    }

    #[test]
    fn foreign_extension_is_ignored() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("longhouse", json!({ "note": "not for us" })));
        chat.update(&extension("advisor", json!({ "note": 42 })));
        assert!(chat.turns.is_empty());
    }

    // ── composer history + kill ring (W2 slice) ───────────────────────────────

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_chars(chat: &mut ChatComponent, text: &str) {
        for c in text.chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn press_left(chat: &mut ChatComponent, count: usize) {
        for _ in 0..count {
            chat.handle_key(key(KeyCode::Left));
        }
    }

    fn composer_rows(chat: &mut ChatComponent, width: u16, height: u16) -> Vec<String> {
        let screen = render_chat_to_string(chat, width, height);
        let bar = g("▎", "|");
        let mut rows: Vec<String> = screen
            .lines()
            .rev()
            .take_while(|line| line.starts_with(bar))
            .map(|line| line.chars().skip(2).collect::<String>())
            .map(|line| line.trim_end().to_string())
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn review_regression_model_command_trims_extra_whitespace() {
        let mut chat = chat_with("/model  anthropic/claude-opus-4-8");

        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match act {
            Some(Action::SetModel(id)) => assert_eq!(id, "anthropic/claude-opus-4-8"),
            other => panic!("expected SetModel, got {other:?}"),
        }
    }

    #[test]
    fn review_regression_slash_tab_completion_resets_cursor_to_command_end() {
        let mut chat = chat_with("/mod");

        press_left(&mut chat, 1);
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "!");

        assert_eq!(chat.input, "/model!");
    }

    #[test]
    fn review_regression_mention_tab_completion_uses_token_at_cursor_and_preserves_suffix() {
        let mut chat = chat_with("fix @src now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 4);
        let wants_tab = chat.wants_tab();
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "X");

        assert_eq!(
            (wants_tab, chat.input.as_str()),
            (true, "fix @src/main.rs Xnow")
        );
    }

    #[test]
    fn review_regression_mention_after_multibyte_whitespace_is_utf8_safe() {
        let mut chat = chat_with("fix\u{00a0}@src now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 4);
        let wants_tab = chat.wants_tab();
        chat.handle_key(key(KeyCode::Tab));

        assert_eq!(
            (wants_tab, chat.input.as_str()),
            (true, "fix\u{00a0}@src/main.rs now")
        );
    }

    #[test]
    fn review_regression_mention_preserves_multiline_suffix_whitespace() {
        let mut chat = chat_with("fix @src\n  now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 6);
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "X");

        assert_eq!(chat.input, "fix @src/main.rs\n  Xnow");
    }

    #[test]
    fn cursor_readline_left_right_insert_at_cursor() {
        let mut chat = chat_with("abcd");

        press_left(&mut chat, 2);
        type_chars(&mut chat, "X");
        chat.handle_key(key(KeyCode::Right));
        type_chars(&mut chat, "Y");

        assert_eq!(chat.input, "abXcYd");
    }

    #[test]
    fn cursor_readline_ctrl_b_ctrl_f_move_by_character() {
        let mut chat = chat_with("rust");

        chat.handle_key(ctrl('b'));
        chat.handle_key(ctrl('b'));
        type_chars(&mut chat, "X");
        chat.handle_key(ctrl('f'));
        type_chars(&mut chat, "Y");

        assert_eq!(chat.input, "ruXsYt");
    }

    #[test]
    fn cursor_readline_backspace_delete_and_ctrl_d_edit_at_cursor() {
        let mut chat = chat_with("abcd");

        press_left(&mut chat, 2);
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "acd", "backspace removes the char before point");

        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(chat.input, "ad", "delete removes the char at point");

        chat.handle_key(ctrl('d'));
        assert_eq!(chat.input, "a", "⌃D also deletes the char at point");
    }

    #[test]
    fn cursor_readline_home_end_and_ctrl_a_ctrl_e_render_cursor_position() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("abc");

        chat.handle_key(key(KeyCode::Home));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("{cursor}abc"));

        chat.handle_key(key(KeyCode::End));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("abc{cursor}"));

        chat.handle_key(ctrl('a'));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("{cursor}abc"));

        chat.handle_key(ctrl('e'));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("abc{cursor}"));
    }

    #[test]
    fn cursor_readline_multiline_home_end_are_current_line_boundaries() {
        let mut chat = chat_with("one\ntwo three");

        press_left(&mut chat, 5);
        chat.handle_key(key(KeyCode::Home));
        type_chars(&mut chat, ">");
        chat.handle_key(key(KeyCode::End));
        type_chars(&mut chat, "<");

        assert_eq!(chat.input, "one\n>two three<");
    }

    #[test]
    fn cursor_readline_ctrl_k_u_w_and_yank_use_cursor_position() {
        let mut word_kill = chat_with("alpha beta gamma");
        press_left(&mut word_kill, 5);
        word_kill.handle_key(ctrl('w'));
        assert_eq!(word_kill.input, "alpha gamma");
        assert_eq!(
            word_kill.kill_ring.last().map(String::as_str),
            Some("beta ")
        );

        word_kill.handle_key(ctrl('y'));
        assert_eq!(word_kill.input, "alpha beta gamma");

        let mut prefix_kill = chat_with("alpha beta gamma");
        press_left(&mut prefix_kill, 5);
        prefix_kill.handle_key(ctrl('u'));
        assert_eq!(prefix_kill.input, "gamma");
        assert_eq!(
            prefix_kill.kill_ring.last().map(String::as_str),
            Some("alpha beta ")
        );

        let mut tail_kill = chat_with("alpha beta gamma");
        press_left(&mut tail_kill, 5);
        tail_kill.handle_key(ctrl('k'));
        assert_eq!(tail_kill.input, "alpha beta ");
        assert_eq!(
            tail_kill.kill_ring.last().map(String::as_str),
            Some("gamma")
        );
    }

    #[test]
    fn cursor_readline_multiline_ctrl_u_kills_only_current_line_prefix() {
        let mut chat = chat_with("first line\nsecond part");

        press_left(&mut chat, 4);
        chat.handle_key(ctrl('u'));

        assert_eq!(chat.input, "first line\npart");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("second "));
    }

    #[test]
    fn cursor_readline_ctrl_l_clears_idle_transcript_preserving_composer_and_cursor() {
        let mut chat = chat_with("abc");
        chat.turns.push(Turn::User("old prompt".into()));

        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(ctrl('l'));

        assert!(chat.turns.is_empty(), "⌃L clears an idle transcript");
        assert_eq!(chat.input, "abc", "⌃L preserves composer text");

        type_chars(&mut chat, "X");
        assert_eq!(chat.input, "abXc", "⌃L preserves cursor position");
    }

    #[test]
    fn cursor_readline_ctrl_l_noops_while_busy() {
        let mut chat = chat_with("abc");
        chat.busy = true;
        chat.turns.push(Turn::User("streaming prompt".into()));

        chat.handle_key(ctrl('l'));

        assert_eq!(chat.turns.len(), 1, "busy transcript is not cleared");
        assert_eq!(chat.input, "abc", "busy ⌃L preserves composer text");
    }

    #[test]
    fn cursor_readline_unicode_editing_treats_wide_chars_as_characters() {
        let mut chat = chat_with("a你🙂b");

        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "a你b", "backspace removes one Unicode scalar");

        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(
            chat.input, "a你",
            "delete removes the Unicode char at point"
        );
    }

    #[test]
    fn cursor_readline_render_wraps_cjk_emoji_before_cursor_by_visual_width() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("你🙂ab");

        chat.handle_key(key(KeyCode::Left));

        assert_eq!(
            composer_rows(&mut chat, 8, 8),
            vec![format!("你 🙂 a{cursor}"), "b".to_string(), String::new()]
        );
    }

    #[test]
    fn cursor_readline_render_can_place_cursor_on_wrapped_line_before_later_input() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("abcdefghijk\nzz");

        press_left(&mut chat, 3);

        assert_eq!(
            composer_rows(&mut chat, 12, 10),
            vec![
                "abcdefghij".to_string(),
                format!("k{cursor}"),
                "zz".to_string(),
                String::new()
            ]
        );
    }

    /// A chat seeded with an in-memory prompt history (no disk I/O: the default
    /// `PromptHistory` has no backing file, so `push` never persists).
    fn chat_with_hist(entries: &[&str]) -> ChatComponent {
        let mut chat = ChatComponent::default();
        for e in entries {
            chat.history.push(e);
        }
        chat
    }

    #[test]
    fn up_down_walks_history_when_composer_empty() {
        let mut chat = chat_with_hist(&["first", "second"]);
        chat.handle_key(key(KeyCode::Up)); // → newest
        assert_eq!(chat.input, "second");
        chat.handle_key(key(KeyCode::Up)); // → older
        assert_eq!(chat.input, "first");
        chat.handle_key(key(KeyCode::Up)); // clamps at oldest
        assert_eq!(chat.input, "first");
        chat.handle_key(key(KeyCode::Down)); // → newer
        assert_eq!(chat.input, "second");
        chat.handle_key(key(KeyCode::Down)); // past newest → restore draft (empty)
        assert_eq!(chat.input, "");
        assert!(chat.history_idx.is_none());
    }

    #[test]
    fn up_down_do_not_hijack_a_draft() {
        let mut chat = chat_with_hist(&["recall me"]);
        chat.input = "in-progress draft".into();
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(
            chat.input, "in-progress draft",
            "↑ must not clobber a draft"
        );
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(
            chat.input, "in-progress draft",
            "↓ no-ops when not navigating"
        );
    }

    #[test]
    fn history_dedupes_and_persists_on_submit() {
        let mut chat = chat_with("build it");
        chat.handle_key(key(KeyCode::Enter));
        chat.input = "build it".into(); // consecutive repeat
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(chat.history.len(), 1, "consecutive repeat is deduped");
        // ↑ recalls the submitted prompt.
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "build it");
    }

    #[test]
    fn ctrl_r_opens_fuzzy_history_search_reusing_slash_scorer() {
        let mut chat = chat_with_hist(&["cargo build", "git status", "cargo test"]);
        chat.handle_key(ctrl('r'));
        assert!(chat.search.is_some(), "⌃R opens the search overlay");
        for c in "crg".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
        // Fuzzy subsequence "crg" hits both cargo entries, newest first; the
        // ranking reuses slash::subseq_score.
        let texts: Vec<&str> = chat
            .search_matches()
            .iter()
            .map(|&i| chat.history.get(i).unwrap())
            .collect();
        assert_eq!(texts, vec!["cargo test", "cargo build"]);
        // Enter inserts the selected (top) match into the composer and closes.
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(chat.input, "cargo test");
        assert!(chat.search.is_none());
    }

    #[test]
    fn ctrl_r_search_esc_dismisses_without_touching_composer() {
        let mut chat = chat_with_hist(&["something"]);
        chat.input = "keep me".into();
        chat.handle_key(ctrl('r'));
        chat.handle_key(key(KeyCode::Esc));
        assert!(chat.search.is_none());
        assert_eq!(chat.input, "keep me");
    }

    #[test]
    fn kill_to_start_then_yank_roundtrips() {
        let mut chat = chat_with("hello world");
        chat.handle_key(ctrl('u'));
        assert_eq!(chat.input, "");
        assert_eq!(
            chat.kill_ring.last().map(String::as_str),
            Some("hello world")
        );
        chat.handle_key(ctrl('y'));
        assert_eq!(
            chat.input, "hello world",
            "⌃Y yanks the newest kill at the end"
        );
    }

    #[test]
    fn cursor_readline_ctrl_k_at_end_is_noop_and_mid_line_kills_tail() {
        let mut chat = chat_with("line one\nline two");

        chat.handle_key(ctrl('k'));
        assert_eq!(
            chat.input, "line one\nline two",
            "⌃K at end has no tail to kill"
        );
        assert!(chat.kill_ring.is_empty(), "empty kills are not recorded");

        press_left(&mut chat, 3);
        chat.handle_key(ctrl('k'));

        assert_eq!(chat.input, "line one\nline ");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("two"));
    }

    #[test]
    fn kill_ring_is_capped() {
        let mut chat = chat_with("");
        for i in 0..(KILL_RING_CAP + 3) {
            chat.input = format!("kill {i}");
            chat.handle_key(ctrl('u'));
        }
        assert_eq!(chat.kill_ring.len(), KILL_RING_CAP);
        // Newest survives; oldest was dropped from the front.
        assert_eq!(
            chat.kill_ring.last().map(String::as_str),
            Some(&format!("kill {}", KILL_RING_CAP + 2)[..])
        );
    }

    #[test]
    fn ctrl_y_precedence_permission_beats_yank() {
        let mut chat = chat_with("yankable");
        chat.handle_key(ctrl('u')); // ring = ["yankable"], composer empty
        let pid = PermissionId::new_v4();
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionRequest {
                tool: "bash".into(),
                reason: "rm -rf x".into(),
                args: json!({}),
            },
        ));
        // Permission pending → ⌃Y allows, does NOT yank.
        let act = chat.handle_key(ctrl('y'));
        assert!(matches!(
            act,
            Some(Action::PermissionDecided { permission_id, allow: true }) if permission_id == pid
        ));
        assert_eq!(
            chat.input, "",
            "yank must not fire while a permission is pending"
        );
        // Resolve the permission; now ⌃Y yanks.
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionDecision {
                allowed: true,
                reason: None,
            },
        ));
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "yankable");
    }

    // ── unknown-command feedback ────────────────────────────────────────────

    #[test]
    fn unknown_command_does_not_submit_as_prompt() {
        let mut chat = chat_with("/notacommand");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Must be a Status, not SubmittedPrompt.
        assert!(
            matches!(&act, Some(Action::Status(s)) if s.contains("unknown command /notacommand")),
            "expected Status with 'unknown command', got {act:?}"
        );
        // No user turn was pushed to the transcript.
        assert!(
            !chat.turns.iter().any(|t| matches!(t, Turn::User(_))),
            "unknown command must not create a user turn"
        );
    }
    #[test]
    fn unknown_command_near_match_suggests_correction() {
        // "/provder xyz" — whitespace closes the palette, so Enter reaches the
        // unknown-command branch instead of palette-run. The near-match path
        // only fires for the `/cmd args` form, not single-token palette use.
        let mut chat = chat_with("/provder xyz");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(&act, Some(Action::Status(s)) if s.contains("did you mean /providers")),
            "fuzzy near-match should suggest /providers, got {act:?}"
        );
        // Also surfaced in the transcript as an Assistant block.
        assert!(
            chat.turns.iter().any(|t| {
                matches!(t, Turn::Assistant(s) if s.contains("did you mean /providers"))
            }),
            "near-match hint should appear in transcript"
        );
    }
    #[test]
    fn path_like_slash_still_submits_as_prompt() {
        // "/etc/passwd hi" — looks like a path (has a second slash), not a
        // command name. Must fall through to normal chat submission.
        let mut chat = chat_with("/etc/passwd hi");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(&act, Some(Action::SubmitPrompt(s)) if s == "/etc/passwd hi"),
            "path-like slash input must submit as prompt, got {act:?}"
        );
    }

    #[test]
    fn slash_known_command_still_works() {
        // Regression: /help must still work as before.
        let mut chat = chat_with("/help");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // /help pushes to transcript, returns None (not a Status or Submit).
        assert!(
            act.is_none(),
            "/help should return None after pushing output, got {act:?}"
        );
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::Assistant(s) if s.contains("/quit"))),
            "/help should list commands in transcript"
        );
    }

    // ── wants_tab ────────────────────────────────────────────────────────────

    #[test]
    fn wants_tab_true_when_palette_open() {
        let chat = chat_with("/mod");
        assert!(
            chat.wants_tab(),
            "palette is open, Tab should route to chat"
        );
    }

    #[test]
    fn wants_tab_false_when_palette_closed() {
        let chat = chat_with("hello");
        assert!(!chat.wants_tab(), "no palette, Tab should cycle focus");
    }

    #[test]
    fn wants_tab_true_when_mention_picker_open() {
        let chat = chat_with("read @src/main.rs");
        assert!(
            chat.wants_tab(),
            "mention picker is open, Tab should route to chat"
        );
    }

    #[test]
    fn wants_tab_false_when_slash_has_whitespace() {
        // "/model gpt" — palette closed (space typed), Tab should cycle focus.
        let chat = chat_with("/model gpt");
        assert!(!chat.wants_tab());
    }

    #[test]
    fn welcome_empty_state_disappears_after_first_transcript_turn() {
        let mut chat = ChatComponent {
            welcome_provider_line: Some("provider sentinel".into()),
            ..Default::default()
        };

        let empty = render_chat_to_string(&mut chat, 80, 24);
        assert!(
            !empty.contains("OCEAN"),
            "welcome must not render product branding"
        );
        assert!(
            !empty.contains('⏎') && !empty.contains('⌃') && !empty.contains("/login"),
            "welcome must not print instruction hints, got: {empty:?}"
        );
        assert!(
            empty.contains("provider sentinel"),
            "empty transcript should show the provider status line"
        );

        chat.turns.push(Turn::User("hello ocean".into()));
        let filled = render_chat_to_string(&mut chat, 80, 24);

        assert!(
            filled.contains("hello ocean"),
            "existing transcript should render the user turn"
        );
        assert!(
            !filled.contains("provider sentinel"),
            "welcome provider line must not appear once transcript is non-empty"
        );
    }

    #[test]
    fn welcome_without_condition_renders_nothing() {
        let mut chat = ChatComponent::default();
        let empty = render_chat_to_string(&mut chat, 80, 24);
        assert!(
            !empty.contains("OCEAN") && !empty.contains('⏎') && !empty.contains("/help"),
            "a bare empty chat renders no branding or hints, got: {empty:?}"
        );
    }

    #[test]
    fn pending_permission_card_prints_no_key_instructions() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::Permission {
            permission_id: PermissionId::new_v4(),
            tool: "bash".into(),
            reason: "wants to run a command".into(),
            resolved: None,
        });
        let screen = render_chat_to_string(&mut chat, 80, 24);
        assert!(
            screen.contains("approval needed"),
            "pending card still renders its condition"
        );
        assert!(
            screen.contains("wants to run a command"),
            "pending card still renders the reason"
        );
        assert!(
            !screen.contains('⌃') && !screen.contains("allow · "),
            "no allow/deny key instructions on the card, got: {screen:?}"
        );
    }

    #[test]
    fn live_thinking_marker_has_no_char_count() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.turns.push(Turn::Thinking("abcdef".into()));
        let screen = render_chat_to_string(&mut chat, 80, 12);
        assert!(
            screen.contains("thinking"),
            "live-tail thinking marker renders"
        );
        assert!(!screen.contains("chars"), "no character counters");
    }

    #[test]
    fn footer_is_blank_when_idle_streaming_or_scrolled() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::User("hi".into()));
        let idle = render_chat_to_string(&mut chat, 80, 12);
        assert!(
            !idle.contains('⏎') && !idle.contains("commands"),
            "idle footer carries no key legend, got: {idle:?}"
        );
        chat.busy = true;
        let busy = render_chat_to_string(&mut chat, 80, 12);
        assert!(
            !busy.contains("streaming"),
            "activity belongs to the bottom status row, not the chat footer"
        );
        // Detached from the live tail: still blank — no scroll counter either.
        for i in 0..30 {
            chat.turns.push(Turn::User(format!("turn {i}")));
        }
        chat.scroll_back = 5;
        let scrolled = render_chat_to_string(&mut chat, 80, 12);
        assert!(
            chat.scroll_back > 0,
            "test premise: the view really is scrolled back"
        );
        assert!(
            !scrolled.contains("lines back") && !scrolled.contains("PgDn"),
            "scrolled footer prints no counter or key hint, got: {scrolled:?}"
        );
    }

    // ── activity accessor: busy/tool-derived, never stale ──────────────────

    #[test]
    fn activity_is_none_when_idle_even_with_stale_running_marker() {
        let mut chat = ChatComponent::default();
        assert_eq!(chat.activity(), None);
        // A Running marker left behind by an aborted turn must not leak once
        // busy clears (TurnFinished does not rewrite tool statuses).
        let id = add_tool(&mut chat, "bash", "");
        if let Some(Turn::Tool { status, .. }) = chat.tool_by_id(&id) {
            *status = ToolStatus::Running;
        }
        chat.busy = false;
        assert_eq!(chat.activity(), None, "idle chat reports no activity");
    }

    #[test]
    fn activity_names_newest_running_tool_then_falls_back_to_working() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        assert_eq!(chat.activity(), Some("working"), "busy with no tool yet");
        chat.turns.push(ok_tool("read", ""));
        let id = add_tool(&mut chat, "bash", "");
        if let Some(Turn::Tool { status, .. }) = chat.tool_by_id(&id) {
            *status = ToolStatus::Running;
        }
        assert_eq!(chat.activity(), Some("bash"), "newest running tool wins");
        if let Some(Turn::Tool { status, .. }) = chat.tool_by_id(&id) {
            *status = ToolStatus::Ok;
        }
        assert_eq!(
            chat.activity(),
            Some("working"),
            "finished tool falls back to working while the turn streams"
        );
        chat.update(&turn_finished(AgentTurnStatus::Completed, None));
        assert_eq!(chat.activity(), None, "turn completion clears activity");
    }
    // ── turn-terminal paths ──────────────────────────────────────────────────

    fn turn_finished(status: AgentTurnStatus, error: Option<&str>) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnFinished {
            session_id: AgentSessionId(Uuid::nil()),
            turn_id: AgentTurnId(Uuid::nil()),
            status,
            error: error.map(|s| s.to_string()),
            wall_ms: None,
            output_tokens: None,
            input_tokens: None,
            cache_read_tokens: None,
            tokens_per_second: None,
            context_usage: None,
        }))
    }

    #[test]
    fn turn_finished_with_error_pushes_error_notice_and_clears_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            Some("HTTP 401 Unauthorized"),
        ));
        assert!(!chat.busy, "busy must be cleared on TurnFinished");
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::ErrorNotice { note } if note.contains("turn failed"))),
            "failed turn with error should push an ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_provider_auth_error_uses_login_recovery_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };

        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            Some("{\"error\":{\"message\":\"token_invalidated\"}}"),
        ));

        assert!(!chat.busy, "terminal failed event must clear busy");
        let notice = chat.turns.iter().find_map(|t| match t {
            Turn::ErrorNotice { note } => Some(note.as_str()),
            _ => None,
        });
        assert!(
            matches!(notice, Some(note) if note.contains("run /login to reconnect")),
            "provider credential failures should render /login recovery guidance, got {notice:?}"
        );
        assert!(
            !chat.turns.iter().any(|t| matches!(t, Turn::Advisor { .. })),
            "turn-terminal failures should render as ErrorNotice, not advisor cards"
        );
    }

    #[test]
    fn turn_finished_failed_no_error_renders_fallback() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            chat.turns.iter().any(
                |t| matches!(t, Turn::ErrorNotice { note } if note.contains("no error detail"))
            ),
            "failed turn without error should push ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_cancelled_pushes_error_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Cancelled,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::ErrorNotice { note } if note.contains("turn failed"))),
            "cancelled turn should push an ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_completed_clears_busy_without_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Completed,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            !chat
                .turns
                .iter()
                .any(|t| matches!(t, Turn::Advisor { .. } | Turn::ErrorNotice { .. })),
            "successful turns should push neither advisor nor error notice"
        );
    }

    // ── SSE reconnect does not clear busy ────────────────────────────────────

    #[test]
    fn stream_reconnect_does_not_clear_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&Action::Status("stream reconnected".into()));
        assert!(
            chat.busy,
            "reconnect must not clear busy — only terminal events do"
        );
        assert!(chat.turns.is_empty(), "reconnect must not push turns");
    }

    #[test]
    fn stream_reconnecting_during_busy_does_not_clear_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&Action::Status("stream reconnecting…".into()));
        assert!(chat.busy, "reconnecting must not clear busy");
        assert!(chat.turns.is_empty(), "reconnecting must not push turns");
    }

    // ── sanitize_line at error-notice boundary ───────────────────────────────

    #[test]
    fn sanitize_line_strips_control_chars() {
        let input = "hello\x1b[31mworld\x07\t tab\r\nnew";
        let got = sanitize_line(input);
        assert!(!got.contains('\x1b'), "ESC stripped");
        assert!(!got.contains('\x07'), "BEL stripped");
        assert!(!got.contains('\r'), "CR stripped");
        assert!(!got.contains('\n'), "LF stripped");
        assert!(got.contains("    "), "tab expanded to spaces");
        assert!(got.contains("hello"), "normal chars preserved");
        assert!(got.contains("world"), "normal chars preserved");
        assert!(got.contains(" tab"), "normal chars preserved");
        assert!(got.contains("new"), "normal chars preserved");
    }

    // ── bracketed paste into the composer ─────────────────────────────────

    #[test]
    fn paste_inserts_multiline_without_submitting() {
        let mut chat = ChatComponent::default();
        let act = chat.handle_event(&crossterm::event::Event::Paste(
            "one\ntwo\r\nthree".to_string(),
        ));
        assert!(act.is_none(), "paste must never submit");
        assert_eq!(chat.input, "one\ntwo\nthree");
        assert!(!chat.busy, "paste must not mark the chat busy");
    }

    #[test]
    fn paste_strips_controls_and_expands_tabs() {
        let mut chat = ChatComponent::default();
        chat.handle_event(&crossterm::event::Event::Paste(
            "a\tb\x1b[31mc\x07".to_string(),
        ));
        assert_eq!(chat.input, "a    b[31mc");
    }

    #[test]
    fn paste_feeds_search_query_while_overlay_open() {
        let mut chat = ChatComponent::default();
        chat.handle_key(ctrl('r'));
        assert!(chat.search.is_some(), "⌃R opens the search overlay");
        chat.handle_event(&crossterm::event::Event::Paste("cargo test\n".to_string()));
        assert_eq!(
            chat.search.as_ref().map(|s| s.query.as_str()),
            Some("cargo test"),
            "paste feeds the query, newline dropped"
        );
        assert!(
            chat.input.is_empty(),
            "composer untouched while search open"
        );
    }

    // ── tool drawers: per-call expansion, sanitized bodies, hit testing ────

    fn ok_tool(name: &str, output: &str) -> Turn {
        Turn::Tool {
            id: ocean_agent_sdk::ToolCallId::new_v4(),
            name: name.to_string(),
            args_json: serde_json::Value::Null,
            output: output.to_string(),
            status: ToolStatus::Ok,
            diff: None,
            expanded: false,
        }
    }

    /// Push a closed ok-status tool drawer and return its call id, so tests can
    /// track per-call expansion independently.
    fn add_tool(chat: &mut ChatComponent, name: &str, output: &str) -> ToolCallId {
        let id = ocean_agent_sdk::ToolCallId::new_v4();
        chat.turns.push(Turn::Tool {
            id: id.clone(),
            name: name.to_string(),
            args_json: serde_json::Value::Null,
            output: output.to_string(),
            status: ToolStatus::Ok,
            diff: None,
            expanded: false,
        });
        id
    }

    /// Read one drawer's local `expanded` flag by call id.
    fn expanded_of(chat: &ChatComponent, id: &ToolCallId) -> bool {
        chat.turns
            .iter()
            .find_map(|t| match t {
                Turn::Tool {
                    id: tid, expanded, ..
                } if tid == id => Some(*expanded),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Build a left-button mouse event at an absolute screen row (crossterm
    /// 0.28 has no `MouseEvent::new`; use a struct literal).
    fn mouse_at(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 2,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// Simulate a full left-click (Down then Up, no Drag) at a screen row —
    /// the toggle commits on Up, so Down alone is not a click.
    fn click_row(chat: &mut ChatComponent, row: u16) {
        chat.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), row));
        chat.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), row));
    }

    #[test]
    fn one_line_drops_escape_bytes() {
        let s = one_line("ok \x1b[2J\x1b[H wiped", 64);
        assert!(!s.contains('\x1b'), "ESC must not reach a Span: {s:?}");
        assert!(s.contains("ok"), "printable text survives");
    }

    #[test]
    fn collapsed_summary_never_paints_control_bytes() {
        let mut chat = ChatComponent::default();
        chat.turns.push(ok_tool("bash", "done\x1b[2J\x07"));
        let screen = render_chat_to_string(&mut chat, 80, 12);
        assert!(!screen.contains('\x1b'), "collapsed summary leaked ESC");
        assert!(!screen.contains('\x07'), "collapsed summary leaked BEL");
        assert!(screen.contains("bash"), "card header renders");
    }

    #[test]
    fn every_drawer_header_stays_visible_no_burst_hiding() {
        // Burst compaction was removed: every tool drawer paints its own header
        // no matter how many run consecutively. A tall viewport shows them all;
        // there is no "earlier tools" compaction row.
        let mut chat = ChatComponent::default();
        for i in 0..6 {
            chat.turns.push(ok_tool(&format!("tool{i}"), "ok"));
        }
        let screen = render_chat_to_string(&mut chat, 90, 24);
        for i in 0..6 {
            assert!(
                screen.contains(&format!("tool{i}")),
                "drawer {i} header must stay visible: {screen:?}"
            );
        }
        assert!(
            !screen.contains("earlier tools"),
            "burst hiding was removed"
        );
    }

    #[test]
    fn alt_keys_focus_and_toggle_only_the_targeted_drawer() {
        let mut chat = ChatComponent::default();
        let alpha = add_tool(&mut chat, "alpha", "x");
        let beta = add_tool(&mut chat, "beta", "y");

        // First Alt-Down from no focus lands on the first drawer.
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(chat.focused_drawer, Some(alpha.clone()));

        // Alt-Space toggles ONLY the focused drawer (alpha); beta stays closed.
        chat.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::ALT));
        assert!(expanded_of(&chat, &alpha), "focused drawer opened");
        assert!(!expanded_of(&chat, &beta), "untargeted drawer untouched");

        // Alt-Enter toggles it back closed.
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert!(!expanded_of(&chat, &alpha), "Alt-Enter toggles back");

        // Traversal wraps end-to-end: alpha -> beta -> alpha (last wraps to
        // first), and Alt-Up from the first wraps back to the last.
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)); // alpha -> beta
        assert_eq!(chat.focused_drawer, Some(beta.clone()));
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)); // beta -> alpha
        assert_eq!(chat.focused_drawer, Some(alpha.clone()));
        chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)); // alpha -> beta
        assert_eq!(chat.focused_drawer, Some(beta.clone()));

        // Plain Space and Enter never toggle a drawer — the composer owns them.
        let beta_was = expanded_of(&chat, &beta);
        chat.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            expanded_of(&chat, &beta),
            beta_was,
            "plain keys don't toggle"
        );
        assert_eq!(
            chat.focused_drawer,
            Some(beta.clone()),
            "focus unchanged by plain keys"
        );
    }

    #[test]
    fn collapsed_edit_drawer_hides_body_until_opened() {
        let edit_turn = |open| Turn::Tool {
            id: ocean_agent_sdk::ToolCallId::new_v4(),
            name: "edit".to_string(),
            args_json: serde_json::Value::Null,
            output: String::new(),
            status: ToolStatus::Ok,
            diff: Some(crate::shell::diff::string_rows("old_alpha\n", "new_beta\n")),
            expanded: open,
        };
        // Collapsed: just the header — no diff body leaks into the transcript.
        let mut closed = ChatComponent::default();
        closed.turns.push(edit_turn(false));
        let screen = render_chat_to_string(&mut closed, 90, 16);
        assert!(
            !screen.contains("new_beta"),
            "collapsed drawer hides diff body: {screen:?}"
        );
        assert!(screen.contains("edit"), "header still renders");
        // Per-call open: the bounded diff body paints.
        let mut open = ChatComponent::default();
        open.turns.push(edit_turn(true));
        let screen = render_chat_to_string(&mut open, 90, 16);
        assert!(
            screen.contains("new_beta"),
            "open drawer shows its diff rows"
        );
    }

    #[test]
    fn mouse_click_toggles_the_drawn_header_after_wrap_and_scroll() {
        // A long assistant line that WRAPS at this narrow width sits above two
        // tool drawers, pushing their headers down by many wrapped rows; the
        // tall content also forces nonzero scroll. The click must still hit the
        // drawer whose header is actually painted under the cursor — the exact
        // logical-line-vs-wrapped-row trap. We do NOT trust the hit map to find
        // a header: we scan the RENDERED SCREEN for the tool name, require the
        // per-frame hit map to land on that same painted row, then click.
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::Assistant("word ".repeat(200)));
        let beta = add_tool(&mut chat, "beta", "out b");
        let alpha = add_tool(&mut chat, "alpha", "out a");

        let screen = render_chat_to_string(&mut chat, 40, 24);
        let row_of = |needle: &str| -> u16 {
            screen
                .lines()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} header painted: {screen:?}")) as u16
        };
        let alpha_row = row_of("alpha");
        let beta_row = row_of("beta");
        assert_ne!(alpha_row, beta_row, "two distinct header rows");
        // If the hit map mapped logical line index -> row directly, the wrapping
        // assistant text plus the scroll would put its entry on the WRONG row
        // and these assertions fail.
        assert!(
            chat.drawer_hits.iter().any(|h| h.row == alpha_row),
            "hit map covers painted alpha row {alpha_row}: {:?}",
            chat.drawer_hits
        );
        assert!(
            chat.drawer_hits.iter().any(|h| h.row == beta_row),
            "hit map covers painted beta row {beta_row}: {:?}",
            chat.drawer_hits
        );

        // Click alpha's painted row: only alpha opens.
        click_row(&mut chat, alpha_row);
        assert!(expanded_of(&chat, &alpha), "click opened alpha");
        assert!(
            !expanded_of(&chat, &beta),
            "beta untouched by alpha's click"
        );

        // Re-render: alpha (the bottom-pinned last turn) grew a body, so beta's
        // header shifts UP. Re-derive beta's row from the NEW screen and click
        // it — the shifted geometry still routes to beta, and alpha stays open.
        let screen2 = render_chat_to_string(&mut chat, 40, 24);
        let beta_row2 = screen2
            .lines()
            .position(|l| l.contains("beta"))
            .unwrap_or_else(|| panic!("beta header painted after alpha opened: {screen2:?}"))
            as u16;
        assert_ne!(beta_row2, beta_row, "opening alpha shifted beta up");
        click_row(&mut chat, beta_row2);
        assert!(
            expanded_of(&chat, &beta),
            "click opened beta at its shifted row"
        );
        assert!(
            expanded_of(&chat, &alpha),
            "alpha stayed open — drawers are independent"
        );
    }

    #[test]
    fn error_notice_render_strips_terminal_control_chars() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::ErrorNotice {
            note: "✗ turn failed — raw\tbad\x1b[31m\rline".into(),
        });

        let screen = render_chat_to_string(&mut chat, 80, 12);

        assert!(
            !screen.contains('\t'),
            "rendered notice must not contain tabs"
        );
        assert!(
            !screen.contains('\x1b'),
            "rendered notice must not contain ESC"
        );
        assert!(
            !screen.contains('\r'),
            "rendered notice must not contain CR"
        );
        assert!(
            screen.contains("raw    bad[31mline"),
            "sanitized notice should preserve readable text and expand tabs, got: {screen:?}"
        );
    }

    // ── sanitize_line on user prompt render path ─────────────────────────

    #[test]
    fn user_prompt_render_strips_control_chars() {
        let mut chat = ChatComponent::default();
        chat.turns
            .push(Turn::User("hey\x1b[31mthere\tbad\rline".into()));

        let screen = render_chat_to_string(&mut chat, 80, 12);

        assert!(!screen.contains('\t'), "user prompt must not contain tabs");
        assert!(!screen.contains('\x1b'), "user prompt must not contain ESC");
        assert!(!screen.contains('\r'), "user prompt must not contain CR");
        assert!(
            screen.contains("hey[31mthere    badline"),
            "sanitized user prompt should preserve readable text, got: {screen:?}"
        );
    }

    // ── TurnSendFailed prefix ────────────────────────────────────────────

    #[test]
    fn turn_send_failed_connect_uses_daemon_prefix() {
        let mut chat = ChatComponent::default();
        chat.update(&Action::TurnSendFailed {
            prompt: "hi".into(),
            err: "tcp connect error: Connection refused (os error 61)".into(),
        });
        assert_eq!(chat.turns.len(), 1, "should push one Assistant turn");
        let Turn::Assistant(msg) = &chat.turns[0] else {
            panic!("expected Assistant turn");
        };
        assert!(
            msg.contains("couldn't reach the daemon"),
            "connect error should use daemon prefix, got: {msg}"
        );
    }

    #[test]
    fn turn_send_failed_non_connect_uses_neutral_prefix() {
        let mut chat = ChatComponent::default();
        chat.update(&Action::TurnSendFailed {
            prompt: "hi".into(),
            err: "turn: HTTP 401 Unauthorized".into(),
        });
        assert_eq!(chat.turns.len(), 1, "should push one Assistant turn");
        let Turn::Assistant(msg) = &chat.turns[0] else {
            panic!("expected Assistant turn");
        };
        assert!(
            msg.contains("turn could not start"),
            "non-connect error should use neutral prefix, got: {msg}"
        );
        assert!(
            !msg.contains("couldn't reach the daemon"),
            "non-connect error must not use daemon prefix, got: {msg}"
        );
    }

    #[test]
    fn unknown_turn_outcome_never_restores_prompt() {
        let mut chat = ChatComponent {
            busy: true,
            ..ChatComponent::default()
        };
        chat.update(&Action::TurnOutcomeUnknown {
            err: "turn: operation timed out".into(),
        });

        assert!(!chat.busy, "unknown response must unwind the spinner");
        assert!(
            chat.input.is_empty(),
            "unknown outcome must not restore a potentially side-effecting prompt"
        );
        let Turn::Assistant(msg) = &chat.turns[0] else {
            panic!("expected warning assistant turn");
        };
        assert!(msg.contains("may already be running"));
        assert!(msg.contains("not restored"));
    }

    // ── spec step 6: drawer body window, streaming invariance, mouse arming ──

    #[test]
    fn tool_runs_single_space_across_hidden_thinking() {
        // Tool -> suppressed Thinking -> Tool must render ADJACENT headers:
        // the hidden thinking emits nothing, so it must not break the run.
        // A non-tool boundary (user turn above) keeps its blank separator.
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::User("go".into()));
        let _a = add_tool(&mut chat, "alpha", "out a");
        chat.turns.push(Turn::Thinking("hidden reasoning".into()));
        let _b = add_tool(&mut chat, "beta", "out b");
        let screen = render_chat_to_string(&mut chat, 80, 20);
        let rows: Vec<&str> = screen.lines().collect();
        let row_of = |needle: &str| {
            rows.iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not painted: {screen:?}"))
        };
        let (user_row, a_row, b_row) = (row_of("go"), row_of("alpha"), row_of("beta"));
        assert_eq!(
            b_row,
            a_row + 1,
            "hidden thinking must not double-space a tool run"
        );
        assert!(
            a_row > user_row + 1,
            "user -> tool boundary keeps its blank separator"
        );
    }

    #[test]
    fn open_drawer_marker_paints_above_the_tail_and_oldest_rows_are_hidden() {
        // 45 output lines in an open drawer: exactly the newest 40 paint, the
        // "5 earlier lines" omission marker sits ABOVE the tail (the omitted
        // rows are chronologically older than every visible one), and the
        // oldest line's text is gone from the screen entirely.
        let mut chat = ChatComponent::default();
        let out: String = (1..=45).map(|i| format!("row-{i:03}\n")).collect();
        let id = add_tool(&mut chat, "bash", &out);
        chat.toggle_drawer(&id);
        let screen = render_chat_to_string(&mut chat, 80, 55);

        let body_rows = screen.lines().filter(|l| l.contains("row-0")).count();
        assert_eq!(body_rows, 40, "exactly DRAWER_BODY_ROWS tail rows paint");
        assert!(
            !screen.contains("row-001"),
            "oldest hidden line never paints"
        );
        let marker_idx = screen
            .lines()
            .position(|l| l.contains("5 earlier lines"))
            .expect("omission marker painted");
        let first_tail_idx = screen
            .lines()
            .position(|l| l.contains("row-006"))
            .expect("oldest visible tail row painted");
        assert!(
            marker_idx < first_tail_idx,
            "marker (row {marker_idx}) must paint ABOVE the tail (first tail row {first_tail_idx})"
        );
    }

    #[test]
    fn open_running_tool_with_empty_output_shows_placeholder() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::Tool {
            id: ocean_agent_sdk::ToolCallId::new_v4(),
            name: "bash".to_string(),
            args_json: serde_json::Value::Null,
            output: String::new(),
            status: ToolStatus::Running,
            diff: None,
            expanded: true,
        });
        let screen = render_chat_to_string(&mut chat, 80, 12);
        assert!(
            screen.contains("(no output yet)"),
            "open Running drawer with no output shows the placeholder: {screen:?}"
        );
    }

    #[test]
    fn streaming_events_never_flip_a_calls_local_expanded() {
        use ocean_agent_sdk::{ToolCall, ToolResult};
        let sid = AgentSessionId::new_v4();
        let tid = AgentTurnId::new_v4();
        let cid = ocean_agent_sdk::ToolCallId::new_v4();
        let started = |id: &ToolCallId| {
            Action::AgentEvent(Box::new(AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ToolCall {
                    id: id.clone(),
                    name: "bash".to_string(),
                    args_json: serde_json::Value::Null,
                },
            }))
        };
        let chunk = |id: &ToolCallId| {
            Action::AgentEvent(Box::new(AgentTurnEvent::ToolCallChunk {
                session_id: sid,
                turn_id: tid,
                call_id: id.clone(),
                chunk: "streamed\n".to_string(),
            }))
        };
        let finished = |id: &ToolCallId| {
            Action::AgentEvent(Box::new(AgentTurnEvent::ToolCallFinished {
                session_id: sid,
                turn_id: tid,
                call_id: id.clone(),
                result: ToolResult {
                    ok: true,
                    output: "done".to_string(),
                    metadata_json: None,
                },
            }))
        };

        let mut chat = ChatComponent::default();
        chat.update(&started(&cid));
        assert!(!expanded_of(&chat, &cid), "a Running tool starts closed");

        // Opened locally: chunks and completion must not flip it back.
        chat.toggle_drawer(&cid);
        chat.update(&chunk(&cid));
        assert!(
            expanded_of(&chat, &cid),
            "ToolCallChunk must not flip expanded"
        );
        chat.update(&finished(&cid));
        assert!(
            expanded_of(&chat, &cid),
            "ToolCallFinished must not flip expanded"
        );

        // Left closed: streaming must not open it either.
        let cid2 = ocean_agent_sdk::ToolCallId::new_v4();
        chat.update(&started(&cid2));
        chat.update(&chunk(&cid2));
        chat.update(&finished(&cid2));
        assert!(
            !expanded_of(&chat, &cid2),
            "streaming must never open a closed drawer"
        );
    }

    #[test]
    fn ctrl_o_override_opens_bodies_while_local_state_stays_independent() {
        let mut chat = ChatComponent::default();
        let alpha = add_tool(&mut chat, "alpha", "alpha-body-text");
        let beta = add_tool(&mut chat, "beta", "beta-body-text");

        // Global ⌃O: both bodies paint even though every local flag is closed.
        chat.handle_key(ctrl('o'));
        let screen = render_chat_to_string(&mut chat, 80, 24);
        assert!(screen.contains("alpha-body-text"), "⌃O opens alpha's body");
        assert!(screen.contains("beta-body-text"), "⌃O opens beta's body");
        assert!(
            !expanded_of(&chat, &alpha) && !expanded_of(&chat, &beta),
            "the global override never rewrites per-call local flags"
        );

        // Override off: local state is still independent afterward.
        chat.handle_key(ctrl('o'));
        chat.toggle_drawer(&alpha);
        let screen = render_chat_to_string(&mut chat, 80, 24);
        assert!(screen.contains("alpha-body-text"), "local open survives ⌃O");
        assert!(
            !screen.contains("beta-body-text"),
            "beta stays closed — locals are independent"
        );
    }

    #[test]
    fn click_during_active_overlay_does_not_toggle_drawer() {
        let mut chat = ChatComponent::default();
        let id = add_tool(&mut chat, "alpha", "x");
        let screen = render_chat_to_string(&mut chat, 80, 16);
        let row = screen
            .lines()
            .position(|l| l.contains("alpha"))
            .expect("header painted") as u16;

        // ⌃R search overlay up: its rows paint OVER the transcript, so a click
        // on the header row must not fall through to the drawer underneath.
        chat.search = Some(HistorySearch::default());
        click_row(&mut chat, row);
        assert!(
            !expanded_of(&chat, &id),
            "search-overlay click fell through"
        );

        // `/` palette up: same guard.
        chat.search = None;
        chat.input = "/mod".into();
        click_row(&mut chat, row);
        assert!(
            !expanded_of(&chat, &id),
            "palette-overlay click fell through"
        );

        // Overlays dismissed: the very same click toggles again.
        chat.input.clear();
        click_row(&mut chat, row);
        assert!(expanded_of(&chat, &id), "plain click still toggles");
    }

    #[test]
    fn down_drag_up_sweep_does_not_toggle_drawer() {
        // A drag-to-select-text gesture arms on the same Down as a click; the
        // Drag in between must disarm the pending toggle so sweeping a text
        // selection across a header never flips it.
        let mut chat = ChatComponent::default();
        let id = add_tool(&mut chat, "alpha", "x");
        let screen = render_chat_to_string(&mut chat, 80, 16);
        let row = screen
            .lines()
            .position(|l| l.contains("alpha"))
            .expect("header painted") as u16;

        chat.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), row));
        chat.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), row));
        chat.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), row));
        assert!(
            !expanded_of(&chat, &id),
            "a selection sweep is not a click — no toggle"
        );
        assert!(chat.focused_drawer.is_none(), "sweep must not steal focus");
    }

    #[test]
    fn palette_render_contains_no_diamond_glyph() {
        let mut chat = chat_with("/");
        let screen = render_chat_to_string(&mut chat, 80, 24);
        assert!(
            screen.contains("commands"),
            "palette popup painted: {screen:?}"
        );
        assert!(
            !screen.contains('◆'),
            "no decorative diamond anywhere on the palette screen"
        );
    }
}
